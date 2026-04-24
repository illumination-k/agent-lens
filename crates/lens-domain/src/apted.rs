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
}
