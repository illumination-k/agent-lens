//! Statement lists inside Rust function bodies, for
//! `similarity --target blocks`.
//!
//! One [`StatementSeq`] per `syn::Block` reachable from a function body —
//! the body itself plus every nested `if` / loop / `match`-arm / `unsafe`
//! / closure block. `syn`'s generated visitor already knows how to reach
//! all of them, so the recursion is a single `visit_block` override
//! rather than a hand-rolled expression ladder that would silently miss
//! whichever `Expr` variant nobody thought of.
//!
//! Statements are lowered with the same `stmt_tree` the function-body
//! tree uses, so a window covering a whole body and that body compare as
//! identical trees.

use lens_domain::{StatementSeq, StatementUnit};
use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::common::{WalkOptions, walk_fn_items};
use crate::parser::{RustParseError, stmt_tree};

/// Collect every statement list in `source`, tagged with the function it
/// belongs to. Lists are emitted outermost-first within a function so a
/// caller de-duplicating by span keeps the enclosing statement's tree.
pub fn extract_statement_seqs(source: &str) -> Result<Vec<StatementSeq>, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut out = Vec::new();
    walk_fn_items(&file.items, WalkOptions::default(), &mut |site| {
        let name = lens_domain::qualify(site.owner, &site.sig.ident.to_string());
        let mut collector = SeqCollector {
            function_name: name,
            is_test: site.is_test,
            out: &mut out,
        };
        collector.visit_block(site.block);
    });
    Ok(out)
}

/// Records one [`StatementSeq`] per block visited under a single
/// function. Overriding `visit_block` and then delegating back to the
/// default walk keeps the traversal depth-first and outermost-first.
struct SeqCollector<'a> {
    function_name: String,
    is_test: bool,
    out: &'a mut Vec<StatementSeq>,
}

impl<'ast> Visit<'ast> for SeqCollector<'_> {
    fn visit_block(&mut self, block: &'ast syn::Block) {
        if !block.stmts.is_empty() {
            self.out.push(StatementSeq {
                function_name: self.function_name.clone(),
                is_test: self.is_test,
                statements: block.stmts.iter().map(statement_unit).collect(),
            });
        }
        syn::visit::visit_block(self, block);
    }
}

fn statement_unit(stmt: &syn::Stmt) -> StatementUnit {
    let span = stmt.span();
    StatementUnit {
        start_line: span.start().line,
        end_line: span.end().line,
        tree: stmt_tree(stmt),
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
        let src = r#"
fn handler(x: i32) -> i32 {
    let a = x + 1;
    if a > 0 {
        let b = a * 2;
        return b;
    }
    a
}
"#;
        let collected = seqs(src);

        assert_eq!(
            shape(&collected),
            vec![
                ("handler", vec![(3, 3), (4, 7), (8, 8)]),
                ("handler", vec![(5, 5), (6, 6)]),
            ],
        );
    }

    /// The whole point of reusing `stmt_tree` is that a window covering
    /// a full body lowers to the same tree that body does; pin it
    /// against the parser's own lowering rather than a label list, which
    /// would drift silently the next time a syntax category is renamed.
    #[test]
    fn statement_trees_match_the_function_body_lowering() {
        let src = "fn f() {\n    let a = 1;\n    a\n}\n";
        let collected = seqs(src);
        let body =
            lens_domain::LanguageParser::extract_functions(&mut crate::RustParser::new(), src)
                .expect("parses")
                .remove(0)
                .body_tree()
                .clone();

        let statement_trees: Vec<_> = collected[0]
            .statements
            .iter()
            .map(|st| st.tree.clone())
            .collect();
        assert_eq!(body.children, statement_trees);
    }

    #[rstest]
    #[case::method(
        "struct S;\nimpl S {\n    fn go(&self) {\n        let a = 1;\n    }\n}\n",
        "S::go"
    )]
    #[case::trait_default(
        "trait T {\n    fn go(&self) {\n        let a = 1;\n    }\n}\n",
        "T::go"
    )]
    #[case::free("fn go() {\n    let a = 1;\n}\n", "go")]
    fn sequences_carry_the_qualified_function_name(#[case] src: &str, #[case] expected: &str) {
        let collected = seqs(src);
        assert_eq!(collected[0].function_name, expected);
    }

    /// `--target blocks` is the one comparison that turns value
    /// matching on, because a statement run's identifiers are what
    /// separate a pasted fragment from the language's own idiom (issue
    /// #441). A `let` pattern that dropped its bound name left the
    /// statement with no such content at all.
    #[rstest]
    #[case::plain("fn f() {\n    let parsed = decode(raw);\n}\n", "PatIdent", "parsed")]
    #[case::mutable("fn f() {\n    let mut acc = Vec::new();\n}\n", "PatIdentMut", "acc")]
    fn a_let_pattern_carries_the_name_it_binds(
        #[case] src: &str,
        #[case] label: &str,
        #[case] expected: &str,
    ) {
        let collected = seqs(src);
        let pat = &collected[0].statements[0].tree.children[0];

        assert_eq!(pat.label, label);
        assert_eq!(pat.value, expected);
    }

    #[test]
    fn test_functions_are_flagged_so_callers_can_filter_them() {
        let collected = seqs("#[test]\nfn t() {\n    let a = 1;\n}\n");
        assert!(collected.iter().all(|s| s.is_test));
    }

    #[test]
    fn empty_bodies_produce_no_sequence() {
        assert!(seqs("fn f() {}\n").is_empty());
    }

    #[test]
    fn closure_and_match_arm_blocks_are_reached() {
        let src = r#"
fn f(v: Option<i32>) {
    let g = |x: i32| {
        let y = x + 1;
        y
    };
    match v {
        Some(n) => {
            let z = n;
            drop(z);
        }
        None => {}
    }
}
"#;
        let collected = seqs(src);
        // Body, closure block, and the `Some` arm block; the empty
        // `None` arm contributes nothing.
        assert_eq!(collected.len(), 3);
        assert_eq!(
            collected[1]
                .statements
                .iter()
                .map(|st| st.start_line)
                .collect::<Vec<_>>(),
            vec![4, 5],
        );
    }

    #[test]
    fn parse_error_is_reported() {
        assert!(extract_statement_seqs("fn (").is_err());
    }
}
