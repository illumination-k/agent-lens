//! Statement lists inside Go function bodies, for
//! `similarity --target blocks`.
//!
//! `tree-sitter-go` gives every statement sequence the same
//! `statement_list` node — function bodies, `if` / `for` / `select`
//! bodies, bare blocks, and `switch` case clauses alike — so one node
//! kind is the whole rule. Collecting by kind rather than by parent also
//! means a case clause's `case 1:` operand can never be mistaken for one
//! of its statements: the operand is a sibling of the `statement_list`,
//! not a child.
//!
//! `func_literal` bodies are descended into and attributed to the
//! enclosing declaration, matching [`crate::walk`]: closures are part of
//! their parent's unit there too, so their statements would otherwise be
//! invisible.

use lens_domain::{StatementSeq, StatementUnit};
use tree_sitter::Node;

use crate::attrs::name_looks_like_test_function;
use crate::parser::{GoParseError, parse_tree, statement_tree};
use crate::walk::walk_top_level_fns;

/// Node kind holding a run of statements. Every Go statement sequence
/// funnels through it.
const STATEMENT_LIST: &str = "statement_list";

/// Collect every statement list in `source`, tagged with the function it
/// belongs to. Lists are emitted outermost-first within a function so a
/// caller de-duplicating by span keeps the enclosing statement's tree.
pub fn extract_statement_seqs(source: &str) -> Result<Vec<StatementSeq>, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    walk_top_level_fns(tree.root_node(), bytes, &mut |site| {
        let owner = site.owner.as_deref();
        let is_test = owner.is_none() && name_looks_like_test_function(site.name);
        let function_name = lens_domain::qualify(owner, site.name);
        collect_lists(site.body, bytes, &function_name, is_test, &mut out);
    });
    Ok(out)
}

/// Record `node`'s statements when it is a [`STATEMENT_LIST`], then
/// recurse into every named child so nested lists are reached.
fn collect_lists(
    node: Node<'_>,
    source: &[u8],
    function_name: &str,
    is_test: bool,
    out: &mut Vec<StatementSeq>,
) {
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
    if node.kind() == STATEMENT_LIST && !children.is_empty() {
        out.push(StatementSeq {
            function_name: function_name.to_owned(),
            is_test,
            statements: children
                .iter()
                .map(|child| statement_unit(*child, source))
                .collect(),
        });
    }
    for child in children {
        collect_lists(child, source, function_name, is_test, out);
    }
}

fn statement_unit(node: Node<'_>, source: &[u8]) -> StatementUnit {
    StatementUnit {
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        tree: statement_tree(node, source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn seqs(source: &str) -> Vec<StatementSeq> {
        extract_statement_seqs(source).expect("parses")
    }

    fn shape(seqs: &[StatementSeq]) -> Vec<(&str, Vec<(usize, usize)>)> {
        seqs.iter()
            .map(|s| {
                (
                    s.function_name.as_str(),
                    s.statements
                        .iter()
                        .map(|st| (st.start_line, st.end_line))
                        .collect(),
                )
            })
            .collect()
    }

    #[test]
    fn collects_the_body_and_every_nested_block_outermost_first() {
        let src = "package p\n\nfunc handler(x int) int {\n\ta := x + 1\n\tif a > 0 {\n\t\tb := a * 2\n\t\treturn b\n\t}\n\treturn a\n}\n";

        assert_eq!(
            shape(&seqs(src)),
            vec![
                ("handler", vec![(4, 4), (5, 8), (9, 9)]),
                ("handler", vec![(6, 6), (7, 7)]),
            ],
        );
    }

    /// Switch and select cases hang their statements off a
    /// `statement_list` that is a sibling of the clause operand, so the
    /// operand (`case 1:`, `case v := <-ch:`) must never appear as a
    /// statement.
    #[rstest]
    #[case::expression_case(
        "package p\n\nfunc f(x int) {\n\tswitch x {\n\tcase 1:\n\t\ta := 1\n\t\tuse(a)\n\t}\n}\n",
        vec![(6, 6), (7, 7)]
    )]
    #[case::default_case(
        "package p\n\nfunc f(x int) {\n\tswitch x {\n\tdefault:\n\t\ta := 1\n\t\tuse(a)\n\t}\n}\n",
        vec![(6, 6), (7, 7)]
    )]
    #[case::type_case(
        "package p\n\nfunc f(x any) {\n\tswitch x.(type) {\n\tcase int:\n\t\ta := 1\n\t\tuse(a)\n\t}\n}\n",
        vec![(6, 6), (7, 7)]
    )]
    #[case::communication_case(
        "package p\n\nfunc f(ch chan int) {\n\tselect {\n\tcase v := <-ch:\n\t\ta := v\n\t\tuse(a)\n\t}\n}\n",
        vec![(6, 6), (7, 7)]
    )]
    fn case_clause_statements_exclude_the_clause_operand(
        #[case] src: &str,
        #[case] expected: Vec<(usize, usize)>,
    ) {
        let collected = seqs(src);
        let last = collected.last().expect("at least one sequence");
        assert_eq!(
            last.statements
                .iter()
                .map(|st| (st.start_line, st.end_line))
                .collect::<Vec<_>>(),
            expected,
        );
    }

    #[test]
    fn closure_bodies_belong_to_the_enclosing_declaration() {
        let src = "package p\n\nfunc outer() {\n\tg := func() int {\n\t\ta := 1\n\t\treturn a\n\t}\n\tuse(g)\n}\n";
        let collected = seqs(src);

        assert!(collected.iter().all(|s| s.function_name == "outer"));
        assert_eq!(shape(&collected)[1].1, vec![(5, 5), (6, 6)]);
    }

    /// The whole point of reusing the shared lowering is that a block
    /// window's statements are the very subtrees the function body tree
    /// carries. Go nests them one level deeper than the other adapters —
    /// its body root is `Block` wrapping a single `statement_list` — so
    /// the comparison unwraps that one node.
    #[test]
    fn statement_trees_match_the_function_body_lowering() {
        let src = "package p\n\nfunc f() int {\n\ta := 1\n\treturn a\n}\n";
        let collected = seqs(src);
        let body = lens_domain::LanguageParser::extract_functions(&mut crate::GoParser::new(), src)
            .expect("parses")
            .remove(0)
            .body_tree()
            .clone();
        let statement_list = &body.children[0];
        assert_eq!(statement_list.label, "statement_list");

        let statement_trees: Vec<_> = collected[0]
            .statements
            .iter()
            .map(|st| st.tree.clone())
            .collect();
        assert_eq!(statement_list.children, statement_trees);
    }

    #[test]
    fn methods_carry_the_receiver_qualified_name() {
        let src =
            "package p\n\ntype C struct{}\n\nfunc (c *C) Get() int {\n\ta := 1\n\treturn a\n}\n";
        assert_eq!(seqs(src)[0].function_name, "C::Get");
    }

    /// `go test` only discovers *free* functions, so the test flag
    /// needs both halves: a test-shaped name and no receiver. A method
    /// called `TestThing` is production code with an unlucky name.
    #[rstest]
    #[case::free_test_function("package p\n\nfunc TestThing(t *testing.T) {\n\ta := 1\n\tuse(a)\n}\n", true)]
    #[case::method_with_test_name(
        "package p\n\ntype C struct{}\n\nfunc (c *C) TestThing() {\n\ta := 1\n\tuse(a)\n}\n",
        false
    )]
    #[case::plain_free_function("package p\n\nfunc Thing() {\n\ta := 1\n\tuse(a)\n}\n", false)]
    fn test_flag_needs_a_test_name_and_no_receiver(#[case] src: &str, #[case] expected: bool) {
        let collected = seqs(src);
        assert!(!collected.is_empty());
        assert!(collected.iter().all(|s| s.is_test == expected));
    }

    #[test]
    fn empty_bodies_produce_no_sequence() {
        assert!(seqs("package p\n\nfunc f() {}\n").is_empty());
    }

    #[test]
    fn parse_error_is_reported() {
        assert!(extract_statement_seqs("package p\n\nfunc (").is_err());
    }
}
