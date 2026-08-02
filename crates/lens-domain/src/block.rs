//! Sub-function comparison units: contiguous runs of statements.
//!
//! Function-granularity similarity cannot see duplication that lives
//! *inside* larger functions — the copy-pasted error-mapping closure that
//! appears fifty times across a dozen handlers never surfaces, because the
//! handlers themselves differ. This module lowers a language adapter's
//! statement lists into sliding windows that the ordinary similarity
//! pipeline can pair, score, and cluster exactly like function bodies.
//!
//! The split of labour is deliberate: adapters only have to answer "which
//! statement sequences does this file contain, and where does each
//! statement start and end", which every parser already knows. All the
//! windowing policy — how long a window may get, how short a window may be
//! before it is noise — lives here so the four languages cannot drift.

use crate::syntax::{BodyShape, FunctionShape, SourceSpan, SyntaxFact};
use crate::tree::TreeNode;

/// One statement inside a [`BlockSite`], with the source lines it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementShape {
    pub span: SourceSpan,
    pub tree: TreeNode,
}

/// One syntactic statement sequence — a function body, or any nested
/// block inside one (an `if` arm, a loop body, a `try` block).
///
/// Adapters emit one site per sequence, including nested ones: a run of
/// statements that repeats inside two different `for` loops is only
/// findable if the loop bodies are their own sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSite {
    /// Enclosing function name, used to report where an occurrence lives.
    pub owner: String,
    pub is_test: bool,
    pub statements: Vec<StatementShape>,
}

/// A contiguous run of statements cut out of a [`BlockSite`], ready to be
/// compared against every other run in the corpus.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockShape {
    pub owner: String,
    pub span: SourceSpan,
    pub statement_count: usize,
    pub is_test: bool,
    /// Synthetic `Block` node holding the window's statement subtrees, so
    /// a window scores against another window the same way one function
    /// body scores against another.
    pub tree: TreeNode,
}

impl BlockShape {
    pub fn line_count(&self) -> usize {
        self.span.line_count()
    }

    /// Lower into the neutral [`FunctionShape`] the similarity pipeline
    /// consumes. A statement run has no signature and no doc comment, so
    /// both are absent; scoring falls back to body-only comparison.
    pub fn into_function_shape(self) -> FunctionShape {
        FunctionShape {
            display_name: self.owner,
            qualified_name: SyntaxFact::Unknown,
            module_path: SyntaxFact::Unknown,
            owner: SyntaxFact::Known(None),
            visibility: SyntaxFact::Unknown,
            signature: SyntaxFact::Unknown,
            doc: None,
            body: BodyShape { tree: self.tree },
            span: self.span,
            is_test: self.is_test,
        }
    }
}

/// Windowing policy for [`block_windows`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockWindowOptions {
    /// Windows spanning fewer source lines than this are skipped. A
    /// window that is too short still gets extended: the cut drops the
    /// window, not the start position.
    pub min_lines: usize,
    /// Longest window, in statements. Bounds the per-site window count at
    /// `statements * max_statements` instead of the quadratic
    /// all-substrings enumeration, which is what keeps a whole-repository
    /// block run tractable.
    pub max_statements: usize,
    /// Smallest window tree, in nodes, counting the synthetic `Block`
    /// root.
    ///
    /// Line count alone is a poor proxy for how much structure a window
    /// carries: an adapter that lowers an opaque construct to a single
    /// leaf — a Rust `println!` / `writeln!` statement is one
    /// `MacroStmt` node no matter how many lines it spans — produces
    /// two-node windows that score 1.0 against every other window of the
    /// same shape. Those matches are real ("both are a macro call") and
    /// useless. The floor drops them.
    pub min_nodes: usize,
}

impl Default for BlockWindowOptions {
    fn default() -> Self {
        Self {
            min_lines: 4,
            max_statements: 6,
            min_nodes: 8,
        }
    }
}

/// Slide every window allowed by `opts` over each site's statement list.
///
/// Windows overlap by construction — the runs starting at statement 0 and
/// at statement 1 share all but one statement — so callers are expected to
/// drop pairs whose source spans overlap, and to collapse clusters that one
/// larger cluster already subsumes.
pub fn block_windows(sites: &[BlockSite], opts: &BlockWindowOptions) -> Vec<BlockShape> {
    let mut out = Vec::new();
    for site in sites {
        push_site_windows(site, opts, &mut out);
    }
    out
}

fn push_site_windows(site: &BlockSite, opts: &BlockWindowOptions, out: &mut Vec<BlockShape>) {
    if opts.max_statements == 0 {
        return;
    }
    let statements = &site.statements;
    for start in 0..statements.len() {
        let last = (start + opts.max_statements).min(statements.len());
        for end in start..last {
            let (Some(first), Some(final_stmt)) = (statements.get(start), statements.get(end))
            else {
                continue;
            };
            let span = SourceSpan {
                start_line: first.span.start_line,
                end_line: final_stmt.span.end_line,
            };
            if span.line_count() < opts.min_lines {
                continue;
            }
            let children: Vec<TreeNode> = statements
                .get(start..=end)
                .unwrap_or_default()
                .iter()
                .map(|stmt| stmt.tree.clone())
                .collect();
            let tree = TreeNode::with_children("Block", "", children);
            if tree.subtree_size() < opts.min_nodes {
                continue;
            }
            out.push(BlockShape {
                owner: site.owner.clone(),
                span,
                statement_count: end - start + 1,
                is_test: site.is_test,
                tree,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// Statement whose tree carries enough nodes that a one-statement
    /// window clears the default [`BlockWindowOptions::min_nodes`]; cases
    /// that care about the node floor build their own statement.
    fn stmt(start_line: usize, end_line: usize, label: &str) -> StatementShape {
        StatementShape {
            span: SourceSpan {
                start_line,
                end_line,
            },
            tree: TreeNode::with_children(
                label,
                "",
                (0..8).map(|i| TreeNode::leaf(format!("L{i}"))).collect(),
            ),
        }
    }

    fn site(statements: Vec<StatementShape>) -> BlockSite {
        BlockSite {
            owner: "handler".to_owned(),
            is_test: false,
            statements,
        }
    }

    #[test]
    fn single_statement_window_survives_when_it_spans_enough_lines() {
        let sites = vec![site(vec![stmt(10, 14, "ExprStmt")])];
        let windows = block_windows(&sites, &BlockWindowOptions::default());

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].statement_count, 1);
        assert_eq!(windows[0].span.start_line, 10);
        assert_eq!(windows[0].span.end_line, 14);
        assert_eq!(windows[0].owner, "handler");
    }

    #[test]
    fn short_statements_only_surface_once_the_window_grows() {
        let sites = vec![site(vec![
            stmt(1, 1, "A"),
            stmt(2, 2, "B"),
            stmt(3, 3, "C"),
        ])];
        let opts = BlockWindowOptions {
            min_lines: 3,
            max_statements: 6,
            min_nodes: 1,
        };
        let windows = block_windows(&sites, &opts);

        // Only [A,B,C] spans 3 lines; the 1- and 2-statement runs are
        // dropped without also dropping the start positions they share.
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].statement_count, 3);
        assert_eq!(windows[0].tree.children.len(), 3);
    }

    #[rstest]
    #[case::one(1)]
    #[case::two(2)]
    #[case::three(3)]
    fn window_count_is_bounded_by_max_statements(#[case] max_statements: usize) {
        let statements: Vec<_> = (0..8).map(|i| stmt(i * 2 + 1, i * 2 + 2, "S")).collect();
        let sites = vec![site(statements)];
        let opts = BlockWindowOptions {
            min_lines: 1,
            max_statements,
            min_nodes: 1,
        };

        let windows = block_windows(&sites, &opts);

        assert!(windows.len() <= 8 * max_statements);
        assert!(
            windows
                .iter()
                .all(|w| w.statement_count <= max_statements && w.statement_count >= 1)
        );
    }

    #[test]
    fn windows_whose_tree_is_too_small_are_dropped() {
        let sites = vec![BlockSite {
            owner: "handler".to_owned(),
            is_test: false,
            statements: vec![StatementShape {
                span: SourceSpan {
                    start_line: 1,
                    end_line: 20,
                },
                // One opaque leaf, the shape a macro statement lowers to.
                tree: TreeNode::leaf("MacroStmt(writeln)"),
            }],
        }];

        assert!(block_windows(&sites, &BlockWindowOptions::default()).is_empty());
    }

    #[test]
    fn zero_max_statements_yields_nothing() {
        let sites = vec![site(vec![stmt(1, 9, "A")])];
        let opts = BlockWindowOptions {
            min_lines: 1,
            max_statements: 0,
            min_nodes: 1,
        };

        assert!(block_windows(&sites, &opts).is_empty());
    }

    #[test]
    fn windows_carry_the_site_test_flag() {
        let sites = vec![BlockSite {
            owner: "test_thing".to_owned(),
            is_test: true,
            statements: vec![stmt(1, 6, "A")],
        }];

        let windows = block_windows(&sites, &BlockWindowOptions::default());

        assert!(windows.iter().all(|w| w.is_test));
    }

    #[test]
    fn into_function_shape_keeps_span_and_drops_signature() {
        let sites = vec![site(vec![stmt(4, 9, "ExprStmt")])];
        let block = block_windows(&sites, &BlockWindowOptions::default())
            .pop()
            .expect("one window");
        let expected_tree = block.tree.clone();

        let shape = block.into_function_shape();

        assert_eq!(shape.display_name, "handler");
        assert_eq!(shape.body_tree(), &expected_tree);
        assert!(shape.signature_shape().is_none());
        assert_eq!(shape.doc, None);
        assert_eq!(shape.span.start_line, 4);
        assert_eq!(shape.span.end_line, 9);
    }

    use proptest::collection::vec as prop_vec;
    use proptest::prelude::*;

    /// Statement list with monotonically non-decreasing, non-overlapping
    /// spans — the shape any real parser produces.
    fn arb_statements() -> impl Strategy<Value = Vec<StatementShape>> {
        prop_vec((1usize..4, 0usize..3), 0..12).prop_map(|gaps| {
            let mut line = 1usize;
            gaps.into_iter()
                .map(|(len, gap)| {
                    let start = line;
                    let end = start + len - 1;
                    line = end + 1 + gap;
                    stmt(start, end, "S")
                })
                .collect()
        })
    }

    proptest! {
        #[test]
        fn every_window_meets_the_line_floor_and_statement_cap(
            statements in arb_statements(),
            min_lines in 1usize..6,
            max_statements in 1usize..5,
        ) {
            let sites = vec![site(statements)];
            let opts = BlockWindowOptions { min_lines, max_statements, min_nodes: 1 };

            for window in block_windows(&sites, &opts) {
                prop_assert!(window.span.line_count() >= min_lines);
                prop_assert!(window.statement_count <= max_statements);
                prop_assert_eq!(window.tree.children.len(), window.statement_count);
                prop_assert!(window.span.start_line <= window.span.end_line);
            }
        }
    }
}
