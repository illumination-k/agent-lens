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
    let mut ctx = AptedContext::new(opts);
    ctx.tree_distance(a, b)
}

/// Precomputed subtree sizes keyed by node address.
///
/// The keys are only valid while the tree used to build the map is alive and
/// unmoved. This is intended for callers comparing the same set of trees many
/// times, where recomputing subtree sizes inside every APTED run dominates
/// otherwise cheap pair checks.
pub type SubtreeSizes = HashMap<usize, usize>;

/// Collect subtree sizes for every node in `tree`.
pub fn collect_subtree_sizes(tree: &TreeNode) -> SubtreeSizes {
    let mut sizes = HashMap::new();
    collect_subtree_sizes_into(tree, &mut sizes);
    sizes
}

/// Compute edit distance using precomputed subtree size tables.
pub fn compute_edit_distance_with_subtree_sizes(
    a: &TreeNode,
    b: &TreeNode,
    opts: &APTEDOptions,
    left_sizes: &SubtreeSizes,
    right_sizes: &SubtreeSizes,
) -> f64 {
    let mut ctx = AptedContext::with_subtree_sizes(opts, left_sizes, right_sizes);
    ctx.tree_distance(a, b)
}

type MemoKey = (usize, usize);

struct AptedContext<'a> {
    opts: &'a APTEDOptions,
    memo: HashMap<MemoKey, f64>,
    left_sizes: Option<&'a SubtreeSizes>,
    right_sizes: Option<&'a SubtreeSizes>,
    computed_left_sizes: HashMap<usize, usize>,
    computed_right_sizes: HashMap<usize, usize>,
}

impl<'a> AptedContext<'a> {
    fn new(opts: &'a APTEDOptions) -> Self {
        Self {
            opts,
            memo: HashMap::new(),
            left_sizes: None,
            right_sizes: None,
            computed_left_sizes: HashMap::new(),
            computed_right_sizes: HashMap::new(),
        }
    }

    fn with_subtree_sizes(
        opts: &'a APTEDOptions,
        left_sizes: &'a SubtreeSizes,
        right_sizes: &'a SubtreeSizes,
    ) -> Self {
        Self {
            opts,
            memo: HashMap::new(),
            left_sizes: Some(left_sizes),
            right_sizes: Some(right_sizes),
            computed_left_sizes: HashMap::new(),
            computed_right_sizes: HashMap::new(),
        }
    }

    fn tree_distance(&mut self, a: &TreeNode, b: &TreeNode) -> f64 {
        let key = node_pair_key(a, b);
        if let Some(&cached) = self.memo.get(&key) {
            return cached;
        }

        let rename = if nodes_match(a, b, self.opts) {
            0.0
        } else {
            self.opts.rename_cost
        };
        let distance = rename + self.align_children(&a.children, &b.children);
        self.memo.insert(key, distance);
        distance
    }

    fn align_children(&mut self, ac: &[TreeNode], bc: &[TreeNode]) -> f64 {
        let n = ac.len();
        let m = bc.len();
        if n == 0 {
            return bc
                .iter()
                .map(|node| self.right_subtree_size(node) as f64 * self.opts.insert_cost)
                .sum();
        }
        if m == 0 {
            return ac
                .iter()
                .map(|node| self.left_subtree_size(node) as f64 * self.opts.delete_cost)
                .sum();
        }

        let ac_sizes: Vec<usize> = ac.iter().map(|node| self.left_subtree_size(node)).collect();
        let bc_sizes: Vec<usize> = bc
            .iter()
            .map(|node| self.right_subtree_size(node))
            .collect();
        let mut prev = vec![0.0_f64; m + 1];
        let mut cur = vec![0.0_f64; m + 1];

        for j in 1..=m {
            prev[j] = prev[j - 1] + bc_sizes[j - 1] as f64 * self.opts.insert_cost;
        }

        for i in 1..=n {
            let delete_cost = ac_sizes[i - 1] as f64 * self.opts.delete_cost;
            cur[0] = prev[0] + delete_cost;
            for j in 1..=m {
                let insert_cost = bc_sizes[j - 1] as f64 * self.opts.insert_cost;
                let delete = prev[j] + delete_cost;
                let insert = cur[j - 1] + insert_cost;
                let rename = prev[j - 1] + self.tree_distance(&ac[i - 1], &bc[j - 1]);
                cur[j] = delete.min(insert).min(rename);
            }
            std::mem::swap(&mut prev, &mut cur);
        }

        prev[m]
    }

    fn left_subtree_size(&mut self, node: &TreeNode) -> usize {
        let key = node_key(node);
        if let Some(sizes) = self.left_sizes
            && let Some(&size) = sizes.get(&key)
        {
            return size;
        }
        Self::subtree_size_from_cache(node, &mut self.computed_left_sizes)
    }

    fn right_subtree_size(&mut self, node: &TreeNode) -> usize {
        let key = node_key(node);
        if let Some(sizes) = self.right_sizes
            && let Some(&size) = sizes.get(&key)
        {
            return size;
        }
        Self::subtree_size_from_cache(node, &mut self.computed_right_sizes)
    }

    fn subtree_size_from_cache(node: &TreeNode, cache: &mut HashMap<usize, usize>) -> usize {
        let key = node_key(node);
        if let Some(&size) = cache.get(&key) {
            return size;
        }

        let size = 1 + node
            .children
            .iter()
            .map(|child| Self::subtree_size_from_cache(child, cache))
            .sum::<usize>();
        cache.insert(key, size);
        size
    }
}

fn collect_subtree_sizes_into(node: &TreeNode, sizes: &mut SubtreeSizes) -> usize {
    let size = 1 + node
        .children
        .iter()
        .map(|child| collect_subtree_sizes_into(child, sizes))
        .sum::<usize>();
    sizes.insert(node_key(node), size);
    size
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

fn node_pair_key(a: &TreeNode, b: &TreeNode) -> MemoKey {
    (
        std::ptr::from_ref::<TreeNode>(a) as usize,
        std::ptr::from_ref::<TreeNode>(b) as usize,
    )
}

fn node_key(node: &TreeNode) -> usize {
    std::ptr::from_ref::<TreeNode>(node) as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

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

    /// One mismatched leaf should cost exactly `rename_cost` — provided the
    /// configured cost is still cheaper than `delete_cost + insert_cost`,
    /// otherwise the optimal path would route around rename entirely.
    #[rstest]
    #[case::default_cost(1.0)]
    #[case::reduced_cost(0.25)]
    #[case::raised_but_below_indel(1.5)]
    fn single_rename_charges_rename_cost(#[case] rename_cost: f64) {
        let a = parent("Root", vec![leaf("A")]);
        let b = parent("Root", vec![leaf("B")]);
        let opts = APTEDOptions {
            rename_cost,
            ..APTEDOptions::default()
        };
        let d = compute_edit_distance(&a, &b, &opts);
        assert!((d - rename_cost).abs() < 1e-9, "got {d}");
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

    /// Insertion and deletion both charge `cost * subtree_size`. Cases cover
    /// the default unit costs, scaled per-op costs, and the rename-locked-out
    /// path that forces the body of `align_children` through its delete /
    /// insert branches (regression coverage for size × cost mutations).
    #[rstest]
    #[case::default_delete_three(
        APTEDOptions::default(),
        parent("Root", vec![leaf("A"), leaf("B"), leaf("C")]),
        parent("Root", vec![]),
        3.0,
    )]
    #[case::default_insert_subtree(
        APTEDOptions::default(),
        parent("Root", vec![leaf("A")]),
        parent("Root", vec![leaf("A"), parent("B", vec![leaf("C"), leaf("D")])]),
        3.0,
    )]
    #[case::scaled_delete_one(
        APTEDOptions { delete_cost: 2.5, ..APTEDOptions::default() },
        parent("Root", vec![leaf("A")]),
        parent("Root", vec![]),
        2.5,
    )]
    #[case::scaled_insert_one(
        APTEDOptions { insert_cost: 3.0, ..APTEDOptions::default() },
        parent("Root", vec![]),
        parent("Root", vec![leaf("A")]),
        3.0,
    )]
    #[case::body_delete_chosen_over_rename(
        APTEDOptions { delete_cost: 1.5, ..APTEDOptions::default() },
        parent("Root", vec![leaf("A"), leaf("B")]),
        parent("Root", vec![]),
        3.0,
    )]
    #[case::body_delete_branch_size_times_cost(
        APTEDOptions {
            rename_cost: 10.0,
            delete_cost: 2.0,
            insert_cost: 2.0,
            ..APTEDOptions::default()
        },
        parent("Root", vec![leaf("X"), leaf("Y"), leaf("Z")]),
        parent("Root", vec![leaf("X")]),
        4.0,
    )]
    #[case::body_insert_branch_size_times_cost(
        APTEDOptions {
            rename_cost: 10.0,
            delete_cost: 2.0,
            insert_cost: 2.0,
            ..APTEDOptions::default()
        },
        parent("Root", vec![leaf("X")]),
        parent("Root", vec![leaf("X"), leaf("Y"), leaf("Z")]),
        4.0,
    )]
    fn indel_cost_scales_with_subtree_size(
        #[case] opts: APTEDOptions,
        #[case] a: TreeNode,
        #[case] b: TreeNode,
        #[case] expected: f64,
    ) {
        let d = compute_edit_distance(&a, &b, &opts);
        assert!((d - expected).abs() < 1e-9, "got {d}");
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
    fn empty_left_against_multiple_right_charges_full_insert_chain() {
        // align_children([], [X, Y, Z]) only ever populates the dp[0] row;
        // the final answer is `dp[0][m]`. This test forces that path so
        // that dp[0][j] depends on dp[0][j - 1], not on dp[0][j], guarding
        // against mutations of the `j - 1` index.
        let a = parent("Root", vec![]);
        let b = parent("Root", vec![leaf("X"), leaf("Y"), leaf("Z")]);
        let d = compute_edit_distance(&a, &b, &APTEDOptions::default());
        assert!((d - 3.0).abs() < 1e-9, "got {d}");
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
}
