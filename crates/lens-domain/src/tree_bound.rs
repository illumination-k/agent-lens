//! Cheap lower bound on the tree edit distance, used to drop a candidate
//! pair before the full [`crate::apted`] computation.
//!
//! A tree edit mapping preserves both the preorder and the postorder of
//! the nodes it maps, so every mapping is also an alignment of the two
//! trees' preorder (and postorder) label strings at the same cost: a
//! mapped pair with different labels is a substitution, an unmapped node
//! a delete or insert. The string edit distance of either traversal is
//! therefore a lower bound on the tree edit distance (Guha et al., 2002;
//! SED-struct, arXiv 2609.03078). The distance [`crate::apted`] computes
//! is a constrained (top-down) edit distance, which is never below the
//! unconstrained one, so the bound holds for it too.
//!
//! The string distance is computed with Ukkonen's cutoff: only the band
//! of diagonals a path of cost `<= cutoff` can reach is filled, and the
//! computation stops as soon as a whole row exceeds the cutoff. A pair
//! that is far apart is rejected in `O(n * cutoff)` rather than the
//! `O(n * m)` a full table would take.

use std::hash::{Hash, Hasher};

use crate::apted::APTEDOptions;
use crate::tree::TreeNode;

/// Preorder and postorder label strings of one tree, precomputed once so
/// that pairwise bounds never re-walk it.
#[derive(Debug, Clone)]
pub struct TraversalProfile {
    preorder: Vec<u64>,
    postorder: Vec<u64>,
}

impl TraversalProfile {
    /// Flatten `tree` into its traversal strings. `compare_values` must
    /// match the [`APTEDOptions::compare_values`] the bound is later
    /// checked against: when set, a node's value folds into its symbol,
    /// so two nodes are free to align exactly when APTED would match
    /// them without a rename.
    pub fn from_tree(tree: &TreeNode, compare_values: bool) -> Self {
        let mut preorder = Vec::new();
        let mut postorder = Vec::new();
        collect(tree, compare_values, &mut preorder, &mut postorder);
        Self {
            preorder,
            postorder,
        }
    }
}

fn collect(node: &TreeNode, compare_values: bool, pre: &mut Vec<u64>, post: &mut Vec<u64>) {
    let symbol = node_symbol(node, compare_values);
    pre.push(symbol);
    for child in &node.children {
        collect(child, compare_values, pre, post);
    }
    post.push(symbol);
}

fn node_symbol(node: &TreeNode, compare_values: bool) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    node.label.hash(&mut hasher);
    if compare_values {
        node.value.hash(&mut hasher);
    }
    hasher.finish()
}

/// Outcome of [`edit_distance_lower_bound`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DistanceBound {
    /// The traversal strings fit within the cutoff; the value is their
    /// string edit distance, a lower bound on the tree edit distance.
    Within(f64),
    /// The tree edit distance is certainly above the cutoff. The value is
    /// a lower bound on it that already exceeds the cutoff.
    Exceeds(f64),
}

impl DistanceBound {
    /// The lower bound on the tree edit distance either variant carries.
    pub fn lower_bound(self) -> f64 {
        match self {
            Self::Within(bound) | Self::Exceeds(bound) => bound,
        }
    }
}

/// Lower-bound the edit distance between the trees behind `a` and `b`
/// under `opts`' costs, giving up early once it is known to exceed
/// `cutoff`.
///
/// Checks the preorder string first and the postorder string only when
/// the first still fits, taking the larger of the two.
pub fn edit_distance_lower_bound(
    a: &TraversalProfile,
    b: &TraversalProfile,
    opts: &APTEDOptions,
    cutoff: f64,
) -> DistanceBound {
    match banded_string_edit_distance(&a.preorder, &b.preorder, opts, cutoff) {
        DistanceBound::Within(pre) => {
            match banded_string_edit_distance(&a.postorder, &b.postorder, opts, cutoff) {
                DistanceBound::Within(post) => DistanceBound::Within(pre.max(post)),
                exceeds @ DistanceBound::Exceeds(_) => exceeds,
            }
        }
        exceeds @ DistanceBound::Exceeds(_) => exceeds,
    }
}

/// Weighted string edit distance (substitution `rename_cost`, deletion
/// `delete_cost`, insertion `insert_cost`) with Ukkonen's cutoff.
fn banded_string_edit_distance(
    x: &[u64],
    y: &[u64],
    opts: &APTEDOptions,
    cutoff: f64,
) -> DistanceBound {
    let band = match Band::for_strings(x.len(), y.len(), opts, cutoff) {
        Ok(band) => band,
        Err(exceeds) => return exceeds,
    };
    let m = y.len();
    let mut prev = vec![f64::INFINITY; m + 2];
    let mut cur = vec![f64::INFINITY; m + 2];
    for (j, cell) in prev.iter_mut().enumerate().take(band.width.min(m) + 1) {
        *cell = j as f64 * opts.insert_cost;
    }
    for (i, &symbol) in x.iter().enumerate() {
        let row_min = fill_row(&prev, &mut cur, i + 1, symbol, y, band.width, opts);
        if row_min > cutoff {
            return DistanceBound::Exceeds(row_min.min(band.off_band));
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    let distance = cell(&prev, m);
    if distance > cutoff {
        DistanceBound::Exceeds(distance.min(band.off_band))
    } else {
        DistanceBound::Within(distance)
    }
}

/// The diagonals a path of cost `<= cutoff` can reach.
struct Band {
    /// Cells further than this off the diagonal are not filled.
    width: usize,
    /// What any path leaving the band has paid at least. An early exit
    /// reports the smaller of this and the in-band cost, so the bound it
    /// carries is still a bound on the full distance.
    off_band: f64,
}

impl Band {
    /// A path through cell `(i, j)` has paid at least `|i - j|` insertions
    /// or deletions, so the band is `cutoff / min(delete, insert)` wide.
    /// `Err` when the two lengths alone already rule the pair out.
    fn for_strings(
        n: usize,
        m: usize,
        opts: &APTEDOptions,
        cutoff: f64,
    ) -> Result<Self, DistanceBound> {
        if cutoff < 0.0 {
            return Err(DistanceBound::Exceeds(0.0));
        }
        let indel = opts.delete_cost.min(opts.insert_cost);
        if indel <= 0.0 || !cutoff.is_finite() {
            return Ok(Self {
                width: n.max(m),
                off_band: f64::INFINITY,
            });
        }
        let width = (cutoff / indel).floor() as usize;
        let length_gap = n.abs_diff(m);
        if length_gap > width {
            return Err(DistanceBound::Exceeds(length_gap as f64 * indel));
        }
        Ok(Self {
            width,
            off_band: (width + 1) as f64 * indel,
        })
    }
}

/// Fill row `i` of the table inside the band from row `i - 1` in `prev`,
/// fencing the band's edges with infinity so the next row never reads a
/// stale cell. Returns the row's minimum.
fn fill_row(
    prev: &[f64],
    cur: &mut [f64],
    i: usize,
    symbol: u64,
    y: &[u64],
    width: usize,
    opts: &APTEDOptions,
) -> f64 {
    let lo = i.saturating_sub(width);
    let hi = (i + width).min(y.len());
    set(cur, lo.wrapping_sub(1), f64::INFINITY);
    let mut row_min = f64::INFINITY;
    for j in lo..=hi {
        let value = match j.checked_sub(1) {
            None => i as f64 * opts.delete_cost,
            Some(left) => {
                let substitution = if y.get(left) == Some(&symbol) {
                    0.0
                } else {
                    opts.rename_cost
                };
                (cell(prev, left) + substitution)
                    .min(cell(prev, j) + opts.delete_cost)
                    .min(cell(cur, left) + opts.insert_cost)
            }
        };
        set(cur, j, value);
        row_min = row_min.min(value);
    }
    set(cur, hi + 1, f64::INFINITY);
    row_min
}

fn cell(row: &[f64], j: usize) -> f64 {
    row.get(j).copied().unwrap_or(f64::INFINITY)
}

/// Write `value` at `j`; an index past the end (including a wrapped
/// `0 - 1`) is outside the table and ignored.
fn set(row: &mut [f64], j: usize, value: f64) {
    if let Some(slot) = row.get_mut(j) {
        *slot = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apted::compute_edit_distance;
    use proptest::collection::vec as prop_vec;
    use proptest::prelude::*;
    use rstest::rstest;

    fn leaf(label: &str) -> TreeNode {
        TreeNode::leaf(label)
    }

    fn parent(label: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode::with_children(label, "", children)
    }

    fn unit_costs() -> APTEDOptions {
        APTEDOptions::default()
    }

    fn bound(a: &TreeNode, b: &TreeNode, opts: &APTEDOptions, cutoff: f64) -> DistanceBound {
        edit_distance_lower_bound(
            &TraversalProfile::from_tree(a, opts.compare_values),
            &TraversalProfile::from_tree(b, opts.compare_values),
            opts,
            cutoff,
        )
    }

    #[test]
    fn identical_trees_have_zero_bound() {
        let tree = parent("Root", vec![leaf("A"), parent("B", vec![leaf("C")])]);
        assert_eq!(
            bound(&tree, &tree, &unit_costs(), 0.0),
            DistanceBound::Within(0.0)
        );
    }

    #[test]
    fn one_renamed_leaf_costs_one_rename() {
        let a = parent("Root", vec![leaf("A"), leaf("B")]);
        let b = parent("Root", vec![leaf("A"), leaf("Z")]);
        let opts = APTEDOptions {
            rename_cost: 0.3,
            ..unit_costs()
        };
        let DistanceBound::Within(d) = bound(&a, &b, &opts, 10.0) else {
            panic!("expected the bound to fit");
        };
        assert!((d - 0.3).abs() < 1e-9, "got {d}");
    }

    #[rstest]
    #[case::length_gap(parent("Root", vec![leaf("A"); 2]), parent("Root", vec![leaf("A"); 9]), 3.0)]
    #[case::disjoint_labels(parent("A", vec![leaf("x"); 6]), parent("B", vec![leaf("y"); 6]), 2.0)]
    fn far_apart_trees_exceed_the_cutoff(
        #[case] a: TreeNode,
        #[case] b: TreeNode,
        #[case] cutoff: f64,
    ) {
        let result = bound(&a, &b, &unit_costs(), cutoff);
        assert!(
            matches!(result, DistanceBound::Exceeds(lb) if lb > cutoff),
            "got {result:?}"
        );
    }

    #[test]
    fn negative_cutoff_rejects_everything() {
        let tree = leaf("A");
        assert!(matches!(
            bound(&tree, &tree, &unit_costs(), -1.0),
            DistanceBound::Exceeds(_)
        ));
    }

    #[test]
    fn value_mismatch_counts_only_under_compare_values() {
        let a = parent("Root", vec![TreeNode::new("Ident", "x")]);
        let b = parent("Root", vec![TreeNode::new("Ident", "y")]);
        let structural = bound(&a, &b, &unit_costs(), 10.0).lower_bound();
        let valued = bound(
            &a,
            &b,
            &APTEDOptions {
                compare_values: true,
                ..unit_costs()
            },
            10.0,
        )
        .lower_bound();
        assert_eq!(structural, 0.0);
        assert!(valued > 0.0, "got {valued}");
    }

    fn arb_tree() -> impl Strategy<Value = TreeNode> {
        let leaf =
            (0u8..5, 0u8..3).prop_map(|(l, v)| TreeNode::new(format!("L{l}"), format!("V{v}")));
        leaf.prop_recursive(4, 24, 4, |inner| {
            (0u8..5, 0u8..3, prop_vec(inner, 0..4)).prop_map(|(l, v, kids)| {
                TreeNode::with_children(format!("L{l}"), format!("V{v}"), kids)
            })
        })
    }

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

    proptest! {
        /// The bound must never exceed the distance APTED computes, or the
        /// filter would drop a pair the exact score would have kept.
        #[test]
        fn bound_never_exceeds_tree_edit_distance(
            a in arb_tree(),
            b in arb_tree(),
            opts in arb_apted_options(),
        ) {
            let exact = compute_edit_distance(&a, &b, &opts);
            let lower = bound(&a, &b, &opts, f64::INFINITY).lower_bound();
            prop_assert!(lower <= exact + 1e-9, "bound={lower}, exact={exact}");
        }

        /// The cutoff only decides when to stop: a pair whose exact
        /// distance fits within it is always reported `Within`, and an
        /// `Exceeds` verdict always carries a bound above the cutoff.
        #[test]
        fn cutoff_verdict_is_sound(
            a in arb_tree(),
            b in arb_tree(),
            opts in arb_apted_options(),
            cutoff in 0.0_f64..12.0,
        ) {
            let exact = compute_edit_distance(&a, &b, &opts);
            match bound(&a, &b, &opts, cutoff) {
                DistanceBound::Within(lower) => {
                    prop_assert!(lower <= cutoff + 1e-9);
                    let unbanded = bound(&a, &b, &opts, f64::INFINITY).lower_bound();
                    prop_assert!((lower - unbanded).abs() < 1e-9);
                }
                DistanceBound::Exceeds(lower) => {
                    prop_assert!(lower > cutoff);
                    prop_assert!(exact > cutoff - 1e-9, "exact={exact}, cutoff={cutoff}");
                }
            }
        }
    }
}
