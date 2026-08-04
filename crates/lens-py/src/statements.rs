//! Statement lists inside Python function bodies, for
//! `similarity --target blocks`.
//!
//! One [`StatementSeq`] per suite reachable from a `def`: the body
//! itself, then every nested `if` / `for` / `while` / `with` / `try` /
//! `match` suite. ruff's generated visitor routes every one of those
//! through `visit_body`, so the recursion is a single override rather
//! than a hand-rolled ladder that would miss whichever compound
//! statement nobody thought of.
//!
//! Nested `def`s and `class`es are descended into and attributed to the
//! enclosing function, matching [`crate::walk`]: a nested definition is
//! part of its parent's unit there too, so its statements would
//! otherwise be invisible.

use lens_domain::{LineIndex, StatementSeq, StatementUnit};
use ruff_python_ast::visitor::{Visitor, walk_body};
use ruff_python_ast::{Stmt, StmtFunctionDef};
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;

use crate::parser::{PythonParseError, stmt_tree};
use crate::walk::walk_module_fns;

/// Collect every statement list in `source`, tagged with the function it
/// belongs to. Lists are emitted outermost-first within a function so a
/// caller de-duplicating by span keeps the enclosing statement's tree.
pub fn extract_statement_seqs(source: &str) -> Result<Vec<StatementSeq>, PythonParseError> {
    let module = parse_module(source)?.into_syntax();
    let lines = LineIndex::new(source);
    let mut out = Vec::new();
    walk_module_fns(&module.body, &mut |site| {
        let name = lens_domain::qualify(site.owner, site.func.name.as_str());
        collect_function(site.func, &name, site.is_test, &lines, &mut out);
    });
    Ok(out)
}

fn collect_function(
    func: &StmtFunctionDef,
    function_name: &str,
    is_test: bool,
    lines: &LineIndex,
    out: &mut Vec<StatementSeq>,
) {
    let mut collector = SeqCollector {
        function_name,
        is_test,
        lines,
        out,
    };
    collector.visit_body(&func.body);
}

struct SeqCollector<'a> {
    function_name: &'a str,
    is_test: bool,
    lines: &'a LineIndex,
    out: &'a mut Vec<StatementSeq>,
}

impl<'ast> Visitor<'ast> for SeqCollector<'_> {
    fn visit_body(&mut self, body: &'ast [Stmt]) {
        if !body.is_empty() {
            self.out.push(StatementSeq {
                function_name: self.function_name.to_owned(),
                is_test: self.is_test,
                statements: body.iter().map(|stmt| self.statement_unit(stmt)).collect(),
            });
        }
        walk_body(self, body);
    }
}

impl SeqCollector<'_> {
    fn statement_unit(&self, stmt: &Stmt) -> StatementUnit {
        let range = stmt.range();
        // `range.end()` lands just past the last byte of the statement;
        // the line we want is the one that byte sits on.
        let end_offset = range.end().to_u32().saturating_sub(1);
        StatementUnit {
            start_line: self.lines.line(range.start().to_u32()),
            end_line: self.lines.line(end_offset),
            tree: stmt_tree(stmt),
        }
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
    fn collects_the_body_and_every_nested_suite_outermost_first() {
        let src = "\ndef handler(x):\n    a = x + 1\n    if a > 0:\n        b = a * 2\n        return b\n    return a\n";

        assert_eq!(
            shape(&seqs(src)),
            vec![
                ("handler", vec![(3, 3), (4, 6), (7, 7)]),
                ("handler", vec![(5, 5), (6, 6)]),
            ],
        );
    }

    #[rstest]
    #[case::except("def f():\n    try:\n        go()\n    except E:\n        log()\n        raise\n", vec![(5, 5), (6, 6)])]
    #[case::for_body("def f(xs):\n    for x in xs:\n        total += x\n        count += 1\n", vec![(3, 3), (4, 4)])]
    #[case::with_body("def f():\n    with open(p) as fh:\n        data = fh.read()\n        use(data)\n", vec![(3, 3), (4, 4)])]
    fn nested_suites_are_reached(#[case] src: &str, #[case] expected: Vec<(usize, usize)>) {
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

    /// A nested `def` is part of its parent's unit in [`crate::walk`],
    /// so its suite is collected under the parent's name rather than
    /// being dropped.
    #[test]
    fn nested_def_suites_belong_to_the_enclosing_function() {
        let src =
            "def outer():\n    def inner():\n        a = 1\n        return a\n    return inner\n";
        let collected = seqs(src);

        assert!(collected.iter().all(|s| s.function_name == "outer"));
        assert_eq!(shape(&collected)[1].1, vec![(3, 3), (4, 4)]);
    }

    /// The whole point of reusing `stmt_tree` is that a window covering
    /// a full body lowers to the same tree that body does.
    #[test]
    fn statement_trees_match_the_function_body_lowering() {
        let src = "def f():\n    a = 1\n    return a\n";
        let collected = seqs(src);
        let body =
            lens_domain::LanguageParser::extract_functions(&mut crate::PythonParser::new(), src)
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
    #[case::method("class Bag:\n    def go(self):\n        a = 1\n", "Bag::go")]
    #[case::free("def go():\n    a = 1\n", "go")]
    fn sequences_carry_the_qualified_function_name(#[case] src: &str, #[case] expected: &str) {
        assert_eq!(seqs(src)[0].function_name, expected);
    }

    #[test]
    fn test_functions_are_flagged_so_callers_can_filter_them() {
        let collected = seqs("def test_thing():\n    a = 1\n");
        assert!(collected.iter().all(|s| s.is_test));
    }

    #[test]
    fn parse_error_is_reported() {
        assert!(extract_statement_seqs("def (").is_err());
    }
}
