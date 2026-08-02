//! Statement-sequence extraction for sub-function similarity.
//!
//! One [`BlockSite`] per statement list reachable from a function body:
//! the body itself, plus every nested block — `if` arms, loop bodies,
//! `try`/`catch`/`finally` blocks, and `switch` case consequents. Nested
//! *functions* are deliberately not descended into here; [`walk_program`]
//! already emits them as their own units, so descending would duplicate
//! every closure body.

use lens_domain::{BlockSite, LineIndex, SourceSpan, StatementShape};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::GetSpan;

use crate::parser::{Dialect, TsParseError, is_test_item};
use crate::tree::stmt_tree;
use crate::walk::{FunctionItem, FunctionVisitor, walk_program};

/// Collect every statement sequence inside every function in `source`.
pub fn extract_blocks(source: &str, dialect: Dialect) -> Result<Vec<BlockSite>, TsParseError> {
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, source, dialect.source_type()).parse();
    if !ret.diagnostics.is_empty() {
        return Err(TsParseError::from_diagnostics(
            ret.diagnostics
                .iter()
                .map(|e| e.message.as_ref().to_owned()),
        ));
    }
    let line_index = LineIndex::new(source);
    let mut collector = BlockCollector {
        out: Vec::new(),
        line_index: &line_index,
    };
    walk_program(&ret.program, &line_index, &mut collector);
    Ok(collector.out)
}

struct BlockCollector<'a> {
    out: Vec<BlockSite>,
    line_index: &'a LineIndex,
}

impl FunctionVisitor for BlockCollector<'_> {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        let owner = Owner {
            name: item.name.as_str(),
            is_test: is_test_item(&item.name),
        };
        self.collect_list(&item.body.statements, owner);
    }
}

#[derive(Clone, Copy)]
struct Owner<'a> {
    name: &'a str,
    is_test: bool,
}

impl BlockCollector<'_> {
    fn collect_list(&mut self, statements: &[Statement<'_>], owner: Owner<'_>) {
        if !statements.is_empty() {
            let shapes = statements
                .iter()
                .map(|stmt| self.statement_shape(stmt))
                .collect();
            self.out.push(BlockSite {
                owner: owner.name.to_owned(),
                is_test: owner.is_test,
                statements: shapes,
            });
        }
        for stmt in statements {
            self.descend(stmt, owner);
        }
    }

    /// Recurse into the statement lists a single statement encloses.
    fn descend(&mut self, stmt: &Statement<'_>, owner: Owner<'_>) {
        match stmt {
            Statement::BlockStatement(block) => self.collect_list(&block.body, owner),
            Statement::IfStatement(it) => {
                self.descend(&it.consequent, owner);
                if let Some(alternate) = &it.alternate {
                    self.descend(alternate, owner);
                }
            }
            Statement::WhileStatement(w) => self.descend(&w.body, owner),
            Statement::DoWhileStatement(w) => self.descend(&w.body, owner),
            Statement::ForStatement(f) => self.descend(&f.body, owner),
            Statement::ForInStatement(f) => self.descend(&f.body, owner),
            Statement::ForOfStatement(f) => self.descend(&f.body, owner),
            Statement::LabeledStatement(l) => self.descend(&l.body, owner),
            Statement::TryStatement(t) => {
                self.collect_list(&t.block.body, owner);
                if let Some(handler) = &t.handler {
                    self.collect_list(&handler.body.body, owner);
                }
                if let Some(finalizer) = &t.finalizer {
                    self.collect_list(&finalizer.body, owner);
                }
            }
            Statement::SwitchStatement(s) => {
                for case in &s.cases {
                    self.collect_list(&case.consequent, owner);
                }
            }
            _ => {}
        }
    }

    fn statement_shape(&self, stmt: &Statement<'_>) -> StatementShape {
        let span = stmt.span();
        StatementShape {
            span: SourceSpan {
                start_line: self.line_index.line(span.start),
                end_line: self.line_index.line(span.end),
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
        extract_blocks(source, Dialect::Ts).expect("parses")
    }

    #[test]
    fn function_body_becomes_one_site_with_per_statement_spans() {
        let source = r"
function handler() {
  const a = 1;
  const b = 2;
}
";
        let sites = sites(source);

        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].owner, "handler");
        assert_eq!(sites[0].statements.len(), 2);
        assert_eq!(sites[0].statements[0].span.start_line, 3);
        assert_eq!(sites[0].statements[1].span.end_line, 4);
    }

    #[test]
    fn a_statement_spanning_several_lines_keeps_its_full_span() {
        let source = r"
function handler() {
  const value = compute(
    first,
    second,
  );
}
";
        let stmt = &sites(source)[0].statements[0];

        assert_eq!(stmt.span.start_line, 3);
        assert_eq!(stmt.span.end_line, 6);
    }

    #[rstest]
    #[case::if_arm("function h(f: boolean) { if (f) { a(); b(); } }")]
    #[case::for_body("function h() { for (const x of xs) { a(); b(); } }")]
    #[case::try_block("function h() { try { a(); b(); } catch (e) {} }")]
    #[case::switch_case("function h(x: number) { switch (x) { case 1: a(); b(); } }")]
    fn nested_statement_lists_become_their_own_sites(#[case] source: &str) {
        let sites = sites(source);

        assert!(sites.len() >= 2, "expected a nested site: {sites:?}");
        assert!(sites.iter().any(|s| s.statements.len() == 2));
    }

    #[test]
    fn nested_functions_are_not_duplicated_into_the_enclosing_site() {
        let source = r"
function outer() {
  const inner = () => {
    a();
    b();
  };
}
";
        let sites = sites(source);

        // One site for `outer`'s body, one for the arrow's — and the
        // arrow's statements belong only to the arrow.
        let outer: Vec<_> = sites.iter().filter(|s| s.owner == "outer").collect();
        assert_eq!(outer.len(), 1);
        assert_eq!(outer[0].statements.len(), 1);
    }

    #[test]
    fn empty_bodies_are_skipped() {
        assert!(sites("function handler() {}").is_empty());
    }

    #[test]
    fn test_functions_are_flagged() {
        let sites = sites("function test_handler() { const a = 1; }");

        assert!(sites.iter().all(|s| s.is_test));
    }

    #[test]
    fn invalid_source_is_an_error() {
        assert!(extract_blocks("function (", Dialect::Ts).is_err());
    }
}
