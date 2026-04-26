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
}
