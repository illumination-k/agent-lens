//! Statement-sequence extraction for sub-function similarity.
//!
//! One [`BlockSite`] per suite reachable from a `def` body: the body
//! itself, plus every nested suite — `if` / `elif` / `else` branches, loop
//! bodies, `with` blocks, `try` / `except` / `finally` blocks, and `match`
//! case bodies. Nested `def`s and classes are descended into as well,
//! because [`walk_module_fns`] leaves them inside their enclosing
//! function rather than emitting them as units of their own.

use lens_domain::{BlockSite, LineIndex, SourceSpan, StatementShape};
use ruff_python_ast::{ExceptHandler, Stmt};
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;

use crate::parser::{PythonParseError, stmt_tree};
use crate::walk::walk_module_fns;

/// Collect every statement suite inside every function in `source`.
pub fn extract_blocks(source: &str) -> Result<Vec<BlockSite>, PythonParseError> {
    let module = parse_module(source)?.into_syntax();
    let lines = LineIndex::new(source);
    let mut out = Vec::new();
    walk_module_fns(&module.body, &mut |site| {
        let owner = lens_domain::qualify(site.owner, site.func.name.as_str());
        let mut collector = BlockCollector {
            owner: &owner,
            is_test: site.is_test,
            lines: &lines,
            out: &mut out,
        };
        collector.collect_suite(&site.func.body);
    });
    Ok(out)
}

struct BlockCollector<'a> {
    owner: &'a str,
    is_test: bool,
    lines: &'a LineIndex,
    out: &'a mut Vec<BlockSite>,
}

impl BlockCollector<'_> {
    fn collect_suite(&mut self, body: &[Stmt]) {
        if !body.is_empty() {
            let statements = body.iter().map(|stmt| self.statement_shape(stmt)).collect();
            self.out.push(BlockSite {
                owner: self.owner.to_owned(),
                is_test: self.is_test,
                statements,
            });
        }
        for stmt in body {
            self.descend(stmt);
        }
    }

    /// Recurse into the suites a single statement encloses.
    fn descend(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::If(it) => {
                self.collect_suite(&it.body);
                for clause in &it.elif_else_clauses {
                    self.collect_suite(&clause.body);
                }
            }
            Stmt::For(it) => {
                self.collect_suite(&it.body);
                self.collect_suite(&it.orelse);
            }
            Stmt::While(it) => {
                self.collect_suite(&it.body);
                self.collect_suite(&it.orelse);
            }
            Stmt::With(it) => self.collect_suite(&it.body),
            Stmt::Try(it) => {
                self.collect_suite(&it.body);
                for handler in &it.handlers {
                    let ExceptHandler::ExceptHandler(handler) = handler;
                    self.collect_suite(&handler.body);
                }
                self.collect_suite(&it.orelse);
                self.collect_suite(&it.finalbody);
            }
            Stmt::Match(it) => {
                for case in &it.cases {
                    self.collect_suite(&case.body);
                }
            }
            Stmt::FunctionDef(it) => self.collect_suite(&it.body),
            Stmt::ClassDef(it) => self.collect_suite(&it.body),
            _ => {}
        }
    }

    fn statement_shape(&self, stmt: &Stmt) -> StatementShape {
        let range = stmt.range();
        // `range.end()` lands just past the last byte, so step back one to
        // land on the line the statement actually ends on.
        let end_offset = range.end().to_u32().saturating_sub(1);
        StatementShape {
            span: SourceSpan {
                start_line: self.lines.line(range.start().to_u32()),
                end_line: self.lines.line(end_offset),
            },
            tree: stmt_tree(stmt),
        }
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
        let source = "def handler():\n    a = 1\n    b = 2\n";
        let sites = sites(source);

        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].owner, "handler");
        assert_eq!(sites[0].statements.len(), 2);
        assert_eq!(sites[0].statements[0].span.start_line, 2);
        assert_eq!(sites[0].statements[1].span.end_line, 3);
    }

    #[test]
    fn a_statement_spanning_several_lines_keeps_its_full_span() {
        let source =
            "def handler():\n    value = compute(\n        first,\n        second,\n    )\n";
        let stmt = &sites(source)[0].statements[0];

        assert_eq!(stmt.span.start_line, 2);
        assert_eq!(stmt.span.end_line, 5);
    }

    #[rstest]
    #[case::if_branch("def h(f):\n    if f:\n        a()\n        b()\n")]
    #[case::else_branch(
        "def h(f):\n    if f:\n        pass\n    else:\n        a()\n        b()\n"
    )]
    #[case::for_body("def h(xs):\n    for x in xs:\n        a()\n        b()\n")]
    #[case::try_block(
        "def h():\n    try:\n        a()\n        b()\n    except E:\n        pass\n"
    )]
    #[case::with_block("def h():\n    with open(p) as f:\n        a()\n        b()\n")]
    fn nested_suites_become_their_own_sites(#[case] source: &str) {
        let sites = sites(source);

        assert!(sites.len() >= 2, "expected a nested site: {sites:?}");
        assert!(sites.iter().any(|s| s.statements.len() == 2));
    }

    #[test]
    fn owner_qualifies_methods_with_their_class() {
        let sites = sites("class C:\n    def go(self):\n        a = 1\n        b = 2\n");

        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].owner, "C::go");
    }

    #[test]
    fn test_functions_are_flagged() {
        let sites = sites("def test_handler():\n    a = 1\n    b = 2\n");

        assert!(sites.iter().all(|s| s.is_test));
    }

    #[test]
    fn invalid_source_is_an_error() {
        assert!(extract_blocks("def (").is_err());
    }
}
