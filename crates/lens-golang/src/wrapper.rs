//! tree-sitter-based thin-wrapper detection for Go.
//!
//! A wrapper is a top-level function or method whose body is a single
//! statement that forwards all parameters to one call, optionally behind
//! a chain of trivial nullary conversion adapters (`.String()`,
//! `.Error()`, …). We keep the same output shape as the Rust / TS /
//! Python adapters so `agent-lens analyze wrapper` can dispatch by
//! language without special handling.

use lens_domain::{WrapperFinding, args_pass_through_by, qualify};
use tree_sitter::Node;

use crate::node_text::node_str;
use crate::parser::{GoParseError, function_name_text, method_receiver_type, parse_tree};

/// Zero-argument method calls that carry no semantic content of their
/// own — they just re-present the receiver in another form. Peeling them
/// keeps a wrapper like `func F(x T) string { return g(x).String() }`
/// visible instead of hiding it behind the conversion. Mirrors the
/// `TRIVIAL_NULLARY_ADAPTERS` allow-lists in the Rust / TS / Python
/// adapters; kept deliberately small to avoid swallowing methods that do
/// real work.
const TRIVIAL_NULLARY_ADAPTERS: &[&str] = &["String", "Error", "Bytes", "GoString"];

/// Extract wrapper findings from Go source.
pub fn find_wrappers(source: &str) -> Result<Vec<WrapperFinding>, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let mut out = Vec::new();

    let mut cursor = tree.root_node().walk();
    for child in tree.root_node().named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(f) = analyze_function(child, bytes, None) {
                    out.push(f);
                }
            }
            "method_declaration" => {
                let owner = method_receiver_type(child, bytes);
                if let Some(f) = analyze_function(child, bytes, owner.as_deref()) {
                    out.push(f);
                }
            }
            _ => {}
        }
    }

    Ok(out)
}

fn analyze_function(node: Node<'_>, source: &[u8], owner: Option<&str>) -> Option<WrapperFinding> {
    let body = node.child_by_field_name("body")?;
    let name = qualify(owner, function_name_text(node, source)?);
    let stmt = single_statement(body)?;
    let expr = statement_expr(stmt, source)?;
    let (inner, adapters) = peel_adapters(expr, source);
    let (callee, args) = core_call(inner, source)?;
    let params = collect_param_names(node, source);

    if !args_pass_through_by(&args, &params, |n| passthrough_arg_name(*n, source)) {
        return None;
    }

    Some(WrapperFinding {
        name,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        callee,
        adapters,
        statement_count: 1,
        reuse: None,
    })
}

/// Strip trivial nullary conversion method calls (`.String()`, …) and
/// surrounding parentheses from the outside of `expr`, returning the
/// innermost expression plus the adapter labels in source order
/// (innermost-applied first, matching the Rust / TS / Python output).
fn peel_adapters<'a>(expr: Node<'a>, source: &[u8]) -> (Node<'a>, Vec<String>) {
    let mut current = expr;
    let mut outer_to_inner = Vec::new();
    loop {
        match current.kind() {
            "parenthesized_expression" => {
                let Some(inner) = first_named_child(current) else {
                    break;
                };
                current = inner;
            }
            "call_expression" => {
                let Some((receiver, member)) = trivial_adapter_call(current, source) else {
                    break;
                };
                outer_to_inner.push(format!(".{member}()"));
                current = receiver;
            }
            _ => break,
        }
    }
    outer_to_inner.reverse();
    (current, outer_to_inner)
}

/// If `call` is a zero-argument method call whose method name is a
/// trivial adapter (`x.String()`), return the receiver expression and
/// the method name so the caller can keep peeling. `None` for anything
/// with arguments, a non-selector callee, or a non-adapter method.
fn trivial_adapter_call<'a>(call: Node<'a>, source: &[u8]) -> Option<(Node<'a>, String)> {
    let func = call.child_by_field_name("function")?;
    if func.kind() != "selector_expression" {
        return None;
    }
    let args = call.child_by_field_name("arguments")?;
    let mut cursor = args.walk();
    if args.named_children(&mut cursor).next().is_some() {
        return None;
    }
    let field = func.child_by_field_name("field")?;
    let member = node_str(field, source)?;
    if !TRIVIAL_NULLARY_ADAPTERS.contains(&member) {
        return None;
    }
    let operand = func.child_by_field_name("operand")?;
    Some((operand, member.to_owned()))
}

fn single_statement(block: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = block.walk();
    let mut iter = block.named_children(&mut cursor);
    let first = iter.next()?;
    if iter.next().is_some() {
        return None;
    }
    if first.kind() == "statement_list" {
        let mut inner = first.walk();
        let mut stmts = first.named_children(&mut inner);
        let stmt = stmts.next()?;
        if stmts.next().is_some() {
            return None;
        }
        return Some(stmt);
    }
    Some(first)
}

fn statement_expr<'a>(stmt: Node<'a>, source: &[u8]) -> Option<Node<'a>> {
    match stmt.kind() {
        "expression_statement" => stmt
            .child_by_field_name("expression")
            .or_else(|| first_named_child(stmt)),
        "return_statement" => {
            let mut cursor = stmt.walk();
            let mut it = stmt.named_children(&mut cursor);
            let expr = it.next()?;
            if it.next().is_some() {
                return None;
            }
            if expr.kind() == "expression_list" {
                return single_expression_list_child(expr);
            }
            Some(expr)
        }
        // `_, _ = call(...)` — a forwarding call whose results are
        // intentionally discarded. With every LHS being a blank
        // identifier, the statement carries no extra semantics, so the
        // RHS expression is the same shape we look at for an
        // expression-statement wrapper.
        "assignment_statement" => {
            let left = stmt.child_by_field_name("left")?;
            if !all_blank_identifiers(left, source) {
                return None;
            }
            let right = stmt.child_by_field_name("right")?;
            single_expression_list_child(right)
        }
        _ => None,
    }
}

fn single_expression_list_child(expr_list: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = expr_list.walk();
    let mut xs = expr_list.named_children(&mut cursor);
    let only = xs.next()?;
    if xs.next().is_some() {
        return None;
    }
    Some(only)
}

fn all_blank_identifiers(expr_list: Node<'_>, source: &[u8]) -> bool {
    let mut cursor = expr_list.walk();
    let mut any = false;
    for child in expr_list.named_children(&mut cursor) {
        any = true;
        if child.kind() != "identifier" || node_str(child, source) != Some("_") {
            return false;
        }
    }
    any
}

fn core_call<'a>(expr: Node<'a>, source: &'a [u8]) -> Option<(String, Vec<Node<'a>>)> {
    if expr.kind() != "call_expression" {
        return None;
    }
    let callee = expr.child_by_field_name("function")?;
    if !is_thin_callee(callee) {
        return None;
    }
    let args_node = expr.child_by_field_name("arguments")?;
    let mut cursor = args_node.walk();
    let args: Vec<Node<'_>> = args_node.named_children(&mut cursor).collect();
    Some((node_str(callee, source)?.to_owned(), args))
}

fn collect_param_names(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let Some(params) = node.child_by_field_name("parameters") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    let mut cursor = params.walk();
    for child in params.named_children(&mut cursor) {
        collect_param_decl_names(child, source, &mut names);
    }
    names
}

fn collect_param_decl_names(node: Node<'_>, source: &[u8], out: &mut Vec<String>) {
    if let Some(name) = node.child_by_field_name("name")
        && name.kind() == "identifier"
        && let Some(text) = node_str(name, source)
    {
        out.push(text.to_owned());
    }
}

fn passthrough_arg_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node_str(node, source).map(str::to_owned),
        "composite_literal" => {
            let body = node.child_by_field_name("body")?;
            single_literal_value_name(body, source)
        }
        "literal_value" | "literal_element" => single_literal_value_name(node, source),
        "keyed_element" => {
            let value = node.child_by_field_name("value")?;
            passthrough_arg_name(value, source)
        }
        // `args...` in a call site. The grammar wraps the spread in a
        // `variadic_argument` whose sole named child is the underlying
        // expression; matching its name against the corresponding
        // variadic parameter (which `collect_param_names` already picks
        // up) keeps `func f(... args ...T) { g(..., args...) }` looking
        // like a pure forward.
        "variadic_argument" => {
            first_named_child(node).and_then(|c| passthrough_arg_name(c, source))
        }
        _ => None,
    }
}

fn single_literal_value_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let mut cursor = node.walk();
    let mut children = node.named_children(&mut cursor);
    let only = children.next()?;
    if children.next().is_some() {
        return None;
    }
    passthrough_arg_name(only, source)
}

fn is_thin_callee(node: Node<'_>) -> bool {
    match node.kind() {
        "identifier" => true,
        "selector_expression" => node
            .child_by_field_name("operand")
            .is_some_and(is_thin_callee),
        _ => false,
    }
}

fn first_named_child(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_free_function_wrapper() {
        let src = "package p\nfunc Wrap(x int) int { return target(x) }\n";
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "Wrap");
        assert_eq!(got[0].callee, "target");
        assert_eq!(got[0].start_line, 2);
        assert_eq!(got[0].end_line, 2);
    }

    #[test]
    fn rejects_argument_transformations() {
        let src = "package p\nfunc Wrap(x int) int { return target(x+1) }\n";
        let got = find_wrappers(src).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn detects_method_wrapper_with_qualified_name() {
        let src =
            "package p\ntype S struct{}\nfunc (s *S) Wrap(x int) int { return s.inner.Do(x) }\n";
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "S::Wrap");
        assert_eq!(got[0].callee, "s.inner.Do");
    }

    #[test]
    fn detects_expression_statement_wrapper() {
        let src = "package p\nfunc Wrap(x int) { target(x) }\n";
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn rejects_non_thin_callee_chains() {
        let src = "package p\nfunc Wrap(x int) int { return mk().Do(x) }\n";
        let got = find_wrappers(src).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn detects_wrapper_behind_trivial_string_adapter() {
        // `target(x).String()` forwards `x` and then applies a
        // semantically-empty conversion. Before adapter peeling this
        // was invisible because `core_call` required the tail to be a
        // bare call. The adapter must be recorded on the finding.
        let src = "package p\nfunc Wrap(x int) string { return target(x).String() }\n";
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "Wrap");
        assert_eq!(got[0].callee, "target");
        assert_eq!(got[0].adapters, vec![".String()".to_owned()]);
    }

    #[test]
    fn detects_wrapper_behind_chained_trivial_adapters() {
        let src = "package p\nfunc Wrap(x int) string { return target(x).Bytes().String() }\n";
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].callee, "target");
        // Source order: `.Bytes()` applied first, then `.String()`.
        assert_eq!(
            got[0].adapters,
            vec![".Bytes()".to_owned(), ".String()".to_owned()],
        );
    }

    #[test]
    fn empty_adapters_for_bare_forwarding_call() {
        // A plain forward keeps `adapters` empty — peeling must not
        // invent labels for a call with no conversion chain.
        let src = "package p\nfunc Wrap(x int) int { return target(x) }\n";
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
        assert!(got[0].adapters.is_empty());
    }

    #[test]
    fn peels_adapter_through_parentheses() {
        // A parenthesized adapter chain must still be peeled to the inner
        // forwarding call.
        let src = "package p\nfunc Wrap(x int) string { return (target(x).String()) }\n";
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].callee, "target");
        assert_eq!(got[0].adapters, vec![".String()".to_owned()]);
    }

    #[test]
    fn rejects_adapter_call_with_arguments() {
        // `.Do(y)` takes an argument, so it is not a trivial adapter —
        // the receiver call is not a pure forward and nothing should be
        // peeled or reported.
        let src = "package p\nfunc Wrap(x int, y int) int { return target(x).Do(y) }\n";
        let got = find_wrappers(src).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn detects_multiple_param_passthrough() {
        let src = "package p\nfunc Wrap(a int, b int) int { return target(b, a) }\n";
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn detects_single_field_options_literal_passthrough() {
        let src = r#"
package p

type Options struct{ flag bool }

func Wrap(source string, flag bool) int {
    return target(source, Options{flag})
}
"#;
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "Wrap");
        assert_eq!(got[0].callee, "target");
    }

    #[test]
    fn detects_keyed_single_field_options_literal_passthrough() {
        let src = r#"
package p

type Options struct{ flag bool }

func Wrap(source string, flag bool) int {
    return target(source, Options{flag: flag})
}
"#;
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn rejects_options_literal_with_extra_values() {
        let src = r#"
package p

type Options struct{ flag bool; mode string }

func Wrap(source string, flag bool) int {
    return target(source, Options{flag, "strict"})
}
"#;
        let got = find_wrappers(src).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn rejects_parenthesized_callee_passthrough() {
        let src = "package p\nfunc Wrap(x int) int { return (target)(x) }\n";
        let got = find_wrappers(src).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn rejects_indexed_callee_passthrough() {
        let src =
            "package p\nfunc Wrap(fs []func(int) int, i int, x int) int { return fs[i](x) }\n";
        let got = find_wrappers(src).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn detects_variadic_passthrough_with_blank_assignment() {
        // The motivating shape: a Fprintf-style wrapper that drops both
        // return values via `_, _ =` and forwards `args...` through to
        // an inner variadic call. Every Go logging helper looks like
        // this, so missing it would dilute wrapper findings on real
        // code.
        let src = r#"
package p

import (
    "fmt"
    "io"
)

func writef(w io.Writer, format string, args ...any) {
    _, _ = fmt.Fprintf(w, format, args...)
}
"#;
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "writef");
        assert_eq!(got[0].callee, "fmt.Fprintf");
    }

    #[test]
    fn detects_single_blank_assignment_wrapper() {
        let src = "package p\nfunc Wrap(x int) { _ = target(x) }\n";
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].callee, "target");
    }

    #[test]
    fn rejects_assignment_with_named_lhs() {
        // `y = ...` keeps the value alive past the call; treating it as
        // a wrapper would hide a use-site that the agent should still
        // see. Only fully-discarded calls qualify.
        let src = "package p\nvar y int\nfunc Wrap(x int) { y = target(x) }\n";
        let got = find_wrappers(src).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn detects_variadic_passthrough_in_return() {
        let src = "package p\nfunc Wrap(args ...int) int { return target(args...) }\n";
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].callee, "target");
    }

    #[test]
    fn rejects_variadic_with_mismatched_name() {
        // `args...` only counts as a forward if the spread name matches
        // a parameter; a bare `xs...` against `args ...int` should not
        // pass the structural check.
        let src = "package p\nvar xs []int\nfunc Wrap(args ...int) int { return target(xs...) }\n";
        let got = find_wrappers(src).unwrap();
        assert!(got.is_empty());
    }
}
