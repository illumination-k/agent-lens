//! Language-agnostic function extraction and pairwise similarity.
//!
//! Language-specific crates implement [`LanguageParser`] to go from source
//! text to a list of [`FunctionDef`] values; this module then takes care of
//! comparing every pair with [`crate::calculate_tsed`] and returning the
//! ones that cross a user-supplied threshold.

use crate::tree::TreeNode;
use crate::tsed::{TSEDOptions, calculate_tsed};

/// A single function extracted from a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionDef {
    pub name: String,
    /// 1-based inclusive start line.
    pub start_line: usize,
    /// 1-based inclusive end line.
    pub end_line: usize,
    pub tree: TreeNode,
}

impl FunctionDef {
    pub fn line_count(&self) -> usize {
        self.end_line.saturating_sub(self.start_line) + 1
    }
}

/// Abstraction over a language's source-to-tree pipeline.
///
/// Implementors are expected to be cheap to reuse across files (e.g. storing
/// a tree-sitter or syn parser by value) but they don't need to be thread
/// safe — `agent-lens` drives comparison sequentially per parser instance.
pub trait LanguageParser {
    type Error: std::error::Error + 'static;

    /// Short identifier for the language (e.g. `"rust"`).
    fn language(&self) -> &'static str;

    /// Parse the whole source into a single tree. Mostly useful for whole-
    /// file comparisons; most callers will reach for [`Self::extract_functions`].
    fn parse(&mut self, source: &str) -> Result<TreeNode, Self::Error>;

    /// Extract every function-like definition in `source`.
    fn extract_functions(&mut self, source: &str) -> Result<Vec<FunctionDef>, Self::Error>;
}

/// A pair of functions that exceed the similarity threshold.
#[derive(Debug, Clone, Copy)]
pub struct SimilarPair<'a> {
    pub a: &'a FunctionDef,
    pub b: &'a FunctionDef,
    pub similarity: f64,
}

/// Compute pairwise similarity over `functions` and return every pair whose
/// TSED score is `>= threshold`, sorted from most to least similar.
pub fn find_similar_functions<'a>(
    functions: &'a [FunctionDef],
    threshold: f64,
    opts: &TSEDOptions,
) -> Vec<SimilarPair<'a>> {
    let mut pairs = Vec::new();
    for (i, a) in functions.iter().enumerate() {
        for b in &functions[i + 1..] {
            let similarity = calculate_tsed(&a.tree, &b.tree, opts);
            if similarity >= threshold {
                pairs.push(SimilarPair { a, b, similarity });
            }
        }
    }
    pairs.sort_by(|x, y| {
        y.similarity
            .partial_cmp(&x.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::TreeNode;

    fn def(name: &str, tree: TreeNode) -> FunctionDef {
        FunctionDef {
            name: name.to_owned(),
            start_line: 1,
            end_line: 10,
            tree,
        }
    }

    fn fn_tree(kinds: &[&str]) -> TreeNode {
        let children = kinds.iter().map(|k| TreeNode::leaf(*k)).collect();
        TreeNode::with_children("Block", "", children)
    }

    #[test]
    fn identical_functions_are_reported() {
        let funcs = vec![
            def("a", fn_tree(&["Let", "Call", "Return"])),
            def("b", fn_tree(&["Let", "Call", "Return"])),
        ];
        let pairs = find_similar_functions(&funcs, 0.9, &TSEDOptions::default());
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a.name, "a");
        assert_eq!(pairs[0].b.name, "b");
        assert!((pairs[0].similarity - 1.0).abs() < 1e-9);
    }

    #[test]
    fn dissimilar_functions_filtered_out() {
        let funcs = vec![
            def("a", fn_tree(&["Let", "Call", "Return"])),
            def(
                "b",
                fn_tree(&["If", "While", "For", "Match", "Break", "Loop"]),
            ),
        ];
        let pairs = find_similar_functions(&funcs, 0.9, &TSEDOptions::default());
        assert!(pairs.is_empty());
    }

    #[test]
    fn pairs_sorted_by_similarity_desc() {
        let funcs = vec![
            def("base", fn_tree(&["Let", "Call", "Return"])),
            def("close", fn_tree(&["Let", "Call", "Return"])),
            def("far", fn_tree(&["Let", "If", "Match"])),
        ];
        let pairs = find_similar_functions(&funcs, 0.0, &TSEDOptions::default());
        assert!(pairs.len() >= 2);
        for w in pairs.windows(2) {
            assert!(w[0].similarity >= w[1].similarity);
        }
    }
}
