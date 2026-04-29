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

use lens_domain::qualify;
use proc_macro2::TokenStream;
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Block, Expr, ExprCall, ExprMethodCall, Ident, ImplItem, Item, TraitItem};

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
    /// 1-based line number of the call expression.
    pub line: usize,
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
    let file = syn::parse_file(source)?;
    let mut visitor = CallVisitor::new(opts);
    for item in &file.items {
        visitor.visit_item_filtered(item);
    }
    Ok(visitor.into_sites())
}

struct CallVisitor {
    opts: CallIndexOptions,
    /// Stack of qualified caller names. The top of the stack is the
    /// nearest enclosing function — closures and nested `fn` items
    /// inherit their parent's name (refining that would require
    /// minting synthetic names like `outer::{closure#1}`, which buys
    /// no agent signal today).
    callers: Vec<String>,
    /// Stack of `impl` self-type names so methods inside `impl Foo`
    /// can be qualified as `Foo::method`. Pushed on entry to
    /// `Item::Impl` and popped on exit.
    impl_owners: Vec<Option<String>>,
    sites: Vec<CallSite>,
}

impl CallVisitor {
    fn new(opts: CallIndexOptions) -> Self {
        Self {
            opts,
            callers: Vec::new(),
            impl_owners: Vec::new(),
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
    fn visit_item_filtered(&mut self, item: &Item) {
        match item {
            Item::Mod(item_mod) => {
                if !self.opts.include_cfg_test_blocks && has_cfg_test(&item_mod.attrs) {
                    return;
                }
                if let Some((_, items)) = &item_mod.content {
                    for nested in items {
                        self.visit_item_filtered(nested);
                    }
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
            other => visit::visit_item(self, other),
        }
    }

    /// Push the qualified name of `ident` onto the caller stack, walk
    /// `block`, and pop. Shared by the `Item::Fn` and `ImplItem::Fn`
    /// arms — both used to spell this loop out themselves.
    fn visit_block_in_fn_scope(&mut self, ident: &Ident, block: &Block) {
        let name = qualify(self.current_owner(), &ident.to_string());
        self.callers.push(name);
        visit::visit_block(self, block);
        self.callers.pop();
    }

    fn current_owner(&self) -> Option<&str> {
        self.impl_owners.last().and_then(|o| o.as_deref())
    }

    fn current_caller(&self) -> Option<String> {
        self.callers.last().cloned()
    }

    fn record(&mut self, callee_name: Option<String>, callee_path: Option<String>, line: usize) {
        self.sites.push(CallSite {
            callee_name,
            callee_path,
            caller_name: self.current_caller(),
            line,
        });
    }
}

impl<'ast> Visit<'ast> for CallVisitor {
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
        self.record(callee_name, callee_path, line);
        // Recurse into arguments and the callee expression so nested
        // calls get their own sites (e.g. `outer(inner())` records both).
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        let line = call.span().start().line;
        let callee_name = Some(call.method.to_string());
        let receiver = render_tokens(call.receiver.as_ref());
        let callee_path = Some(format!("{receiver}.{}", call.method));
        self.record(callee_name, callee_path, line);
        visit::visit_expr_method_call(self, call);
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
