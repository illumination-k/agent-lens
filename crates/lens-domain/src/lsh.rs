//! Locality-Sensitive Hashing for similarity candidate generation.
//!
//! Pre-filter for the all-pairs similarity pipeline. The full cartesian
//! product is fine for small corpora, but the dominant cost is the per-pair
//! TSED computation (O(N² × tree size)). Once N grows past a couple hundred
//! functions, MinHash signatures + banded LSH cut down the set of pairs that
//! TSED actually has to score, replacing the quadratic step with one that is
//! linear in the number of *similar* pairs.
//!
//! Approach: AST preorder label k-shingles → MinHash signature → split the
//! signature into B bands of R rows. Pairs whose signatures agree on at
//! least one band become candidates. Recall is tuned high (a true
//! near-duplicate pair appears in the output with very high probability);
//! the false positives are absorbed by the TSED scoring step downstream.

use std::collections::HashMap;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};

use crate::function::FunctionDef;
use crate::tree::TreeNode;

/// Configuration for [`lsh_candidate_pairs`].
///
/// Defaults are tuned for Jaccard threshold ≈ 0.85 (the same cutoff the
/// PostToolUse `similarity` hook uses): recall ≈ 0.99, false positive rate
/// low enough that TSED verification stays the cheap path.
#[derive(Debug, Clone)]
pub struct LshOptions {
    /// Window size for AST preorder label n-grams. The standard
    /// "type-3 clone" value is 3; smaller windows increase recall at the
    /// cost of lots of low-similarity false positives.
    pub shingle_size: usize,
    /// Number of MinHash hash functions per signature. Must be `>= num_bands`
    /// and divisible by it.
    pub num_hashes: usize,
    /// Bands per signature. With `R = num_hashes / num_bands` rows per band,
    /// candidate-recall at Jaccard `s` is `1 - (1 - s^R)^num_bands` — more
    /// bands lifts recall, fewer bands tightens precision.
    pub num_bands: usize,
    /// Seed for the deterministic MinHash family. Same seed → same
    /// candidates across runs.
    pub seed: u64,
}

impl Default for LshOptions {
    fn default() -> Self {
        // (K=128, B=32, R=4): at Jaccard 0.85 the recall ≈ 1 - (1 - 0.522)^32
        // which rounds to "always", and at Jaccard 0.5 the candidate rate is
        // low enough that the TSED step doesn't see most of the noise.
        Self {
            shingle_size: 3,
            num_hashes: 128,
            num_bands: 32,
            seed: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

/// Generate candidate `(i, j)` pairs (`i < j`) from `functions` using
/// MinHash + banded LSH. Output is sorted for determinism.
///
/// Candidates are an upper bound on the truly-similar pairs: every
/// near-duplicate above the analyzer's similarity threshold is virtually
/// guaranteed to be in the output, but the output also contains weakly-
/// similar pairs that the downstream TSED filter will drop.
pub fn lsh_candidate_pairs(functions: &[FunctionDef], opts: &LshOptions) -> Vec<(usize, usize)> {
    let trees: Vec<&TreeNode> = functions.iter().map(|function| &function.tree).collect();
    lsh_candidate_pairs_for_trees(&trees, opts)
}

/// Generate candidate pairs for an already-borrowed tree corpus.
///
/// This avoids cloning full [`FunctionDef`] values in callers that already
/// keep their own index mapping, such as directory-level analyzers.
pub fn lsh_candidate_pairs_for_trees(
    trees: &[&TreeNode],
    opts: &LshOptions,
) -> Vec<(usize, usize)> {
    if trees.len() < 2 || opts.num_hashes == 0 || opts.num_bands == 0 {
        return Vec::new();
    }
    let rows_per_band = opts.num_hashes / opts.num_bands;
    if rows_per_band == 0 {
        return Vec::new();
    }
    let family = HashFamily::new(opts.num_hashes, opts.seed);
    let signatures: Vec<Vec<u64>> = trees
        .iter()
        .map(|tree| {
            let features = extract_shingles(tree, opts.shingle_size);
            minhash_signature(&features, &family)
        })
        .collect();

    let mut buckets: HashMap<(usize, u64), Vec<usize>> = HashMap::new();
    for (idx, sig) in signatures.iter().enumerate() {
        for b in 0..opts.num_bands {
            let start = b * rows_per_band;
            let end = start + rows_per_band;
            let Some(band) = sig.get(start..end) else {
                continue;
            };
            let key = hash_band(b, band);
            buckets.entry((b, key)).or_default().push(idx);
        }
    }

    let mut candidates: HashSet<(usize, usize)> = HashSet::new();
    for indices in buckets.into_values() {
        if indices.len() < 2 {
            continue;
        }
        for (pos, &i) in indices.iter().enumerate() {
            for &j in &indices[pos + 1..] {
                let key = if i < j { (i, j) } else { (j, i) };
                candidates.insert(key);
            }
        }
    }

    let mut out: Vec<(usize, usize)> = candidates.into_iter().collect();
    out.sort();
    out
}

/// K-shingle feature set: every k-window over preorder AST labels, hashed
/// to u64. For trees smaller than `k`, fall back to a single feature
/// covering the full label sequence so even tiny functions still get a
/// stable signature.
fn extract_shingles(tree: &TreeNode, k: usize) -> HashSet<u64> {
    let mut labels: Vec<&str> = Vec::new();
    collect_labels(tree, &mut labels);
    let mut shingles = HashSet::new();
    if labels.is_empty() || k == 0 {
        return shingles;
    }
    if labels.len() < k {
        shingles.insert(hash_one(&labels));
        return shingles;
    }
    for window in labels.windows(k) {
        shingles.insert(hash_one(&window));
    }
    shingles
}

fn collect_labels<'a>(tree: &'a TreeNode, out: &mut Vec<&'a str>) {
    out.push(tree.label.as_str());
    for child in &tree.children {
        collect_labels(child, out);
    }
}

fn hash_one<T: Hash>(value: &T) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn hash_band(band_index: usize, band: &[u64]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    band_index.hash(&mut hasher);
    band.hash(&mut hasher);
    hasher.finish()
}

/// Deterministic K-element MinHash family. Each `h_i(x) = a_i * x + b_i`
/// is computed mod 2^64 (wrapping arithmetic). The `(a_i, b_i)` pairs
/// come from a single seed via an LCG so repeated runs see the same
/// candidates.
struct HashFamily {
    a: Vec<u64>,
    b: Vec<u64>,
}

impl HashFamily {
    fn new(k: usize, seed: u64) -> Self {
        let mut state = seed;
        let mut a = Vec::with_capacity(k);
        let mut b = Vec::with_capacity(k);
        for _ in 0..k {
            state = next_lcg(state);
            // Force `a` odd so multiplication stays a bijection on u64.
            a.push(state | 1);
            state = next_lcg(state);
            b.push(state);
        }
        Self { a, b }
    }

    fn hash_at(&self, idx: usize, x: u64) -> u64 {
        let a = self.a.get(idx).copied().unwrap_or(1);
        let b = self.b.get(idx).copied().unwrap_or(0);
        a.wrapping_mul(x).wrapping_add(b)
    }

    fn len(&self) -> usize {
        self.a.len()
    }
}

fn next_lcg(s: u64) -> u64 {
    // Knuth LCG constants (multiplier from MMIX, increment from
    // Numerical Recipes). Cheap, statistically adequate for seeding
    // a hash family.
    s.wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407)
}

fn minhash_signature(features: &HashSet<u64>, family: &HashFamily) -> Vec<u64> {
    let mut sig = vec![u64::MAX; family.len()];
    for &f in features {
        for (i, slot) in sig.iter_mut().enumerate() {
            let h = family.hash_at(i, f);
            if h < *slot {
                *slot = h;
            }
        }
    }
    sig
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::collection::vec;
    use proptest::prelude::*;

    fn def(label_kinds: &[&str]) -> FunctionDef {
        let children = label_kinds
            .iter()
            .map(|k| TreeNode::leaf(*k))
            .collect::<Vec<_>>();
        FunctionDef {
            name: "f".into(),
            start_line: 1,
            end_line: 10,
            tree: TreeNode::with_children("Block", "", children),
        }
    }

    #[test]
    fn empty_corpus_yields_no_candidates() {
        assert!(lsh_candidate_pairs(&[], &LshOptions::default()).is_empty());
    }

    #[test]
    fn single_function_yields_no_candidates() {
        let funcs = vec![def(&["Let", "Call", "Return"])];
        assert!(lsh_candidate_pairs(&funcs, &LshOptions::default()).is_empty());
    }

    #[test]
    fn identical_functions_are_paired() {
        // The signatures are identical, so every band hashes to the same
        // bucket; the pair must surface as a candidate regardless of
        // tuning.
        let funcs = vec![
            def(&["Let", "Call", "Return"]),
            def(&["Let", "Call", "Return"]),
        ];
        let pairs = lsh_candidate_pairs(&funcs, &LshOptions::default());
        assert_eq!(pairs, vec![(0, 1)]);
    }

    #[test]
    fn highly_similar_functions_are_paired_under_default_recall() {
        // The two trees share most of their preorder shingles. With the
        // default (K=128, B=32, R=4) tuning, recall should comfortably
        // pick this pair up.
        let funcs = vec![
            def(&["Let", "Call", "Add", "Mul", "Return", "Block"]),
            def(&["Let", "Call", "Add", "Mul", "Return", "If"]),
        ];
        let pairs = lsh_candidate_pairs(&funcs, &LshOptions::default());
        assert!(pairs.contains(&(0, 1)));
    }

    #[test]
    fn deterministic_across_runs() {
        let funcs = vec![
            def(&["Let", "Call", "Add", "Return"]),
            def(&["Let", "Call", "Add", "Return"]),
            def(&["If", "While", "Match"]),
        ];
        let opts = LshOptions::default();
        let first = lsh_candidate_pairs(&funcs, &opts);
        let second = lsh_candidate_pairs(&funcs, &opts);
        assert_eq!(first, second);
    }

    #[test]
    fn borrowed_tree_entrypoint_matches_function_entrypoint() {
        let funcs = vec![
            def(&["Let", "Call", "Add", "Return"]),
            def(&["Let", "Call", "Add", "Return"]),
            def(&["If", "While", "Match"]),
        ];
        let trees: Vec<&TreeNode> = funcs.iter().map(|f| &f.tree).collect();
        let opts = LshOptions::default();
        assert_eq!(
            lsh_candidate_pairs_for_trees(&trees, &opts),
            lsh_candidate_pairs(&funcs, &opts),
        );
    }

    #[test]
    fn output_pairs_are_sorted_and_have_i_lt_j() {
        let funcs = vec![
            def(&["A", "B", "C"]),
            def(&["A", "B", "C"]),
            def(&["A", "B", "C"]),
        ];
        let pairs = lsh_candidate_pairs(&funcs, &LshOptions::default());
        for (i, j) in &pairs {
            assert!(i < j, "expected i < j, got ({i}, {j})");
        }
        let mut sorted = pairs.clone();
        sorted.sort();
        assert_eq!(pairs, sorted, "output must be sorted");
    }

    #[test]
    fn small_trees_fall_back_to_single_shingle() {
        // Trees too small for k=3 windows still need to produce a stable
        // feature so identical small functions stay matchable.
        let funcs = vec![
            FunctionDef {
                name: "a".into(),
                start_line: 1,
                end_line: 1,
                tree: TreeNode::leaf("Lit"),
            },
            FunctionDef {
                name: "b".into(),
                start_line: 1,
                end_line: 1,
                tree: TreeNode::leaf("Lit"),
            },
        ];
        let pairs = lsh_candidate_pairs(&funcs, &LshOptions::default());
        assert_eq!(pairs, vec![(0, 1)]);
    }

    #[test]
    fn zero_band_or_hash_count_returns_no_candidates() {
        let funcs = vec![def(&["A", "B", "C"]), def(&["A", "B", "C"])];
        let opts = LshOptions {
            num_hashes: 0,
            ..Default::default()
        };
        assert!(lsh_candidate_pairs(&funcs, &opts).is_empty());
        let opts = LshOptions {
            num_bands: 0,
            ..Default::default()
        };
        assert!(lsh_candidate_pairs(&funcs, &opts).is_empty());
    }

    #[test]
    fn rows_per_band_zero_returns_no_candidates() {
        // num_bands > num_hashes ⇒ rows_per_band == 0 ⇒ defensively bail.
        let funcs = vec![def(&["A", "B", "C"]), def(&["A", "B", "C"])];
        let opts = LshOptions {
            num_hashes: 4,
            num_bands: 8,
            ..Default::default()
        };
        assert!(lsh_candidate_pairs(&funcs, &opts).is_empty());
    }

    fn generated_def(name: impl Into<String>, label_kinds: &[u8]) -> FunctionDef {
        let children = label_kinds
            .iter()
            .map(|kind| TreeNode::leaf(format!("K{kind}")))
            .collect::<Vec<_>>();
        FunctionDef {
            name: name.into(),
            start_line: 1,
            end_line: label_kinds.len().max(1),
            tree: TreeNode::with_children("Block", "", children),
        }
    }

    fn analyzer_lsh_options() -> LshOptions {
        LshOptions {
            num_bands: 24,
            ..Default::default()
        }
    }

    proptest! {
        #[test]
        fn identical_generated_functions_are_always_candidates(
            labels in vec(0_u8..16, 0..48),
            prefix in vec(vec(0_u8..16, 0..24), 0..8),
            suffix in vec(vec(0_u8..16, 0..24), 0..8),
        ) {
            let clone_a = prefix.len();
            let clone_b = prefix.len() + 1;
            let funcs = prefix
                .iter()
                .enumerate()
                .map(|(idx, labels)| generated_def(format!("prefix_{idx}"), labels))
                .chain([
                    generated_def("clone_a", &labels),
                    generated_def("clone_b", &labels),
                ])
                .chain(
                    suffix
                        .iter()
                        .enumerate()
                        .map(|(idx, labels)| generated_def(format!("suffix_{idx}"), labels)),
                )
                .collect::<Vec<_>>();

            for opts in [LshOptions::default(), analyzer_lsh_options()] {
                let pairs = lsh_candidate_pairs(&funcs, &opts);
                prop_assert!(
                    pairs.contains(&(clone_a, clone_b)),
                    "identical pair missing with opts {opts:?}; labels={labels:?}, pairs={pairs:?}",
                );
            }
        }

        #[test]
        fn one_label_mutation_in_long_generated_functions_stays_candidate(
            mut labels in vec(0_u8..16, 24..64),
            mutation_index in 0_usize..64,
            replacement in 0_u8..16,
        ) {
            let idx = mutation_index % labels.len();
            let original = labels.clone();
            labels[idx] = if replacement == original[idx] {
                replacement.wrapping_add(1) % 16
            } else {
                replacement
            };
            let funcs = vec![
                generated_def("original", &original),
                generated_def("mutated", &labels),
            ];

            for opts in [LshOptions::default(), analyzer_lsh_options()] {
                let pairs = lsh_candidate_pairs(&funcs, &opts);
                prop_assert!(
                    pairs.contains(&(0, 1)),
                    "near-clone pair missing with opts {opts:?}; original={original:?}, mutated={labels:?}",
                );
            }
        }

        #[test]
        fn generated_candidate_output_is_sorted_unique_and_in_bounds(
            corpus in vec(vec(0_u8..12, 0..32), 0..24),
        ) {
            let funcs = corpus
                .iter()
                .enumerate()
                .map(|(idx, labels)| generated_def(format!("f_{idx}"), labels))
                .collect::<Vec<_>>();
            let pairs = lsh_candidate_pairs(&funcs, &analyzer_lsh_options());

            prop_assert!(pairs.windows(2).all(|w| w[0] < w[1]), "pairs not sorted/unique: {pairs:?}");
            for (i, j) in pairs {
                prop_assert!(i < j, "expected i < j, got ({i}, {j})");
                prop_assert!(j < funcs.len(), "candidate index out of bounds: ({i}, {j}) for len {}", funcs.len());
            }
        }
    }
}
