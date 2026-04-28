//! TSED — Tree Structure Edit Distance similarity.
//!
//! Normalises the raw edit distance produced by [`crate::apted`] into a value
//! in `[0.0, 1.0]` and, optionally, applies a size penalty so that pairs of
//! short functions don't get falsely-high scores just because they happen to
//! share a small handful of tokens.

use crate::apted::{
    APTEDOptions, SubtreeSizes, compute_edit_distance, compute_edit_distance_with_subtree_sizes,
};
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
///
/// # Examples
///
/// ```
/// use lens_domain::{TSEDOptions, TreeNode, calculate_tsed};
///
/// let a = TreeNode::with_children(
///     "Root",
///     "",
///     vec![TreeNode::leaf("A"), TreeNode::leaf("B")],
/// );
/// let b = a.clone();
/// let sim = calculate_tsed(&a, &b, &TSEDOptions::default());
/// assert!((sim - 1.0).abs() < 1e-9);
/// ```
pub fn calculate_tsed(a: &TreeNode, b: &TreeNode, opts: &TSEDOptions) -> f64 {
    let size_a = a.subtree_size();
    let size_b = b.subtree_size();
    calculate_tsed_with_distance(a, b, size_a, size_b, opts, |a, b| {
        compute_edit_distance(a, b, &opts.apted)
    })
}

/// [`calculate_tsed`] variant for callers that compare a stable corpus many
/// times and can precompute subtree sizes once per tree.
pub fn calculate_tsed_with_subtree_sizes(
    a: &TreeNode,
    b: &TreeNode,
    size_a: usize,
    size_b: usize,
    sizes_a: &SubtreeSizes,
    sizes_b: &SubtreeSizes,
    opts: &TSEDOptions,
) -> f64 {
    calculate_tsed_with_distance(a, b, size_a, size_b, opts, |a, b| {
        compute_edit_distance_with_subtree_sizes(a, b, &opts.apted, sizes_a, sizes_b)
    })
}

fn calculate_tsed_with_distance(
    a: &TreeNode,
    b: &TreeNode,
    size_a: usize,
    size_b: usize,
    opts: &TSEDOptions,
    distance: impl FnOnce(&TreeNode, &TreeNode) -> f64,
) -> f64 {
    let max_size = size_a.max(size_b);
    if max_size == 0 {
        return 1.0;
    }

    let distance = distance(a, b);
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

    /// `max_size == 0` is only reachable through the precomputed-sizes API,
    /// where the caller can legitimately pass empty subtree-size tables for
    /// trees that have been pruned to zero nodes. The contract is that two
    /// such "empty" trees compare as fully similar.
    #[test]
    fn empty_trees_score_one() {
        let empty = TreeNode::leaf("");
        let sizes = SubtreeSizes::new();
        let sim = calculate_tsed_with_subtree_sizes(
            &empty,
            &empty,
            0,
            0,
            &sizes,
            &sizes,
            &TSEDOptions::default(),
        );
        assert!((sim - 1.0).abs() < 1e-9, "got {sim}");
    }

    use proptest::collection::vec as prop_vec;
    use proptest::prelude::*;

    /// Same shape as [`crate::tree::tests::arb_tree`] — duplicated rather
    /// than shared because cross-module test fixtures aren't worth the
    /// `#[cfg(test)] pub(crate)` plumbing.
    fn arb_tree() -> impl Strategy<Value = TreeNode> {
        let leaf =
            (0u8..8, 0u8..4).prop_map(|(l, v)| TreeNode::new(format!("L{l}"), format!("V{v}")));
        leaf.prop_recursive(4, 24, 4, |inner| {
            (0u8..8, 0u8..4, prop_vec(inner, 0..4)).prop_map(|(l, v, kids)| {
                TreeNode::with_children(format!("L{l}"), format!("V{v}"), kids)
            })
        })
    }

    /// Arbitrary cost configuration. `delete_cost` and `insert_cost` are
    /// drawn independently so swap-symmetry only holds for the dedicated
    /// strategy below.
    fn arb_apted_options() -> impl Strategy<Value = APTEDOptions> {
        (0.0_f64..3.0, 0.1_f64..3.0, 0.1_f64..3.0, any::<bool>()).prop_map(
            |(rename_cost, delete_cost, insert_cost, compare_values)| APTEDOptions {
                rename_cost,
                delete_cost,
                insert_cost,
                compare_values,
            },
        )
    }

    fn arb_tsed_options() -> impl Strategy<Value = TSEDOptions> {
        (arb_apted_options(), any::<bool>()).prop_map(|(apted, size_penalty)| TSEDOptions {
            apted,
            size_penalty,
        })
    }

    /// Symmetric APTED costs: tying `delete_cost == insert_cost` is exactly
    /// the regime under which swapping the two trees swaps the roles of
    /// insert and delete at the same per-op cost, so the optimum is
    /// invariant.
    fn arb_symmetric_tsed_options() -> impl Strategy<Value = TSEDOptions> {
        (0.0_f64..3.0, 0.1_f64..3.0, any::<bool>(), any::<bool>()).prop_map(
            |(rename_cost, indel, compare_values, size_penalty)| TSEDOptions {
                apted: APTEDOptions {
                    rename_cost,
                    delete_cost: indel,
                    insert_cost: indel,
                    compare_values,
                },
                size_penalty,
            },
        )
    }

    proptest! {
        /// The function clamps to `[0.0, 1.0]` at the end, but the property
        /// also implicitly checks that no NaN slips through under arbitrary
        /// finite cost configurations (a NaN here would panic in `clamp`).
        #[test]
        fn similarity_is_in_unit_interval(
            a in arb_tree(),
            b in arb_tree(),
            opts in arb_tsed_options(),
        ) {
            let sim = calculate_tsed(&a, &b, &opts);
            prop_assert!((0.0..=1.0).contains(&sim), "got {sim}");
        }

        /// A tree compared against itself has zero edit distance, so the
        /// raw similarity is 1; the size-penalty multiplier is `1` too
        /// (`min/max == 1`), so the score must land on 1.0 for any options.
        #[test]
        fn identical_trees_score_one_pbt(tree in arb_tree(), opts in arb_tsed_options()) {
            let sim = calculate_tsed(&tree, &tree, &opts);
            prop_assert!((sim - 1.0).abs() < 1e-9, "got {sim}");
        }

        /// Symmetry of TSED follows from symmetry of APTED when
        /// `delete_cost == insert_cost`; the size-penalty multiplier
        /// `sqrt(min/max)` is symmetric in the two sizes regardless.
        #[test]
        fn similarity_is_symmetric(
            a in arb_tree(),
            b in arb_tree(),
            opts in arb_symmetric_tsed_options(),
        ) {
            let s_ab = calculate_tsed(&a, &b, &opts);
            let s_ba = calculate_tsed(&b, &a, &opts);
            prop_assert!((s_ab - s_ba).abs() < 1e-9, "ab={s_ab}, ba={s_ba}");
        }

        /// Enabling the size penalty multiplies the raw similarity by
        /// `sqrt(min/max) ∈ [0, 1]`, so it can only ever shrink the score.
        /// The clamp at the end preserves the ordering on both branches.
        #[test]
        fn size_penalty_does_not_increase_score(
            a in arb_tree(),
            b in arb_tree(),
            apted in arb_apted_options(),
        ) {
            let with_penalty = calculate_tsed(
                &a,
                &b,
                &TSEDOptions { apted: apted.clone(), size_penalty: true },
            );
            let without_penalty = calculate_tsed(
                &a,
                &b,
                &TSEDOptions { apted, size_penalty: false },
            );
            prop_assert!(
                with_penalty <= without_penalty + 1e-9,
                "with={with_penalty}, without={without_penalty}"
            );
        }

        /// The precomputed-sizes variant is the same algorithm fed
        /// pre-built subtree-size tables; for any tree pair and any
        /// options it must produce the same score as the on-the-fly
        /// variant.
        #[test]
        fn precomputed_sizes_agree_with_on_the_fly(
            a in arb_tree(),
            b in arb_tree(),
            opts in arb_tsed_options(),
        ) {
            let sizes_a = crate::apted::collect_subtree_sizes(&a);
            let sizes_b = crate::apted::collect_subtree_sizes(&b);
            let direct = calculate_tsed(&a, &b, &opts);
            let cached = calculate_tsed_with_subtree_sizes(
                &a,
                &b,
                a.subtree_size(),
                b.subtree_size(),
                &sizes_a,
                &sizes_b,
                &opts,
            );
            prop_assert!(
                (direct - cached).abs() < 1e-9,
                "direct={direct}, cached={cached}"
            );
        }
    }
}
