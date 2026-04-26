//! TSED — Tree Structure Edit Distance similarity.
//!
//! Normalises the raw edit distance produced by [`crate::apted`] into a value
//! in `[0.0, 1.0]` and, optionally, applies a size penalty so that pairs of
//! short functions don't get falsely-high scores just because they happen to
//! share a small handful of tokens.

use crate::apted::{APTEDOptions, compute_edit_distance};
use crate::tree::TreeNode;

/// Tuning knobs for [`calculate_tsed`].
#[derive(Debug, Clone, PartialEq)]
pub struct TSEDOptions {
    pub apted: APTEDOptions,
    /// Scale the raw similarity by `min_size / max_size`. Useful when the
    /// bigger tree genuinely extends the smaller one but we don't want a
    /// 5-node function to count as "80% similar" to a 50-node function.
    pub size_penalty: bool,
}

impl Default for TSEDOptions {
    fn default() -> Self {
        Self {
            apted: APTEDOptions {
                // similarity-ts-core's TSED runs APTED with a reduced rename
                // cost so that a renamed identifier stays cheaper than a
                // full delete + insert of the node.
                rename_cost: 0.3,
                ..APTEDOptions::default()
            },
            size_penalty: true,
        }
    }
}

/// Similarity between two trees in `[0.0, 1.0]`.
///
/// `1.0` means identical (up to the value comparison setting); `0.0` means
/// totally different (or empty on one side). The score is clamped to the
/// valid range, so callers can compare raw floats safely.
pub fn calculate_tsed(a: &TreeNode, b: &TreeNode, opts: &TSEDOptions) -> f64 {
    let size_a = a.subtree_size();
    let size_b = b.subtree_size();
    let max_size = size_a.max(size_b);
    if max_size == 0 {
        return 1.0;
    }

    let distance = compute_edit_distance(a, b, &opts.apted);
    let base = 1.0 - distance / max_size as f64;

    let similarity = if opts.size_penalty {
        let min_size = size_a.min(size_b) as f64;
        let max_size = max_size as f64;
        // A square root keeps the penalty gentle for roughly-equal trees
        // while still pulling down pairs where one side is much larger.
        base * (min_size / max_size).sqrt()
    } else {
        base
    };

    similarity.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::TreeNode;
    use rstest::rstest;

    fn leaf(label: &str) -> TreeNode {
        TreeNode::leaf(label)
    }

    fn parent(label: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode::with_children(label, "", children)
    }

    /// Identity always returns 1.0, including the empty / single-leaf edge
    /// cases that would otherwise hit the `max_size == 0` divide-by-zero
    /// guard.
    #[rstest]
    #[case::small_pair(
        parent("Root", vec![leaf("A"), leaf("B")]),
        parent("Root", vec![leaf("A"), leaf("B")]),
    )]
    #[case::empty_pair(TreeNode::leaf(""), TreeNode::leaf(""))]
    #[case::single_leaf_pair(leaf("X"), leaf("X"))]
    fn identical_trees_score_one(#[case] a: TreeNode, #[case] b: TreeNode) {
        let sim = calculate_tsed(&a, &b, &TSEDOptions::default());
        assert!((sim - 1.0).abs() < 1e-9, "got {sim}");
    }

    /// The score must stay clamped to `[0.0, 1.0]` for any pair, including
    /// fully-disjoint same-size trees and asymmetric tiny-vs-medium pairs.
    #[rstest]
    #[case::same_size_disjoint(
        parent("A", vec![leaf("x"); 10]),
        parent("B", vec![leaf("y"); 10]),
    )]
    #[case::asymmetric_disjoint(
        parent("Root", vec![leaf("A")]),
        parent("Other", vec![leaf("B"), leaf("C"), leaf("D")]),
    )]
    fn similarity_stays_in_unit_interval(#[case] a: TreeNode, #[case] b: TreeNode) {
        let sim = calculate_tsed(&a, &b, &TSEDOptions::default());
        assert!((0.0..=1.0).contains(&sim), "got {sim}");
    }

    #[test]
    fn completely_disjoint_small_trees_penalised() {
        let a = parent("Root", vec![leaf("A")]);
        let b = parent("Other", vec![leaf("B"), leaf("C"), leaf("D")]);
        let sim = calculate_tsed(&a, &b, &TSEDOptions::default());
        assert!(sim < 0.5);
    }

    #[test]
    fn size_penalty_reduces_score() {
        let small = parent("Root", vec![leaf("A"), leaf("B")]);
        let large = parent(
            "Root",
            vec![
                leaf("A"),
                leaf("B"),
                parent("Extra", vec![leaf("C"), leaf("D"), leaf("E")]),
            ],
        );

        let with_penalty = calculate_tsed(
            &small,
            &large,
            &TSEDOptions {
                size_penalty: true,
                ..Default::default()
            },
        );
        let without_penalty = calculate_tsed(
            &small,
            &large,
            &TSEDOptions {
                size_penalty: false,
                ..Default::default()
            },
        );
        assert!(
            with_penalty < without_penalty,
            "with={with_penalty}, without={without_penalty}"
        );
    }

    #[test]
    fn score_follows_one_minus_distance_over_max_size() {
        // Concrete arithmetic check pinned to the formula: identical roots
        // with one renamed leaf out of five total nodes yields
        // 1 - rename_cost / 5 once the size penalty is disabled.
        let a = parent("Root", vec![leaf("A"), leaf("B"), leaf("C"), leaf("D")]);
        let b = parent("Root", vec![leaf("A"), leaf("B"), leaf("C"), leaf("Z")]);
        let opts = TSEDOptions {
            apted: APTEDOptions {
                rename_cost: 1.0,
                ..APTEDOptions::default()
            },
            size_penalty: false,
        };
        let sim = calculate_tsed(&a, &b, &opts);
        assert!((sim - 0.8).abs() < 1e-9, "got {sim}");
    }
}
