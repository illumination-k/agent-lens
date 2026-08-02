//! Statement-sequence extraction for sub-function similarity.
//!
//! tree-sitter-go groups every run of statements under a `statement_list`
//! node — a function body, an `if` / `for` / `select` body, and a `switch`
//! case clause all reach their statements through one — so a single node
//! kind is enough to find every sequence. `func_literal` bodies are
//! included: Go closures belong to their enclosing function here, the same
//! way they do for whole-function similarity.

use lens_domain::{BlockSite, SourceSpan, StatementShape};
use tree_sitter::Node;

use crate::parser::{GoParseError, build_tree, parse_tree};
use crate::walk::walk_top_level_fns;

/// Collect every statement sequence inside every function in `source`.
pub fn extract_blocks(source: &str) -> Result<Vec<BlockSite>, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    walk_top_level_fns(tree.root_node(), bytes, &mut |site| {
        let owner = lens_domain::qualify(site.owner.as_deref(), site.name);
        let is_test =
            site.owner.is_none() && crate::attrs::name_looks_like_test_function(site.name);
        collect(site.body, bytes, &owner, is_test, &mut out);
    });
    Ok(out)
}

fn collect(node: Node<'_>, source: &[u8], owner: &str, is_test: bool, out: &mut Vec<BlockSite>) {
    if node.kind() == "statement_list" {
        let statements = statement_shapes(node, source);
        if !statements.is_empty() {
            out.push(BlockSite {
                owner: owner.to_owned(),
                is_test,
                statements,
            });
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect(child, source, owner, is_test, out);
    }
}

/// Lower a `statement_list`'s children. Comments are dropped: they sit in
/// the named-child list but are not statements, and letting them in would
/// let two unrelated runs match on their comment shape.
fn statement_shapes(node: Node<'_>, source: &[u8]) -> Vec<StatementShape> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| child.kind() != "comment")
        .map(|child| statement_shape(child, source))
        .collect()
}

fn statement_shape(node: Node<'_>, source: &[u8]) -> StatementShape {
    StatementShape {
        span: SourceSpan {
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
        },
        tree: build_tree(node, source, /* is_root = */ false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn sites(source: &str) -> Vec<BlockSite> {
        extract_blocks(source).expect("parses")
    }

    #[test]
    fn function_body_becomes_one_site_with_per_statement_spans() {
        let source = "package p\n\nfunc handler() {\n\ta := 1\n\tb := 2\n\t_, _ = a, b\n}\n";
        let sites = sites(source);

        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].owner, "handler");
        assert_eq!(sites[0].statements.len(), 3);
        assert_eq!(sites[0].statements[0].span.start_line, 4);
        assert_eq!(sites[0].statements[2].span.end_line, 6);
    }

    #[test]
    fn a_statement_spanning_several_lines_keeps_its_full_span() {
        let source = "package p\n\nfunc handler() {\n\tvalue := compute(\n\t\tfirst,\n\t\tsecond,\n\t)\n\t_ = value\n}\n";
        let stmt = &sites(source)[0].statements[0];

        assert_eq!(stmt.span.start_line, 4);
        assert_eq!(stmt.span.end_line, 7);
    }

    #[rstest]
    #[case::if_body("package p\n\nfunc h(f bool) {\n\tif f {\n\t\ta()\n\t\tb()\n\t}\n}\n")]
    #[case::for_body(
        "package p\n\nfunc h() {\n\tfor i := 0; i < 3; i++ {\n\t\ta()\n\t\tb()\n\t}\n}\n"
    )]
    #[case::switch_case(
        "package p\n\nfunc h(x int) {\n\tswitch x {\n\tcase 1:\n\t\ta()\n\t\tb()\n\t}\n}\n"
    )]
    fn nested_statement_lists_become_their_own_sites(#[case] source: &str) {
        let sites = sites(source);

        assert!(sites.len() >= 2, "expected a nested site: {sites:?}");
        assert!(sites.iter().any(|s| s.statements.len() == 2));
    }

    #[test]
    fn a_case_clause_reports_only_its_body_statements() {
        let source =
            "package p\n\nfunc h(x int) {\n\tswitch x {\n\tcase 1:\n\t\ta()\n\t\tb()\n\t}\n}\n";
        let case_site = sites(source)
            .into_iter()
            .find(|s| s.statements.len() == 2)
            .expect("case clause site");

        assert_eq!(case_site.statements[0].span.start_line, 6);
        assert_eq!(case_site.statements[1].span.start_line, 7);
    }

    #[test]
    fn comments_are_not_statements() {
        let source = "package p\n\nfunc handler() {\n\t// leading\n\ta := 1\n\t_ = a\n}\n";
        let sites = sites(source);

        assert_eq!(sites[0].statements.len(), 2);
    }

    #[test]
    fn owner_qualifies_methods_with_their_receiver() {
        let source = "package p\n\ntype C struct{}\n\nfunc (c *C) Go() {\n\ta := 1\n\t_ = a\n}\n";
        let sites = sites(source);

        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].owner, "C::Go");
    }

    #[test]
    fn test_functions_are_flagged() {
        let source = "package p\n\nfunc TestHandler(t *T) {\n\ta := 1\n\t_ = a\n}\n";

        assert!(sites(source).iter().all(|s| s.is_test));
    }

    #[test]
    fn invalid_source_is_an_error() {
        assert!(extract_blocks("package p\n\nfunc (").is_err());
    }
}
