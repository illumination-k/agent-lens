//! Generic labelled tree used as the comparison currency for similarity.
//!
//! Each node carries a structural `label` (e.g. `"If"`, `"Ident"`) and an
//! optional `value` (e.g. the identifier text, literal source). Comparisons
//! can opt in to value-level matching via [`crate::APTEDOptions`].

/// A tree node with a label, a value, and owned children.
///
/// The tree is built bottom-up by language-specific parsers. Nodes own their
/// children directly; no shared ownership is needed because we only read
/// trees during comparison.
///
/// # Examples
///
/// ```
/// use lens_domain::TreeNode;
///
/// let tree = TreeNode::with_children(
///     "Root",
///     "",
///     vec![
///         TreeNode::leaf("A"),
///         TreeNode::with_children("B", "", vec![TreeNode::leaf("C")]),
///     ],
/// );
/// assert_eq!(tree.subtree_size(), 4);
/// assert_eq!(tree.children.len(), 2);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeNode {
    pub label: String,
    pub value: String,
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            children: Vec::new(),
        }
    }

    pub fn leaf(label: impl Into<String>) -> Self {
        Self::new(label, "")
    }

    pub fn with_children(
        label: impl Into<String>,
        value: impl Into<String>,
        children: Vec<TreeNode>,
    ) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            children,
        }
    }

    pub fn push_child(&mut self, child: TreeNode) {
        self.children.push(child);
    }

    /// Number of nodes in the subtree rooted at `self` (including `self`).
    pub fn subtree_size(&self) -> usize {
        1 + self.children.iter().map(Self::subtree_size).sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtree_size_counts_all_nodes() {
        let tree = TreeNode::with_children(
            "Root",
            "",
            vec![
                TreeNode::leaf("A"),
                TreeNode::with_children("B", "", vec![TreeNode::leaf("C"), TreeNode::leaf("D")]),
            ],
        );
        assert_eq!(tree.subtree_size(), 5);
    }

    #[test]
    fn leaf_has_size_one() {
        assert_eq!(TreeNode::leaf("X").subtree_size(), 1);
    }

    #[test]
    fn push_child_appends_to_children_in_order() {
        let mut tree = TreeNode::new("Root", "");
        assert!(tree.children.is_empty());

        tree.push_child(TreeNode::leaf("A"));
        tree.push_child(TreeNode::leaf("B"));

        assert_eq!(tree.children.len(), 2);
        assert_eq!(tree.children[0].label, "A");
        assert_eq!(tree.children[1].label, "B");
    }

    use proptest::collection::vec as prop_vec;
    use proptest::prelude::*;

    /// Random labelled tree. Keeps the alphabet small (`L0..L7` / `V0..V3`)
    /// so that generated trees have realistic label collisions; bounded
    /// depth and breadth keep cases small enough for shrinking to terminate.
    fn arb_tree() -> impl Strategy<Value = TreeNode> {
        let leaf =
            (0u8..8, 0u8..4).prop_map(|(l, v)| TreeNode::new(format!("L{l}"), format!("V{v}")));
        leaf.prop_recursive(4, 24, 4, |inner| {
            (0u8..8, 0u8..4, prop_vec(inner, 0..4)).prop_map(|(l, v, kids)| {
                TreeNode::with_children(format!("L{l}"), format!("V{v}"), kids)
            })
        })
    }

    /// Independent iterative node counter so the property check doesn't
    /// just repeat `subtree_size`'s recursive shape.
    fn count_nodes_iterative(root: &TreeNode) -> usize {
        let mut stack = vec![root];
        let mut count = 0;
        while let Some(n) = stack.pop() {
            count += 1;
            for c in &n.children {
                stack.push(c);
            }
        }
        count
    }

    proptest! {
        #[test]
        fn subtree_size_matches_independent_iterative_count(tree in arb_tree()) {
            prop_assert_eq!(tree.subtree_size(), count_nodes_iterative(&tree));
        }

        #[test]
        fn subtree_size_is_at_least_one(tree in arb_tree()) {
            prop_assert!(tree.subtree_size() >= 1);
        }

        #[test]
        fn push_child_grows_size_and_children_by_one_subtree(
            parent in arb_tree(),
            child in arb_tree(),
        ) {
            let parent_size = parent.subtree_size();
            let child_size = child.subtree_size();
            let parent_children_len = parent.children.len();
            let mut p = parent;
            p.push_child(child);
            prop_assert_eq!(p.subtree_size(), parent_size + child_size);
            prop_assert_eq!(p.children.len(), parent_children_len + 1);
        }

        #[test]
        fn with_children_size_is_one_plus_sum_of_child_sizes(
            label in "[A-Z]{1,3}",
            value in "[a-z]{0,3}",
            kids in prop_vec(arb_tree(), 0..5),
        ) {
            let expected = 1 + kids.iter().map(TreeNode::subtree_size).sum::<usize>();
            let t = TreeNode::with_children(label, value, kids);
            prop_assert_eq!(t.subtree_size(), expected);
        }
    }
}
