//! Detect "thin wrapper" Rust functions: bodies that, after peeling a
//! short chain of trivial adapters, are just a forwarding call to
//! another function with the parameters passed straight through.
//!
//! Conceptually a wrapper is a function whose body adds no logic of its
//! own — it only renames, narrows visibility, or coerces types. Things
//! that DO add logic (extra statements, branching, argument
//! transformations, literal arguments) keep the function out of the
//! report.

use lens_domain::WrapperFinding;
use proc_macro2::TokenStream;
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::{
    Block, Expr, ExprCall, ExprMethodCall, FnArg, ImplItem, Item, ItemImpl, ItemMod, ItemTrait,
    Pat, PatIdent, Signature, Stmt, TraitItem, UnOp,
};

use crate::attrs::has_cfg_test;
use crate::parser::RustParseError;

/// Method names with no arguments that we treat as "no semantic content":
/// type/borrow coercions and single-call result unwrapping.
const TRIVIAL_NULLARY_ADAPTERS: &[&str] = &[
    "into",
    "try_into",
    "unwrap",
    "unwrap_or_default",
    "clone",
    "to_owned",
    "to_string",
    "as_ref",
    "as_mut",
    "as_str",
    "as_slice",
];

/// Method names whose only argument is a literal we treat as a no-op
/// (e.g. `expect("msg")`).
const TRIVIAL_LITERAL_ADAPTERS: &[&str] = &["expect"];

/// `(trait_name, method_name)` pairs whose forwarding bodies are
/// idiomatic boilerplate, not refactoring opportunities. The trait spec
/// itself dictates the signature, so the only reasonable implementation
/// is to forward — flagging these would just add noise to the report.
///
/// Match is on the last segment of the trait path (e.g. `From` for
/// `std::convert::From`), so users don't need to spell out the full
/// import path.
const BOILERPLATE_TRAIT_METHODS: &[(&str, &str)] = &[
    ("From", "from"),
    ("Default", "default"),
    ("Deref", "deref"),
    ("DerefMut", "deref_mut"),
    ("Borrow", "borrow"),
    ("BorrowMut", "borrow_mut"),
    ("AsRef", "as_ref"),
    ("AsMut", "as_mut"),
];

/// Walk the file and return every function whose body is just a
/// forwarding call.
pub fn find_wrappers(source: &str) -> Result<Vec<WrapperFinding>, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut out = Vec::new();
    for item in &file.items {
        collect_item(item, &mut out);
    }
    Ok(out)
}

fn collect_item(item: &Item, out: &mut Vec<WrapperFinding>) {
    match item {
        Item::Fn(item_fn) => {
            if let Some(finding) = analyze_fn(None, None, &item_fn.sig, &item_fn.block) {
                out.push(finding);
            }
        }
        Item::Impl(item_impl) => collect_impl(item_impl, out),
        Item::Trait(item_trait) => collect_trait(item_trait, out),
        Item::Mod(item_mod) => collect_mod(item_mod, out),
        _ => {}
    }
}

fn collect_mod(item_mod: &ItemMod, out: &mut Vec<WrapperFinding>) {
    // Skip `#[cfg(test)] mod tests { ... }` (and any other cfg(test)-gated
    // module). Test helpers are forwarding by design — they exist to
    // shorten call sites in `assert_eq!` lines, not because the wrapped
    // function needed an extra layer. Reporting them is pure noise.
    if has_cfg_test(&item_mod.attrs) {
        return;
    }
    if let Some((_, items)) = &item_mod.content {
        for nested in items {
            collect_item(nested, out);
        }
    }
}

fn collect_impl(item_impl: &ItemImpl, out: &mut Vec<WrapperFinding>) {
    let self_name = type_path_last_ident(&item_impl.self_ty);
    let trait_name = item_impl
        .trait_
        .as_ref()
        .and_then(|(_, path, _)| path.segments.last().map(|s| s.ident.to_string()));
    for impl_item in &item_impl.items {
        if let ImplItem::Fn(method) = impl_item
            && let Some(finding) = analyze_fn(
                self_name.as_deref(),
                trait_name.as_deref(),
                &method.sig,
                &method.block,
            )
        {
            out.push(finding);
        }
    }
}

fn collect_trait(item_trait: &ItemTrait, out: &mut Vec<WrapperFinding>) {
    let trait_name = item_trait.ident.to_string();
    for trait_item in &item_trait.items {
        if let TraitItem::Fn(method) = trait_item
            && let Some(block) = &method.default
            && let Some(finding) =
                analyze_fn(Some(&trait_name), Some(&trait_name), &method.sig, block)
        {
            out.push(finding);
        }
    }
}

fn type_path_last_ident(ty: &syn::Type) -> Option<String> {
    if let syn::Type::Path(type_path) = ty {
        type_path
            .path
            .segments
            .last()
            .map(|seg| seg.ident.to_string())
    } else {
        None
    }
}

fn qualify_name(owner: Option<&str>, method: &str) -> String {
    match owner {
        Some(owner) => format!("{owner}::{method}"),
        None => method.to_owned(),
    }
}

fn analyze_fn(
    owner: Option<&str>,
    trait_name: Option<&str>,
    sig: &Signature,
    block: &Block,
) -> Option<WrapperFinding> {
    let method_name = sig.ident.to_string();
    if is_boilerplate_trait_method(trait_name, &method_name) {
        return None;
    }
    let tail = single_tail_expr(block)?;
    let (core, adapters) = peel_adapters(tail);
    let (callee, call_args) = core_call(core)?;
    let params = collect_param_idents(sig);
    if !args_pass_through(call_args, &params) {
        return None;
    }
    Some(WrapperFinding {
        name: qualify_name(owner, &method_name),
        start_line: sig.span().start().line,
        end_line: block.span().end().line,
        callee,
        adapters,
    })
}

fn is_boilerplate_trait_method(trait_name: Option<&str>, method: &str) -> bool {
    let Some(trait_name) = trait_name else {
        return false;
    };
    BOILERPLATE_TRAIT_METHODS
        .iter()
        .any(|&(t, m)| t == trait_name && m == method)
}

/// Return the single tail expression of a block, ignoring an optional
/// `return ...;` wrapping. Anything more elaborate (let-bindings,
/// multiple statements, macros, control flow) returns None.
fn single_tail_expr(block: &Block) -> Option<&Expr> {
    let [stmt] = block.stmts.as_slice() else {
        return None;
    };
    match stmt {
        Stmt::Expr(expr, None) => Some(strip_return(expr)),
        Stmt::Expr(Expr::Return(ret), Some(_)) => ret.expr.as_deref(),
        _ => None,
    }
}

fn strip_return(expr: &Expr) -> &Expr {
    match expr {
        Expr::Return(ret) => ret.expr.as_deref().unwrap_or(expr),
        other => other,
    }
}

/// Strip `?` and trivial method-call adapters from the outside. Returns
/// the innermost expression and the list of adapter labels in
/// outer-to-inner order (so a render-friendly chain is straightforward).
fn peel_adapters(expr: &Expr) -> (&Expr, Vec<String>) {
    let mut current = expr;
    let mut adapters_outer_to_inner = Vec::new();
    loop {
        match current {
            Expr::Try(try_expr) => {
                adapters_outer_to_inner.push("?".to_owned());
                current = &try_expr.expr;
            }
            Expr::Paren(paren) => current = &paren.expr,
            Expr::Group(group) => current = &group.expr,
            Expr::MethodCall(method) if is_trivial_method_call(method) => {
                adapters_outer_to_inner.push(format!(".{}()", method.method));
                current = &method.receiver;
            }
            _ => break,
        }
    }
    // Inner-to-outer matches the source order ("first b(x), then .unwrap(), then ?").
    adapters_outer_to_inner.reverse();
    (current, adapters_outer_to_inner)
}

fn is_trivial_method_call(method: &ExprMethodCall) -> bool {
    let name = method.method.to_string();
    if method.args.is_empty() && TRIVIAL_NULLARY_ADAPTERS.contains(&name.as_str()) {
        return true;
    }
    if TRIVIAL_LITERAL_ADAPTERS.contains(&name.as_str())
        && method.args.len() == 1
        && matches!(method.args.first(), Some(Expr::Lit(_)))
    {
        return true;
    }
    false
}

/// If `expr` is a function call or a method call whose callee/receiver
/// is itself a "thin" path (no nested computation), return its rendered
/// callee path and the argument list.
///
/// This rejects forms like `foo(x).bar(x)` where the method receiver is
/// already doing real work: the outer function adds a real call on top
/// of another, so it isn't a wrapper.
fn core_call(expr: &Expr) -> Option<(String, &syn::punctuated::Punctuated<Expr, syn::Token![,]>)> {
    match expr {
        Expr::Call(ExprCall { func, args, .. }) => {
            if !is_thin_path(func) {
                return None;
            }
            Some((render_tokens(func), args))
        }
        Expr::MethodCall(ExprMethodCall {
            receiver,
            method,
            args,
            ..
        }) => {
            if !is_thin_path(receiver) {
                return None;
            }
            let recv = render_tokens(receiver.as_ref());
            Some((format!("{recv}.{method}"), args))
        }
        _ => None,
    }
}

/// Path-shaped expressions: a name, a field chain (`self.inner.x`), or
/// the same wrapped in references, parens, or invisible groups. Method
/// calls and function calls anywhere inside are rejected — those add
/// computation, not just navigation.
fn is_thin_path(expr: &Expr) -> bool {
    match expr {
        Expr::Path(_) => true,
        Expr::Field(field) => is_thin_path(&field.base),
        Expr::Reference(reference) => is_thin_path(&reference.expr),
        Expr::Paren(paren) => is_thin_path(&paren.expr),
        Expr::Group(group) => is_thin_path(&group.expr),
        _ => false,
    }
}

fn render_tokens<T: ToTokens>(node: &T) -> String {
    let mut stream = TokenStream::new();
    node.to_tokens(&mut stream);
    let raw = stream.to_string();
    // `proc-macro2` re-emits tokens with stable spacing, but it still
    // injects spaces around `::` and `.`. Collapse them so that the
    // rendered callee reads like a Rust path rather than a token dump.
    raw.replace(" :: ", "::")
        .replace(" . ", ".")
        .replace(" ;", ";")
        .replace("& ", "&")
}

fn collect_param_idents(sig: &Signature) -> Vec<String> {
    let mut out = Vec::new();
    for input in &sig.inputs {
        if let FnArg::Typed(typed) = input
            && let Pat::Ident(PatIdent { ident, .. }) = typed.pat.as_ref()
        {
            out.push(ident.to_string());
        }
        // `Receiver` (self / &self / &mut self) carries no name binding
        // for the call; we skip it deliberately.
    }
    out
}

/// True iff every call argument is a parameter passed through (with at
/// most a wrapping `&`/`&mut`/`*`), every parameter is used exactly
/// once, and the arity matches.
fn args_pass_through(
    args: &syn::punctuated::Punctuated<Expr, syn::Token![,]>,
    params: &[String],
) -> bool {
    if args.len() != params.len() {
        return false;
    }
    let mut seen = vec![false; params.len()];
    for arg in args {
        let Some(name) = passthrough_ident(arg) else {
            return false;
        };
        let Some(pos) = params.iter().position(|p| p == &name) else {
            return false;
        };
        if seen[pos] {
            return false;
        }
        seen[pos] = true;
    }
    seen.iter().all(|hit| *hit)
}

fn passthrough_ident(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Path(path) => path.path.get_ident().map(|i| i.to_string()),
        Expr::Reference(reference) => passthrough_ident(&reference.expr),
        Expr::Unary(unary) if matches!(unary.op, UnOp::Deref(_)) => passthrough_ident(&unary.expr),
        Expr::Paren(paren) => passthrough_ident(&paren.expr),
        Expr::Group(group) => passthrough_ident(&group.expr),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn run(src: &str) -> Vec<WrapperFinding> {
        find_wrappers(src).unwrap()
    }

    fn names(findings: &[WrapperFinding]) -> Vec<&str> {
        findings.iter().map(|f| f.name.as_str()).collect()
    }

    #[test]
    fn detects_simple_forward() {
        let src = "fn a(x: i32) -> i32 { b(x) }\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].callee, "b");
        assert!(findings[0].adapters.is_empty());
    }

    #[test]
    fn detects_method_delegation() {
        let src = r#"
struct Service;
impl Service {
    fn handle(&self, x: i32) -> i32 { self.inner.handle(x) }
}
"#;
        let findings = run(src);
        assert_eq!(names(&findings), ["Service::handle"]);
        assert_eq!(findings[0].callee, "self.inner.handle");
    }

    #[test]
    fn detects_with_into_adapter() {
        let src = "fn a(x: i32) -> u64 { b(x).into() }\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].callee, "b");
        assert_eq!(findings[0].adapters, vec![".into()".to_string()]);
    }

    #[test]
    fn detects_with_try_operator() {
        let src = "fn a(x: i32) -> Result<i32, E> { b(x)? }\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].adapters, vec!["?".to_string()]);
    }

    #[test]
    fn detects_with_chained_adapters() {
        let src = "fn a(x: i32) -> String { b(x).unwrap().to_string() }\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(
            findings[0].adapters,
            vec![".unwrap()".to_string(), ".to_string()".to_string()]
        );
    }

    #[test]
    fn detects_with_expect_literal_adapter() {
        let src = "fn a(x: i32) -> i32 { b(x).expect(\"oops\") }\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].adapters, vec![".expect()".to_string()]);
    }

    /// Pure passthrough variants: each body forwards arguments to a
    /// single tail call in a different shape (explicit `return`, borrow
    /// / deref of args, reordering). The detector should report `a`
    /// in every case — the only thing that varies is the source.
    #[rstest]
    #[case::explicit_return("fn a(x: i32) -> i32 { return b(x); }\n")]
    #[case::borrow_and_deref_args("fn a(x: i32, y: i32) -> i32 { b(&x, *y) }\n")]
    #[case::reordered_args("fn a(x: i32, y: i32) -> i32 { b(y, x) }\n")]
    fn detects_passthrough_variant(#[case] src: &str) {
        assert_eq!(names(&run(src)), ["a"]);
    }

    /// Body shapes that disqualify a function from being a wrapper. The
    /// detector must return an empty report for each — the names label
    /// the rejection reason for at-a-glance failure attribution.
    #[rstest]
    #[case::arg_transformation("fn a(x: i32) -> i32 { b(x + 1) }\n")]
    #[case::multi_statement_body("fn a(x: i32) -> i32 { let y = x; b(y) }\n")]
    #[case::literal_only_body("fn a() -> i32 { 42 }\n")]
    #[case::empty_body("fn a() {}\n")]
    #[case::unrelated_arg_name("fn a(x: i32) -> i32 { b(y) }\n")]
    #[case::arity_mismatch_too_few("fn a(x: i32) -> i32 { b() }\n")]
    #[case::arity_mismatch_too_many("fn a(x: i32) -> i32 { b(x, x) }\n")]
    #[case::branching_body("fn a(x: i32) -> i32 { if x > 0 { b(x) } else { c(x) } }\n")]
    // foo(x).bar(x) — bar's receiver is itself a call, not a passthrough.
    #[case::chain_receiver_call("fn a(x: i32) -> i32 { foo(x).bar(x) }\n")]
    fn rejects_non_wrapper_shape(#[case] src: &str) {
        assert!(run(src).is_empty(), "expected no wrapper for: {src}");
    }

    #[test]
    fn extracts_qualified_method_name_for_traits() {
        let src = r#"
trait Greet {
    fn say(&self, x: i32) -> i32 { other(x) }
}
"#;
        let findings = run(src);
        assert_eq!(names(&findings), ["Greet::say"]);
    }

    #[test]
    fn records_line_numbers_from_signature_to_block_end() {
        let src = "\nfn first(x: i32) -> i32 {\n    b(x)\n}\n";
        let findings = run(src);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].start_line, 2);
        assert_eq!(findings[0].end_line, 4);
    }

    #[test]
    fn finds_wrappers_in_inline_modules() {
        let src = r#"
mod inner {
    fn shim(x: i32) -> i32 { core(x) }
}
"#;
        let findings = run(src);
        assert_eq!(names(&findings), ["shim"]);
    }

    #[test]
    fn skips_cfg_test_modules() {
        // `mod tests` under `#[cfg(test)]` is full of forwarding helpers
        // by construction; flagging them is noise, not signal.
        let src = r#"
fn shim(x: i32) -> i32 { core(x) }

#[cfg(test)]
mod tests {
    use super::*;
    fn run(s: &str) -> Vec<i32> { core_helper(s).unwrap() }
    fn extract(s: &str) -> Vec<i32> { core_helper(s).unwrap() }
}
"#;
        let findings = run(src);
        // Only the production-side `shim` survives; `run` and `extract`
        // are dropped because they live inside `#[cfg(test)]`.
        assert_eq!(names(&findings), ["shim"]);
    }

    /// `impl <Trait> for T` bodies whose `(trait, method)` pair is on
    /// the boilerplate skip list must drop out. Without that filter
    /// each of these forwarding bodies would otherwise trip the wrapper
    /// detector — there's no non-forwarding implementation that would
    /// be idiomatic.
    #[rstest]
    #[case::from_trait(
        r#"
struct Err;
impl From<std::io::Error> for Err {
    fn from(e: std::io::Error) -> Self { Self::Io(e) }
}
"#
    )]
    #[case::default_trait(
        r#"
struct T;
impl Default for T {
    fn default() -> Self { Self::new() }
}
"#
    )]
    #[case::deref_and_as_ref(
        r#"
struct W(String);
impl std::ops::Deref for W {
    type Target = String;
    fn deref(&self) -> &Self::Target { &self.0 }
}
impl AsRef<str> for W {
    fn as_ref(&self) -> &str { self.0.as_str() }
}
"#
    )]
    fn skips_boilerplate_trait_methods(#[case] src: &str) {
        assert!(
            run(src).is_empty(),
            "boilerplate trait method must be filtered, got: {:?}",
            names(&run(src)),
        );
    }

    #[test]
    fn does_not_skip_inherent_method_named_from() {
        // The boilerplate filter is keyed on `(trait, method)`, so an
        // inherent `fn from(...)` (not part of `impl From for ...`)
        // should still be reported.
        let src = r#"
struct T;
impl T {
    fn from(s: &str) -> Self { build(s) }
}
"#;
        let findings = run(src);
        assert_eq!(names(&findings), ["T::from"]);
    }

    #[test]
    fn boilerplate_filter_requires_both_trait_and_method_match() {
        // `From::other` shares the trait but not the method name; if the
        // filter degenerated to "trait OR method matches" it would drop
        // this finding. The forwarding shape qualifies as a wrapper, so
        // it must surface.
        let src = r#"
struct T;
impl From<i32> for T {
    fn other(x: i32) -> Self { build(x) }
}
"#;
        let findings = run(src);
        assert_eq!(
            names(&findings),
            ["T::other"],
            "From::other shares trait but not method, must still report",
        );
    }

    #[test]
    fn boilerplate_filter_requires_both_trait_and_method_match_other_side() {
        // Symmetric case: trait `Other` shares the method name `from`
        // with the boilerplate list but the trait doesn't. Must still
        // report — only `(From, from)` is the boilerplate combo.
        let src = r#"
struct T;
impl Other<i32> for T {
    fn from(x: i32) -> Self { build(x) }
}
"#;
        let findings = run(src);
        assert_eq!(
            names(&findings),
            ["T::from"],
            "Other::from shares method but not trait, must still report",
        );
    }

    #[test]
    fn parenthesised_adapter_chain_is_peeled() {
        // `(b(x).unwrap())` — the call sits inside parentheses, then
        // `.unwrap()`. `peel_adapters` must descend into Expr::Paren so
        // the underlying call is exposed and the wrapper is detected.
        let src = "fn a(x: i32) -> i32 { (b(x)).unwrap() }\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].callee, "b");
        assert_eq!(findings[0].adapters, vec![".unwrap()".to_string()]);
    }
}
