//! Workspace-wide call-site enumeration for the wrapper analyzer's
//! "low reuse" axis.
//!
//! [`extract_call_sites`] walks a parsed Rust file with `syn::visit::Visit`
//! and yields one [`CallSite`] per function-call or method-call expression.
//! Each site is tagged with the qualified name of the enclosing function
//! (e.g. `Service::handle`), so the analyzer can group call sites by
//! caller.
//!
//! # Heuristic limits
//!
//! * **Name-only matching.** Calls are recorded by their last path
//!   segment — `crate::a::foo()`, `Self::foo()`, `obj.foo()`, and a bare
//!   `foo()` all collapse into the same `foo` bucket. Same-named methods
//!   on different types are indistinguishable.
//! * **No macro expansion.** Calls invoked via macros are invisible to
//!   `syn` and therefore to the visitor.
//! * **`#[cfg(test)]` modules are skipped.** Test scaffolding is
//!   forwarding by design and would inflate reuse counts without
//!   reflecting production usage. This matches the existing wrapper
//!   detector's policy.
//!
//! Treat the result as guidance for an LLM, not as a precise call graph.

use std::collections::{BTreeMap, HashSet};

use lens_domain::{
    CallShape, ImportShape, LexicalResolutionStatus, ReceiverExprKind, SyntaxFact, qualify,
    qualify_module,
};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Block, Expr, ExprCall, ExprMethodCall, FnArg, GenericArgument, GenericParam, Generics,
    ImplItem, Item, ItemFn, ItemUse, Local, Pat, PathArguments, Signature, TraitItem, Type,
    TypeParamBound, UseTree, WherePredicate,
};

use crate::attrs::has_cfg_test;
use crate::common::{render_tokens, type_path_last_ident};
use crate::parser::RustParseError;

/// One call-site occurrence inside a Rust source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    /// Last path segment of the callee — `foo` for `crate::a::foo()`,
    /// `bar` for `obj.bar()`. `None` when the callee expression is not
    /// a plain path (e.g. `(closures())()`); such sites are still
    /// counted because they are calls, just not attributable to a name.
    pub callee_name: Option<String>,
    /// Rendered callee path when the syntax exposes one. For free calls
    /// this can be a qualified Rust path such as `crate::a::foo`; for
    /// method calls it includes the receiver expression, e.g.
    /// `self.inner.handle`.
    pub callee_path: Option<String>,
    /// Qualified name of the function this call is written inside,
    /// e.g. `Service::handle`. `None` for calls at module scope (a
    /// `const` initialiser, a top-level `let` in a binary's `main`-less
    /// stub, etc.).
    pub caller_name: Option<String>,
    /// Absolute lexical module containing this call site.
    pub module: String,
    /// Absolute lexical name of the enclosing function, rooted at `crate`.
    pub caller_qualified_name: Option<String>,
    /// `impl` self-type or trait name of the enclosing function, when known.
    pub caller_impl_owner: Option<String>,
    /// Whether this was a free/path call or a receiver method call.
    pub call_kind: CallKind,
    /// True when the callee is a bare name the enclosing function binds
    /// to a callable — a closure or nested `fn` held in a `let`, or a
    /// parameter of `Fn`/`FnMut`/`FnOnce`/`fn` type. The binding shadows
    /// every definition outside the function, so such a call has no
    /// workspace target.
    pub callee_is_locally_bound: bool,
    /// Lexically visible `use` aliases at this call site.
    pub visible_aliases: Vec<UseAlias>,
    /// 1-based line number of the call expression.
    pub line: usize,
}

/// Syntactic shape of a call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind {
    /// `foo()`, `crate::a::foo()`, `Self::foo()`, etc.
    Path,
    /// `receiver.foo()`. The receiver type is unknown without semantic
    /// analysis, so function-graph resolution keeps these unresolved.
    ReceiverMethod,
}

/// One imported local name visible at a call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UseAlias {
    pub alias: String,
    pub target: String,
}

/// Filtering knobs for [`extract_call_sites_with_options`].
///
/// The default preserves [`extract_call_sites`]'s historical wrapper
/// behaviour: skip `#[cfg(test)]` blocks so test scaffolding does not
/// inflate reuse counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CallIndexOptions {
    pub include_cfg_test_blocks: bool,
}

/// Walk `source` and return every function/method call expression. See
/// the module docstring for the heuristics this applies.
pub fn extract_call_sites(source: &str) -> Result<Vec<CallSite>, RustParseError> {
    extract_call_sites_with_options(source, CallIndexOptions::default())
}

/// [`extract_call_sites`] with explicit filtering options.
pub fn extract_call_sites_with_options(
    source: &str,
    opts: CallIndexOptions,
) -> Result<Vec<CallSite>, RustParseError> {
    extract_call_sites_with_options_and_base_module(source, opts, "crate")
}

/// [`extract_call_sites_with_options`] with an explicit lexical module
/// assigned to the file body. Inline modules below it extend that path.
pub fn extract_call_sites_with_options_and_base_module(
    source: &str,
    opts: CallIndexOptions,
    base_module: &str,
) -> Result<Vec<CallSite>, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut visitor = CallVisitor::new(opts, base_module);
    visitor.visit_items(&file.items);
    Ok(visitor.into_sites())
}

/// Extract neutral call syntax facts with an explicit lexical base module.
pub fn extract_call_shapes_with_options_and_base_module(
    source: &str,
    opts: CallIndexOptions,
    base_module: &str,
) -> Result<Vec<CallShape>, RustParseError> {
    extract_call_sites_with_options_and_base_module(source, opts, base_module)
        .map(|sites| sites.into_iter().map(CallShape::from).collect())
}

impl From<CallSite> for CallShape {
    fn from(site: CallSite) -> Self {
        let receiver_expr_kind = match site.call_kind {
            CallKind::Path => ReceiverExprKind::None,
            CallKind::ReceiverMethod => {
                let is_bare_self = site
                    .callee_name
                    .as_deref()
                    .zip(site.callee_path.as_deref())
                    .is_some_and(|(name, path)| path.strip_prefix("self.") == Some(name));
                if is_bare_self {
                    ReceiverExprKind::SelfValue
                } else {
                    ReceiverExprKind::Expression
                }
            }
        };
        Self {
            caller_qualified_name: SyntaxFact::Known(site.caller_qualified_name),
            caller_module: SyntaxFact::Known(site.module),
            caller_owner: SyntaxFact::Known(site.caller_impl_owner),
            callee_display_name: SyntaxFact::Known(site.callee_name),
            callee_path_segments: site
                .callee_path
                .map(path_segments)
                .map_or(SyntaxFact::Unknown, SyntaxFact::Known),
            receiver_expr_kind: SyntaxFact::Known(receiver_expr_kind),
            callee_is_locally_bound: SyntaxFact::Known(site.callee_is_locally_bound),
            lexical_resolution: LexicalResolutionStatus::NotAttempted,
            visible_imports: site
                .visible_aliases
                .into_iter()
                .map(ImportShape::from)
                .collect(),
            line: site.line,
        }
    }
}

impl From<UseAlias> for ImportShape {
    fn from(alias: UseAlias) -> Self {
        Self {
            imported_module: SyntaxFact::Known(alias.target),
            local_alias: SyntaxFact::Known(Some(alias.alias)),
            exported_symbol: SyntaxFact::Unknown,
        }
    }
}

fn path_segments(path: String) -> Vec<String> {
    if path.contains("::") {
        path.split("::").map(ToOwned::to_owned).collect()
    } else {
        vec![path]
    }
}

#[derive(Debug, Clone)]
struct CallerContext {
    name: String,
    qualified_name: String,
    impl_owner: Option<String>,
}

struct CallVisitor {
    opts: CallIndexOptions,
    /// Stack of qualified caller names. The top of the stack is the
    /// nearest enclosing function — closures and nested `fn` items
    /// inherit their parent's name (refining that would require
    /// minting synthetic names like `outer::{closure#1}`, which buys
    /// no agent signal today).
    callers: Vec<CallerContext>,
    /// Lexical module stack. The top is the module currently being walked.
    modules: Vec<String>,
    /// Stack of `impl` self-type names so methods inside `impl Foo`
    /// can be qualified as `Foo::method`. Pushed on entry to
    /// `Item::Impl` and popped on exit.
    impl_owners: Vec<Option<String>>,
    alias_scopes: Vec<BTreeMap<String, String>>,
    /// Callable names bound in each enclosing function's own scope, one
    /// entry per function scope — see [`local_callable_bindings`]. Only
    /// the innermost entry applies: a nested `fn` does not see the outer
    /// function's locals, while a closure body does (it shares the
    /// scope, and the visitor never pushes one for it).
    local_callables: Vec<HashSet<String>>,
    sites: Vec<CallSite>,
}

impl CallVisitor {
    fn new(opts: CallIndexOptions, base_module: &str) -> Self {
        Self {
            opts,
            callers: Vec::new(),
            modules: vec![base_module.to_owned()],
            impl_owners: Vec::new(),
            alias_scopes: Vec::new(),
            local_callables: Vec::new(),
            sites: Vec::new(),
        }
    }

    fn into_sites(self) -> Vec<CallSite> {
        self.sites
    }

    /// Walk an item, but bail out early on `#[cfg(test)]`-gated modules.
    /// Free items are dispatched via the standard visitor; the early
    /// check is needed because `syn::visit::Visit` does not expose a
    /// "skip this subtree" hook.
    fn visit_items(&mut self, items: &[Item]) {
        self.alias_scopes.push(BTreeMap::new());
        for item in items {
            if let Item::Use(item_use) = item {
                self.add_aliases_from_use(item_use);
            }
        }
        for item in items {
            if !matches!(item, Item::Use(_)) {
                self.visit_item_filtered(item);
            }
        }
        self.alias_scopes.pop();
    }

    fn visit_item_filtered(&mut self, item: &Item) {
        match item {
            Item::Mod(item_mod) => {
                if !self.opts.include_cfg_test_blocks && has_cfg_test(&item_mod.attrs) {
                    return;
                }
                if let Some((_, items)) = &item_mod.content {
                    let outer_alias_scopes = std::mem::take(&mut self.alias_scopes);
                    self.modules.push(qualify_module(
                        self.current_module(),
                        &item_mod.ident.to_string(),
                    ));
                    self.visit_items(items);
                    self.modules.pop();
                    self.alias_scopes = outer_alias_scopes;
                }
            }
            Item::Impl(item_impl) => {
                let owner = type_path_last_ident(&item_impl.self_ty);
                self.impl_owners.push(owner);
                for impl_item in &item_impl.items {
                    self.visit_impl_item(impl_item);
                }
                self.impl_owners.pop();
            }
            Item::Trait(item_trait) => {
                let owner = Some(item_trait.ident.to_string());
                self.impl_owners.push(owner);
                for trait_item in &item_trait.items {
                    self.visit_trait_item(trait_item);
                }
                self.impl_owners.pop();
            }
            Item::Fn(item_fn) => self.visit_block_in_fn_scope(&item_fn.sig, &item_fn.block),
            Item::Use(item_use) => self.add_aliases_from_use(item_use),
            other => visit::visit_item(self, other),
        }
    }

    /// Push the qualified name of `sig`'s function onto the caller stack
    /// along with the callables its own scope binds, walk `block`, and
    /// pop both. Shared by the `Item::Fn` and `ImplItem::Fn` arms — both
    /// used to spell this loop out themselves.
    fn visit_block_in_fn_scope(&mut self, sig: &Signature, block: &Block) {
        let ident = &sig.ident;
        let name = qualify(self.current_owner(), &ident.to_string());
        let qualified_name = self.current_owner().map_or_else(
            || qualify_module(self.current_module(), &ident.to_string()),
            |owner| qualify_module(self.current_module(), &format!("{owner}::{ident}")),
        );
        self.callers.push(CallerContext {
            name,
            qualified_name,
            impl_owner: self.current_owner().map(ToOwned::to_owned),
        });
        self.local_callables
            .push(local_callable_bindings(sig, block));
        visit::visit_block(self, block);
        self.local_callables.pop();
        self.callers.pop();
    }

    fn current_owner(&self) -> Option<&str> {
        self.impl_owners.last().and_then(|o| o.as_deref())
    }

    fn current_module(&self) -> &str {
        self.modules.last().map(String::as_str).unwrap_or("crate")
    }

    fn current_caller(&self) -> Option<CallerContext> {
        self.callers.last().cloned()
    }

    fn current_aliases(&self) -> Vec<UseAlias> {
        let mut aliases = BTreeMap::new();
        for scope in &self.alias_scopes {
            for (alias, target) in scope {
                aliases.insert(alias.clone(), target.clone());
            }
        }
        aliases
            .into_iter()
            .map(|(alias, target)| UseAlias { alias, target })
            .collect()
    }

    fn add_aliases_from_use(&mut self, item_use: &ItemUse) {
        let aliases = use_aliases_for(self.current_module(), &item_use.tree);
        let Some(scope) = self.alias_scopes.last_mut() else {
            return;
        };
        for alias in aliases {
            scope.insert(alias.alias, alias.target);
        }
    }

    fn record(
        &mut self,
        callee_name: Option<String>,
        callee_path: Option<String>,
        call_kind: CallKind,
        line: usize,
    ) {
        let caller = self.current_caller();
        let module = self.current_module().to_owned();
        let callee_is_locally_bound =
            call_kind == CallKind::Path && self.is_locally_bound(callee_path.as_deref());
        let callee_path = callee_path.map(|p| rewrite_crate_prefix(&p, &module));
        self.sites.push(CallSite {
            callee_name,
            callee_path,
            caller_name: caller.as_ref().map(|c| c.name.clone()),
            module,
            caller_qualified_name: caller.as_ref().map(|c| c.qualified_name.clone()),
            caller_impl_owner: caller.and_then(|c| c.impl_owner),
            call_kind,
            callee_is_locally_bound,
            visible_aliases: self.current_aliases(),
            line,
        });
    }

    /// A bare, single-segment callee that the innermost function scope
    /// binds to a callable. A qualified path (`a::b::emit`) names its
    /// owner and so is not shadowed by a local of the same name.
    fn is_locally_bound(&self, callee_path: Option<&str>) -> bool {
        let Some(path) = callee_path.filter(|path| !path.contains("::")) else {
            return false;
        };
        self.local_callables
            .last()
            .is_some_and(|bound| bound.contains(path))
    }
}

impl<'ast> Visit<'ast> for CallVisitor {
    fn visit_block(&mut self, block: &'ast Block) {
        self.alias_scopes.push(BTreeMap::new());
        visit::visit_block(self, block);
        self.alias_scopes.pop();
    }

    fn visit_item(&mut self, item: &'ast Item) {
        self.visit_item_filtered(item);
    }

    fn visit_impl_item(&mut self, impl_item: &'ast ImplItem) {
        if let ImplItem::Fn(method) = impl_item {
            self.visit_block_in_fn_scope(&method.sig, &method.block);
        } else {
            visit::visit_impl_item(self, impl_item);
        }
    }

    fn visit_trait_item(&mut self, trait_item: &'ast TraitItem) {
        if let TraitItem::Fn(method) = trait_item
            && let Some(block) = &method.default
        {
            self.visit_block_in_fn_scope(&method.sig, block);
        } else {
            visit::visit_trait_item(self, trait_item);
        }
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        let line = call.span().start().line;
        let callee_name = path_call_name(&call.func);
        let callee_path = path_call_path(&call.func);
        self.record(callee_name, callee_path, CallKind::Path, line);
        // Recurse into arguments and the callee expression so nested
        // calls get their own sites (e.g. `outer(inner())` records both).
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        let line = call.span().start().line;
        let callee_name = Some(call.method.to_string());
        let receiver = render_tokens(call.receiver.as_ref());
        let callee_path = Some(format!("{receiver}.{}", call.method));
        self.record(callee_name, callee_path, CallKind::ReceiverMethod, line);
        visit::visit_expr_method_call(self, call);
    }
}

/// Names bound to a callable inside one function's own scope: closures
/// and nested `fn` items held in a `let`, `fn`-typed locals, and
/// parameters of `Fn`/`FnMut`/`FnOnce`/`fn` type.
///
/// A call to one of these targets the binding, which shadows anything the
/// workspace defines under that name. Without this the resolver's
/// last-segment fallback attributes `emit(ev)` to whichever module
/// happens to define an `emit`, fabricating a cross-module edge — and
/// short local names (`emit`, `send`, `next`, `done`) collide with a
/// method somewhere in any medium-sized corpus.
///
/// Bindings are collected body-wide rather than from the `let` onwards:
/// a call that precedes its binding and means an outer name is rare, and
/// losing an edge beats fabricating one.
fn local_callable_bindings(sig: &Signature, block: &Block) -> HashSet<String> {
    let callable_generics = fn_bounded_generics(&sig.generics);
    let mut out = HashSet::new();
    for input in &sig.inputs {
        if let FnArg::Typed(pat_type) = input
            && type_is_callable(&pat_type.ty, &callable_generics)
            && let Some(name) = binding_ident(&pat_type.pat)
        {
            out.insert(name);
        }
    }
    let mut collector = LocalBindingCollector {
        out,
        callable_generics,
    };
    collector.visit_block(block);
    collector.out
}

/// Generic parameters carrying an `Fn`/`FnMut`/`FnOnce` bound, from both
/// the parameter list (`fn f<F: Fn()>`) and the `where` clause.
fn fn_bounded_generics(generics: &Generics) -> HashSet<String> {
    let mut out = HashSet::new();
    for param in &generics.params {
        if let GenericParam::Type(type_param) = param
            && type_param.bounds.iter().any(bound_is_fn_trait)
        {
            out.insert(type_param.ident.to_string());
        }
    }
    for predicate in generics.where_clause.iter().flat_map(|w| &w.predicates) {
        if let WherePredicate::Type(predicate) = predicate
            && predicate.bounds.iter().any(bound_is_fn_trait)
            && let Some(ident) = type_path_last_ident(&predicate.bounded_ty)
        {
            out.insert(ident);
        }
    }
    out
}

fn bound_is_fn_trait(bound: &TypeParamBound) -> bool {
    matches!(bound, TypeParamBound::Trait(trait_bound)
        if trait_bound
            .path
            .segments
            .last()
            .is_some_and(|segment| is_fn_trait_name(&segment.ident.to_string())))
}

fn is_fn_trait_name(name: &str) -> bool {
    matches!(name, "Fn" | "FnMut" | "FnOnce")
}

/// Whether `ty` denotes something callable: a bare `fn` pointer, an
/// `impl`/`dyn` `Fn*` bound, a generic parameter carrying such a bound,
/// or any of those behind a reference or a wrapper like `Box<dyn Fn()>`.
fn type_is_callable(ty: &Type, callable_generics: &HashSet<String>) -> bool {
    match ty {
        Type::BareFn(_) => true,
        Type::ImplTrait(imp) => imp.bounds.iter().any(bound_is_fn_trait),
        Type::TraitObject(obj) => obj.bounds.iter().any(bound_is_fn_trait),
        Type::Reference(reference) => type_is_callable(&reference.elem, callable_generics),
        Type::Paren(paren) => type_is_callable(&paren.elem, callable_generics),
        Type::Group(group) => type_is_callable(&group.elem, callable_generics),
        Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return false;
            };
            let name = segment.ident.to_string();
            if is_fn_trait_name(&name) || callable_generics.contains(&name) {
                return true;
            }
            // `Box<dyn Fn()>`, `Option<F>`, `Arc<dyn FnMut()>`.
            match &segment.arguments {
                PathArguments::AngleBracketed(args) => args.args.iter().any(|arg| {
                    matches!(arg, GenericArgument::Type(inner)
                        if type_is_callable(inner, callable_generics))
                }),
                _ => false,
            }
        }
        _ => false,
    }
}

/// The bound name of an irrefutable binding pattern (`emit`, `mut emit`,
/// `&emit`). Destructuring patterns bind no single callable name.
fn binding_ident(pat: &Pat) -> Option<String> {
    match pat {
        Pat::Ident(ident) => Some(ident.ident.to_string()),
        Pat::Reference(reference) => binding_ident(&reference.pat),
        Pat::Paren(paren) => binding_ident(&paren.pat),
        Pat::Type(pat_type) => binding_ident(&pat_type.pat),
        _ => None,
    }
}

/// Collects the callable names one function body binds, without
/// descending into nested `fn` items — their bodies are separate scopes.
/// Closure bodies are walked, since a closure shares the enclosing
/// function's scope and the call visitor attributes its calls there too.
struct LocalBindingCollector {
    out: HashSet<String>,
    callable_generics: HashSet<String>,
}

impl<'ast> Visit<'ast> for LocalBindingCollector {
    fn visit_local(&mut self, local: &'ast Local) {
        let init_is_closure = local
            .init
            .as_ref()
            .is_some_and(|init| matches!(&*init.expr, Expr::Closure(_)));
        let typed_callable = match &local.pat {
            Pat::Type(pat_type) => type_is_callable(&pat_type.ty, &self.callable_generics),
            _ => false,
        };
        if (init_is_closure || typed_callable)
            && let Some(name) = binding_ident(&local.pat)
        {
            self.out.insert(name);
        }
        visit::visit_local(self, local);
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        self.out.insert(item.sig.ident.to_string());
    }
}

fn use_aliases_for(current_module: &str, tree: &UseTree) -> Vec<UseAlias> {
    let mut aliases = Vec::new();
    let mut prefix = Vec::new();
    walk_use_tree(current_module, tree, &mut prefix, &mut aliases);
    aliases
}

fn walk_use_tree(
    current_module: &str,
    tree: &UseTree,
    prefix: &mut Vec<String>,
    aliases: &mut Vec<UseAlias>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            walk_use_tree(current_module, &path.tree, prefix, aliases);
            prefix.pop();
        }
        UseTree::Name(name) => {
            record_use_leaf(
                current_module,
                prefix,
                &name.ident.to_string(),
                None,
                aliases,
            );
        }
        UseTree::Rename(rename) => {
            record_use_leaf(
                current_module,
                prefix,
                &rename.ident.to_string(),
                Some(rename.rename.to_string()),
                aliases,
            );
        }
        UseTree::Glob(_) => {
            if let Some(target) = absolutize_use_segments(current_module, prefix) {
                aliases.push(UseAlias {
                    alias: "*".to_owned(),
                    target,
                });
            }
        }
        UseTree::Group(group) => {
            for item in &group.items {
                walk_use_tree(current_module, item, prefix, aliases);
            }
        }
    }
}

fn record_use_leaf(
    current_module: &str,
    prefix: &[String],
    tail: &str,
    rename: Option<String>,
    aliases: &mut Vec<UseAlias>,
) {
    let mut target_segments = prefix.to_vec();
    let alias = if tail == "self" {
        let Some(alias) = prefix.last().cloned() else {
            return;
        };
        alias
    } else {
        target_segments.push(tail.to_owned());
        rename.unwrap_or_else(|| tail.to_owned())
    };
    if let Some(target) = absolutize_use_segments(current_module, &target_segments) {
        aliases.push(UseAlias { alias, target });
    }
}

fn absolutize_use_segments(current_module: &str, segments: &[String]) -> Option<String> {
    let first = segments.first()?;
    match first.as_str() {
        "crate" => {
            // Rewrite `crate::a::b` to `<crate_name>::a::b` so the
            // alias target lines up with the absolute module prefix
            // attached to the file. Falls back to the literal `crate`
            // when the caller did not provide a real crate name.
            let crate_name = current_module_crate_name(current_module);
            let mut rewritten: Vec<String> = vec![crate_name.to_owned()];
            rewritten.extend(segments.iter().skip(1).cloned());
            Some(rewritten.join("::"))
        }
        "self" => {
            if segments.len() == 1 {
                Some(current_module.to_owned())
            } else {
                let mut absolute = module_segments(current_module);
                absolute.extend(segments.iter().skip(1).cloned());
                Some(absolute.join("::"))
            }
        }
        "super" => {
            let mut absolute = module_segments(current_module);
            for segment in segments {
                if segment == "super" {
                    if absolute.len() <= 1 {
                        return None;
                    }
                    absolute.pop();
                } else {
                    absolute.push(segment.clone());
                }
            }
            Some(absolute.join("::"))
        }
        _ => None,
    }
}

fn module_segments(module: &str) -> Vec<String> {
    module.split("::").map(ToOwned::to_owned).collect()
}

/// First segment of `current_module`, treated as the absolute crate
/// prefix the caller threaded in. Used to rewrite the literal `crate`
/// keyword in callee paths and `use` targets so the function-graph
/// resolver sees a consistent crate-qualified namespace.
fn current_module_crate_name(current_module: &str) -> &str {
    let first = current_module.split("::").next().unwrap_or("");
    if first.is_empty() { "crate" } else { first }
}

/// Rewrite a callee path's leading `crate::` segment to the actual
/// crate name. Single-crate analyses (where `current_module` already
/// starts with `crate`) are unaffected.
fn rewrite_crate_prefix(path: &str, current_module: &str) -> String {
    let crate_name = current_module_crate_name(current_module);
    if crate_name == "crate" {
        return path.to_owned();
    }
    if path == "crate" {
        return crate_name.to_owned();
    }
    if let Some(tail) = path.strip_prefix("crate::") {
        return format!("{crate_name}::{tail}");
    }
    path.to_owned()
}

/// Pull the last path segment out of a free-call callee expression. We
/// peel through `&`, parens, and invisible groups so e.g.
/// `(crate::a::foo)(x)` still resolves to `foo`. Anything more
/// elaborate (closures, projection, casts) returns `None`.
fn path_call_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        Expr::Reference(r) => path_call_name(&r.expr),
        Expr::Paren(p) => path_call_name(&p.expr),
        Expr::Group(g) => path_call_name(&g.expr),
        _ => None,
    }
}

fn path_call_path(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Path(_) => Some(render_tokens(expr)),
        Expr::Reference(r) => path_call_path(&r.expr),
        Expr::Paren(p) => path_call_path(&p.expr),
        Expr::Group(g) => path_call_path(&g.expr),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn run(src: &str) -> Vec<CallSite> {
        extract_call_sites(src).unwrap()
    }

    fn names(sites: &[CallSite]) -> Vec<(Option<&str>, Option<&str>)> {
        sites
            .iter()
            .map(|s| (s.callee_name.as_deref(), s.caller_name.as_deref()))
            .collect()
    }

    #[test]
    fn bare_function_call_records_callee_and_caller() {
        let sites = run("fn a() { b() }\n");
        assert_eq!(names(&sites), [(Some("b"), Some("a"))]);
    }

    /// A callee bound to a closure, a nested `fn`, or an `Fn`-typed
    /// parameter in the caller's own scope is shadowed: the resolver must
    /// be told so it does not fall back to a same-named function in
    /// another module.
    #[rstest]
    #[case::closure_local("fn pump() { let emit = |e: u8| {}; emit(1); }", true)]
    #[case::nested_fn("fn pump() { fn emit(e: u8) {} emit(1); }", true)]
    #[case::fn_pointer_local("fn pump() { let emit: fn(u8) = other; emit(1); }", true)]
    #[case::impl_fn_param("fn pump(emit: impl Fn(u8)) { emit(1); }", true)]
    #[case::generic_fn_param("fn pump<F: Fn(u8)>(emit: F) { emit(1); }", true)]
    #[case::where_bounded_param("fn pump<F>(emit: F) where F: FnMut(u8) { emit(1); }", true)]
    #[case::boxed_dyn_fn_param("fn pump(emit: Box<dyn FnOnce(u8)>) { emit(1); }", true)]
    #[case::reference_dyn_fn_param("fn pump(emit: &dyn Fn(u8)) { emit(1); }", true)]
    #[case::fn_pointer_param("fn pump(emit: fn(u8)) { emit(1); }", true)]
    #[case::binding_in_nested_block("fn pump() { if x { let emit = |e: u8| {}; emit(1); } }", true)]
    #[case::closure_body_shares_the_scope(
        "fn pump() { let emit = |e: u8| {}; run(|| emit(1)); }",
        true
    )]
    #[case::plain_local("fn pump() { let emit = compute(); emit(1); }", false)]
    #[case::value_param("fn pump(emit: u8) { emit(1); }", false)]
    #[case::unbound_name("fn pump() { emit(1); }", false)]
    fn local_callable_bindings_shadow_bare_calls(#[case] src: &str, #[case] expected: bool) {
        let sites = run(src);
        let site = sites
            .iter()
            .find(|site| site.callee_name.as_deref() == Some("emit"))
            .expect("emit call site");
        assert_eq!(site.callee_is_locally_bound, expected);
    }

    /// A qualified path names its owner, so a local `emit` does not
    /// shadow `other::emit()`.
    #[test]
    fn qualified_paths_are_not_shadowed_by_a_local_binding() {
        let sites = run("fn pump() { let emit = |e: u8| {}; other::emit(1); }");
        let site = sites
            .iter()
            .find(|site| site.callee_path.as_deref() == Some("other::emit"))
            .expect("qualified emit call site");
        assert!(!site.callee_is_locally_bound);
    }

    /// A nested `fn` has its own scope: the outer function's locals are
    /// not visible inside it, and its own bindings do not leak out.
    #[test]
    fn nested_fn_scopes_do_not_share_bindings() {
        let src = "fn pump() { let emit = |e: u8| {}; fn inner() { emit(1); } emit(2); }";
        let flags: Vec<_> = run(src)
            .iter()
            .filter(|site| site.callee_name.as_deref() == Some("emit"))
            .map(|site| (site.caller_name.clone(), site.callee_is_locally_bound))
            .collect();
        assert_eq!(
            flags,
            [
                (Some("inner".to_owned()), false),
                (Some("pump".to_owned()), true),
            ]
        );
    }

    #[test]
    fn method_call_uses_last_segment_as_callee() {
        let sites = run("fn a(x: T) { x.foo() }\n");
        assert_eq!(names(&sites), [(Some("foo"), Some("a"))]);
    }

    #[test]
    fn qualified_path_call_uses_last_segment() {
        let sites = run("fn a() { crate::other::foo() }\n");
        assert_eq!(names(&sites), [(Some("foo"), Some("a"))]);
    }

    #[test]
    fn chained_calls_are_each_recorded() {
        // `a().b().c()` — three syntactic calls, all attributable.
        // Inner-to-outer order is the visitor's natural traversal: the
        // free call `a()` is the receiver, then `.b()` wraps it, then
        // `.c()` wraps that. We assert membership rather than order so
        // refactors of the visitor traversal don't churn the test.
        let sites = run("fn outer() { a().b().c() }\n");
        let callees: Vec<&str> = sites
            .iter()
            .filter_map(|s| s.callee_name.as_deref())
            .collect();
        assert!(callees.contains(&"a"));
        assert!(callees.contains(&"b"));
        assert!(callees.contains(&"c"));
        assert_eq!(sites.len(), 3);
    }

    #[test]
    fn impl_methods_qualify_caller_with_self_type() {
        let src = "
struct S;
impl S {
    fn x(&self) { y() }
}
";
        assert_eq!(names(&run(src)), [(Some("y"), Some("S::x"))]);
    }

    #[test]
    fn trait_default_methods_qualify_caller_with_trait_name() {
        let src = "
trait T {
    fn say(&self) { other() }
}
";
        assert_eq!(names(&run(src)), [(Some("other"), Some("T::say"))]);
    }

    #[test]
    fn nested_modules_inherit_parent_visitor() {
        let src = "
mod inner {
    fn shim() { core() }
}
";
        assert_eq!(names(&run(src)), [(Some("core"), Some("shim"))]);
    }

    #[test]
    fn cfg_test_modules_are_skipped() {
        let src = "
fn a() { b() }

#[cfg(test)]
mod tests {
    fn helper() { dropped() }
}
";
        // Only the production-side call survives; calls inside
        // `#[cfg(test)] mod tests` would otherwise inflate reuse
        // counts of helpers used only in tests.
        assert_eq!(names(&run(src)), [(Some("b"), Some("a"))]);
    }

    #[test]
    fn options_can_include_cfg_test_modules() {
        let src = "
#[cfg(test)]
mod tests {
    fn helper() { target() }
}
";
        let sites = extract_call_sites_with_options(
            src,
            CallIndexOptions {
                include_cfg_test_blocks: true,
            },
        )
        .unwrap();
        assert_eq!(names(&sites), [(Some("target"), Some("helper"))]);
    }

    #[test]
    fn records_rendered_callee_path() {
        let sites = run("fn a(x: T) { crate::other::foo(); x.bar(); }\n");
        let paths: Vec<_> = sites.iter().map(|s| s.callee_path.as_deref()).collect();
        assert_eq!(paths, [Some("crate::other::foo"), Some("x.bar")]);
    }

    #[rstest]
    #[case::bare_self("fn caller(x: S) { self.method() }\n", ReceiverExprKind::SelfValue)]
    #[case::self_field(
        "fn caller(x: S) { self.field.method() }\n",
        ReceiverExprKind::Expression
    )]
    #[case::value_receiver("fn caller(x: S) { x.method() }\n", ReceiverExprKind::Expression)]
    #[case::path_call("fn caller() { Foo::method() }\n", ReceiverExprKind::None)]
    fn receiver_expr_kind_distinguishes_bare_self_from_dotted(
        #[case] src: &str,
        #[case] expected: ReceiverExprKind,
    ) {
        let shapes = extract_call_shapes_with_options_and_base_module(
            src,
            CallIndexOptions {
                include_cfg_test_blocks: true,
            },
            "crate",
        )
        .unwrap();
        let kind = shapes[0]
            .receiver_expr_kind
            .known_value()
            .copied()
            .expect("Rust adapter sets receiver_expr_kind");
        assert_eq!(kind, expected);
    }

    #[test]
    fn neutral_call_shapes_preserve_callee_path_segments() {
        let shapes = extract_call_shapes_with_options_and_base_module(
            "fn a(x: T) { crate::other::foo(); x.bar(); }\n",
            CallIndexOptions {
                include_cfg_test_blocks: true,
            },
            "crate",
        )
        .unwrap();

        assert_eq!(shapes.len(), 2);
        assert_eq!(
            shapes[0].callee_path_segments.known_value(),
            Some(&vec![
                "crate".to_owned(),
                "other".to_owned(),
                "foo".to_owned()
            ]),
        );
        assert_eq!(
            shapes[1].callee_path_segments.known_value(),
            Some(&vec!["x.bar".to_owned()]),
        );
    }

    #[test]
    fn records_module_caller_and_visible_aliases() {
        let src = r#"
mod b {
    use crate::a::parse;
    fn caller() { parse(); }
}
"#;
        let sites = extract_call_sites_with_options_and_base_module(
            src,
            CallIndexOptions {
                include_cfg_test_blocks: true,
            },
            "crate::root",
        )
        .unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].module, "crate::root::b");
        assert_eq!(
            sites[0].caller_qualified_name.as_deref(),
            Some("crate::root::b::caller"),
        );
        assert_eq!(
            sites[0].visible_aliases,
            [UseAlias {
                alias: "parse".to_owned(),
                target: "crate::a::parse".to_owned(),
            }]
        );
    }

    #[test]
    fn nested_inline_modules_do_not_inherit_parent_use_aliases() {
        let src = r#"
use crate::a::parse;
mod b {
    fn caller() { parse(); }
}
"#;
        let sites = extract_call_sites_with_options_and_base_module(
            src,
            CallIndexOptions {
                include_cfg_test_blocks: true,
            },
            "crate",
        )
        .unwrap();
        assert_eq!(sites.len(), 1);
        assert!(sites[0].visible_aliases.is_empty());
    }

    #[test]
    fn block_scoped_use_aliases_are_visible_only_inside_that_block() {
        let src = r#"
fn caller() {
    {
        use crate::a::parse;
        parse();
    }
    parse();
}
"#;
        let sites = extract_call_sites_with_options_and_base_module(
            src,
            CallIndexOptions {
                include_cfg_test_blocks: true,
            },
            "crate",
        )
        .unwrap();
        assert_eq!(sites.len(), 2);
        assert_eq!(
            sites[0].visible_aliases,
            [UseAlias {
                alias: "parse".to_owned(),
                target: "crate::a::parse".to_owned(),
            }]
        );
        assert!(sites[1].visible_aliases.is_empty());
    }

    #[test]
    fn use_absolutization_handles_self_super_and_root_boundaries() {
        assert_eq!(
            absolutize_use_segments("crate::m", &["self".to_owned(), "parse".to_owned()])
                .as_deref(),
            Some("crate::m::parse"),
        );
        assert_eq!(
            absolutize_use_segments("crate::a::b", &["super".to_owned(), "parse".to_owned()])
                .as_deref(),
            Some("crate::a::parse"),
        );
        assert_eq!(
            absolutize_use_segments(
                "crate::a::b",
                &["super".to_owned(), "super".to_owned(), "parse".to_owned()],
            )
            .as_deref(),
            Some("crate::parse"),
        );
        assert_eq!(
            absolutize_use_segments("crate", &["super".to_owned(), "parse".to_owned()]),
            None,
        );
        assert_eq!(module_segments("crate::a"), ["crate", "a"]);
    }

    #[test]
    fn callee_paths_rewrite_crate_keyword_to_real_crate_name() {
        let sites = extract_call_sites_with_options_and_base_module(
            "fn caller() { crate::a::foo(); foo(); }\n",
            CallIndexOptions::default(),
            "agent_lens",
        )
        .unwrap();
        assert_eq!(
            sites[0].callee_path.as_deref(),
            Some("agent_lens::a::foo"),
            "leading `crate::` segment should be rewritten when a real crate name is supplied",
        );
        assert_eq!(
            sites[1].callee_path.as_deref(),
            Some("foo"),
            "non-prefixed paths should pass through unchanged",
        );
    }

    #[test]
    fn use_aliases_rewrite_crate_keyword_to_real_crate_name() {
        let sites = extract_call_sites_with_options_and_base_module(
            "use crate::a::parse;\nfn caller() { parse(); }\n",
            CallIndexOptions::default(),
            "agent_lens",
        )
        .unwrap();
        let alias = sites[0]
            .visible_aliases
            .iter()
            .find(|a| a.alias == "parse")
            .expect("parse alias should be visible");
        assert_eq!(alias.target, "agent_lens::a::parse");
    }

    #[test]
    fn rewrite_crate_prefix_is_noop_under_legacy_crate_module() {
        assert_eq!(
            rewrite_crate_prefix("crate::a::foo", "crate"),
            "crate::a::foo"
        );
        assert_eq!(rewrite_crate_prefix("foo", "agent_lens"), "foo");
        assert_eq!(rewrite_crate_prefix("crate", "agent_lens"), "agent_lens");
        assert_eq!(
            rewrite_crate_prefix("crate::a", "agent_lens::m"),
            "agent_lens::a"
        );
    }

    #[test]
    fn free_call_paths_peel_reference_paren_and_group_wrappers() {
        let reference_sites = run("fn a() { (&crate::other::foo)(); }\n");
        assert_eq!(
            reference_sites[0].callee_path.as_deref(),
            Some("crate::other::foo")
        );

        let paren_sites = run("fn a() { (crate::other::bar)(); }\n");
        assert_eq!(
            paren_sites[0].callee_path.as_deref(),
            Some("crate::other::bar")
        );

        let grouped: syn::Expr = syn::Expr::Group(syn::ExprGroup {
            attrs: Vec::new(),
            group_token: Default::default(),
            expr: Box::new(syn::parse_quote!(crate::other::baz)),
        });
        assert_eq!(
            path_call_path(&grouped).as_deref(),
            Some("crate::other::baz")
        );
    }

    #[test]
    fn module_scope_call_has_no_caller() {
        // A call written outside any function (`const X: i32 = f();`)
        // produces a site with `caller_name = None`. The visitor still
        // records it so the analyzer sees that the called name is
        // referenced from this file.
        let src = "const X: i32 = f();\n";
        assert_eq!(names(&run(src)), [(Some("f"), None)]);
    }

    #[test]
    fn closure_callee_records_no_name_but_still_counts() {
        // `(make_callable())(x)` — the callee expression is itself a
        // call, not a path. We record the outer call with
        // `callee_name = None`, plus the inner free call.
        let src = "fn outer() { (make_callable())(x) }\n";
        let sites = run(src);
        assert_eq!(sites.len(), 2);
        let outer = sites
            .iter()
            .find(|s| s.callee_name.is_none())
            .expect("outer call should be recorded with None name");
        assert_eq!(outer.caller_name.as_deref(), Some("outer"));
        let inner = sites
            .iter()
            .find(|s| s.callee_name.as_deref() == Some("make_callable"))
            .expect("inner call should be recorded by name");
        assert_eq!(inner.caller_name.as_deref(), Some("outer"));
    }

    #[rstest]
    #[case::receiver_call_records_inner_too(
        "fn a() { foo(x).bar() }\n",
        &["foo", "bar"]
    )]
    #[case::nested_arg(
        "fn a() { outer(inner()) }\n",
        &["outer", "inner"]
    )]
    fn each_syntactic_call_is_recorded(#[case] src: &str, #[case] expected: &[&str]) {
        let sites = run(src);
        let callees: Vec<&str> = sites
            .iter()
            .filter_map(|s| s.callee_name.as_deref())
            .collect();
        assert_eq!(callees.len(), expected.len(), "got {callees:?}");
        for name in expected {
            assert!(callees.contains(name), "missing {name} in {callees:?}");
        }
    }
}
