//! tree-sitter-based thin-wrapper detection for Go.
//!
//! A wrapper is a top-level function or method whose body is a single
//! statement that directly forwards all parameters to one call. We keep
//! the same output shape as the Rust / TS / Python adapters so
//! `agent-lens analyze wrapper` can dispatch by language without special
//! handling.

use lens_domain::{WrapperFinding, args_pass_through_by, qualify};
use tree_sitter::Node;

use crate::parser::{GoParseError, function_name_text, method_receiver_type, parse_tree};

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
    let expr = statement_expr(stmt)?;
    let (callee, args) = core_call(expr, source)?;
    let params = collect_param_names(node, source);

    if !args_pass_through_by(&args, &params, |n| ident_arg(*n, source).map(str::to_owned)) {
        return None;
    }

    Some(WrapperFinding {
        name,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        callee,
        adapters: Vec::new(),
        statement_count: 1,
        reuse: None,
    })
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

fn statement_expr(stmt: Node<'_>) -> Option<Node<'_>> {
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
                let mut inner = expr.walk();
                let mut xs = expr.named_children(&mut inner);
                let only = xs.next()?;
                if xs.next().is_some() {
                    return None;
                }
                return Some(only);
            }
            Some(expr)
        }
        _ => None,
    }
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
    Some((node_text(callee, source), args))
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
    {
        out.push(node_text(name, source));
    }
}

fn ident_arg<'a>(node: Node<'a>, source: &'a [u8]) -> Option<&'a str> {
    if node.kind() == "identifier" {
        node.utf8_text(source).ok()
    } else {
        None
    }
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

fn node_text(node: Node<'_>, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or_default().to_owned()
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
    fn detects_multiple_param_passthrough() {
        let src = "package p\nfunc Wrap(a int, b int) int { return target(b, a) }\n";
        let got = find_wrappers(src).unwrap();
        assert_eq!(got.len(), 1);
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
}
