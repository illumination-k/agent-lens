//! Statement-sequence windows: the unit compared by
//! `analyze similarity --target blocks`.
//!
//! Function- and type-granularity comparison can only see duplication
//! that owns a whole definition. The most common copy-paste in practice
//! lives *inside* a function — an error-mapping tail repeated at every
//! call site, a URL-assembly preamble repeated in every endpoint method —
//! and it never surfaces because the enclosing functions differ.
//!
//! Adapters supply [`StatementSeq`]s: one per statement list in the file
//! (a function body, an `if` arm, a loop body, a `match` arm), each
//! statement already positioned and lowered to the [`TreeNode`]
//! comparison currency. [`block_windows`] then slides over each list and
//! mints one [`BlockShape`] per contiguous run of statements, which the
//! similarity pipeline scores and clusters exactly like a function body.
//!
//! Windowing lives here rather than in the adapters so every language
//! produces the same unit population for the same shape of code — the
//! adapters only decide what counts as a statement in their grammar.

use std::collections::HashSet;

use crate::syntax::{BodyShape, FunctionShape, SourceSpan, SyntaxFact};
use crate::tree::TreeNode;

/// Root label of a window's comparison tree. Deliberately the same
/// `"Block"` every adapter uses for a function body root, so a window
/// covering a whole body and that body itself lower to identical trees.
const BLOCK_ROOT_LABEL: &str = "Block";

/// Smallest comparison tree a window may have, counting the `Block`
/// root.
///
/// Line count is a poor size proxy at this granularity: adapters lower
/// some constructs to a single leaf (a Rust `matches!` body is one
/// `MacroStmt` node however many lines it spans), and two such windows
/// score a perfect 1.0 against each other no matter how unrelated their
/// source is. A node floor is the standard clone-detection guard for
/// exactly this, and it costs nothing real — a window of three lines of
/// ordinary statements is well past it.
pub const MIN_WINDOW_TREE_NODES: usize = 8;

/// Largest statement run a single window covers.
///
/// Windows are enumerated per start index, so the unit count is bounded
/// by `statements × max_statements`; the cap is what keeps that linear
/// instead of quadratic. Eight statements is well past the size of the
/// boilerplate this target exists to find, and a longer repeated run
/// still surfaces — as a chain of overlapping windows that the report's
/// containment pruning collapses back into one finding.
pub const DEFAULT_MAX_WINDOW_STATEMENTS: usize = 8;

/// One statement of a function body, positioned and lowered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementUnit {
    /// 1-based inclusive first line of the statement.
    pub start_line: usize,
    /// 1-based inclusive last line of the statement.
    pub end_line: usize,
    /// The statement's subtree, in the same label vocabulary the
    /// adapter uses for function bodies.
    pub tree: TreeNode,
}

/// A contiguous statement list, tagged with the function it sits in.
///
/// One per block in the file, at every nesting depth: the function body
/// itself, then each nested `if` / loop / `match`-arm body. Windows never
/// straddle two lists, so a run can only pair statements that really are
/// consecutive siblings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatementSeq {
    /// Display name of the enclosing function, used to report where an
    /// occurrence lives. Nested lists carry their function's name, not a
    /// synthetic block name.
    pub function_name: String,
    pub is_test: bool,
    pub statements: Vec<StatementUnit>,
}

/// Windowing knobs for [`block_windows`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockWindowOptions {
    /// Drop windows spanning fewer source lines than this. The same cut
    /// `--min-lines` applies to functions and types.
    pub min_lines: usize,
    /// Longest statement run a window covers; see
    /// [`DEFAULT_MAX_WINDOW_STATEMENTS`].
    pub max_statements: usize,
    /// Drop windows whose comparison tree has fewer nodes than this; see
    /// [`MIN_WINDOW_TREE_NODES`].
    pub min_nodes: usize,
}

impl Default for BlockWindowOptions {
    fn default() -> Self {
        Self {
            min_lines: 1,
            max_statements: DEFAULT_MAX_WINDOW_STATEMENTS,
            min_nodes: MIN_WINDOW_TREE_NODES,
        }
    }
}

/// A window over consecutive statements — the `--target blocks` unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockShape {
    /// Enclosing function, for reporting.
    pub function_name: String,
    pub is_test: bool,
    /// How many statements the window covers.
    pub statement_count: usize,
    pub span: SourceSpan,
    tree: TreeNode,
}

impl BlockShape {
    pub fn tree(&self) -> &TreeNode {
        &self.tree
    }

    /// Lower into the [`FunctionShape`] corpus currency the similarity
    /// pipeline runs on. The window tree becomes the body; there is no
    /// signature, and the caller is expected to score blocks on the body
    /// alone rather than treat the missing signature as a perfect match.
    pub fn into_function_shape(self) -> FunctionShape {
        FunctionShape {
            display_name: self.function_name,
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

/// Slide over every statement list in `seqs` and mint one window per
/// contiguous run of up to `opts.max_statements` statements that spans at
/// least `opts.min_lines` source lines.
///
/// Call this once per file: windows are de-duplicated by source span, so
/// the single-statement list of a nested block cannot report the same
/// span twice (once as the outer list's statement, once as the inner
/// list's whole content). Outer lists win, which keeps the window whose
/// tree carries the full nested statement.
pub fn block_windows(seqs: &[StatementSeq], opts: BlockWindowOptions) -> Vec<BlockShape> {
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut out = Vec::new();
    for seq in seqs {
        for start in 0..seq.statements.len() {
            let limit = seq
                .statements
                .len()
                .min(start.saturating_add(opts.max_statements));
            for end in start..limit {
                let Some(window) = window_shape(seq, start, end, opts) else {
                    continue;
                };
                if seen.insert((window.span.start_line, window.span.end_line)) {
                    out.push(window);
                }
            }
        }
    }
    out
}

/// Build the window covering `seq.statements[start..=end]`, or `None`
/// when it is too short in source lines or too small in tree nodes.
fn window_shape(
    seq: &StatementSeq,
    start: usize,
    end: usize,
    opts: BlockWindowOptions,
) -> Option<BlockShape> {
    let first = seq.statements.get(start)?;
    let last = seq.statements.get(end)?;
    let span = SourceSpan {
        start_line: first.start_line,
        end_line: last.end_line,
    };
    if span.line_count() < opts.min_lines {
        return None;
    }
    let children: Vec<TreeNode> = seq.statements[start..=end]
        .iter()
        .map(|stmt| stmt.tree.clone())
        .collect();
    let tree = TreeNode::with_children(BLOCK_ROOT_LABEL, "", children);
    if tree.subtree_size() < opts.min_nodes {
        return None;
    }
    Some(BlockShape {
        function_name: seq.function_name.clone(),
        is_test: seq.is_test,
        statement_count: end - start + 1,
        span,
        tree,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    fn stmt(start_line: usize, end_line: usize, label: &str) -> StatementUnit {
        StatementUnit {
            start_line,
            end_line,
            tree: TreeNode::leaf(label),
        }
    }

    fn seq(statements: Vec<StatementUnit>) -> StatementSeq {
        StatementSeq {
            function_name: "handler".to_owned(),
            is_test: false,
            statements,
        }
    }

    fn spans(windows: &[BlockShape]) -> Vec<(usize, usize)> {
        windows
            .iter()
            .map(|w| (w.span.start_line, w.span.end_line))
            .collect()
    }

    #[test]
    fn windows_cover_every_contiguous_run_within_the_statement_cap() {
        let seqs = vec![seq(vec![
            stmt(1, 1, "Let"),
            stmt(2, 2, "Let"),
            stmt(3, 3, "Return"),
        ])];
        let windows = block_windows(
            &seqs,
            BlockWindowOptions {
                min_lines: 1,
                max_statements: 2,
                min_nodes: 0,
            },
        );

        assert_eq!(
            spans(&windows),
            vec![(1, 1), (1, 2), (2, 2), (2, 3), (3, 3)]
        );
    }

    #[test]
    fn min_lines_drops_windows_that_span_too_few_lines() {
        let seqs = vec![seq(vec![
            stmt(1, 1, "Let"),
            stmt(2, 2, "Let"),
            stmt(3, 5, "If"),
        ])];
        let windows = block_windows(
            &seqs,
            BlockWindowOptions {
                min_lines: 3,
                max_statements: 8,
                min_nodes: 0,
            },
        );

        // `[1..=2]` spans two lines and is cut; every surviving window
        // has to reach into the three-line `If` to clear the floor.
        assert_eq!(spans(&windows), vec![(1, 5), (2, 5), (3, 5)]);
    }

    #[test]
    fn identical_spans_are_reported_once_with_the_outer_lists_tree() {
        // An `if` whose body is one statement: the outer list holds the
        // whole `If` (lines 1-4), and its nested list holds the single
        // inner statement. Distinct spans, so both survive; the nested
        // list of the `If`'s sole child spans exactly the same lines as
        // that child does in the outer list and must not double-count.
        let outer = StatementSeq {
            function_name: "handler".to_owned(),
            is_test: false,
            statements: vec![StatementUnit {
                start_line: 1,
                end_line: 4,
                tree: TreeNode::with_children("If", "", vec![TreeNode::leaf("Call")]),
            }],
        };
        let inner = seq(vec![stmt(1, 4, "Call")]);
        let windows = block_windows(
            &[outer, inner],
            BlockWindowOptions {
                min_lines: 1,
                max_statements: 8,
                min_nodes: 0,
            },
        );

        assert_eq!(spans(&windows), vec![(1, 4)]);
        assert_eq!(windows[0].tree().children[0].label, "If");
    }

    #[rstest]
    #[case::empty(vec![], 0)]
    #[case::single(vec![stmt(1, 4, "Let")], 1)]
    fn degenerate_statement_lists_produce_no_spurious_windows(
        #[case] statements: Vec<StatementUnit>,
        #[case] expected: usize,
    ) {
        let windows = block_windows(
            &[seq(statements)],
            BlockWindowOptions {
                min_nodes: 0,
                ..BlockWindowOptions::default()
            },
        );
        assert_eq!(windows.len(), expected);
    }

    /// A window can span plenty of lines and still lower to almost
    /// nothing — a Rust `matches!` body is one leaf however long it is.
    /// Two such windows score 1.0 against each other, so the node floor
    /// has to cut them before they reach the corpus.
    #[test]
    fn node_floor_drops_windows_whose_tree_is_too_small_to_score() {
        let leafy = seq(vec![stmt(1, 40, "MacroStmt(matches)")]);
        let real = seq(vec![StatementUnit {
            start_line: 1,
            end_line: 3,
            tree: TreeNode::with_children(
                "Let",
                "",
                vec![
                    TreeNode::leaf("Pat"),
                    TreeNode::with_children(
                        "Call",
                        "",
                        vec![
                            TreeNode::leaf("Path"),
                            TreeNode::leaf("Arg"),
                            TreeNode::leaf("Arg"),
                            TreeNode::leaf("Arg"),
                        ],
                    ),
                ],
            ),
        }]);
        let opts = BlockWindowOptions {
            min_lines: 1,
            max_statements: 8,
            min_nodes: MIN_WINDOW_TREE_NODES,
        };

        assert!(block_windows(&[leafy], opts).is_empty());
        assert_eq!(block_windows(&[real], opts).len(), 1);
    }

    #[test]
    fn window_tree_nests_the_run_under_one_block_root() {
        let seqs = vec![seq(vec![stmt(1, 1, "Let"), stmt(2, 2, "Return")])];
        let windows = block_windows(
            &seqs,
            BlockWindowOptions {
                min_lines: 2,
                max_statements: 8,
                min_nodes: 0,
            },
        );

        assert_eq!(windows.len(), 1);
        let tree = windows[0].tree();
        assert_eq!(tree.label, BLOCK_ROOT_LABEL);
        assert_eq!(
            tree.children
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            vec!["Let", "Return"],
        );
        assert_eq!(windows[0].statement_count, 2);
    }

    #[test]
    fn into_function_shape_carries_span_name_and_no_signature() {
        let seqs = vec![StatementSeq {
            function_name: "fetch".to_owned(),
            is_test: true,
            statements: vec![stmt(7, 9, "Let")],
        }];
        let windows = block_windows(
            &seqs,
            BlockWindowOptions {
                min_nodes: 0,
                ..BlockWindowOptions::default()
            },
        );
        let shape = windows
            .into_iter()
            .next()
            .expect("one window")
            .into_function_shape();

        assert_eq!(shape.display_name, "fetch");
        assert!(shape.is_test);
        assert_eq!(shape.span.start_line, 7);
        assert_eq!(shape.span.end_line, 9);
        assert_eq!(shape.line_count(), 3);
        assert!(shape.signature_shape().is_none());
        assert!(shape.doc.is_none());
        assert_eq!(shape.body_tree().label, BLOCK_ROOT_LABEL);
    }

    /// Statement list with monotonically non-overlapping line spans, the
    /// shape any real parser produces for siblings in one block.
    fn arb_seq() -> impl Strategy<Value = StatementSeq> {
        proptest::collection::vec((1usize..4, 0usize..3), 0..8).prop_map(|shapes| {
            let mut line = 1usize;
            let statements = shapes
                .into_iter()
                .map(|(height, gap)| {
                    let start = line + gap;
                    let end = start + height - 1;
                    line = end + 1;
                    stmt(start, end, "Stmt")
                })
                .collect();
            seq(statements)
        })
    }

    proptest! {
        /// Every reported window must be a real contiguous run: its span
        /// has to start on some statement's first line and end on some
        /// statement's last line, and cover at least `min_lines`.
        #[test]
        fn windows_align_to_statement_boundaries_and_respect_min_lines(
            s in arb_seq(),
            min_lines in 1usize..6,
            max_statements in 1usize..5,
        ) {
            let windows = block_windows(
                std::slice::from_ref(&s),
                BlockWindowOptions { min_lines, max_statements, min_nodes: 0 },
            );
            let starts: HashSet<usize> = s.statements.iter().map(|st| st.start_line).collect();
            let ends: HashSet<usize> = s.statements.iter().map(|st| st.end_line).collect();
            for w in &windows {
                prop_assert!(starts.contains(&w.span.start_line));
                prop_assert!(ends.contains(&w.span.end_line));
                prop_assert!(w.span.line_count() >= min_lines);
                prop_assert!(w.statement_count <= max_statements);
                prop_assert!(w.statement_count >= 1);
            }
        }

        /// Window count is bounded by `statements × max_statements`, the
        /// linearity the cap exists to guarantee.
        #[test]
        fn window_count_stays_linear_in_the_statement_count(
            s in arb_seq(),
            max_statements in 1usize..5,
        ) {
            let windows = block_windows(
                std::slice::from_ref(&s),
                BlockWindowOptions { min_lines: 1, max_statements, min_nodes: 0 },
            );
            prop_assert!(windows.len() <= s.statements.len() * max_statements);
        }
    }
}
