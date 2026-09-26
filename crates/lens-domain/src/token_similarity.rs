//! Token-based similarity scoring.
//!
//! An alternative to TSED tree-edit distance: each function body is
//! flattened into a preorder sequence of node tokens, and similarity is
//! the weighted Jaccard overlap of the two token k-gram multisets. This is
//! cheaper than TSED and more tolerant of reordered code, at the cost of
//! some precision — see `SimilarityMethod::Token` in `agent-lens`.
//!
//! Two refinements sit on the same profile:
//!
//! - [`weighted_token_similarity`] weights each k-gram by its inverse
//!   document frequency over the corpus ([`TokenIdf`]), so boilerplate
//!   that appears in every body — logging, error plumbing, builder
//!   chains — counts for less than the logic that makes a body its own
//!   (Siamese, EMSE 2019; ECScan, arXiv 2502.19219).
//! - [`lcs_similarity`] compares the token *sequences* by their longest
//!   common subsequence, the order-aware verifier NIL (ESEC/FSE 2021)
//!   uses to catch Type-3 clones with large inserted gaps that a k-gram
//!   multiset scores low on.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::TreeNode;

/// Width of the token k-gram window. Matches the preorder shingle width
/// used by the LSH candidate filter, so the token score stays close to
/// the Jaccard estimate that LSH banding is already tuned for.
const SHINGLE_WIDTH: usize = 3;

/// Flattened token view of one function body, precomputed once per
/// function so pairwise scoring never re-walks the tree.
#[derive(Debug)]
pub struct TokenProfile {
    tokens: Vec<u64>,
    unigrams: HashMap<u64, usize>,
    shingles: HashMap<u64, usize>,
}

impl TokenProfile {
    /// Flatten `tree` into a preorder token sequence and precompute its
    /// unigram and k-gram multisets. `compare_values` mirrors the APTED
    /// option: when set, leaf values (identifiers, literals) fold into the
    /// token; otherwise only structural labels are compared.
    pub fn from_tree(tree: &TreeNode, compare_values: bool) -> Self {
        let mut tokens = Vec::new();
        collect_tokens(tree, compare_values, &mut tokens);
        let unigrams = multiset(tokens.iter().copied());
        let shingles = multiset(k_grams(&tokens, SHINGLE_WIDTH));
        Self {
            tokens,
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
pub fn token_similarity(a: &TokenProfile, b: &TokenProfile) -> f64 {
    if uses_shingles(a, b) {
        weighted_jaccard(&a.shingles, &b.shingles, |_| 1.0)
    } else {
        weighted_jaccard(&a.unigrams, &b.unigrams, |_| 1.0)
    }
}

/// [`token_similarity`] with every k-gram (or unigram, for tiny bodies)
/// weighted by its inverse document frequency in `idf`'s corpus.
///
/// Still reflexive, symmetric and in `[0.0, 1.0]`: the weights are
/// positive, so this is the Ruzicka similarity over weighted counts. A
/// pair that shares only corpus-wide boilerplate scores lower than it
/// does unweighted; a pair that shares rare k-grams scores higher.
pub fn weighted_token_similarity(a: &TokenProfile, b: &TokenProfile, idf: &TokenIdf) -> f64 {
    if uses_shingles(a, b) {
        weighted_jaccard(&a.shingles, &b.shingles, |key| idf.shingle_weight(key))
    } else {
        weighted_jaccard(&a.unigrams, &b.unigrams, |key| idf.unigram_weight(key))
    }
}

fn uses_shingles(a: &TokenProfile, b: &TokenProfile) -> bool {
    a.tokens.len() >= SHINGLE_WIDTH && b.tokens.len() >= SHINGLE_WIDTH
}

/// Inverse document frequency of every k-gram and unigram across a
/// corpus of [`TokenProfile`]s: a body counts once per distinct key,
/// however often the key repeats inside it.
#[derive(Debug, Default)]
pub struct TokenIdf {
    documents: usize,
    shingle_df: HashMap<u64, usize>,
    unigram_df: HashMap<u64, usize>,
}

impl TokenIdf {
    pub fn from_profiles<'a>(profiles: impl IntoIterator<Item = &'a TokenProfile>) -> Self {
        let mut idf = Self::default();
        for profile in profiles {
            idf.documents += 1;
            for &key in profile.shingles.keys() {
                *idf.shingle_df.entry(key).or_insert(0) += 1;
            }
            for &key in profile.unigrams.keys() {
                *idf.unigram_df.entry(key).or_insert(0) += 1;
            }
        }
        idf
    }

    fn shingle_weight(&self, key: u64) -> f64 {
        self.weight(self.shingle_df.get(&key).copied().unwrap_or(0))
    }

    fn unigram_weight(&self, key: u64) -> f64 {
        self.weight(self.unigram_df.get(&key).copied().unwrap_or(0))
    }

    /// Smoothed IDF, `ln((1 + N) / (1 + df)) + 1`: always positive, so a
    /// key present in every body still counts, just least.
    fn weight(&self, document_frequency: usize) -> f64 {
        ((1 + self.documents) as f64 / (1 + document_frequency) as f64).ln() + 1.0
    }
}

/// Longest-common-subsequence similarity of the two token sequences,
/// `|LCS| / max(|a|, |b|)`, in `[0.0, 1.0]`.
///
/// Order-aware where [`token_similarity`] is not, and tolerant of gaps:
/// a clone with a block of statements inserted in the middle keeps every
/// token of the original as a common subsequence. Dividing by the longer
/// sequence keeps a small body from scoring 1 against a large one that
/// merely contains it. Computed bit-parallel (Hyyrö 2004) in
/// `O(|a| * |b| / 64)`.
pub fn lcs_similarity(a: &TokenProfile, b: &TokenProfile) -> f64 {
    ratio(lcs_length(&a.tokens, &b.tokens), a, b)
}

/// Cheap upper bound on [`lcs_similarity`]: a common subsequence uses
/// each token at most as often as the rarer side holds it, so the
/// unigram multiset intersection bounds its length.
pub fn lcs_similarity_upper_bound(a: &TokenProfile, b: &TokenProfile) -> f64 {
    let shared = a
        .unigrams
        .iter()
        .map(|(key, &count)| count.min(b.unigrams.get(key).copied().unwrap_or(0)))
        .sum();
    ratio(shared, a, b)
}

fn ratio(common: usize, a: &TokenProfile, b: &TokenProfile) -> f64 {
    let longest = a.tokens.len().max(b.tokens.len());
    if longest == 0 {
        1.0
    } else {
        common as f64 / longest as f64
    }
}

/// Bit-parallel LCS length: one bit per position of `pattern`, cleared
/// once that position is matched in the current best alignment.
fn lcs_length(pattern: &[u64], text: &[u64]) -> usize {
    if pattern.is_empty() {
        return 0;
    }
    let words = pattern.len().div_ceil(64);
    let mut masks: HashMap<u64, Vec<u64>> = HashMap::new();
    for (pos, &symbol) in pattern.iter().enumerate() {
        let mask = masks.entry(symbol).or_insert_with(|| vec![0; words]);
        if let Some(word) = mask.get_mut(pos / 64) {
            *word |= 1 << (pos % 64);
        }
    }
    let mut v = vec![u64::MAX; words];
    for symbol in text {
        let Some(mask) = masks.get(symbol) else {
            continue;
        };
        let mut carry = false;
        for (word, &m) in v.iter_mut().zip(mask) {
            let u = *word & m;
            let (sum, overflow_a) = word.overflowing_add(u);
            let (sum, overflow_b) = sum.overflowing_add(u64::from(carry));
            carry = overflow_a || overflow_b;
            // `u` is a subset of `word`, so `word - u` never borrows.
            *word = sum | (*word & !u);
        }
    }
    // Bits past the pattern's end start set and never meet a mask bit;
    // `word & !u` keeps every such bit set, so only matched positions
    // are ever cleared.
    v.iter().map(|word| word.count_zeros() as usize).sum()
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

/// Ruzicka similarity: `sum(w * min) / sum(w * max)` over the union of
/// keys, with `weight` giving each key's `w`.
fn weighted_jaccard(
    a: &HashMap<u64, usize>,
    b: &HashMap<u64, usize>,
    weight: impl Fn(u64) -> f64,
) -> f64 {
    let mut intersection = 0.0;
    let mut union = 0.0;
    for (&token, &count_a) in a {
        let count_b = b.get(&token).copied().unwrap_or(0);
        let w = weight(token);
        intersection += w * count_a.min(count_b) as f64;
        union += w * count_a.max(count_b) as f64;
    }
    for (&token, &count_b) in b {
        if !a.contains_key(&token) {
            union += weight(token) * count_b as f64;
        }
    }
    // Two empty multisets (e.g. two empty bodies) have no union; treat
    // them as identical rather than dividing by zero.
    if union == 0.0 {
        1.0
    } else {
        intersection / union
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

    fn seq(labels: &[&str]) -> TokenProfile {
        TokenProfile::from_tree(
            &block(labels.iter().map(|l| TreeNode::leaf(*l)).collect()),
            false,
        )
    }

    #[test]
    fn lcs_survives_a_large_inserted_gap() {
        let original = ["Let", "Call", "If", "Assign", "Return"];
        let mut gapped: Vec<&str> = original[..2].to_vec();
        gapped.extend(["Loop", "Loop", "Loop"]);
        gapped.extend(&original[2..]);
        let a = seq(&original);
        let b = seq(&gapped);
        // Block + 5 of the original tokens are all a common subsequence of
        // the 9-token gapped body.
        assert!((lcs_similarity(&a, &b) - 6.0 / 9.0).abs() < 1e-9);
    }

    #[test]
    fn lcs_is_order_aware_where_token_jaccard_is_not() {
        let forward = seq(&["A", "B", "C", "D", "E", "F"]);
        let reversed = seq(&["F", "E", "D", "C", "B", "A"]);
        assert_eq!(lcs_similarity(&forward, &forward), 1.0);
        // Only the Block root and one statement line up in order.
        assert!((lcs_similarity(&forward, &reversed) - 2.0 / 7.0).abs() < 1e-9);
        assert_eq!(
            lcs_similarity_upper_bound(&forward, &reversed),
            1.0,
            "the unigram bound cannot see order"
        );
        let other = seq(&["A", "B", "X", "Y", "Z", "W"]);
        // Block, A and B are shared: 3 of 7.
        assert!((lcs_similarity_upper_bound(&forward, &other) - 3.0 / 7.0).abs() < 1e-12);
    }

    #[test]
    fn idf_weights_tiny_bodies_by_unigram_document_frequency() {
        // Below the k-gram width, so the unigram weights decide the score.
        let a = seq(&["Let"]);
        let b = seq(&["Call"]);
        let bare: Vec<TokenProfile> = (0..3).map(|_| seq(&[])).collect();
        let idf = TokenIdf::from_profiles(bare.iter().chain([&a, &b]));
        // N = 5; Block is in all 5 bodies, Let and Call in one each.
        let weight = |df: f64| (6.0 / (1.0 + df)).ln() + 1.0;
        let expected = weight(5.0) / (weight(5.0) + 2.0 * weight(1.0));
        let got = weighted_token_similarity(&a, &b, &idf);
        assert!((got - expected).abs() < 1e-12, "got {got}, want {expected}");
    }

    #[test]
    fn idf_down_weights_corpus_wide_boilerplate() {
        let boiler = ["Log", "Log", "Log", "Log"];
        let with_boiler = |core: &[&'static str]| {
            let mut labels: Vec<&str> = boiler.to_vec();
            labels.extend(core);
            seq(&labels)
        };
        let a = with_boiler(&["Loop", "If", "Return"]);
        let b = with_boiler(&["Match", "Call", "Assign"]);
        let others: Vec<TokenProfile> = (0..8).map(|_| with_boiler(&["Break"])).collect();
        let idf = TokenIdf::from_profiles(others.iter().chain([&a, &b]));

        let plain = token_similarity(&a, &b);
        let weighted = weighted_token_similarity(&a, &b, &idf);
        assert!(
            weighted < plain,
            "shared boilerplate should count for less: plain={plain}, weighted={weighted}"
        );
        assert!((weighted_token_similarity(&a, &a, &idf) - 1.0).abs() < 1e-9);
    }

    /// Reference LCS by the textbook quadratic table, to check the
    /// bit-parallel version against.
    fn naive_lcs(a: &[u64], b: &[u64]) -> usize {
        let mut table = vec![vec![0usize; b.len() + 1]; a.len() + 1];
        for (i, x) in a.iter().enumerate() {
            for (j, y) in b.iter().enumerate() {
                table[i + 1][j + 1] = if x == y {
                    table[i][j] + 1
                } else {
                    table[i][j + 1].max(table[i + 1][j])
                };
            }
        }
        table[a.len()][b.len()]
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

        /// Sequences long enough to span several 64-bit words, so the
        /// carry between words is exercised.
        #[test]
        fn bit_parallel_lcs_matches_the_quadratic_table(
            a in vec(0u64..4, 0..200),
            b in vec(0u64..4, 0..200),
        ) {
            prop_assert_eq!(lcs_length(&a, &b), naive_lcs(&a, &b));
        }

        #[test]
        fn lcs_similarity_is_bounded_symmetric_and_below_its_upper_bound(
            a in arb_tree(),
            b in arb_tree(),
        ) {
            let profile_a = TokenProfile::from_tree(&a, false);
            let profile_b = TokenProfile::from_tree(&b, false);
            let ab = lcs_similarity(&profile_a, &profile_b);
            prop_assert!((0.0..=1.0).contains(&ab), "out of range: {ab}");
            prop_assert!((ab - lcs_similarity(&profile_b, &profile_a)).abs() < 1e-9);
            prop_assert!(ab <= lcs_similarity_upper_bound(&profile_a, &profile_b) + 1e-9);
            prop_assert!((lcs_similarity(&profile_a, &profile_a) - 1.0).abs() < 1e-9);
        }

        #[test]
        fn idf_weighted_similarity_is_reflexive_symmetric_and_bounded(
            a in arb_tree(),
            b in arb_tree(),
            others in vec(arb_tree(), 0..4),
        ) {
            let profile_a = TokenProfile::from_tree(&a, false);
            let profile_b = TokenProfile::from_tree(&b, false);
            let rest: Vec<_> = others.iter().map(|t| TokenProfile::from_tree(t, false)).collect();
            let idf = TokenIdf::from_profiles(rest.iter().chain([&profile_a, &profile_b]));
            let ab = weighted_token_similarity(&profile_a, &profile_b, &idf);
            let ba = weighted_token_similarity(&profile_b, &profile_a, &idf);
            prop_assert!((0.0..=1.0 + 1e-12).contains(&ab), "out of range: {ab}");
            prop_assert!((ab - ba).abs() < 1e-9, "asymmetric: {ab} vs {ba}");
            prop_assert!(
                (weighted_token_similarity(&profile_a, &profile_a, &idf) - 1.0).abs() < 1e-9,
            );
        }
    }
}
