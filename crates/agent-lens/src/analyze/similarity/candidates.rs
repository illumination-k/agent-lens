use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use lens_domain::{
    CandidateStrategy, TSEDOptions, collect_subtree_sizes, lsh_candidate_pairs_for_trees,
};

use super::OwnedFunction;

#[derive(Debug)]
pub(super) struct TreeProfile {
    pub size: usize,
    pub subtree_sizes: lens_domain::SubtreeSizes,
    filters: Option<TreeFilterProfile>,
}

#[derive(Debug)]
struct TreeFilterProfile {
    label_counts: HashMap<u64, usize>,
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
        Self {
            size,
            subtree_sizes,
            filters: Some(filters),
        }
    }

    pub(super) fn from_tree_for_scoring(tree: &lens_domain::TreeNode) -> Self {
        let subtree_sizes = collect_subtree_sizes(tree);
        let size = subtree_sizes
            .get(&(std::ptr::from_ref::<lens_domain::TreeNode>(tree) as usize))
            .copied()
            .unwrap_or(0);
        Self {
            size,
            subtree_sizes,
            filters: None,
        }
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
    pub arity_filtered_count: usize,
    pub shingle_filtered_count: usize,
    pub strategy: CandidatePairStrategy,
}

impl CandidatePairs {
    pub(super) fn len(&self) -> usize {
        self.pairs.len()
    }

    pub(super) fn total_len(&self) -> usize {
        self.pairs.len()
            + self.size_filtered_count
            + self.label_filtered_count
            + self.arity_filtered_count
            + self.shingle_filtered_count
    }
}

#[derive(Debug, Default)]
struct CheapFilterCounts {
    size: usize,
    label: usize,
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
pub(super) fn candidate_pairs(
    corpus: &[OwnedFunction],
    min_lines: usize,
    profiles: &[TreeProfile],
    threshold: f64,
    opts: &TSEDOptions,
) -> CandidatePairs {
    let eligible_indices: Vec<usize> = corpus
        .iter()
        .enumerate()
        .filter(move |(_, a)| a.def.line_count() >= min_lines)
        .map(|(i, _)| i)
        .collect();
    let mut strategy = CandidateStrategy::default();
    // Keep directory analysis on a high-recall LSH setting. Property tests
    // cover the exact setting here; tighter banding has missed one-label
    // near-clones that still clear the analyzer threshold.
    strategy.lsh.num_bands = 24;
    let use_lsh = strategy_uses_lsh(&strategy, eligible_indices.len());
    let (pairs, filter_counts) = if use_lsh {
        let trees: Vec<&lens_domain::TreeNode> = eligible_indices
            .iter()
            .filter_map(|&i| corpus.get(i).map(|f| &f.def.tree))
            .collect();
        filter_size_compatible_pairs(
            lsh_candidate_pairs_for_trees(&trees, &strategy.lsh)
                .into_iter()
                .filter_map(|(i, j)| {
                    let a = eligible_indices.get(i).copied()?;
                    let b = eligible_indices.get(j).copied()?;
                    Some((a, b))
                }),
            profiles,
            threshold,
            opts,
        )
    } else {
        filter_tsed_compatible_pairs(
            eligible_indices
                .iter()
                .enumerate()
                .flat_map(|(pos, &i)| eligible_indices[pos + 1..].iter().map(move |&j| (i, j))),
            profiles,
            threshold,
            opts,
        )
    };
    CandidatePairs {
        pairs,
        eligible_function_count: eligible_indices.len(),
        size_filtered_count: filter_counts.size,
        label_filtered_count: filter_counts.label,
        arity_filtered_count: filter_counts.arity,
        shingle_filtered_count: filter_counts.shingle,
        strategy: if use_lsh {
            CandidatePairStrategy::Lsh
        } else {
            CandidatePairStrategy::Cartesian
        },
    }
}

pub(super) fn eligible_function_count(corpus: &[OwnedFunction], min_lines: usize) -> usize {
    corpus
        .iter()
        .filter(|function| function.def.line_count() >= min_lines)
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

fn filter_size_compatible_pairs(
    pairs: impl IntoIterator<Item = (usize, usize)>,
    profiles: &[TreeProfile],
    threshold: f64,
    opts: &TSEDOptions,
) -> (Vec<(usize, usize)>, CheapFilterCounts) {
    let mut out = Vec::new();
    let mut counts = CheapFilterCounts::default();
    for (i, j) in pairs {
        let Some(profile_a) = profiles.get(i) else {
            continue;
        };
        let Some(profile_b) = profiles.get(j) else {
            continue;
        };
        if tsed_upper_bound(profile_a, profile_b, 0.0, opts.size_penalty) < threshold {
            counts.size += 1;
        } else {
            out.push((i, j));
        }
    }
    (out, counts)
}

fn filter_tsed_compatible_pairs(
    pairs: impl IntoIterator<Item = (usize, usize)>,
    profiles: &[TreeProfile],
    threshold: f64,
    opts: &TSEDOptions,
) -> (Vec<(usize, usize)>, CheapFilterCounts) {
    let mut out = Vec::new();
    let mut counts = CheapFilterCounts::default();
    for (i, j) in pairs {
        match tsed_upper_bound_filter(profiles, i, j, threshold, opts) {
            Some(CheapFilter::Size) => counts.size += 1,
            Some(CheapFilter::LabelMultiset) => counts.label += 1,
            Some(CheapFilter::RootChildArity) => counts.arity += 1,
            Some(CheapFilter::PreorderShingle) => counts.shingle += 1,
            None => out.push((i, j)),
        }
    }
    (out, counts)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CheapFilter {
    Size,
    LabelMultiset,
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
    let label_distance = label_multiset_distance_lower_bound(profile_a, profile_b, opts);
    if tsed_upper_bound(profile_a, profile_b, label_distance, opts.size_penalty) < threshold {
        return Some(CheapFilter::LabelMultiset);
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

fn label_multiset_distance_lower_bound(
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
    let l1 = multiset_l1(&filter_a.label_counts, &filter_b.label_counts);
    // A rename can fix at most one missing and one extra label. Insert and
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
