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

use std::collections::BTreeMap;

use lens_domain::{
    CallShape, ImportShape, LexicalResolutionStatus, ReceiverExprKind, SyntaxFact, qualify,
};
use proc_macro2::TokenStream;
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Block, Expr, ExprCall, ExprMethodCall, Ident, ImplItem, Item, ItemUse, TraitItem, UseTree,
};

use crate::attrs::has_cfg_test;
use crate::common::type_path_last_ident;
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
                if site
                    .callee_path
                    .as_deref()
                    .is_some_and(|path| path.starts_with("self."))
                {
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
            Item::Fn(item_fn) => self.visit_block_in_fn_scope(&item_fn.sig.ident, &item_fn.block),
            Item::Use(item_use) => self.add_aliases_from_use(item_use),
            other => visit::visit_item(self, other),
        }
    }

    /// Push the qualified name of `ident` onto the caller stack, walk
    /// `block`, and pop. Shared by the `Item::Fn` and `ImplItem::Fn`
    /// arms — both used to spell this loop out themselves.
    fn visit_block_in_fn_scope(&mut self, ident: &Ident, block: &Block) {
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
        visit::visit_block(self, block);
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
        self.sites.push(CallSite {
            callee_name,
            callee_path,
            caller_name: caller.as_ref().map(|c| c.name.clone()),
            module: self.current_module().to_owned(),
            caller_qualified_name: caller.as_ref().map(|c| c.qualified_name.clone()),
            caller_impl_owner: caller.and_then(|c| c.impl_owner),
            call_kind,
            visible_aliases: self.current_aliases(),
            line,
        });
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
            self.visit_block_in_fn_scope(&method.sig.ident, &method.block);
        } else {
            visit::visit_impl_item(self, impl_item);
        }
    }

    fn visit_trait_item(&mut self, trait_item: &'ast TraitItem) {
        if let TraitItem::Fn(method) = trait_item
            && let Some(block) = &method.default
        {
            self.visit_block_in_fn_scope(&method.sig.ident, block);
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
        "crate" => Some(segments.join("::")),
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

fn qualify_module(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{module}::{name}")
    }
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

fn render_tokens<T: ToTokens>(node: &T) -> String {
    let mut stream = TokenStream::new();
    node.to_tokens(&mut stream);
    let raw = stream.to_string();
    raw.replace(" :: ", "::")
        .replace(" . ", ".")
        .replace(" ;", ";")
        .replace("& ", "&")
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
