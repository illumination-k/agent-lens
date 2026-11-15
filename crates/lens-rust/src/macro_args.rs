//! The expressions written in a macro invocation's arguments.
//!
//! `syn` hands a macro's arguments over as tokens and does not expand
//! the macro, so a call or a field read written there is invisible
//! unless the tokens are parsed again. The call graph
//! ([`crate::call_index`]) and cohesion ([`crate::cohesion`]) both read
//! them through [`macro_argument_exprs`], so the two agree on what a
//! function's macro arguments reference.

use proc_macro2::TokenStream;
use syn::parse::ParseStream;
use syn::punctuated::Punctuated;
use syn::{Expr, Macro, Token};

/// The expressions written in the arguments of the macro invocation
/// `mac`.
///
/// * Arguments that parse as a comma-separated list of expressions
///   (`assert_eq!`, `vec!`, `format!`, `write!`, `println!`, `dbg!`, a
///   local `macro_rules!` taking `$e:expr`) yield each expression. A
///   named argument (`format!("{n}", n = f())`) yields its value: the
///   name on the left is a label, not a read.
/// * `matches!(expr, pattern ..)` and the `assert_matches!` family
///   yield the scrutinee only. What follows it is a pattern, where
///   `Foo::Bar(_)` is a variant, not a call.
/// * Anything else yields nothing: a macro with its own grammar
///   (`vec![x; n]`, `select! { .. }`, a DSL) keeps its contents
///   invisible rather than being read half-way. `let_assert!(pat = expr)`
///   leads with a pattern and is skipped for the same reason.
///
/// What the macro's own definition does with these arguments stays out
/// of reach.
pub(crate) fn macro_argument_exprs(mac: &Macro) -> Vec<Expr> {
    let name = mac.path.segments.last().map(|s| s.ident.to_string());
    match name.as_deref() {
        Some("let_assert") => Vec::new(),
        Some("matches" | "assert_matches" | "debug_assert_matches") => mac
            .parse_body_with(leading_expr)
            .map(|scrutinee| vec![scrutinee])
            .unwrap_or_default(),
        _ => mac
            .parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated)
            .map(|args| args.into_iter().map(named_argument_value).collect())
            .unwrap_or_default(),
    }
}

/// Parse the expression a token stream starts with and discard the rest.
fn leading_expr(input: ParseStream<'_>) -> syn::Result<Expr> {
    let expr = input.parse()?;
    input.parse::<TokenStream>()?;
    Ok(expr)
}

/// `name = value` in a macro's argument list is a named argument, so
/// only `value` is an expression of the enclosing function.
fn named_argument_value(arg: Expr) -> Expr {
    match arg {
        Expr::Assign(assign) if is_bare_ident(&assign.left) => *assign.right,
        other => other,
    }
}

fn is_bare_ident(expr: &Expr) -> bool {
    matches!(expr, Expr::Path(p) if p.path.get_ident().is_some())
}

/// Property-based tests: cohesion and the call graph agree (#660). A
/// call or a `self.field` read stays visible to both however deep it
/// sits in macros whose arguments are expressions, and invisible to
/// both once any wrapper on the way has its own grammar. The
/// example-based tables live with each consumer.
#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use crate::call_index::extract_call_sites;
    use crate::cohesion::extract_cohesion_units;

    /// Deepest generated nesting of wrappers.
    const MAX_DEPTH: usize = 6;

    type Layer = (&'static str, &'static str);

    /// Wrappers whose argument is an expression of the enclosing function.
    const TRANSPARENT: &[Layer] = &[
        ("vec![", "]"),
        ("format!(\"{}\", ", ")"),
        ("format!(\"{v}\", v = ", ")"),
        ("dbg!(", ")"),
        ("assert!(", ")"),
        ("assert_eq!(", ", 1)"),
        ("matches!(", ", _)"),
        ("assert_matches!(", ", Some(_))"),
        ("id!(", ")"),
        ("(", ")"),
        ("(|| ", ")()"),
        ("{ ", " }"),
    ];

    /// Wrappers that hide their argument: no expression list.
    const OPAQUE: &[Layer] = &[
        ("vec![", "; 2]"),
        ("select! { v = ", " => v }"),
        ("matches!(x, ", ")"),
        ("let_assert!(Some(_) = ", ")"),
    ];

    /// `inner` wrapped in `layers`, the first layer innermost.
    fn wrap(inner: &str, layers: &[Layer]) -> String {
        layers.iter().fold(inner.to_owned(), |acc, (open, close)| {
            format!("{open}{acc}{close}")
        })
    }

    fn layers(of: &'static [Layer]) -> impl Strategy<Value = Vec<Layer>> {
        prop::collection::vec(prop::sample::select(of), 0..=MAX_DEPTH)
    }

    /// Sibling calls cohesion records for the module function `caller`.
    fn module_calls(source: &str, caller: &str) -> Vec<String> {
        let units = extract_cohesion_units(source).expect("generated source must parse");
        let unit = units.last().expect("the module unit comes last");
        let found = unit.methods.iter().find(|m| m.name == caller);
        found.expect("caller is a module function").calls.clone()
    }

    /// Whether the call graph has a `caller` → `callee` call site.
    fn has_call_site(source: &str, caller: &str, callee: &str) -> bool {
        extract_call_sites(source)
            .expect("generated source must parse")
            .iter()
            .any(|site| {
                site.caller_name.as_deref() == Some(caller)
                    && site.callee_name.as_deref() == Some(callee)
            })
    }

    fn caller_of(expr: &str) -> String {
        format!("fn caller(x: u8) {{ let _ = {expr}; }}\nfn callee(x: u8) -> u8 {{ x }}\n")
    }

    proptest! {
        /// However deep a call sits in expression-list macros, cohesion
        /// records the sibling edge and the call graph the call site.
        #[test]
        fn calls_in_macro_arguments_are_seen_by_cohesion_and_call_graph(
            layers in layers(TRANSPARENT),
        ) {
            let source = caller_of(&wrap("callee(x)", &layers));
            prop_assert_eq!(module_calls(&source, "caller"), ["callee"], "{}", source);
            prop_assert!(has_call_site(&source, "caller", "callee"), "{}", source);
        }

        /// One wrapper with its own grammar anywhere on the way hides the
        /// call from both, so they never disagree.
        #[test]
        fn an_opaque_macro_hides_the_call_from_both(
            inner in layers(TRANSPARENT),
            opaque in prop::sample::select(OPAQUE),
            outer in layers(TRANSPARENT),
        ) {
            let wrapped = wrap(&wrap(&wrap("callee(x)", &inner), &[opaque]), &outer);
            let source = caller_of(&wrapped);
            prop_assert!(module_calls(&source, "caller").is_empty(), "{}", source);
            prop_assert!(!has_call_site(&source, "caller", "callee"), "{}", source);
        }

        /// The same for `impl` units: a `self.field` read and a
        /// `self.method()` call survive any depth of transparent wrappers.
        #[test]
        fn self_refs_in_macro_arguments_are_seen_by_cohesion(
            field_layers in layers(TRANSPARENT),
            call_layers in layers(TRANSPARENT),
        ) {
            let source = format!(
                "impl S {{\n    fn a(&self) {{ let _ = {}; let _ = {}; }}\n    fn b(&self) -> u8 {{ 1 }}\n}}\n",
                wrap("self.n", &field_layers),
                wrap("self.b()", &call_layers),
            );
            let units = extract_cohesion_units(&source).expect("generated source must parse");
            let a = &units[0].methods[0];
            prop_assert_eq!(&a.fields, &["n"], "{}", source);
            prop_assert_eq!(&a.calls, &["b"], "{}", source);
        }
    }
}
