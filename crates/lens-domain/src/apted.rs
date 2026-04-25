//! Tree edit distance used as the raw signal for similarity.
//!
//! This is a Zhang-Shasha-style recursion on children with a DP alignment
//! table, modelled after the APTED variant used by
//! [`similarity-ts-core`](https://crates.io/crates/similarity-ts-core). The
//! returned distance is the minimum number of weighted `rename`, `insert`,
//! and `delete` operations that turn tree `a` into tree `b`.
//!
//! The recursion is memoised by `(a, b)` pointer pairs so that pre-order
//! exploration of common subtrees does not blow up exponentially.

use std::collections::HashMap;

use crate::tree::TreeNode;

/// Costs for the three edit operations.
///
/// Defaults mirror the usual choice: unit costs for delete/insert, unit cost
/// for rename. TSED bumps the rename cost down (see [`crate::TSEDOptions`])
/// so that matching nodes of a different kind are still cheaper than full
/// delete+insert.
#[derive(Debug, Clone, PartialEq)]
pub struct APTEDOptions {
    pub rename_cost: f64,
    pub delete_cost: f64,
    pub insert_cost: f64,
    /// When `true`, two nodes must also share `value` to skip the rename cost.
    pub compare_values: bool,
}

impl Default for APTEDOptions {
    fn default() -> Self {
        Self {
            rename_cost: 1.0,
            delete_cost: 1.0,
            insert_cost: 1.0,
            compare_values: false,
        }
    }
}

/// Compute the edit distance between two trees.
///
/// Runs in `O(|a| * |b|)` time thanks to memoisation on node-pointer pairs.
/// Callers must keep both trees alive for the duration of the call; raw
/// pointer keys are only dereferenced via the borrowed references that
/// produced them.
pub fn compute_edit_distance(a: &TreeNode, b: &TreeNode, opts: &APTEDOptions) -> f64 {
    let mut memo = HashMap::new();
    tree_distance(a, b, opts, &mut memo)
}

type MemoKey = (usize, usize);

fn tree_distance(
    a: &TreeNode,
    b: &TreeNode,
    opts: &APTEDOptions,
    memo: &mut HashMap<MemoKey, f64>,
) -> f64 {
    let key = node_key(a, b);
    if let Some(&cached) = memo.get(&key) {
        return cached;
    }

    let rename = if nodes_match(a, b, opts) {
        0.0
    } else {
        opts.rename_cost
    };
    let distance = rename + align_children(&a.children, &b.children, opts, memo);
    memo.insert(key, distance);
    distance
}

fn align_children(
    ac: &[TreeNode],
    bc: &[TreeNode],
    opts: &APTEDOptions,
    memo: &mut HashMap<MemoKey, f64>,
) -> f64 {
    let n = ac.len();
    let m = bc.len();
    let mut dp = vec![vec![0.0_f64; m + 1]; n + 1];

    for i in 1..=n {
        dp[i][0] = dp[i - 1][0] + ac[i - 1].subtree_size() as f64 * opts.delete_cost;
    }
    for j in 1..=m {
        dp[0][j] = dp[0][j - 1] + bc[j - 1].subtree_size() as f64 * opts.insert_cost;
    }

    for i in 1..=n {
        for j in 1..=m {
            let delete = dp[i - 1][j] + ac[i - 1].subtree_size() as f64 * opts.delete_cost;
            let insert = dp[i][j - 1] + bc[j - 1].subtree_size() as f64 * opts.insert_cost;
            let rename = dp[i - 1][j - 1] + tree_distance(&ac[i - 1], &bc[j - 1], opts, memo);
            dp[i][j] = delete.min(insert).min(rename);
        }
    }

    dp[n][m]
}

fn nodes_match(a: &TreeNode, b: &TreeNode, opts: &APTEDOptions) -> bool {
    if a.label != b.label {
        return false;
    }
    if opts.compare_values && a.value != b.value {
        return false;
    }
    true
}

fn node_key(a: &TreeNode, b: &TreeNode) -> MemoKey {
    (
        std::ptr::from_ref::<TreeNode>(a) as usize,
        std::ptr::from_ref::<TreeNode>(b) as usize,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(label: &str) -> TreeNode {
        TreeNode::leaf(label)
    }

    fn parent(label: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode::with_children(label, "", children)
    }

    #[test]
    fn identical_trees_have_zero_distance() {
        let a = parent("Root", vec![leaf("A"), leaf("B")]);
        let b = parent("Root", vec![leaf("A"), leaf("B")]);
        assert_eq!(compute_edit_distance(&a, &b, &APTEDOptions::default()), 0.0);
    }

    #[test]
    fn single_rename_has_rename_cost() {
        let a = parent("Root", vec![leaf("A")]);
        let b = parent("Root", vec![leaf("B")]);
        let d = compute_edit_distance(&a, &b, &APTEDOptions::default());
        assert!((d - 1.0).abs() < 1e-9);
    }

    #[test]
    fn insertion_counts_subtree_size() {
        let a = parent("Root", vec![leaf("A")]);
        let b = parent(
            "Root",
            vec![leaf("A"), parent("B", vec![leaf("C"), leaf("D")])],
        );
        let d = compute_edit_distance(&a, &b, &APTEDOptions::default());
        assert!((d - 3.0).abs() < 1e-9);
    }

    #[test]
    fn value_mismatch_ignored_without_compare_values() {
        let a = parent("Ident", vec![]);
        let b = TreeNode::new("Ident", "foo");
        assert_eq!(compute_edit_distance(&a, &b, &APTEDOptions::default()), 0.0);
    }

    #[test]
    fn value_mismatch_charged_when_compare_values_set() {
        let a = TreeNode::new("Ident", "foo");
        let b = TreeNode::new("Ident", "bar");
        let opts = APTEDOptions {
            compare_values: true,
            ..Default::default()
        };
        assert!((compute_edit_distance(&a, &b, &opts) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn deleting_multiple_children_sums_subtree_sizes() {
        let a = parent("Root", vec![leaf("A"), leaf("B"), leaf("C")]);
        let b = parent("Root", vec![]);
        let d = compute_edit_distance(&a, &b, &APTEDOptions::default());
        assert!((d - 3.0).abs() < 1e-9, "got {d}");
    }

    #[test]
    fn inserting_multiple_children_sums_subtree_sizes() {
        let a = parent("Root", vec![]);
        let b = parent(
            "Root",
            vec![leaf("A"), parent("B", vec![leaf("C"), leaf("D")])],
        );
        let d = compute_edit_distance(&a, &b, &APTEDOptions::default());
        assert!((d - 4.0).abs() < 1e-9, "got {d}");
    }

    #[test]
    fn delete_cost_scales_distance() {
        let a = parent("Root", vec![leaf("A")]);
        let b = parent("Root", vec![]);
        let opts = APTEDOptions {
            delete_cost: 2.5,
            ..Default::default()
        };
        let d = compute_edit_distance(&a, &b, &opts);
        assert!((d - 2.5).abs() < 1e-9, "got {d}");
    }

    #[test]
    fn insert_cost_scales_distance() {
        let a = parent("Root", vec![]);
        let b = parent("Root", vec![leaf("A")]);
        let opts = APTEDOptions {
            insert_cost: 3.0,
            ..Default::default()
        };
        let d = compute_edit_distance(&a, &b, &opts);
        assert!((d - 3.0).abs() < 1e-9, "got {d}");
    }

    #[test]
    fn rename_cost_scales_distance() {
        let a = parent("Root", vec![leaf("A")]);
        let b = parent("Root", vec![leaf("B")]);
        let opts = APTEDOptions {
            rename_cost: 0.25,
            ..Default::default()
        };
        let d = compute_edit_distance(&a, &b, &opts);
        assert!((d - 0.25).abs() < 1e-9, "got {d}");
    }

    #[test]
    fn transposed_children_force_two_renames() {
        // Aligning [A,B] against [B,A] cannot match either pair without paying
        // the rename cost on both children, so the optimal path costs 2.
        let a = parent("Root", vec![leaf("A"), leaf("B")]);
        let b = parent("Root", vec![leaf("B"), leaf("A")]);
        let d = compute_edit_distance(&a, &b, &APTEDOptions::default());
        assert!((d - 2.0).abs() < 1e-9, "got {d}");
    }

    #[test]
    fn body_picks_minimum_of_delete_insert_rename() {
        // Make rename strictly cheaper than delete+insert so the body of
        // align_children is forced to take the rename branch and consume
        // dp[i - 1][j - 1].
        let a = parent("Root", vec![leaf("A"), leaf("B"), leaf("C")]);
        let b = parent("Root", vec![leaf("X"), leaf("Y"), leaf("Z")]);
        let opts = APTEDOptions {
            rename_cost: 0.5,
            delete_cost: 10.0,
            insert_cost: 10.0,
            ..Default::default()
        };
        let d = compute_edit_distance(&a, &b, &opts);
        assert!((d - 1.5).abs() < 1e-9, "got {d}");
    }

    #[test]
    fn body_chooses_delete_when_cheaper_than_rename() {
        // Both children of `a` mismatch every child of `b`, but `b` is empty
        // so the only path is to delete every child of `a` — exercising the
        // dp[i - 1][j] + delete_cost branch with j = 0 throughout.
        let a = parent("Root", vec![leaf("A"), leaf("B")]);
        let b = parent("Root", vec![]);
        let opts = APTEDOptions {
            delete_cost: 1.5,
            ..Default::default()
        };
        let d = compute_edit_distance(&a, &b, &opts);
        assert!((d - 3.0).abs() < 1e-9, "got {d}");
    }

    #[test]
    fn body_delete_branch_uses_size_times_delete_cost() {
        // Force the body of align_children to take the `dp[i - 1][j] + size *
        // delete_cost` branch by making rename so expensive that deleting the
        // mismatched extra children is always cheaper. With size = 1 across
        // the board, only a non-unit delete_cost can distinguish `*` from
        // `/` or `+`.
        let a = parent("Root", vec![leaf("X"), leaf("Y"), leaf("Z")]);
        let b = parent("Root", vec![leaf("X")]);
        let opts = APTEDOptions {
            rename_cost: 10.0,
            delete_cost: 2.0,
            insert_cost: 2.0,
            ..Default::default()
        };
        let d = compute_edit_distance(&a, &b, &opts);
        // X aligns with X for free, then Y and Z get deleted at cost 2 each.
        assert!((d - 4.0).abs() < 1e-9, "got {d}");
    }

    #[test]
    fn body_insert_branch_uses_size_times_insert_cost() {
        // Mirror image of the delete-branch test: force the insert path in
        // the body of align_children with a non-unit insert_cost so the `*`
        // can't be confused with `/` (1 / 1 == 1 * 1 hides the mutation).
        let a = parent("Root", vec![leaf("X")]);
        let b = parent("Root", vec![leaf("X"), leaf("Y"), leaf("Z")]);
        let opts = APTEDOptions {
            rename_cost: 10.0,
            delete_cost: 2.0,
            insert_cost: 2.0,
            ..Default::default()
        };
        let d = compute_edit_distance(&a, &b, &opts);
        assert!((d - 4.0).abs() < 1e-9, "got {d}");
    }
}
