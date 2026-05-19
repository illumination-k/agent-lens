//! Token-based similarity scoring.
//!
//! An alternative to TSED tree-edit distance: each function body is
//! flattened into a preorder sequence of node tokens, and similarity is
//! the weighted Jaccard overlap of the two token k-gram multisets. This is
//! cheaper than TSED and more tolerant of reordered code, at the cost of
//! some precision — see [`super::SimilarityMethod`].

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use lens_domain::TreeNode;

/// Width of the token k-gram window. Matches the preorder shingle width
/// used by the LSH candidate filter, so the token score stays close to
/// the Jaccard estimate that LSH banding is already tuned for.
const SHINGLE_WIDTH: usize = 3;

/// Flattened token view of one function body, precomputed once per
/// function so pairwise scoring never re-walks the tree.
#[derive(Debug)]
pub(super) struct TokenProfile {
    token_count: usize,
    unigrams: HashMap<u64, usize>,
    shingles: HashMap<u64, usize>,
}

impl TokenProfile {
    /// Flatten `tree` into a preorder token sequence and precompute its
    /// unigram and k-gram multisets. `compare_values` mirrors the APTED
    /// option: when set, leaf values (identifiers, literals) fold into the
    /// token; otherwise only structural labels are compared.
    pub(super) fn from_tree(tree: &TreeNode, compare_values: bool) -> Self {
        let mut tokens = Vec::new();
        collect_tokens(tree, compare_values, &mut tokens);
        let unigrams = multiset(tokens.iter().copied());
        let shingles = multiset(k_grams(&tokens, SHINGLE_WIDTH));
        Self {
            token_count: tokens.len(),
            unigrams,
            shingles,
        }
    }
}

/// Weighted Jaccard overlap of two token profiles, in `[0.0, 1.0]`.
///
/// Uses k-gram multisets when both bodies have at least [`SHINGLE_WIDTH`]
/// tokens; tiny bodies fall back to the unigram multiset so the score
/// stays defined (their k-gram sets would be empty and incomparable).
pub(super) fn token_similarity(a: &TokenProfile, b: &TokenProfile) -> f64 {
    if a.token_count >= SHINGLE_WIDTH && b.token_count >= SHINGLE_WIDTH {
        weighted_jaccard(&a.shingles, &b.shingles)
    } else {
        weighted_jaccard(&a.unigrams, &b.unigrams)
    }
}

fn collect_tokens(node: &TreeNode, compare_values: bool, out: &mut Vec<u64>) {
    out.push(token_hash(node, compare_values));
    for child in &node.children {
        collect_tokens(child, compare_values, out);
    }
}

fn token_hash(node: &TreeNode, compare_values: bool) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    node.label.hash(&mut hasher);
    if compare_values {
        node.value.hash(&mut hasher);
    }
    hasher.finish()
}

fn k_grams(tokens: &[u64], width: usize) -> impl Iterator<Item = u64> + '_ {
    tokens.windows(width).map(k_gram_hash)
}

fn k_gram_hash(window: &[u64]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    window.hash(&mut hasher);
    hasher.finish()
}

fn multiset(items: impl Iterator<Item = u64>) -> HashMap<u64, usize> {
    let mut counts = HashMap::new();
    for item in items {
        *counts.entry(item).or_insert(0) += 1;
    }
    counts
}

/// Ruzicka similarity: `sum(min) / sum(max)` over the union of keys.
fn weighted_jaccard(a: &HashMap<u64, usize>, b: &HashMap<u64, usize>) -> f64 {
    let mut intersection = 0usize;
    let mut union = 0usize;
    for (token, &count_a) in a {
        let count_b = b.get(token).copied().unwrap_or(0);
        intersection += count_a.min(count_b);
        union += count_a.max(count_b);
    }
    for (token, &count_b) in b {
        if !a.contains_key(token) {
            union += count_b;
        }
    }
    // Two empty multisets (e.g. two empty bodies) have no union; treat
    // them as identical rather than dividing by zero.
    if union == 0 {
        1.0
    } else {
        intersection as f64 / union as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::collection::vec;
    use proptest::prelude::*;
    use rstest::rstest;

    fn block(children: Vec<TreeNode>) -> TreeNode {
        TreeNode::with_children("Block", "", children)
    }

    /// A four-statement body, long enough to clear the k-gram width.
    fn sample_body() -> TreeNode {
        block(vec![
            TreeNode::leaf("Let"),
            TreeNode::leaf("Let"),
            TreeNode::leaf("If"),
            TreeNode::leaf("Return"),
        ])
    }

    #[test]
    fn identical_bodies_score_one() {
        let a = TokenProfile::from_tree(&sample_body(), false);
        let b = TokenProfile::from_tree(&sample_body(), false);
        assert_eq!(token_similarity(&a, &b), 1.0);
    }

    #[test]
    fn disjoint_bodies_score_zero() {
        let left = block(vec![
            TreeNode::leaf("Let"),
            TreeNode::leaf("Let"),
            TreeNode::leaf("Let"),
        ]);
        let right = TreeNode::with_children(
            "Loop",
            "",
            vec![
                TreeNode::leaf("Call"),
                TreeNode::leaf("Call"),
                TreeNode::leaf("Call"),
            ],
        );
        let a = TokenProfile::from_tree(&left, false);
        let b = TokenProfile::from_tree(&right, false);
        assert_eq!(token_similarity(&a, &b), 0.0);
    }

    #[test]
    fn a_shared_prefix_scores_between_zero_and_one() {
        let shared = sample_body();
        let mut extended = sample_body();
        extended.children.push(TreeNode::leaf("Return"));
        let a = TokenProfile::from_tree(&shared, false);
        let b = TokenProfile::from_tree(&extended, false);
        let score = token_similarity(&a, &b);
        assert!(score > 0.0 && score < 1.0, "got {score}");
    }

    #[test]
    fn compare_values_distinguishes_leaf_values() {
        let left = block(vec![
            TreeNode::new("Ident", "alpha"),
            TreeNode::new("Ident", "beta"),
            TreeNode::new("Ident", "gamma"),
        ]);
        let right = block(vec![
            TreeNode::new("Ident", "delta"),
            TreeNode::new("Ident", "epsilon"),
            TreeNode::new("Ident", "zeta"),
        ]);

        let structural = token_similarity(
            &TokenProfile::from_tree(&left, false),
            &TokenProfile::from_tree(&right, false),
        );
        let value_aware = token_similarity(
            &TokenProfile::from_tree(&left, true),
            &TokenProfile::from_tree(&right, true),
        );

        assert_eq!(structural, 1.0);
        assert!(value_aware < structural, "value_aware={value_aware}");
    }

    #[rstest]
    #[case::single_leaf(TreeNode::leaf("Block"))]
    #[case::two_nodes(block(vec![TreeNode::leaf("Return")]))]
    fn tiny_bodies_fall_back_to_unigrams(#[case] body: TreeNode) {
        let profile = TokenProfile::from_tree(&body, false);
        // Below the k-gram width, so the score must still be defined.
        assert_eq!(token_similarity(&profile, &profile), 1.0);
    }

    #[test]
    fn a_long_body_paired_with_a_tiny_one_falls_back_to_unigrams() {
        // Only one body clears the k-gram width. The fallback must trigger
        // for the *pair*: scoring the tiny body's empty k-gram set against
        // the long body's would force the score to 0 despite shared tokens.
        let long = block(vec![
            TreeNode::leaf("Let"),
            TreeNode::leaf("Let"),
            TreeNode::leaf("Let"),
            TreeNode::leaf("Let"),
        ]);
        let tiny = block(vec![TreeNode::leaf("Let")]);
        let score = token_similarity(
            &TokenProfile::from_tree(&long, false),
            &TokenProfile::from_tree(&tiny, false),
        );
        assert!(
            score > 0.0 && score < 1.0,
            "shared unigrams must score between 0 and 1: {score}",
        );
    }

    #[test]
    fn reordered_statements_drop_below_exact_match() {
        let labels = ["Let", "Assign", "Call", "If", "Loop", "Return"];
        let forward = block(labels.iter().map(|l| TreeNode::leaf(*l)).collect());
        // Swap two adjacent statements in the middle: k-grams away from
        // the swap survive, so the bodies stay similar but not exact.
        let mut swapped = labels;
        swapped.swap(2, 3);
        let reordered = block(swapped.iter().map(|l| TreeNode::leaf(*l)).collect());

        let score = token_similarity(
            &TokenProfile::from_tree(&forward, false),
            &TokenProfile::from_tree(&reordered, false),
        );
        assert!(score > 0.0 && score < 1.0, "got {score}");
    }

    fn arb_tree() -> impl Strategy<Value = TreeNode> {
        let leaf = prop_oneof![
            Just(TreeNode::leaf("A")),
            Just(TreeNode::leaf("B")),
            Just(TreeNode::leaf("C")),
            Just(TreeNode::leaf("D")),
        ];
        leaf.prop_recursive(4, 32, 4, |inner| {
            (
                prop_oneof![Just("A"), Just("B"), Just("C")],
                vec(inner, 0..4),
            )
                .prop_map(|(label, children)| TreeNode::with_children(label, "", children))
        })
    }

    proptest! {
        #[test]
        fn similarity_is_reflexive_symmetric_and_bounded(
            a in arb_tree(),
            b in arb_tree(),
        ) {
            let profile_a = TokenProfile::from_tree(&a, false);
            let profile_b = TokenProfile::from_tree(&b, false);
            let ab = token_similarity(&profile_a, &profile_b);
            let ba = token_similarity(&profile_b, &profile_a);

            prop_assert!((0.0..=1.0).contains(&ab), "out of range: {ab}");
            prop_assert!((ab - ba).abs() < 1e-9, "asymmetric: {ab} vs {ba}");
            prop_assert!(
                (token_similarity(&profile_a, &profile_a) - 1.0).abs() < 1e-9,
                "not reflexive",
            );
        }
    }
}
