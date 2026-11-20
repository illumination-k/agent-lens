//! The Quint adapter's [`DependenceVocabulary`] over the labels
//! [`crate::parser`] emits, for `similarity --method pdg`.
//!
//! Quint bodies are expressions, so the statement-like lists are the
//! action combinators: every alternative of `any { ... }` and every
//! conjunct of `all { ... }` is one statement. `val` / `nondet` / `def`
//! bindings inside a body (`Let` nodes, labelled by qualifier) bind their
//! name for the expression that follows; a lambda binds its parameters.
//! A `Name` is a read. Quint has no loops.

use lens_domain::{DependenceRole, DependenceVocabulary, TreeNode};

/// Quint's dependence vocabulary.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuintVocabulary;

/// Qualifiers a `let` binding can carry, which [`crate::parser`] uses as
/// the node label.
const LET_QUALIFIERS: &[&str] = &["val", "nondet", "def", "puredef", "pureval", "action"];

impl DependenceVocabulary for QuintVocabulary {
    fn role<'a>(&self, node: &'a TreeNode) -> DependenceRole<'a> {
        match node.label.as_str() {
            "actionAll" | "actionAny" => DependenceRole::StatementList,
            "Name" if !node.value.is_empty() => DependenceRole::Reference(&node.value),
            label if LET_QUALIFIERS.contains(&label) => DependenceRole::Binding {
                names: vec![node.value.as_str()],
                targets: Vec::new(),
            },
            "Lambda" => {
                let targets: Vec<usize> = node
                    .children
                    .iter()
                    .enumerate()
                    .filter(|(_, child)| child.label == "Param")
                    .map(|(index, _)| index)
                    .collect();
                let names = targets
                    .iter()
                    .filter_map(|&index| node.children.get(index))
                    .map(|child| child.value.as_str())
                    .collect();
                DependenceRole::Binding { names, targets }
            }
            _ => DependenceRole::Plain,
        }
    }

    fn is_loop(&self, _node: &TreeNode) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_read_and_lets_bind() {
        let vocab = QuintVocabulary;
        assert_eq!(
            vocab.role(&TreeNode::new("Name", "x")),
            DependenceRole::Reference("x")
        );
        let let_node = TreeNode::with_children(
            "nondet",
            "v",
            vec![TreeNode::leaf("oneOf"), TreeNode::leaf("assign")],
        );
        assert_eq!(
            vocab.role(&let_node),
            DependenceRole::Binding {
                names: vec!["v"],
                targets: Vec::new()
            }
        );
        assert_eq!(
            vocab.role(&TreeNode::leaf("actionAny")),
            DependenceRole::StatementList
        );
    }

    #[test]
    fn lambdas_bind_their_parameters() {
        let lambda = TreeNode::with_children(
            "Lambda",
            "",
            vec![TreeNode::new("Param", "e"), TreeNode::new("Name", "e")],
        );
        assert_eq!(
            QuintVocabulary.role(&lambda),
            DependenceRole::Binding {
                names: vec!["e"],
                targets: vec![0]
            }
        );
    }
}
