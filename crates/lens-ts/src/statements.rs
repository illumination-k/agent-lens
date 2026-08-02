//! Statement lists inside TypeScript / JavaScript function bodies, for
//! `similarity --target blocks`.
//!
//! One [`StatementSeq`] per statement list reachable from a function
//! body without crossing into another function: the body itself, then
//! nested `{}` blocks, loop and `if` bodies, `switch` case consequents,
//! and `try` / `catch` / `finally` blocks. Nested functions are *not*
//! descended into here — [`crate::walk`] already emits each one as its
//! own `<parent>::closure#N` unit, so recursing would attribute their
//! statements to the wrong function and report every window twice.
//!
//! Statements are lowered with the same `stmt_tree` the function-body
//! tree uses, so a window covering a whole body and that body compare as
//! identical trees.

use lens_domain::{LineIndex, StatementSeq, StatementUnit};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::GetSpan;

use crate::Dialect;
use crate::parser::TsParseError;
use crate::tree::stmt_tree;
use crate::walk::{FunctionItem, FunctionVisitor, walk_program};

/// Collect every statement list in `source`, tagged with the function it
/// belongs to. Lists are emitted outermost-first within a function so a
/// caller de-duplicating by span keeps the enclosing statement's tree.
pub fn extract_statement_seqs(
    source: &str,
    dialect: Dialect,
) -> Result<Vec<StatementSeq>, TsParseError> {
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
    let mut collector = SeqCollector {
        line_index: &line_index,
        out: Vec::new(),
    };
    walk_program(&ret.program, &line_index, &mut collector);
    Ok(collector.out)
}

struct SeqCollector<'a> {
    line_index: &'a LineIndex,
    out: Vec<StatementSeq>,
}

impl FunctionVisitor for SeqCollector<'_> {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        let is_test = crate::parser::is_test_item(&item.name);
        self.collect_list(&item.name, is_test, &item.body.statements);
    }
}

impl SeqCollector<'_> {
    /// Record `statements` as one sequence, then descend into the
    /// statement lists nested inside them.
    fn collect_list(&mut self, function_name: &str, is_test: bool, statements: &[Statement<'_>]) {
        if statements.is_empty() {
            return;
        }
        self.out.push(StatementSeq {
            function_name: function_name.to_owned(),
            is_test,
            statements: statements
                .iter()
                .map(|stmt| self.statement_unit(stmt))
                .collect(),
        });
        for stmt in statements {
            self.descend(function_name, is_test, stmt);
        }
    }

    /// Recurse into whatever statement lists `stmt` encloses. Function
    /// and class declarations terminate the walk: their bodies belong to
    /// a different unit.
    fn descend(&mut self, function_name: &str, is_test: bool, stmt: &Statement<'_>) {
        match stmt {
            Statement::BlockStatement(b) => self.collect_list(function_name, is_test, &b.body),
            Statement::IfStatement(it) => {
                self.descend(function_name, is_test, &it.consequent);
                if let Some(alternate) = &it.alternate {
                    self.descend(function_name, is_test, alternate);
                }
            }
            Statement::WhileStatement(w) => self.descend(function_name, is_test, &w.body),
            Statement::DoWhileStatement(w) => self.descend(function_name, is_test, &w.body),
            Statement::ForStatement(f) => self.descend(function_name, is_test, &f.body),
            Statement::ForInStatement(f) => self.descend(function_name, is_test, &f.body),
            Statement::ForOfStatement(f) => self.descend(function_name, is_test, &f.body),
            Statement::LabeledStatement(l) => self.descend(function_name, is_test, &l.body),
            Statement::WithStatement(w) => self.descend(function_name, is_test, &w.body),
            Statement::SwitchStatement(s) => {
                for case in &s.cases {
                    self.collect_list(function_name, is_test, &case.consequent);
                }
            }
            Statement::TryStatement(t) => {
                self.collect_list(function_name, is_test, &t.block.body);
                if let Some(handler) = &t.handler {
                    self.collect_list(function_name, is_test, &handler.body.body);
                }
                if let Some(finalizer) = &t.finalizer {
                    self.collect_list(function_name, is_test, &finalizer.body);
                }
            }
            _ => {}
        }
    }

    fn statement_unit(&self, stmt: &Statement<'_>) -> StatementUnit {
        let span = stmt.span();
        StatementUnit {
            start_line: self.line_index.line(span.start),
            end_line: self.line_index.line(span.end),
            tree: stmt_tree(stmt),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn seqs(source: &str) -> Vec<StatementSeq> {
        extract_statement_seqs(source, Dialect::Ts).expect("parses")
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
function handler(x: number): number {
  const a = x + 1;
  if (a > 0) {
    const b = a * 2;
    return b;
  }
  return a;
}
"#;
        assert_eq!(
            shape(&seqs(src)),
            vec![
                ("handler", vec![(3, 3), (4, 7), (8, 8)]),
                ("handler", vec![(5, 5), (6, 6)]),
            ],
        );
    }

    /// One case per statement form that can enclose a statement list.
    /// The body is always the same two statements on lines 3 and 4 (or
    /// 4 and 5 where the head needs an extra line), so a missing
    /// `descend` arm shows up as the nested list never being collected.
    #[rstest]
    #[case::block("function f() {\n  {\n    a();\n    b();\n  }\n}\n", vec![(3, 3), (4, 4)])]
    #[case::if_consequent(
        "function f(x: number) {\n  if (x) {\n    a();\n    b();\n  }\n}\n",
        vec![(3, 3), (4, 4)]
    )]
    #[case::else_branch(
        "function f(x: number) {\n  if (x) {\n    c();\n  } else {\n    a();\n    b();\n  }\n}\n",
        vec![(5, 5), (6, 6)]
    )]
    #[case::while_body("function f(x: number) {\n  while (x) {\n    a();\n    b();\n  }\n}\n", vec![(3, 3), (4, 4)])]
    #[case::do_while_body(
        "function f(x: number) {\n  do {\n    a();\n    b();\n  } while (x);\n}\n",
        vec![(3, 3), (4, 4)]
    )]
    #[case::for_body(
        "function f() {\n  for (let i = 0; i < 3; i++) {\n    a();\n    b();\n  }\n}\n",
        vec![(3, 3), (4, 4)]
    )]
    #[case::for_in_body(
        "function f(o: object) {\n  for (const k in o) {\n    a();\n    b();\n  }\n}\n",
        vec![(3, 3), (4, 4)]
    )]
    #[case::for_of_body(
        "function f(xs: number[]) {\n  for (const x of xs) {\n    a();\n    b();\n  }\n}\n",
        vec![(3, 3), (4, 4)]
    )]
    #[case::labeled_body(
        "function f() {\n  outer: {\n    a();\n    b();\n  }\n}\n",
        vec![(3, 3), (4, 4)]
    )]
    #[case::switch_case(
        "function f(x: number) {\n  switch (x) {\n    case 1:\n      a();\n      b();\n  }\n}\n",
        vec![(4, 4), (5, 5)]
    )]
    #[case::try_block(
        "function f() {\n  try {\n    a();\n    b();\n  } catch (e) {}\n}\n",
        vec![(3, 3), (4, 4)]
    )]
    #[case::catch_block(
        "function f() {\n  try {\n    go();\n  } catch (e) {\n    a();\n    b();\n  }\n}\n",
        vec![(5, 5), (6, 6)]
    )]
    #[case::finally_block(
        "function f() {\n  try {\n    go();\n  } finally {\n    a();\n    b();\n  }\n}\n",
        vec![(5, 5), (6, 6)]
    )]
    fn nested_statement_lists_are_reached(
        #[case] src: &str,
        #[case] expected: Vec<(usize, usize)>,
    ) {
        let collected = seqs(src);
        // The body plus at least the nested list under test; forms with
        // two branches (if/else, try/catch) contribute a third.
        assert!(collected.len() >= 2, "nested list was not collected");
        let last = collected.last().expect("at least one sequence");
        assert_eq!(
            last.statements
                .iter()
                .map(|st| (st.start_line, st.end_line))
                .collect::<Vec<_>>(),
            expected,
        );
    }

    /// `with` is a sloppy-mode-only form, so it needs a script dialect
    /// rather than the module default the other cases use.
    #[test]
    fn with_statement_body_is_reached() {
        let collected = extract_statement_seqs(
            "function f(o) {\n  with (o) {\n    a();\n    b();\n  }\n}\n",
            Dialect::Cjs,
        )
        .expect("parses");

        assert_eq!(collected.len(), 2);
        assert_eq!(
            collected[1]
                .statements
                .iter()
                .map(|st| (st.start_line, st.end_line))
                .collect::<Vec<_>>(),
            vec![(3, 3), (4, 4)],
        );
    }

    /// A nested arrow function is its own `closure#N` unit; its
    /// statements must be attributed there, not to the parent, and must
    /// not be reported twice.
    #[test]
    fn nested_functions_are_attributed_to_their_own_unit() {
        let src = r#"
function outer() {
  const inner = () => {
    const a = 1;
    return a;
  };
  return inner;
}
"#;
        let collected = seqs(src);
        let names: Vec<&str> = collected.iter().map(|s| s.function_name.as_str()).collect();
        assert_eq!(names, vec!["outer", "outer::closure#1"]);
        assert_eq!(
            shape(&collected)[1].1,
            vec![(4, 4), (5, 5)],
            "closure body statements belong to the closure unit",
        );
    }

    /// The whole point of reusing `stmt_tree` is that a window covering
    /// a full body lowers to the same tree that body does.
    #[test]
    fn statement_trees_match_the_function_body_lowering() {
        let src = "function f() {\n  const a = 1;\n  return a;\n}\n";
        let collected = seqs(src);
        let body = lens_domain::LanguageParser::extract_functions(
            &mut crate::TypeScriptParser::with_dialect(Dialect::Ts),
            src,
        )
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

    #[test]
    fn test_functions_are_flagged_so_callers_can_filter_them() {
        let collected = seqs("function test_thing() {\n  const a = 1;\n}\n");
        assert!(collected.iter().all(|s| s.is_test));
    }

    #[test]
    fn empty_bodies_produce_no_sequence() {
        assert!(seqs("function f() {}\n").is_empty());
    }

    #[test]
    fn parse_error_is_reported() {
        assert!(extract_statement_seqs("function (", Dialect::Ts).is_err());
    }
}
