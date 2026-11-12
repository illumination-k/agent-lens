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
//! * **No macro expansion.** A call a macro's own definition makes is
//!   invisible. Calls written in the arguments of an expression- or
//!   statement-position macro (`assert_eq!(f(x), 1)`, `vec![g()]`,
//!   `write!(out, "{}", h())`) are recorded when the arguments parse as a
//!   comma-separated list of expressions — see [`macro_argument_exprs`].
//! * **`#[cfg(test)]` modules are skipped.** Test scaffolding is
//!   forwarding by design and would inflate reuse counts without
//!   reflecting production usage. This matches the existing wrapper
//!   detector's policy.
//!
//! Treat the result as guidance for an LLM, not as a precise call graph.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use lens_domain::{
    ArgumentShape, CallShape, GLOB_IMPORT_ALIAS, ImportShape, LexicalResolutionStatus,
    ReceiverExprKind, SyntaxFact, qualify, qualify_module, starts_uppercase,
};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Block, Expr, ExprCall, ExprMacro, ExprMethodCall, ExprPath, FnArg, GenericArgument,
    GenericParam, Generics, ImplItem, Item, ItemFn, ItemUse, Local, Macro, Pat, PatIdent,
    PathArguments, Signature, StmtMacro, Token, TraitItem, Type, TypeParamBound, UnOp, UseTree,
    Visibility, WherePredicate,
};

use crate::attrs::has_cfg_test;
use crate::common::{impl_self_type_name, render_tokens, type_path_last_ident};
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
    /// Syntactic shape of each argument, in source order. For a method
    /// call the receiver is not an argument.
    pub arguments: Vec<ArgumentShape>,
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
    collect_sites(source, opts, base_module, Collect::Calls)
}

fn collect_sites(
    source: &str,
    opts: CallIndexOptions,
    base_module: &str,
    collect: Collect,
) -> Result<Vec<CallSite>, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut visitor = CallVisitor::new(opts, base_module, collect);
    visitor.visit_items(&file.items);
    Ok(visitor.into_sites())
}

/// Every place `source` names an item by path *without calling it*, in
/// the shape of a call so the same resolver can attribute it: a path
/// expression outside call position (`.map(fusion::key)`, `apply(v,
/// key)`, `let f: fn(&str) -> usize = m::key;`) and each leaf of a `use`
/// declaration (`use m::key;`).
///
/// Both need the named item to be visible from where they are written
/// exactly as a call would, which is what a visibility audit has to know
/// and the call sites alone do not say. Most of what comes back names no
/// function at all (`Color::Red`, an imported type): the consumer
/// resolves each against the workspace's functions and drops the rest.
///
/// A bare name the enclosing function binds — a parameter, a `let`, a
/// closure parameter, a pattern binding, a nested `fn` — is a local, not
/// a path to an item, and is left out; so are `self` and any path whose
/// last segment starts uppercase (a type, variant, or constant by
/// convention, never a function). A `use` leaf is
/// reported with its absolute target where the path is rooted at
/// `crate`/`self`/`super` or a child module, and as written otherwise
/// (a path through another crate of the workspace).
///
/// A leaf of a `use` declared with a visibility (`pub use`, `pub(crate)
/// use`, …) is a re-export, and carries how far it hands the name out:
/// the item it names has to be at least that visible for the
/// declaration itself to compile.
pub fn extract_value_reference_shapes_with_module(
    source: &str,
    opts: CallIndexOptions,
    base_module: &str,
) -> Result<Vec<ValueReferenceShape>, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut visitor = CallVisitor::new(opts, base_module, Collect::ValueReferences);
    visitor.visit_items(&file.items);
    let mut re_exports = std::mem::take(&mut visitor.re_exports);
    Ok(visitor
        .into_sites()
        .into_iter()
        .enumerate()
        .map(|(index, site)| ValueReferenceShape {
            site: CallShape::from(site),
            re_export: re_exports.remove(&index),
        })
        .collect())
}

/// One value reference: the path shaped as a call, and — for a leaf of
/// a re-exporting `use` — the scope the re-export is visible in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueReferenceShape {
    pub site: CallShape,
    pub re_export: Option<ReExportScope>,
}

/// How far a `use` declaration with a visibility hands a name out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReExportScope {
    /// `pub use`: the name is public wherever the module is.
    Public,
    /// `pub(crate) use`, `pub(super) use`, `pub(in path) use`: the
    /// absolute path of the module the name is visible within.
    Module(String),
}

/// Extract neutral call syntax facts with an explicit lexical base module.
///
/// Same entry point as every other adapter's
/// `extract_call_shapes_with_module`; `opts` (Rust's `#[cfg(test)]`
/// knob) sits in the slot where `lens-ts` takes its dialect.
pub fn extract_call_shapes_with_module(
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
            arguments: SyntaxFact::Known(site.arguments),
            callee_is_locally_bound: SyntaxFact::Known(site.callee_is_locally_bound),
            callee_binding: SyntaxFact::Unknown,
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

/// The `use` declarations of one lexical scope.
#[derive(Debug, Clone, Default)]
struct AliasScope {
    /// Names the scope binds itself (`use a::parse;`, `use a::b as c;`).
    named: BTreeMap<String, String>,
    /// Names that reach the scope through a glob of an enclosing module
    /// which imports them. A `named` entry shadows these.
    inherited: BTreeMap<String, String>,
    /// Modules glob-imported into the scope.
    globs: BTreeSet<String>,
}

#[derive(Debug, Clone)]
struct CallerContext {
    name: String,
    qualified_name: String,
    impl_owner: Option<String>,
}

/// What one walk of [`CallVisitor`] gathers into its `sites`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Collect {
    /// Call expressions — the call index proper.
    Calls,
    /// Paths that name an item without calling it — see
    /// [`extract_value_reference_shapes_with_module`].
    ValueReferences,
}

struct CallVisitor {
    opts: CallIndexOptions,
    collect: Collect,
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
    alias_scopes: Vec<AliasScope>,
    /// The inline modules enclosing the one being walked, outermost
    /// first: each one's path and module-level `use` scope. What a
    /// `use super::*;` in a nested module brings in beyond the parent's
    /// own items — the parent's imports are in scope for the child too.
    enclosing_modules: Vec<(String, AliasScope)>,
    /// Names of the `mod` items declared in each module being walked,
    /// innermost last — what a bare leading `use` segment may name
    /// relative to the current module (see [`use_aliases_for`]).
    child_modules: Vec<HashSet<String>>,
    /// Callable names bound in each enclosing function's own scope, one
    /// entry per function scope — see [`local_callable_bindings`]. Only
    /// the innermost entry applies: a nested `fn` does not see the outer
    /// function's locals, while a closure body does (it shares the
    /// scope, and the visitor never pushes one for it).
    local_callables: Vec<HashSet<String>>,
    /// Every name bound in each enclosing function's own scope, with the
    /// same stacking as `local_callables`. Filled only when collecting
    /// value references, where a bare name that is a local names no item.
    local_names: Vec<HashSet<String>>,
    sites: Vec<CallSite>,
    /// Re-export scope of the value-reference sites that are leaves of a
    /// `use` declared with a visibility, by index into `sites`.
    re_exports: BTreeMap<usize, ReExportScope>,
}

impl CallVisitor {
    fn new(opts: CallIndexOptions, base_module: &str, collect: Collect) -> Self {
        Self {
            opts,
            collect,
            callers: Vec::new(),
            modules: vec![base_module.to_owned()],
            impl_owners: Vec::new(),
            alias_scopes: Vec::new(),
            enclosing_modules: Vec::new(),
            child_modules: Vec::new(),
            local_callables: Vec::new(),
            local_names: Vec::new(),
            sites: Vec::new(),
            re_exports: BTreeMap::new(),
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
        self.alias_scopes.push(AliasScope::default());
        self.child_modules.push(
            items
                .iter()
                .filter_map(|item| match item {
                    Item::Mod(item_mod) => Some(item_mod.ident.to_string()),
                    _ => None,
                })
                .collect(),
        );
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
        self.child_modules.pop();
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
                    self.enclosing_modules.push((
                        self.current_module().to_owned(),
                        outer_alias_scopes.first().cloned().unwrap_or_default(),
                    ));
                    self.modules.push(qualify_module(
                        self.current_module(),
                        &item_mod.ident.to_string(),
                    ));
                    self.visit_items(items);
                    self.modules.pop();
                    self.enclosing_modules.pop();
                    self.alias_scopes = outer_alias_scopes;
                }
            }
            Item::Impl(item_impl) => {
                let owner = impl_self_type_name(&item_impl.self_ty);
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
    ///
    /// A `fn` declared inside another function's body is no graph node,
    /// so its calls stay with the enclosing function, as a closure's do:
    /// `fn glob_vec(p) { glob(p) }` inside `main` is `main → glob`.
    fn visit_block_in_fn_scope(&mut self, sig: &Signature, block: &Block) {
        // The owner is still the innermost `impl`'s: a method of an `impl`
        // nested in the body has its own `self`, which is no node's.
        let caller = match self.current_caller() {
            Some(outer) => CallerContext {
                impl_owner: self.current_owner().map(ToOwned::to_owned),
                ..outer
            },
            None => self.caller_context(sig),
        };
        self.callers.push(caller);
        self.local_callables
            .push(local_callable_bindings(sig, block));
        if self.collect == Collect::ValueReferences {
            self.local_names.push(local_names(sig, block));
        }
        visit::visit_block(self, block);
        if self.collect == Collect::ValueReferences {
            self.local_names.pop();
        }
        self.local_callables.pop();
        self.callers.pop();
    }

    fn caller_context(&self, sig: &Signature) -> CallerContext {
        let ident = &sig.ident;
        let name = qualify(self.current_owner(), &ident.to_string());
        let qualified_name = self.current_owner().map_or_else(
            || qualify_module(self.current_module(), &ident.to_string()),
            |owner| qualify_module(self.current_module(), &format!("{owner}::{ident}")),
        );
        CallerContext {
            name,
            qualified_name,
            impl_owner: self.current_owner().map(ToOwned::to_owned),
        }
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
        let mut named = BTreeMap::new();
        let mut inherited = BTreeMap::new();
        let mut globs = BTreeSet::new();
        for scope in &self.alias_scopes {
            named.extend(scope.named.clone());
            inherited.extend(scope.inherited.clone());
            globs.extend(scope.globs.iter().cloned());
        }
        // A name the scope binds itself shadows one a glob brought in.
        inherited.extend(named);
        globs
            .into_iter()
            .map(|target| UseAlias {
                alias: GLOB_IMPORT_ALIAS.to_owned(),
                target,
            })
            .chain(
                inherited
                    .into_iter()
                    .map(|(alias, target)| UseAlias { alias, target }),
            )
            .collect()
    }

    /// Record each non-glob leaf of a `use` as a value reference; the
    /// leaves of one declared with a visibility carry its re-export
    /// scope.
    fn record_use_references(&mut self, item_use: &ItemUse, leaves: &[(String, Vec<String>)]) {
        let line = item_use.span().start().line;
        let re_export = re_export_scope(self.current_module(), &item_use.vis);
        for path in use_leaf_paths(self.current_module(), leaves) {
            let recorded = self.sites.len();
            self.record_value_reference(path, line);
            if let Some(scope) = &re_export
                && self.sites.len() > recorded
            {
                self.re_exports.insert(recorded, scope.clone());
            }
        }
    }

    fn add_aliases_from_use(&mut self, item_use: &ItemUse) {
        let no_children = HashSet::new();
        let child_modules = self.child_modules.last().unwrap_or(&no_children);
        let leaves = use_leaves(child_modules, &item_use.tree);
        let aliases = use_aliases_for(self.current_module(), &leaves);
        if self.collect == Collect::ValueReferences {
            self.record_use_references(item_use, &leaves);
        }
        let Some(scope) = self.alias_scopes.last_mut() else {
            return;
        };
        for alias in aliases {
            if alias.alias != GLOB_IMPORT_ALIAS {
                scope.named.insert(alias.alias, alias.target);
                continue;
            }
            // `use super::*;` in an inline module: the enclosing
            // module's own imports come along with its items.
            if let Some((_, outer)) = self
                .enclosing_modules
                .iter()
                .find(|(module, _)| *module == alias.target)
            {
                scope.inherited.extend(outer.inherited.clone());
                scope.inherited.extend(outer.named.clone());
                scope.globs.extend(outer.globs.iter().cloned());
            }
            scope.globs.insert(alias.target);
        }
    }

    fn record(
        &mut self,
        callee_name: Option<String>,
        callee_path: Option<String>,
        call_kind: CallKind,
        arguments: Vec<ArgumentShape>,
        line: usize,
    ) {
        if self.collect != Collect::Calls {
            return;
        }
        let callee_is_locally_bound =
            call_kind == CallKind::Path && self.is_locally_bound(callee_path.as_deref());
        self.push_site(
            callee_name,
            callee_path,
            call_kind,
            callee_is_locally_bound,
            arguments,
            line,
        );
    }

    /// Record `path` — `::`-joined segments naming an item outside call
    /// position — as a site of its own, shaped like a path call with no
    /// arguments.
    ///
    /// A path whose last segment starts uppercase is dropped: by Rust's
    /// naming convention it is a type, a variant, or a constant, and
    /// those are most of the paths in any file.
    fn record_value_reference(&mut self, path: String, line: usize) {
        let name = path.rsplit("::").next().unwrap_or(&path).to_owned();
        if starts_uppercase(&name) {
            return;
        }
        self.push_site(
            Some(name),
            Some(path),
            CallKind::Path,
            false,
            Vec::new(),
            line,
        );
    }

    fn push_site(
        &mut self,
        callee_name: Option<String>,
        callee_path: Option<String>,
        call_kind: CallKind,
        callee_is_locally_bound: bool,
        arguments: Vec<ArgumentShape>,
        line: usize,
    ) {
        let caller = self.current_caller();
        let module = self.current_module().to_owned();
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
            arguments,
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

    /// Whether a path expression is a bare name the innermost function
    /// scope binds, or `self`: a value, not a path to an item.
    fn names_local_value(&self, path: &syn::Path) -> bool {
        let Some(ident) = path.get_ident() else {
            return false;
        };
        ident == "self"
            || self
                .local_names
                .last()
                .is_some_and(|bound| bound.contains(&ident.to_string()))
    }
}

impl<'ast> Visit<'ast> for CallVisitor {
    fn visit_block(&mut self, block: &'ast Block) {
        self.alias_scopes.push(AliasScope::default());
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
        let arguments = call.args.iter().map(argument_shape).collect();
        self.record(callee_name, callee_path, CallKind::Path, arguments, line);
        // Recurse into arguments and the callee expression so nested
        // calls get their own sites (e.g. `outer(inner())` records both).
        // A callee that is a plain path is the call just recorded, not a
        // value reference, so it is walked without being visited as one.
        match callee_path_expr(&call.func) {
            Some(path) => visit::visit_expr_path(self, path),
            None => self.visit_expr(&call.func),
        }
        for argument in &call.args {
            self.visit_expr(argument);
        }
    }

    fn visit_expr_path(&mut self, expr: &'ast ExprPath) {
        if self.collect == Collect::ValueReferences
            && expr.qself.is_none()
            && !self.names_local_value(&expr.path)
        {
            self.record_value_reference(path_idents(&expr.path), expr.span().start().line);
        }
        visit::visit_expr_path(self, expr);
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        let line = call.span().start().line;
        let callee_name = Some(call.method.to_string());
        let receiver = render_tokens(call.receiver.as_ref());
        let callee_path = Some(format!("{receiver}.{}", call.method));
        let arguments = call.args.iter().map(argument_shape).collect();
        self.record(
            callee_name,
            callee_path,
            CallKind::ReceiverMethod,
            arguments,
            line,
        );
        visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_macro(&mut self, mac: &'ast ExprMacro) {
        self.visit_macro_arguments(&mac.mac);
    }

    fn visit_stmt_macro(&mut self, mac: &'ast StmtMacro) {
        self.visit_macro_arguments(&mac.mac);
    }
}

impl CallVisitor {
    /// Walk the arguments of a macro invocation as expressions, so
    /// `assert_eq!(parse(s), Ok(v))` records its call to `parse`. Item-
    /// position macros (`macro_rules!`, `thread_local!`) are not routed
    /// here: their bodies are no expression lists.
    fn visit_macro_arguments(&mut self, mac: &Macro) {
        for expr in macro_argument_exprs(mac) {
            self.visit_expr(&expr);
        }
    }
}

/// The arguments of `mac` when they parse as a comma-separated list of
/// expressions (`assert_eq!`, `vec!`, `format!`, `write!`, `println!`),
/// otherwise nothing: a macro with its own grammar (`vec![x; n]`,
/// `select! { .. }`, a DSL) keeps its calls invisible rather than being
/// read half-way. The pattern-taking macros are skipped even when their
/// pattern parses as an expression, since `Foo::Bar(_)` there is a
/// variant pattern, not a call.
fn macro_argument_exprs(mac: &Macro) -> Vec<Expr> {
    let name = mac.path.segments.last().map(|s| s.ident.to_string());
    if name.as_deref().is_some_and(is_pattern_macro) {
        return Vec::new();
    }
    mac.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
        .map(|args| args.into_iter().collect())
        .unwrap_or_default()
}

fn is_pattern_macro(name: &str) -> bool {
    matches!(
        name,
        "matches" | "assert_matches" | "debug_assert_matches" | "let_assert"
    )
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
        Type::FnPtr(_) => true,
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

/// Every name `sig` and `block` bind as a value: parameters, `let` and
/// pattern bindings, closure parameters, and nested `fn` items. Collected
/// body-wide for the reason [`local_callable_bindings`] is: a use that
/// precedes its binding and means an outer item is rare, and here a
/// dropped reference only leaves a visibility row as it was.
fn local_names(sig: &Signature, block: &Block) -> HashSet<String> {
    let mut collector = LocalNameCollector {
        out: HashSet::new(),
    };
    collector.visit_signature(sig);
    collector.visit_block(block);
    collector.out
}

/// Collects binding names without descending into nested `fn` items,
/// whose bodies are separate scopes.
struct LocalNameCollector {
    out: HashSet<String>,
}

impl<'ast> Visit<'ast> for LocalNameCollector {
    fn visit_pat_ident(&mut self, pat: &'ast PatIdent) {
        self.out.insert(pat.ident.to_string());
        visit::visit_pat_ident(self, pat);
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        self.out.insert(item.sig.ident.to_string());
    }
}

/// Every name a `use` item binds, as an alias of its absolute target.
///
/// `child_modules` are the `mod` items declared beside the `use`: under
/// 2018 uniform paths a leading segment naming one of them (`use
/// geo::area;` next to `mod geo;`) is relative to the current module, as
/// if it were spelled `self::geo::area`. Any other bare leading segment
/// names an extern crate, which no workspace node can match, so it
/// contributes no alias.
fn use_aliases_for(current_module: &str, leaves: &[(String, Vec<String>)]) -> Vec<UseAlias> {
    leaves
        .iter()
        .filter_map(|(alias, segments)| {
            absolutize_use_segments(current_module, segments).map(|target| UseAlias {
                alias: alias.clone(),
                target,
            })
        })
        .collect()
}

/// `(bound name, path segments)` for every leaf of a `use` tree, with a
/// leading child-module segment spelled out as `self::<child>`.
fn use_leaves(child_modules: &HashSet<String>, tree: &UseTree) -> Vec<(String, Vec<String>)> {
    let mut leaves = Vec::new();
    walk_use_tree(tree, &mut Vec::new(), &mut leaves);
    for (_, segments) in &mut leaves {
        if segments
            .first()
            .is_some_and(|first| child_modules.contains(first))
        {
            segments.insert(0, "self".to_owned());
        }
    }
    leaves
}

/// The path each non-glob `use` leaf names: absolute where
/// [`absolutize_use_segments`] can root it, as written otherwise.
fn use_leaf_paths(current_module: &str, leaves: &[(String, Vec<String>)]) -> Vec<String> {
    leaves
        .iter()
        .filter(|(alias, _)| alias != "*")
        .map(|(_, segments)| {
            absolutize_use_segments(current_module, segments).unwrap_or_else(|| segments.join("::"))
        })
        .collect()
}

/// Collect `(bound name, path segments as written)` for every leaf of a
/// `use` tree; a glob binds `*`.
fn walk_use_tree(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    leaves: &mut Vec<(String, Vec<String>)>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.to_string());
            walk_use_tree(&path.tree, prefix, leaves);
            prefix.pop();
        }
        UseTree::Name(name) => record_use_leaf(prefix, &name.ident.to_string(), None, leaves),
        UseTree::Rename(rename) => record_use_leaf(
            prefix,
            &rename.ident.to_string(),
            Some(rename.rename.to_string()),
            leaves,
        ),
        UseTree::Glob(_) => leaves.push((GLOB_IMPORT_ALIAS.to_owned(), prefix.clone())),
        UseTree::Group(group) => {
            for item in &group.items {
                walk_use_tree(item, prefix, leaves);
            }
        }
    }
}

fn record_use_leaf(
    prefix: &[String],
    tail: &str,
    rename: Option<String>,
    leaves: &mut Vec<(String, Vec<String>)>,
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
    leaves.push((alias, target_segments));
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

/// The scope a `use` with visibility `vis`, written in `current_module`,
/// re-exports its names to. `None` for a plain `use`, for `pub(self)`,
/// and for a `pub(in …)` path that cannot be rooted: those hand the name
/// no further than an import does.
fn re_export_scope(current_module: &str, vis: &Visibility) -> Option<ReExportScope> {
    match vis {
        Visibility::Public(_) => Some(ReExportScope::Public),
        Visibility::Restricted(restricted) => {
            let segments: Vec<String> = restricted
                .path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect();
            absolutize_use_segments(current_module, &segments)
                .filter(|scope| scope != current_module)
                .map(ReExportScope::Module)
        }
        Visibility::Inherited => None,
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

/// Classify one call argument. References, parens, and groups are
/// peeled first — `f(&CONFIG)` names the same value as `f(CONFIG)` for
/// the "same text, same value" question — and the rendered text is
/// then the peeled expression's. A path is [`ArgumentShape::Const`]
/// when it has more than one segment (`Color::Red`, `u32::MAX`) or its
/// one segment starts uppercase (`None`, a unit variant, a `const`):
/// Rust's naming convention keeps locals lowercase, so an
/// uppercase-initial bare name denotes the same item wherever it is in
/// scope. Anything else that is not a literal is an arbitrary
/// expression.
fn argument_shape(expr: &Expr) -> ArgumentShape {
    match expr {
        Expr::Lit(_) => ArgumentShape::Literal {
            text: render_tokens(expr),
        },
        Expr::Unary(unary) if matches!(unary.op, UnOp::Neg(_)) => {
            match argument_shape(&unary.expr) {
                ArgumentShape::Literal { text } => ArgumentShape::Literal {
                    text: format!("-{text}"),
                },
                _ => ArgumentShape::Other,
            }
        }
        Expr::Path(path) if path.qself.is_none() => {
            let text = render_tokens(expr);
            match path.path.segments.len() {
                1 if starts_uppercase(&text) => ArgumentShape::Const { text },
                1 => ArgumentShape::Identifier { text },
                _ => ArgumentShape::Const { text },
            }
        }
        Expr::Reference(reference) => argument_shape(&reference.expr),
        Expr::Paren(paren) => argument_shape(&paren.expr),
        Expr::Group(group) => argument_shape(&group.expr),
        _ => ArgumentShape::Other,
    }
}

/// Pull the last path segment out of a free-call callee expression. We
/// peel through `&`, parens, and invisible groups so e.g.
/// `(crate::a::foo)(x)` still resolves to `foo`. Anything more
/// elaborate (closures, projection, casts) returns `None`.
fn path_call_name(expr: &Expr) -> Option<String> {
    callee_path_expr(expr)?
        .path
        .segments
        .last()
        .map(|s| s.ident.to_string())
}

fn path_call_path(expr: &Expr) -> Option<String> {
    let path = callee_path_expr(expr)?;
    if path.qself.is_some() {
        return Some(render_tokens(path));
    }
    Some(path_idents(&path.path))
}

/// The segments of `path` joined by `::`, without generic arguments:
/// `m::apply::<T>` names the same item as `m::apply`, and the turbofish
/// rendered as a segment of its own would match no qualified name.
fn path_idents(path: &syn::Path) -> String {
    let segments: Vec<String> = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect();
    segments.join("::")
}

/// The path a free-call callee names, behind any `&`, parens, and
/// invisible groups.
fn callee_path_expr(expr: &Expr) -> Option<&ExprPath> {
    match expr {
        Expr::Path(path) => Some(path),
        Expr::Reference(r) => callee_path_expr(&r.expr),
        Expr::Paren(p) => callee_path_expr(&p.expr),
        Expr::Group(g) => callee_path_expr(&g.expr),
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

    /// `(path, enclosing function)` of every value reference in `src`.
    fn value_references(src: &str) -> Vec<(String, Option<String>)> {
        extract_value_reference_shapes_with_module(src, CallIndexOptions::default(), "app")
            .unwrap()
            .into_iter()
            .map(|reference| {
                (
                    reference.site.callee_path().unwrap(),
                    reference
                        .site
                        .caller_qualified_name()
                        .map(ToOwned::to_owned),
                )
            })
            .collect()
    }

    fn reference_paths(src: &str) -> Vec<String> {
        value_references(src)
            .into_iter()
            .map(|(path, _)| path)
            .collect()
    }

    #[rstest]
    #[case::method_argument("fn a(v: V) { v.map(m::key); }", &["m::key"])]
    #[case::call_argument("fn a(v: V) { apply(v, m::key); }", &["m::key"])]
    #[case::let_initialiser("fn a() { let f: fn() = m::key; }", &["m::key"])]
    #[case::bare_name("fn a(v: V) { v.map(key); }", &["key"])]
    #[case::behind_a_reference("fn a(v: V) { apply(v, &m::key); }", &["m::key"])]
    #[case::macro_argument("fn a() { let _ = vec![m::key]; }", &["m::key"])]
    #[case::turbofish_is_dropped("fn a(v: V) { v.map(m::key::<u8>); }", &["m::key"])]
    #[case::crate_keyword_is_rewritten("fn a(v: V) { v.map(crate::m::key); }", &["app::m::key"])]
    #[case::self_type_path("impl S { fn a(v: V) { v.map(Self::key); } }", &["Self::key"])]
    #[case::callee_is_a_call_not_a_reference("fn a() { m::key(); (m::other)(); }", &[])]
    #[case::callee_arguments_still_count("fn a() { m::run(m::key); }", &["m::key"])]
    #[case::receiver_is_no_item("fn a(&self) { self.go(); }", &[])]
    #[case::uppercase_last_segment("fn a(v: V) { v.map(Some); f(Color::Red, LIMIT); }", &[])]
    #[case::qualified_self("fn a(v: V) { v.map(<S as T>::key); }", &[])]
    #[case::parameter("fn a(key: u8, v: V) { v.push(key); }", &[])]
    #[case::let_binding("fn a(v: V) { let key = 1; v.push(key); }", &[])]
    #[case::closure_parameter("fn a(v: V) { v.map(|key| key); }", &[])]
    #[case::pattern_binding("fn a(v: V) { if let Some(key) = v { drop(key); } }", &[])]
    #[case::nested_fn("fn a(v: V) { fn key() {} v.map(key); }", &[])]
    #[case::qualified_path_is_not_shadowed("fn a(key: u8, v: V) { v.map(m::key); }", &["m::key"])]
    fn paths_outside_call_position_are_value_references(
        #[case] src: &str,
        #[case] expected: &[&str],
    ) {
        assert_eq!(reference_paths(src), expected);
    }

    #[rstest]
    #[case::rooted_at_super("mod b { use super::m::key; }", &["app::m::key"])]
    #[case::rooted_at_crate("use crate::m::{key, other as o};", &["app::m::key", "app::m::other"])]
    #[case::child_module("mod m {}\nuse m::key;", &["app::m::key"])]
    #[case::another_crate("use core_lib::m::key;", &["core_lib::m::key"])]
    #[case::glob_names_nothing("use crate::m::*;", &[])]
    #[case::type_import("use crate::m::Key;", &[])]
    #[case::block_scoped("fn a() { use crate::m::key; }", &["app::m::key"])]
    fn use_leaves_are_value_references(#[case] src: &str, #[case] expected: &[&str]) {
        assert_eq!(reference_paths(src), expected);
    }

    #[rstest]
    #[case::plain_import("use crate::m::key;", None)]
    #[case::public("pub use crate::m::key;", Some(ReExportScope::Public))]
    #[case::crate_wide("mod a { pub(crate) use crate::m::key; }", Some(ReExportScope::Module("app".to_owned())))]
    #[case::parent("mod a { mod b { pub(super) use crate::m::key; } }", Some(ReExportScope::Module("app::a".to_owned())))]
    #[case::in_path("mod a { mod b { pub(in crate::a) use crate::m::key; } }", Some(ReExportScope::Module("app::a".to_owned())))]
    #[case::own_module("mod a { pub(self) use crate::m::key; }", None)]
    #[case::super_above_the_root("pub(super) use crate::m::key;", None)]
    fn a_use_leaf_carries_the_scope_it_re_exports_to(
        #[case] src: &str,
        #[case] expected: Option<ReExportScope>,
    ) {
        let references =
            extract_value_reference_shapes_with_module(src, CallIndexOptions::default(), "app")
                .unwrap();
        let scopes: Vec<_> = references.into_iter().map(|r| r.re_export).collect();
        assert_eq!(scopes, [expected]);
    }

    /// Only the leaves of the re-exporting `use` carry its scope: a type
    /// leaf records no site, and the sites around it keep their own.
    #[test]
    fn a_re_export_scope_stays_on_its_own_leaves() {
        let references = extract_value_reference_shapes_with_module(
            "use crate::m::first;\npub use crate::m::{Key, second};\nfn a(v: V) { v.map(m::third); }\n",
            CallIndexOptions::default(),
            "app",
        )
        .unwrap();
        let facts: Vec<_> = references
            .iter()
            .map(|r| (r.site.callee_name(), r.re_export.clone()))
            .collect();
        assert_eq!(
            facts,
            [
                (Some("first"), None),
                (Some("second"), Some(ReExportScope::Public)),
                (Some("third"), None),
            ],
        );
    }

    #[test]
    fn value_references_carry_their_enclosing_function_and_module() {
        let shapes = extract_value_reference_shapes_with_module(
            "use crate::m::key;\nmod inner { impl S { fn go(v: V) { v.map(super::m::key); } } }\n",
            CallIndexOptions::default(),
            "app",
        )
        .unwrap();
        let facts: Vec<_> = shapes
            .iter()
            .map(|reference| {
                (
                    reference.site.caller_module(),
                    reference.site.caller_qualified_name(),
                )
            })
            .collect();
        assert_eq!(
            facts,
            [
                (Some("app"), None),
                (Some("app::inner"), Some("app::inner::S::go")),
            ],
        );
    }

    /// The reference walk shares the visitor with the call index, which
    /// must keep returning calls and nothing else.
    #[test]
    fn the_call_index_records_no_value_references() {
        let sites = run("use crate::m::key;\nfn a(v: V) { v.map(m::key); (m::other)(m::key); }\n");
        let names: Vec<_> = sites.iter().map(|s| s.callee_name.as_deref()).collect();
        assert_eq!(names, [Some("map"), Some("other")]);
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
    #[case::parenthesised_fn_pointer_param("fn pump(emit: (fn(u8))) { emit(1); }", true)]
    #[case::reference_pattern_param("fn pump(&emit: &fn(u8)) { emit(1); }", true)]
    #[case::parenthesised_pattern_local("fn pump() { let (emit) = |e: u8| {}; emit(1); }", true)]
    #[case::plain_local("fn pump() { let emit = compute(); emit(1); }", false)]
    #[case::value_param("fn pump(emit: u8) { emit(1); }", false)]
    // A generic bound that is not an `Fn` trait says nothing about
    // callability, so the parameter is an ordinary value.
    #[case::non_fn_generic_param("fn pump<T: Clone>(emit: T) { emit(1); }", false)]
    #[case::non_fn_impl_trait_param("fn pump(emit: impl Clone) { emit(1); }", false)]
    #[case::unbound_name("fn pump() { emit(1); }", false)]
    fn local_callable_bindings_shadow_bare_calls(#[case] src: &str, #[case] expected: bool) {
        let sites = run(src);
        let site = sites
            .iter()
            .find(|site| site.callee_name.as_deref() == Some("emit"))
            .expect("emit call site");
        assert_eq!(site.callee_is_locally_bound, expected);
    }

    /// `Type::Group` wraps a type that arrived through a macro's token
    /// stream, so it never appears in a file parsed from source — build
    /// one directly to pin that the wrapper is peeled like `Type::Paren`.
    #[test]
    fn grouped_types_are_peeled_when_deciding_callability() {
        let grouped = Type::Group(syn::TypeGroup {
            attrs: Vec::new(),
            group_token: Default::default(),
            elem: Box::new(syn::parse_str("fn(u8)").unwrap()),
        });
        let grouped_value = Type::Group(syn::TypeGroup {
            attrs: Vec::new(),
            group_token: Default::default(),
            elem: Box::new(syn::parse_str("u8").unwrap()),
        });

        assert!(type_is_callable(&grouped, &HashSet::new()));
        assert!(!type_is_callable(&grouped_value, &HashSet::new()));
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
    /// not visible inside it, and its own bindings do not leak out. Its
    /// calls are still the enclosing function's, since it is no node.
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
                (Some("pump".to_owned()), false),
                (Some("pump".to_owned()), true),
            ]
        );
    }

    /// A method of an `impl` nested in a function body is no node either,
    /// so its calls go to the enclosing function, but its `self` is the
    /// nested type's: `self.c()` must not read as the outer `S::c`.
    #[test]
    fn nested_impl_methods_keep_their_own_owner() {
        let sites = run("impl S { fn a() { struct T; impl T { fn b(&self) { self.c(); } } } }");
        let site = sites
            .iter()
            .find(|site| site.callee_name.as_deref() == Some("c"))
            .expect("c call site");
        assert_eq!(site.caller_name.as_deref(), Some("S::a"));
        assert_eq!(site.caller_impl_owner.as_deref(), Some("T"));
    }

    /// Calls in the arguments of an expression- or statement-position
    /// macro are sites of the enclosing function, nested macros
    /// included; arguments with their own grammar (`vec![x; n]`) or a
    /// pattern (`matches!`) stay opaque.
    #[rstest]
    #[case::assert_eq("fn t() { assert_eq!(f(1), 2); }", &["f"])]
    #[case::format_in_expr("fn t() -> String { format!(\"{}\", f(1)) }", &["f"])]
    #[case::write_method("fn t(o: &mut W) { write!(o, \"{}\", x.g()).unwrap(); }", &["unwrap", "g"])]
    #[case::nested("fn t() { assert!(vec![f()].is_empty()); }", &["is_empty", "f"])]
    #[case::repeat_form("fn t() { let _ = vec![f(); 2]; }", &[])]
    #[case::pattern("fn t() { assert!(matches!(x, Foo::Bar(_))); }", &[])]
    #[case::item_position("m!(f()); fn t() {}", &[])]
    fn macro_arguments_are_call_sites(#[case] src: &str, #[case] expected: &[&str]) {
        let sites = run(src);
        let found: Vec<&str> = sites
            .iter()
            .filter_map(|site| site.callee_name.as_deref())
            .collect();
        assert_eq!(found, expected, "{src}");
        assert!(
            sites
                .iter()
                .all(|site| site.caller_name.as_deref() == Some("t")),
            "{src}"
        );
    }

    /// Argument shapes: literals and constant-looking paths carry their
    /// text, bare lowercase identifiers stay identifiers, and arbitrary
    /// expressions are opaque. References peel to the value they name.
    #[test]
    fn call_arguments_are_classified_by_shape() {
        let sites =
            run("fn a(x: u8) { f(1, -2, \"s\", true, None, Color::Red, x, g(), &LIMIT); }\n");
        let site = sites
            .iter()
            .find(|s| s.callee_name.as_deref() == Some("f"))
            .expect("f call site");
        let text = |t: &str| t.to_owned();
        assert_eq!(
            site.arguments,
            vec![
                ArgumentShape::Literal { text: text("1") },
                ArgumentShape::Literal { text: text("-2") },
                ArgumentShape::Literal {
                    text: text("\"s\"")
                },
                ArgumentShape::Literal { text: text("true") },
                ArgumentShape::Const { text: text("None") },
                ArgumentShape::Const {
                    text: text("Color::Red")
                },
                ArgumentShape::Identifier { text: text("x") },
                ArgumentShape::Other,
                ArgumentShape::Const {
                    text: text("LIMIT")
                },
            ],
        );
    }

    /// The non-constant boundary of the classifier: negation of
    /// anything but a literal, a qualified-self path, and other
    /// expressions stay opaque, while parens peel to the value inside.
    #[rstest]
    #[case::negated_identifier("fn a(x: i32) { f(-x); }", ArgumentShape::Other)]
    #[case::not_a_negation("fn a() { f(!true); }", ArgumentShape::Other)]
    #[case::qualified_self_path("fn a() { f(<S as T>::VALUE); }", ArgumentShape::Other)]
    #[case::parenthesised_identifier(
        "fn a(x: i32) { f((x)); }",
        ArgumentShape::Identifier { text: "x".to_owned() }
    )]
    fn argument_shape_edge_cases(#[case] src: &str, #[case] expected: ArgumentShape) {
        let sites = run(src);
        let site = sites
            .iter()
            .find(|s| s.callee_name.as_deref() == Some("f"))
            .expect("f call site");
        assert_eq!(site.arguments, vec![expected]);
    }

    /// `Expr::Group` wraps an expression that arrived through a macro's
    /// token stream, so it never appears in a file parsed from source —
    /// build one directly to pin that the wrapper is peeled like a
    /// paren.
    #[test]
    fn grouped_argument_expressions_are_peeled() {
        let grouped = Expr::Group(syn::ExprGroup {
            attrs: Vec::new(),
            group_token: Default::default(),
            expr: Box::new(syn::parse_quote!(x)),
        });
        assert_eq!(
            argument_shape(&grouped),
            ArgumentShape::Identifier {
                text: "x".to_owned()
            },
        );
    }

    /// A method call's receiver is not an argument: only the
    /// parenthesised list is classified.
    #[test]
    fn method_call_arguments_exclude_the_receiver() {
        let sites = run("fn a(x: T) { x.foo(1); }\n");
        let site = sites
            .iter()
            .find(|s| s.callee_name.as_deref() == Some("foo"))
            .expect("foo call site");
        assert_eq!(
            site.arguments,
            vec![ArgumentShape::Literal {
                text: "1".to_owned()
            }],
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
        let shapes = extract_call_shapes_with_module(
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
        let shapes = extract_call_shapes_with_module(
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

    /// A glob binds no name, so two of them must not overwrite each
    /// other, and `use super::*;` in an inline module also brings in what
    /// the enclosing module imported — by name or by glob — unless the
    /// module binds that name itself. Regression for #659.
    #[rstest]
    #[case::two_globs(
        "use crate::a::*;\nuse crate::b::*;\nfn caller() { parse(); }\n",
        &[("*", "crate::a"), ("*", "crate::b")]
    )]
    #[case::super_glob_inherits_named_import(
        "use crate::a::parse;\nmod tests {\n    use super::*;\n    fn caller() { parse(); }\n}\n",
        &[("*", "crate"), ("parse", "crate::a::parse")]
    )]
    #[case::super_glob_inherits_glob(
        "use crate::a::*;\nmod tests {\n    use super::*;\n    fn caller() { parse(); }\n}\n",
        &[("*", "crate"), ("*", "crate::a")]
    )]
    #[case::own_import_shadows_inherited(
        "use crate::a::parse;\nmod tests {\n    use super::*;\n    use crate::b::parse;\n    fn caller() { parse(); }\n}\n",
        &[("*", "crate"), ("parse", "crate::b::parse")]
    )]
    #[case::own_import_shadows_inherited_in_either_order(
        "use crate::a::parse;\nmod tests {\n    use crate::b::parse;\n    use super::*;\n    fn caller() { parse(); }\n}\n",
        &[("*", "crate"), ("parse", "crate::b::parse")]
    )]
    #[case::grandparent_imports_come_through_two_globs(
        "use crate::a::parse;\nmod outer {\n    use super::*;\n    mod inner {\n        use super::*;\n        fn caller() { parse(); }\n    }\n}\n",
        &[("*", "crate"), ("*", "crate::outer"), ("parse", "crate::a::parse")]
    )]
    #[case::glob_of_another_module_inherits_nothing(
        "use crate::a::parse;\nmod tests {\n    use crate::b::*;\n    fn caller() { parse(); }\n}\n",
        &[("*", "crate::b")]
    )]
    fn glob_imports_are_kept_and_carry_the_enclosing_modules_imports(
        #[case] src: &str,
        #[case] expected: &[(&str, &str)],
    ) {
        let sites = extract_call_sites_with_options_and_base_module(
            src,
            CallIndexOptions {
                include_cfg_test_blocks: true,
            },
            "crate",
        )
        .unwrap();
        assert_eq!(sites.len(), 1);
        let aliases: Vec<(&str, &str)> = sites[0]
            .visible_aliases
            .iter()
            .map(|alias| (alias.alias.as_str(), alias.target.as_str()))
            .collect();
        assert_eq!(aliases, expected);
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

    /// 2018 uniform paths: a `use` whose leading segment is a `mod`
    /// declared beside it is relative to the current module, exactly as
    /// if written `self::…`. A bare extern-crate path binds no alias —
    /// no workspace node can match it. Regression for #579, where
    /// `use geo::area as surface;` left `surface()` unresolved.
    #[rstest]
    #[case::file_module(
        "mod geo;\nuse geo::area as surface;\nuse serde::Serialize;\nfn caller() { surface(); }\n",
        "shapes::geo::area"
    )]
    #[case::inline_module(
        "mod outer {\n    mod inner {}\n    use inner::area as surface;\n    fn caller() { surface(); }\n}\n",
        "shapes::outer::inner::area"
    )]
    #[case::block_scoped_use(
        "mod geo;\nfn caller() { use geo::area as surface; surface(); }\n",
        "shapes::geo::area"
    )]
    fn use_paths_through_a_child_module_are_absolutized(#[case] src: &str, #[case] target: &str) {
        let sites = extract_call_sites_with_options_and_base_module(
            src,
            CallIndexOptions::default(),
            "shapes",
        )
        .unwrap();
        assert_eq!(sites.len(), 1);
        assert_eq!(
            sites[0].visible_aliases,
            [UseAlias {
                alias: "surface".to_owned(),
                target: target.to_owned(),
            }]
        );
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

    #[rstest]
    #[case::function("fn a() { m::apply::<T>(1); }", "m::apply")]
    #[case::type_arguments("fn a() { Vec::<u8>::new(); }", "Vec::new")]
    #[case::qualified_self_is_kept("fn a() { <S as T>::make(); }", "< S as T >::make")]
    fn generic_arguments_are_no_part_of_a_callee_path(#[case] src: &str, #[case] expected: &str) {
        let sites = run(src);
        assert_eq!(sites[0].callee_path.as_deref(), Some(expected));
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
