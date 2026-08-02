//! Statement-sequence extraction for sub-function similarity.
//!
//! Every `syn::Block` reachable from a function body becomes one
//! [`BlockSite`] — the body itself, plus each nested `if` arm, loop body,
//! `match` arm block, and closure body. Windowing over those statements is
//! `lens_domain`'s job; this module only reports "which statements, at
//! which lines, inside which function".

use lens_domain::{BlockSite, SourceSpan, StatementShape};
use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::common::{WalkOptions, walk_fn_items};
use crate::parser::RustParseError;
use crate::parser::stmt_tree;

/// Collect every statement sequence inside every function in `source`.
pub fn extract_blocks(source: &str) -> Result<Vec<BlockSite>, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut out = Vec::new();
    walk_fn_items(&file.items, WalkOptions::default(), &mut |site| {
        let owner = lens_domain::qualify(site.owner, &site.sig.ident.to_string());
        let mut collector = BlockCollector {
            owner,
            is_test: site.is_test,
            out: &mut out,
        };
        collector.visit_block(site.block);
    });
    Ok(out)
}

struct BlockCollector<'a> {
    owner: String,
    is_test: bool,
    out: &'a mut Vec<BlockSite>,
}

impl<'ast> Visit<'ast> for BlockCollector<'_> {
    fn visit_block(&mut self, block: &'ast syn::Block) {
        if !block.stmts.is_empty() {
            self.out.push(BlockSite {
                owner: self.owner.clone(),
                is_test: self.is_test,
                statements: block.stmts.iter().map(statement_shape).collect(),
            });
        }
        // Keep descending: a nested `if` arm or loop body is its own site,
        // and the repeated run may live only there.
        syn::visit::visit_block(self, block);
    }
}

fn statement_shape(stmt: &syn::Stmt) -> StatementShape {
    let span = stmt.span();
    StatementShape {
        span: SourceSpan {
            start_line: span.start().line,
            end_line: span.end().line,
        },
        tree: stmt_tree(stmt),
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
        let source = r"
fn handler() {
    let a = 1;
    let b = 2;
}
";
        let sites = sites(source);

        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].owner, "handler");
        assert_eq!(sites[0].statements.len(), 2);
        assert_eq!(sites[0].statements[0].span.start_line, 3);
        assert_eq!(sites[0].statements[1].span.start_line, 4);
    }

    #[test]
    fn a_statement_spanning_several_lines_keeps_its_full_span() {
        let source = r"
fn handler() {
    let value = compute(
        first,
        second,
    );
}
";
        let sites = sites(source);
        let stmt = &sites[0].statements[0];

        assert_eq!(stmt.span.start_line, 3);
        assert_eq!(stmt.span.end_line, 6);
    }

    #[test]
    fn nested_blocks_are_their_own_sites() {
        let source = r"
fn handler(flag: bool) {
    if flag {
        step_one();
        step_two();
    }
}
";
        let sites = sites(source);

        // The body (one `if` statement) plus the `if` arm.
        assert_eq!(sites.len(), 2);
        assert!(sites.iter().all(|s| s.owner == "handler"));
        assert!(sites.iter().any(|s| s.statements.len() == 2));
    }

    #[test]
    fn empty_blocks_are_skipped() {
        assert!(sites("fn handler() {}").is_empty());
    }

    #[rstest]
    #[case::free_fn("fn handler() { let a = 1; }", "handler")]
    #[case::method("struct S; impl S { fn go(&self) { let a = 1; } }", "S::go")]
    fn owner_is_the_qualified_function_name(#[case] source: &str, #[case] expected: &str) {
        let sites = sites(source);

        assert_eq!(sites.len(), 1);
        assert_eq!(sites[0].owner, expected);
    }

    #[test]
    fn test_functions_are_flagged() {
        let source = r"
#[test]
fn checks_something() {
    let a = 1;
}
";
        let sites = sites(source);

        assert!(sites.iter().all(|s| s.is_test));
    }

    #[test]
    fn invalid_source_is_an_error() {
        assert!(extract_blocks("fn (").is_err());
    }
}
