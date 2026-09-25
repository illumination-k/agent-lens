use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use lens_domain::{
    CandidateStrategy, TSEDOptions, collect_subtree_sizes, lsh_candidate_pairs_for_trees,
};
use rayon::prelude::*;

use super::{OwnedUnit, SimilarityMethod};

#[derive(Debug)]
pub(super) struct TreeProfile {
    pub size: usize,
    subtree_sizes: OnceLock<lens_domain::SubtreeSizes>,
    filters: Option<TreeFilterProfile>,
    exact_hash_ignoring_values: u64,
    exact_hash_with_values: u64,
}

#[derive(Debug)]
struct TreeFilterProfile {
    label_counts: HashMap<u64, usize>,
    /// Multiset of `(label, value)` pairs. The label-only multiset is
    /// blind to what a node names; under `compare_values` a node only
    /// matches for free when both agree, so this is the tighter bound.
    label_value_counts: HashMap<u64, usize>,
    preorder_shingles: HashMap<u64, usize>,
    child_sizes: Vec<usize>,
    root_arity: usize,
}

impl TreeProfile {
    pub(super) fn from_tree(tree: &lens_domain::TreeNode) -> Self {
        let subtree_sizes = collect_subtree_sizes(tree);
        let size = subtree_sizes
            .get(&(std::ptr::from_ref::<lens_domain::TreeNode>(tree) as usize))
            .copied()
            .unwrap_or(0);
        let filters = TreeFilterProfile::from_tree(tree, size, &subtree_sizes);
        let exact_hashes = structural_hashes(tree);
        Self {
            size,
            subtree_sizes: initialized_once_lock(subtree_sizes),
            filters: Some(filters),
            exact_hash_ignoring_values: exact_hashes.ignoring_values,
            exact_hash_with_values: exact_hashes.with_values,
        }
    }

    pub(super) fn from_tree_for_scoring(tree: &lens_domain::TreeNode) -> Self {
        let exact_hashes = structural_hashes(tree);
        Self {
            size: tree.subtree_size(),
            subtree_sizes: OnceLock::new(),
            filters: None,
            exact_hash_ignoring_values: exact_hashes.ignoring_values,
            exact_hash_with_values: exact_hashes.with_values,
        }
    }

    pub(super) fn subtree_sizes<'a>(
        &'a self,
        tree: &lens_domain::TreeNode,
    ) -> &'a lens_domain::SubtreeSizes {
        self.subtree_sizes
            .get_or_init(|| collect_subtree_sizes(tree))
    }

    /// Whether this profile carries the cheap-filter data (`from_tree`)
    /// rather than the scoring-only shape (`from_tree_for_scoring`).
    /// Test-only: lets `build_tree_profiles` callers pin which shape the
    /// LSH gate selected.
    #[cfg(test)]
    pub(super) fn has_filters(&self) -> bool {
        self.filters.is_some()
    }

    pub(super) fn exact_hash(&self, compare_values: bool) -> u64 {
        if compare_values {
            self.exact_hash_with_values
        } else {
            self.exact_hash_ignoring_values
        }
    }
}

fn initialized_once_lock<T>(value: T) -> OnceLock<T> {
    let lock = OnceLock::new();
    let _ = lock.set(value);
    lock
}

struct StructuralHashes {
    ignoring_values: u64,
    with_values: u64,
}

fn structural_hashes(tree: &lens_domain::TreeNode) -> StructuralHashes {
    let mut ignoring_values = std::collections::hash_map::DefaultHasher::new();
    let mut with_values = std::collections::hash_map::DefaultHasher::new();
    hash_tree_into(tree, &mut ignoring_values, &mut with_values);
    StructuralHashes {
        ignoring_values: ignoring_values.finish(),
        with_values: with_values.finish(),
    }
}

fn hash_tree_into(
    tree: &lens_domain::TreeNode,
    ignoring_values: &mut std::collections::hash_map::DefaultHasher,
    with_values: &mut std::collections::hash_map::DefaultHasher,
) {
    tree.label.hash(ignoring_values);
    tree.label.hash(with_values);
    tree.value.hash(with_values);
    tree.children.len().hash(ignoring_values);
    tree.children.len().hash(with_values);
    for child in &tree.children {
        hash_tree_into(child, ignoring_values, with_values);
    }
}

impl TreeFilterProfile {
    fn from_tree(
        tree: &lens_domain::TreeNode,
        size: usize,
        subtree_sizes: &lens_domain::SubtreeSizes,
    ) -> Self {
        let mut labels = Vec::with_capacity(size);
        collect_preorder_label_hashes(tree, &mut labels);
        let mut label_counts = HashMap::new();
        for &label in &labels {
            *label_counts.entry(label).or_insert(0) += 1;
        }
        let preorder_shingles = shingle_counts(&labels, PREORDER_SHINGLE_WIDTH);
        let mut label_value_counts = HashMap::new();
        count_label_value_pairs(tree, &mut label_value_counts);
        let child_sizes = tree
            .children
            .iter()
            .filter_map(|child| {
                subtree_sizes
                    .get(&(std::ptr::from_ref::<lens_domain::TreeNode>(child) as usize))
                    .copied()
            })
            .collect();
        Self {
            label_counts,
            label_value_counts,
            preorder_shingles,
            child_sizes,
            root_arity: tree.children.len(),
        }
    }
}

const PREORDER_SHINGLE_WIDTH: usize = 3;

fn collect_preorder_label_hashes(tree: &lens_domain::TreeNode, out: &mut Vec<u64>) {
    out.push(label_fingerprint(&tree.label));
    for child in &tree.children {
        collect_preorder_label_hashes(child, out);
    }
}

fn count_label_value_pairs(tree: &lens_domain::TreeNode, out: &mut HashMap<u64, usize>) {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tree.label.hash(&mut hasher);
    tree.value.hash(&mut hasher);
    *out.entry(hasher.finish()).or_insert(0) += 1;
    for child in &tree.children {
        count_label_value_pairs(child, out);
    }
}

fn label_fingerprint(label: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    label.hash(&mut hasher);
    hasher.finish()
}

fn shingle_counts(labels: &[u64], width: usize) -> HashMap<u64, usize> {
    if width == 0 || labels.len() < width {
        return HashMap::new();
    }
    let mut counts = HashMap::new();
    for window in labels.windows(width) {
        *counts.entry(shingle_fingerprint(window)).or_insert(0) += 1;
    }
    counts
}

fn shingle_fingerprint(window: &[u64]) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for &label in window {
        hash ^= label;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[derive(Debug)]
pub(super) struct CandidatePairs {
    pub pairs: Vec<(usize, usize)>,
    pub eligible_function_count: usize,
    pub size_filtered_count: usize,
    pub label_filtered_count: usize,
    /// Pairs dropped by the `(label, value)` multiset bound. Only ever
    /// non-zero when values are compared (`--target blocks`).
    pub value_filtered_count: usize,
    pub arity_filtered_count: usize,
    pub shingle_filtered_count: usize,
    /// Pairs dropped because the two units cover overlapping source
    /// lines. Only ever non-zero for `--target blocks`; see
    /// [`CandidatePairs::drop_overlapping`].
    pub overlap_filtered_count: usize,
    pub strategy: CandidatePairStrategy,
}

impl CandidatePairs {
    pub(super) fn total_len(&self) -> usize {
        self.pairs.len()
            + self.size_filtered_count
            + self.label_filtered_count
            + self.value_filtered_count
            + self.arity_filtered_count
            + self.shingle_filtered_count
            + self.overlap_filtered_count
    }

    /// Drop pairs whose two units cover overlapping lines of the same
    /// file.
    ///
    /// Essential for `--target blocks` and meaningless for the others.
    /// Sliding windows mint one unit per statement run, so the window
    /// starting at statement 3 and the window starting at statement 3
    /// that is one statement longer are near-identical by construction —
    /// and so are all their neighbours. Left in, every function with a
    /// few statements reports itself as a cluster of "duplicates" and
    /// real cross-function repetition is buried. Overlapping windows are
    /// one piece of code compared against itself, never duplication.
    pub(super) fn drop_overlapping(&mut self, corpus: &[OwnedUnit]) {
        let before = self.pairs.len();
        self.pairs.retain(|&(i, j)| {
            corpus
                .get(i)
                .zip(corpus.get(j))
                .is_none_or(|(a, b)| !units_overlap(a, b))
        });
        self.overlap_filtered_count += before - self.pairs.len();
    }
}

/// True when two units cover overlapping lines of the same file.
fn units_overlap(a: &OwnedUnit, b: &OwnedUnit) -> bool {
    a.file == b.file && a.start_line() <= b.end_line() && b.start_line() <= a.end_line()
}

#[derive(Debug, Default)]
struct CheapFilterCounts {
    size: usize,
    label: usize,
    value: usize,
    arity: usize,
    shingle: usize,
}

#[derive(Debug)]
pub(super) enum CandidatePairStrategy {
    Cartesian,
    Lsh,
}

impl CandidatePairStrategy {
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::Cartesian => "cartesian",
            Self::Lsh => "lsh",
        }
    }
}

/// Return every candidate `(i, j)` index pair from `corpus` (i < j) where
/// both functions meet the `min_lines` filter. Large corpora go through the
/// same LSH pre-filter used by `lens-domain`; small corpora keep the exact
/// cartesian path because LSH setup costs more than it saves there.
///
/// The TSED-only cheap filters (size / label / arity / shingle bounds)
/// prune pairs that cannot reach `threshold` under tree-edit distance.
/// They are unsound for the other methods, whose scores are not bounded
/// by those quantities, so those methods skip them and score every
/// enumerated pair (see [`SimilarityMethod::uses_tree_filters`]).
pub(super) fn candidate_pairs(
    corpus: &[OwnedUnit],
    min_lines: usize,
    profiles: &[TreeProfile],
    threshold: f64,
    opts: &TSEDOptions,
    method: SimilarityMethod,
    allow_lsh: bool,
) -> CandidatePairs {
    let eligible_indices: Vec<usize> = corpus
        .iter()
        .enumerate()
        .filter(move |(_, a)| a.line_count() >= min_lines)
        .map(|(i, _)| i)
        .collect();
    let mut strategy = CandidateStrategy::default();
    // Keep directory analysis on a high-recall LSH setting. Property tests
    // cover the exact setting here; tighter banding has missed one-label
    // near-clones that still clear the analyzer threshold.
    strategy.lsh.num_bands = 24;
    let use_lsh = allow_lsh && strategy_uses_lsh(&strategy, eligible_indices.len());
    let (pairs, filter_counts) = if use_lsh {
        let trees: Vec<&lens_domain::TreeNode> = eligible_indices
            .iter()
            .filter_map(|&i| corpus.get(i).map(OwnedUnit::body_tree))
            .collect();
        let lsh_pairs = lsh_candidate_pairs_for_trees(&trees, &strategy.lsh)
            .into_iter()
            .filter_map(|(i, j)| {
                let a = eligible_indices.get(i).copied()?;
                let b = eligible_indices.get(j).copied()?;
                Some((a, b))
            })
            .filter(|(i, j)| same_test_class(corpus, *i, *j));
        if method.uses_tree_filters() {
            filter_tsed_compatible_pairs(lsh_pairs, profiles, threshold, opts)
        } else {
            (lsh_pairs.collect::<Vec<_>>(), CheapFilterCounts::default())
        }
    } else {
        let cartesian = eligible_indices
            .iter()
            .enumerate()
            .flat_map(|(pos, &i)| eligible_indices[pos + 1..].iter().map(move |&j| (i, j)))
            .filter(|(i, j)| same_test_class(corpus, *i, *j));
        if method.uses_tree_filters() {
            filter_tsed_compatible_pairs(cartesian, profiles, threshold, opts)
        } else {
            (cartesian.collect::<Vec<_>>(), CheapFilterCounts::default())
        }
    };
    CandidatePairs {
        pairs,
        eligible_function_count: eligible_indices.len(),
        size_filtered_count: filter_counts.size,
        overlap_filtered_count: 0,
        label_filtered_count: filter_counts.label,
        value_filtered_count: filter_counts.value,
        arity_filtered_count: filter_counts.arity,
        shingle_filtered_count: filter_counts.shingle,
        strategy: if use_lsh {
            CandidatePairStrategy::Lsh
        } else {
            CandidatePairStrategy::Cartesian
        },
    }
}

fn same_test_class(corpus: &[OwnedUnit], i: usize, j: usize) -> bool {
    match (corpus.get(i), corpus.get(j)) {
        (Some(a), Some(b)) => a.is_test == b.is_test,
        _ => false,
    }
}

pub(super) fn eligible_function_count(corpus: &[OwnedUnit], min_lines: usize) -> usize {
    corpus
        .iter()
        .filter(|function| function.line_count() >= min_lines)
        .count()
}

pub(super) fn similarity_uses_lsh(eligible_count: usize) -> bool {
    strategy_uses_lsh(&CandidateStrategy::default(), eligible_count)
}

fn strategy_uses_lsh(strategy: &CandidateStrategy, eligible_count: usize) -> bool {
    strategy
        .lsh_min_functions
        .is_some_and(|min_n| eligible_count >= min_n)
}

fn filter_tsed_compatible_pairs(
    pairs: impl IntoIterator<Item = (usize, usize)>,
    profiles: &[TreeProfile],
    threshold: f64,
    opts: &TSEDOptions,
) -> (Vec<(usize, usize)>, CheapFilterCounts) {
    let pairs: Vec<_> = pairs.into_iter().collect();
    let verdicts: Vec<_> = pairs
        .par_iter()
        .map(|&(i, j)| tsed_upper_bound_filter(profiles, i, j, threshold, opts))
        .collect();
    let mut out = Vec::new();
    let mut counts = CheapFilterCounts::default();
    for (pair, verdict) in pairs.into_iter().zip(verdicts) {
        match verdict {
            Some(CheapFilter::Size) => counts.size += 1,
            Some(CheapFilter::LabelMultiset) => counts.label += 1,
            Some(CheapFilter::LabelValueMultiset) => counts.value += 1,
            Some(CheapFilter::RootChildArity) => counts.arity += 1,
            Some(CheapFilter::PreorderShingle) => counts.shingle += 1,
            None => out.push(pair),
        }
    }
    (out, counts)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CheapFilter {
    Size,
    LabelMultiset,
    LabelValueMultiset,
    RootChildArity,
    PreorderShingle,
}

pub(super) fn tsed_upper_bound_filter(
    profiles: &[TreeProfile],
    i: usize,
    j: usize,
    threshold: f64,
    opts: &TSEDOptions,
) -> Option<CheapFilter> {
    let profile_a = profiles.get(i)?;
    let profile_b = profiles.get(j)?;
    if tsed_upper_bound(profile_a, profile_b, 0.0, opts.size_penalty) < threshold {
        return Some(CheapFilter::Size);
    }
    let label_distance = label_multiset_distance_lower_bound(profile_a, profile_b, opts, false);
    if tsed_upper_bound(profile_a, profile_b, label_distance, opts.size_penalty) < threshold {
        return Some(CheapFilter::LabelMultiset);
    }
    if opts.apted.compare_values {
        let value_distance = label_multiset_distance_lower_bound(profile_a, profile_b, opts, true);
        if tsed_upper_bound(profile_a, profile_b, value_distance, opts.size_penalty) < threshold {
            return Some(CheapFilter::LabelValueMultiset);
        }
    }
    let arity_distance = root_child_arity_distance_lower_bound(profile_a, profile_b, opts);
    if tsed_upper_bound(profile_a, profile_b, arity_distance, opts.size_penalty) < threshold {
        return Some(CheapFilter::RootChildArity);
    }
    let shingle_distance = preorder_shingle_distance_lower_bound(profile_a, profile_b, opts);
    if tsed_upper_bound(profile_a, profile_b, shingle_distance, opts.size_penalty) < threshold {
        return Some(CheapFilter::PreorderShingle);
    }
    None
}

fn tsed_upper_bound(
    a: &TreeProfile,
    b: &TreeProfile,
    distance_lower_bound: f64,
    size_penalty: bool,
) -> f64 {
    let max_size = a.size.max(b.size);
    if max_size == 0 {
        return 1.0;
    }
    let base = 1.0 - distance_lower_bound / max_size as f64;
    let penalty = if size_penalty {
        let min_size = a.size.min(b.size) as f64;
        (min_size / max_size as f64).sqrt()
    } else {
        1.0
    };
    (base * penalty).clamp(0.0, 1.0)
}

/// Lower bound on the edit distance from the multiset L1 over node
/// labels, or over `(label, value)` pairs when `with_values` is set.
/// The value-aware form is sound only when the distance charges a value
/// mismatch (`compare_values`): then a node pair costs nothing exactly
/// when both label and value agree.
fn label_multiset_distance_lower_bound(
    a: &TreeProfile,
    b: &TreeProfile,
    opts: &TSEDOptions,
    with_values: bool,
) -> f64 {
    let Some(filter_a) = &a.filters else {
        return 0.0;
    };
    let Some(filter_b) = &b.filters else {
        return 0.0;
    };
    let l1 = if with_values {
        multiset_l1(&filter_a.label_value_counts, &filter_b.label_value_counts)
    } else {
        multiset_l1(&filter_a.label_counts, &filter_b.label_counts)
    };
    // A rename can fix at most one missing and one extra entry. Insert and
    // delete each change one multiset slot; use the cheapest per-slot cost.
    let per_delta_cost = opts
        .apted
        .delete_cost
        .min(opts.apted.insert_cost)
        .min(opts.apted.rename_cost / 2.0);
    l1 as f64 * per_delta_cost
}

fn root_child_arity_distance_lower_bound(
    a: &TreeProfile,
    b: &TreeProfile,
    opts: &TSEDOptions,
) -> f64 {
    let Some(filter_a) = &a.filters else {
        return 0.0;
    };
    let Some(filter_b) = &b.filters else {
        return 0.0;
    };
    if filter_a.root_arity == filter_b.root_arity {
        return 0.0;
    }
    let (extra, edit_side, unit_cost) = if filter_a.root_arity > filter_b.root_arity {
        (
            filter_a.root_arity - filter_b.root_arity,
            &filter_a.child_sizes,
            opts.apted.delete_cost,
        )
    } else {
        (
            filter_b.root_arity - filter_a.root_arity,
            &filter_b.child_sizes,
            opts.apted.insert_cost,
        )
    };
    let mut sizes = edit_side.clone();
    sizes.sort_unstable();
    sizes.into_iter().take(extra).sum::<usize>() as f64 * unit_cost
}

fn preorder_shingle_distance_lower_bound(
    a: &TreeProfile,
    b: &TreeProfile,
    opts: &TSEDOptions,
) -> f64 {
    let Some(filter_a) = &a.filters else {
        return 0.0;
    };
    let Some(filter_b) = &b.filters else {
        return 0.0;
    };
    let l1 = multiset_l1(&filter_a.preorder_shingles, &filter_b.preorder_shingles);
    if l1 == 0 {
        return 0.0;
    }
    let max_changed_shingles_per_edit = 2 * PREORDER_SHINGLE_WIDTH;
    let unit_cost = opts
        .apted
        .rename_cost
        .min(opts.apted.insert_cost)
        .min(opts.apted.delete_cost);
    l1 as f64 * unit_cost / max_changed_shingles_per_edit as f64
}

fn multiset_l1<K>(a: &HashMap<K, usize>, b: &HashMap<K, usize>) -> usize
where
    K: std::hash::Hash + Eq,
{
    let mut total = 0usize;
    for (key, count_a) in a {
        let count_b = b.get(key).copied().unwrap_or(0);
        total += count_a.abs_diff(count_b);
    }
    for (key, count_b) in b {
        if !a.contains_key(key) {
            total += count_b;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned_function(name: &str, is_test: bool) -> OwnedUnit {
        OwnedUnit {
            file: std::path::PathBuf::from("lib.rs"),
            rel_path: "lib.rs".to_owned(),
            is_test,
            kind: None,
            implements: None,
            lang: crate::analyze::SourceLang::Rust,
            shape: lens_domain::FunctionShape::from(lens_domain::FunctionDef {
                name: name.to_owned(),
                start_line: 1,
                end_line: 5,
                is_test,
                signature: None,
                doc: None,
                implements: None,
                tree: lens_domain::TreeNode::with_children(
                    "Block",
                    "",
                    vec![
                        lens_domain::TreeNode::leaf("Let"),
                        lens_domain::TreeNode::leaf("Return"),
                    ],
                ),
            }),
        }
    }

    fn window(file: &str, start_line: usize, end_line: usize) -> OwnedUnit {
        OwnedUnit {
            file: std::path::PathBuf::from(file),
            rel_path: file.to_owned(),
            is_test: false,
            kind: None,
            implements: None,
            lang: crate::analyze::SourceLang::Rust,
            shape: lens_domain::FunctionShape::from(lens_domain::FunctionDef {
                name: "f".to_owned(),
                start_line,
                end_line,
                is_test: false,
                signature: None,
                doc: None,
                implements: None,
                tree: lens_domain::TreeNode::leaf("Block"),
            }),
        }
    }

    /// Windows sharing so much as one line are the same code seen
    /// twice. Only pairs in different files, or disjoint in the same
    /// file, survive.
    #[test]
    fn drop_overlapping_keeps_only_disjoint_or_cross_file_pairs() {
        let corpus = vec![
            window("a.rs", 10, 14), // 0
            window("a.rs", 11, 13), // 1: nested inside 0
            window("a.rs", 14, 18), // 2: shares line 14 with 0
            window("a.rs", 20, 24), // 3: disjoint from 0
            window("b.rs", 11, 13), // 4: same lines, different file
        ];
        let mut candidates = CandidatePairs {
            pairs: vec![(0, 1), (0, 2), (0, 3), (0, 4), (1, 4)],
            eligible_function_count: corpus.len(),
            size_filtered_count: 0,
            label_filtered_count: 0,
            value_filtered_count: 0,
            arity_filtered_count: 0,
            shingle_filtered_count: 0,
            overlap_filtered_count: 0,
            strategy: CandidatePairStrategy::Cartesian,
        };

        candidates.drop_overlapping(&corpus);

        assert_eq!(candidates.pairs, vec![(0, 3), (0, 4), (1, 4)]);
        assert_eq!(candidates.overlap_filtered_count, 2);
        // Dropped pairs stay in the enumerated total so the profiling
        // counters still add up.
        assert_eq!(candidates.total_len(), 5);
    }

    #[test]
    fn cartesian_candidates_never_include_self_pairs() {
        let corpus = vec![
            owned_function("a", false),
            owned_function("b", false),
            owned_function("c", false),
        ];
        let profiles: Vec<_> = corpus
            .iter()
            .map(|f| TreeProfile::from_tree(f.body_tree()))
            .collect();
        let candidates = candidate_pairs(
            &corpus,
            1,
            &profiles,
            0.0,
            &lens_domain::TSEDOptions::default(),
            SimilarityMethod::Tsed,
            true,
        );

        assert_eq!(candidates.pairs, vec![(0, 1), (0, 2), (1, 2)]);
    }

    fn owned_function_with_tree(name: &str, tree: lens_domain::TreeNode) -> OwnedUnit {
        OwnedUnit {
            file: std::path::PathBuf::from("lib.rs"),
            rel_path: "lib.rs".to_owned(),
            is_test: false,
            kind: None,
            implements: None,
            lang: crate::analyze::SourceLang::Rust,
            shape: lens_domain::FunctionShape::from(lens_domain::FunctionDef {
                name: name.to_owned(),
                start_line: 1,
                end_line: 5,
                is_test: false,
                signature: None,
                doc: None,
                implements: None,
                tree,
            }),
        }
    }

    #[test]
    fn token_method_skips_tsed_cheap_filters() {
        // These two bodies share no labels, so the TSED cheap filters
        // prune the pair outright. The token method must still enumerate
        // it: its score is not bounded by tree-edit distance.
        let corpus = vec![
            owned_function_with_tree(
                "a",
                lens_domain::TreeNode::with_children(
                    "Block",
                    "",
                    vec![lens_domain::TreeNode::leaf("Let")],
                ),
            ),
            owned_function_with_tree(
                "b",
                lens_domain::TreeNode::with_children(
                    "Loop",
                    "",
                    vec![lens_domain::TreeNode::leaf("Call")],
                ),
            ),
        ];
        let profiles: Vec<_> = corpus
            .iter()
            .map(|f| TreeProfile::from_tree(f.body_tree()))
            .collect();
        let opts = lens_domain::TSEDOptions::default();

        let tsed = candidate_pairs(
            &corpus,
            1,
            &profiles,
            0.99,
            &opts,
            SimilarityMethod::Tsed,
            true,
        );
        let token = candidate_pairs(
            &corpus,
            1,
            &profiles,
            0.99,
            &opts,
            SimilarityMethod::Token,
            true,
        );

        assert!(tsed.pairs.is_empty(), "TSED should prune the disjoint pair");
        assert_eq!(token.pairs, vec![(0, 1)]);
        assert_eq!(token.total_len(), 1);
    }

    /// The types target passes `allow_lsh = false`; even a corpus past
    /// the LSH switch-over must stay on the exact cartesian path, since
    /// MinHash recall collapses on small type trees.
    #[test]
    fn allow_lsh_false_forces_cartesian_past_the_lsh_threshold() {
        let corpus: Vec<OwnedUnit> = (0..200)
            .map(|i| owned_function(&format!("t{i}"), false))
            .collect();
        let profiles: Vec<_> = corpus
            .iter()
            .map(|f| TreeProfile::from_tree(f.body_tree()))
            .collect();
        let opts = lens_domain::TSEDOptions::default();

        let gated = candidate_pairs(
            &corpus,
            1,
            &profiles,
            0.0,
            &opts,
            SimilarityMethod::Tsed,
            false,
        );
        let open = candidate_pairs(
            &corpus,
            1,
            &profiles,
            0.0,
            &opts,
            SimilarityMethod::Tsed,
            true,
        );

        assert!(
            matches!(gated.strategy, CandidatePairStrategy::Cartesian),
            "expected cartesian, got {}",
            gated.strategy.as_str()
        );
        assert!(
            matches!(open.strategy, CandidatePairStrategy::Lsh),
            "expected lsh, got {}",
            open.strategy.as_str()
        );
        // All-identical trees: the exact path must enumerate every pair.
        assert_eq!(gated.pairs.len(), 200 * 199 / 2);
    }

    /// Past the LSH switch-over the full cheap filters still run when
    /// the profiles carry them: 200 bodies of one shape whose values are
    /// all distinct land in one MinHash bucket, and under
    /// `compare_values` the `(label, value)` bound must reject every
    /// pair before APTED (#562).
    #[test]
    fn lsh_path_runs_value_aware_filter_on_value_disjoint_bodies() {
        let corpus: Vec<OwnedUnit> = (0..200)
            .map(|i| {
                owned_function_with_tree(
                    &format!("f{i}"),
                    lens_domain::TreeNode::with_children(
                        "Block",
                        "",
                        vec![
                            lens_domain::TreeNode::new("Ident", format!("a{i}")),
                            lens_domain::TreeNode::new("Call", format!("load{i}")),
                            lens_domain::TreeNode::new("Str", format!("key-{i}")),
                        ],
                    ),
                )
            })
            .collect();
        let profiles: Vec<_> = corpus
            .iter()
            .map(|f| TreeProfile::from_tree(f.body_tree()))
            .collect();
        let mut opts = lens_domain::TSEDOptions::default();
        opts.apted.compare_values = true;
        opts.apted.rename_cost = 1.0;

        let candidates = candidate_pairs(
            &corpus,
            1,
            &profiles,
            0.5,
            &opts,
            SimilarityMethod::Tsed,
            true,
        );

        assert!(
            matches!(candidates.strategy, CandidatePairStrategy::Lsh),
            "expected lsh, got {}",
            candidates.strategy.as_str()
        );
        assert!(candidates.pairs.is_empty(), "{:?}", candidates.pairs.len());
        assert_eq!(candidates.value_filtered_count, 200 * 199 / 2);
        assert_eq!(candidates.total_len(), 200 * 199 / 2);
    }

    /// Same labels and root arity, different preorder: only the shingle
    /// bound rejects the pair, and its counter must record it.
    #[test]
    fn shingle_filter_counts_pairs_it_drops() {
        let leaf = lens_domain::TreeNode::leaf;
        let node = |label, children| lens_domain::TreeNode::with_children(label, "", children);
        let corpus = vec![
            owned_function_with_tree(
                "a",
                node("Block", vec![node("A", vec![leaf("B")]), leaf("C")]),
            ),
            owned_function_with_tree(
                "b",
                node("Block", vec![leaf("A"), node("C", vec![leaf("B")])]),
            ),
        ];
        let profiles: Vec<_> = corpus
            .iter()
            .map(|f| TreeProfile::from_tree(f.body_tree()))
            .collect();

        let candidates = candidate_pairs(
            &corpus,
            1,
            &profiles,
            0.99,
            &lens_domain::TSEDOptions::default(),
            SimilarityMethod::Tsed,
            true,
        );

        assert!(candidates.pairs.is_empty());
        assert_eq!(candidates.shingle_filtered_count, 1);
        assert_eq!(candidates.total_len(), 1);
    }

    #[test]
    fn eligible_function_count_uses_min_lines_threshold() {
        let corpus = vec![
            owned_function("short", false),
            owned_function("also_short", false),
            owned_function("long_enough", false),
        ];

        assert_eq!(eligible_function_count(&corpus, 5), 3);
        assert_eq!(eligible_function_count(&corpus, 6), 0);
    }

    #[test]
    fn scoring_profile_initializes_subtree_sizes_lazily() {
        let tree = lens_domain::TreeNode::with_children(
            "Block",
            "",
            vec![
                lens_domain::TreeNode::leaf("Let"),
                lens_domain::TreeNode::with_children(
                    "If",
                    "",
                    vec![lens_domain::TreeNode::leaf("Return")],
                ),
            ],
        );
        let profile = TreeProfile::from_tree_for_scoring(&tree);
        assert!(profile.subtree_sizes.get().is_none());

        let sizes = profile.subtree_sizes(&tree);
        let root_key = std::ptr::from_ref::<lens_domain::TreeNode>(&tree) as usize;
        assert_eq!(sizes.get(&root_key).copied(), Some(tree.subtree_size()));
        assert!(profile.subtree_sizes.get().is_some());
    }

    #[test]
    fn filter_profile_initializes_subtree_sizes_eagerly() {
        let tree = lens_domain::TreeNode::with_children(
            "Block",
            "",
            vec![lens_domain::TreeNode::leaf("Return")],
        );
        let profile = TreeProfile::from_tree(&tree);

        assert!(profile.subtree_sizes.get().is_some());
        assert_eq!(profile.size, tree.subtree_size());
    }

    #[test]
    fn exact_hash_distinguishes_structures_and_compare_value_modes() {
        let left = lens_domain::TreeNode::with_children(
            "Call",
            "",
            vec![lens_domain::TreeNode::new("Ident", "alpha")],
        );
        let renamed_value = lens_domain::TreeNode::with_children(
            "Call",
            "",
            vec![lens_domain::TreeNode::new("Ident", "beta")],
        );
        let renamed_label = lens_domain::TreeNode::with_children(
            "Call",
            "",
            vec![lens_domain::TreeNode::new("Literal", "alpha")],
        );
        let left_profile = TreeProfile::from_tree_for_scoring(&left);
        let renamed_value_profile = TreeProfile::from_tree_for_scoring(&renamed_value);
        let renamed_label_profile = TreeProfile::from_tree_for_scoring(&renamed_label);

        assert_eq!(
            left_profile.exact_hash(false),
            renamed_value_profile.exact_hash(false)
        );
        assert_ne!(
            left_profile.exact_hash(true),
            renamed_value_profile.exact_hash(true)
        );
        assert_ne!(
            left_profile.exact_hash(false),
            renamed_label_profile.exact_hash(false)
        );
    }

    #[test]
    fn structural_hash_can_ignore_values_when_requested() {
        let left = lens_domain::TreeNode::with_children(
            "Call",
            "",
            vec![lens_domain::TreeNode::new("Ident", "alpha")],
        );
        let right = lens_domain::TreeNode::with_children(
            "Call",
            "",
            vec![lens_domain::TreeNode::new("Ident", "beta")],
        );
        let left_hashes = structural_hashes(&left);
        let right_hashes = structural_hashes(&right);

        assert_eq!(left_hashes.ignoring_values, right_hashes.ignoring_values);
        assert_ne!(left_hashes.with_values, right_hashes.with_values);
    }
}
