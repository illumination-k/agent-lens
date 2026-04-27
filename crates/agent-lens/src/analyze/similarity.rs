//! `analyze similarity` — surface near-duplicate function pairs.
//!
//! Accepts either a single source file or a directory. When the input is a
//! directory the analyzer walks it recursively (respecting `.gitignore`
//! via the `ignore` crate, the same one used by ripgrep), parses every
//! supported file, and reports cross-file pairs in addition to in-file
//! ones — modelled on `similarity-ts` (mizchi). Output is JSON by default;
//! the markdown mode emits a compact summary tuned for LLM context windows
//! rather than for humans, in line with the project's "agent-friendly lint"
//! ethos.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Instant;

use ignore::WalkBuilder;
use lens_domain::{
    CandidateStrategy, FunctionDef, LanguageParser, SimilarCluster, TSEDOptions,
    calculate_tsed_with_subtree_sizes, cluster_similar_pairs, collect_subtree_sizes,
    lsh_candidate_pairs_for_trees,
};
use lens_rust::{RustParser, extract_functions_excluding_tests};
use rayon::prelude::*;
use serde::Serialize;
use tracing::debug;

use super::{
    AnalyzePathFilter, AnalyzerError, CompiledPathFilter, LineRange, OutputFormat, SourceLang,
    changed_line_ranges, read_source,
};

/// Default similarity threshold. Picked to match the cutoff used by the
/// PostToolUse `similarity` hook so the on-demand analyzer reports the
/// same pairs that show up in the hook's transcript message.
pub const DEFAULT_THRESHOLD: f64 = 0.85;

/// Default minimum line count for a function to be considered. Mirrors the
/// `--min-lines` default in `similarity-ts`: tiny functions (one-liners,
/// trivial getters) form too many spurious matches.
pub const DEFAULT_MIN_LINES: usize = 5;

const PROFILE_TARGET: &str = "agent_lens::similarity_profile";

/// Analyzer entry point. Holds the threshold and TSED options so per-run
/// configuration can be threaded through `analyze` without changing the
/// CLI surface.
#[derive(Debug, Clone)]
pub struct SimilarityAnalyzer {
    threshold: f64,
    opts: TSEDOptions,
    diff_only: bool,
    exclude_tests: bool,
    path_filter: AnalyzePathFilter,
    min_lines: usize,
}

/// Generate `pub fn $name(mut self, $field: $ty) -> Self { self.$field = $field; self }`,
/// forwarding any `///` docs through `$attr`. Used to keep the family of
/// `SimilarityAnalyzer::with_*` setters from drifting out of shape.
macro_rules! with_setter {
    ($(#[$attr:meta])* fn $name:ident, $field:ident: $ty:ty) => {
        $(#[$attr])*
        pub fn $name(mut self, $field: $ty) -> Self {
            self.$field = $field;
            self
        }
    };
}

impl SimilarityAnalyzer {
    pub fn new() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            opts: TSEDOptions::default(),
            diff_only: false,
            exclude_tests: false,
            path_filter: AnalyzePathFilter::new(),
            min_lines: DEFAULT_MIN_LINES,
        }
    }

    with_setter! {
        /// Override the similarity threshold. Callers passing a non-default
        /// value via `--threshold` go through here.
        fn with_threshold, threshold: f64
    }

    with_setter! {
        /// Restrict reports to pairs where at least one function intersects
        /// an unstaged changed line in `git diff -U0`.
        fn with_diff_only, diff_only: bool
    }

    /// Drop test paths and test scaffolding before computing similarity.
    /// In addition to the shared path-level filter, this filters
    /// `#[test]` / `#[rstest]` / `#[<runner>::test]` functions and items
    /// inside `#[cfg(test)] mod` blocks.
    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.exclude_tests = exclude_tests;
        self.path_filter = self.path_filter.with_exclude_tests(exclude_tests);
        self
    }

    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_only_tests(only_tests);
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.path_filter = self.path_filter.with_exclude_patterns(exclude);
        self
    }

    with_setter! {
        /// Skip functions shorter than this many source lines. `similarity-ts`
        /// uses the same idea: tiny one-liners produce too many spurious
        /// matches to be useful, so the default is `DEFAULT_MIN_LINES`.
        fn with_min_lines, min_lines: usize
    }

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let started = Instant::now();
        let corpus = self.collect_corpus(path)?;
        let function_count = corpus.len();
        let clusters = self.find_clusters(&corpus);
        let report = Report::new(
            path,
            self.threshold,
            self.min_lines,
            function_count,
            &clusters,
        );
        debug!(
            target: PROFILE_TARGET,
            path = %path.display(),
            function_count,
            cluster_count = clusters.len(),
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "analyze similarity finished"
        );
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).map_err(AnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&report)),
        }
    }

    /// Collect every function under `path` into a flat corpus, tagging each
    /// with the file it came from. Single-file inputs return a 1-element
    /// per-file slice; directory inputs walk recursively, honouring
    /// `.gitignore`.
    fn collect_corpus(&self, path: &Path) -> Result<Vec<OwnedFunction>, AnalyzerError> {
        let filter = self.path_filter.compile(path)?;
        if path.is_dir() {
            self.collect_directory(path, &filter)
        } else if filter.includes_path(path) {
            self.collect_file(path, None)
        } else {
            Ok(Vec::new())
        }
    }

    fn collect_directory(
        &self,
        root: &Path,
        filter: &CompiledPathFilter,
    ) -> Result<Vec<OwnedFunction>, AnalyzerError> {
        let started = Instant::now();
        let mut files = Vec::new();
        for entry in WalkBuilder::new(root).build() {
            let entry = entry.map_err(|e| AnalyzerError::Io {
                path: root.to_path_buf(),
                source: std::io::Error::other(e),
            })?;
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let p = entry.path();
            if !filter.includes_path(p) {
                continue;
            }
            if SourceLang::from_path(p).is_none() {
                continue;
            }
            files.push(p.to_path_buf());
        }
        files.sort();

        let parsed: Vec<Vec<OwnedFunction>> = files
            .par_iter()
            .map(|p| {
                let rel = p
                    .strip_prefix(root)
                    .unwrap_or(p)
                    .display()
                    .to_string()
                    .replace('\\', "/");
                self.collect_file(p, Some(rel))
            })
            .collect::<Result<_, _>>()?;

        let out: Vec<_> = parsed.into_iter().flatten().collect();
        let file_count = files.len();
        debug!(
            target: PROFILE_TARGET,
            root = %root.display(),
            file_count,
            function_count = out.len(),
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "similarity corpus directory collected"
        );
        Ok(out)
    }

    fn collect_file(
        &self,
        path: &Path,
        rel_override: Option<String>,
    ) -> Result<Vec<OwnedFunction>, AnalyzerError> {
        let started = Instant::now();
        let (lang, source) = read_source(path)?;
        let funcs = extract_functions(lang, &source, self.exclude_tests)?;
        let rel = rel_override.unwrap_or_else(|| path.display().to_string());
        let out: Vec<_> = funcs
            .into_iter()
            .map(|def| OwnedFunction {
                file: path.to_path_buf(),
                rel_path: rel.clone(),
                def,
            })
            .collect();
        debug!(
            target: PROFILE_TARGET,
            path = %path.display(),
            language = ?lang,
            bytes = source.len(),
            function_count = out.len(),
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "similarity source parsed"
        );
        Ok(out)
    }

    /// Pairwise TSED over the corpus, then complete-link clustering. Inlined
    /// rather than calling [`lens_domain::find_similar_pair_indices`] +
    /// [`lens_domain::cluster_similar_pairs`] in two passes so the per-pair
    /// `--diff-only` filter sees the file/line metadata that domain doesn't
    /// know about.
    fn find_clusters<'a>(&self, corpus: &'a [OwnedFunction]) -> Vec<ClusterView<'a>> {
        let started = Instant::now();
        let changed_by_file = if self.diff_only {
            let diff_started = Instant::now();
            let changed = collect_changed_ranges(corpus);
            debug!(
                target: PROFILE_TARGET,
                file_count = changed.len(),
                elapsed_ms = diff_started.elapsed().as_secs_f64() * 1000.0,
                "similarity changed ranges collected"
            );
            changed
        } else {
            HashMap::new()
        };
        let use_lsh_profiles = similarity_uses_lsh(eligible_function_count(corpus, self.min_lines));
        let profiles: Vec<TreeProfile> = corpus
            .iter()
            .map(|f| {
                if use_lsh_profiles {
                    TreeProfile::from_tree_for_scoring(&f.def.tree)
                } else {
                    TreeProfile::from_tree(&f.def.tree)
                }
            })
            .collect();
        let candidate_started = Instant::now();
        let candidates = candidate_pairs(
            corpus,
            self.min_lines,
            &profiles,
            self.threshold,
            &self.opts,
        );
        debug!(
            target: PROFILE_TARGET,
            function_count = corpus.len(),
            eligible_function_count = candidates.eligible_function_count,
            min_lines = self.min_lines,
            strategy = candidates.strategy.as_str(),
            candidate_count = candidates.total_len(),
            retained_candidate_count = candidates.len(),
            size_filtered_count = candidates.size_filtered_count,
            label_filtered_count = candidates.label_filtered_count,
            arity_filtered_count = candidates.arity_filtered_count,
            shingle_filtered_count = candidates.shingle_filtered_count,
            elapsed_ms = candidate_started.elapsed().as_secs_f64() * 1000.0,
            "similarity candidates enumerated"
        );

        let (pairs_to_score, diff_prefiltered_count): (Cow<'_, [(usize, usize)]>, usize) =
            if self.diff_only {
                let mut filtered = 0usize;
                let pairs: Vec<_> = candidates
                    .pairs
                    .iter()
                    .copied()
                    .filter(|&(i, j)| {
                        let keep = corpus
                            .get(i)
                            .zip(corpus.get(j))
                            .is_some_and(|(a, b)| pair_touches_changes(a, b, &changed_by_file));
                        if !keep {
                            filtered += 1;
                        }
                        keep
                    })
                    .collect();
                (Cow::Owned(pairs), filtered)
            } else {
                (Cow::Borrowed(candidates.pairs.as_slice()), 0)
            };

        let score_started = Instant::now();
        let mut score_stats = pairs_to_score
            .par_iter()
            .fold(ScoreStats::default, |mut stats, &(i, j)| {
                let Some(a) = corpus.get(i) else {
                    return stats;
                };
                let Some(b) = corpus.get(j) else {
                    return stats;
                };
                let Some(profile_a) = profiles.get(i) else {
                    return stats;
                };
                let Some(profile_b) = profiles.get(j) else {
                    return stats;
                };
                let similarity = if trees_match_without_distance(
                    &a.def.tree,
                    &b.def.tree,
                    self.opts.apted.compare_values,
                ) {
                    stats.exact_match_count += 1;
                    1.0
                } else {
                    calculate_tsed_with_subtree_sizes(
                        &a.def.tree,
                        &b.def.tree,
                        profile_a.size,
                        profile_b.size,
                        &profile_a.subtree_sizes,
                        &profile_b.subtree_sizes,
                        &self.opts,
                    )
                };
                if similarity < self.threshold {
                    stats.below_threshold_count += 1;
                    return stats;
                }
                stats.pairs.push((i, j, similarity));
                stats
            })
            .reduce(ScoreStats::default, |mut a, mut b| {
                a.below_threshold_count += b.below_threshold_count;
                a.diff_filtered_count += b.diff_filtered_count;
                a.exact_match_count += b.exact_match_count;
                a.pairs.append(&mut b.pairs);
                a
            })
            .sorted();
        score_stats.diff_filtered_count += diff_prefiltered_count;
        debug!(
            target: PROFILE_TARGET,
            candidate_count = candidates.total_len(),
            retained_candidate_count = candidates.len(),
            scored_pair_count = score_stats.scored_pair_count(),
            matched_pair_count = score_stats.pairs.len(),
            exact_match_count = score_stats.exact_match_count,
            size_filtered_count = candidates.size_filtered_count,
            label_filtered_count = candidates.label_filtered_count,
            arity_filtered_count = candidates.arity_filtered_count,
            shingle_filtered_count = candidates.shingle_filtered_count,
            below_threshold_count = score_stats.below_threshold_count,
            diff_filtered_count = score_stats.diff_filtered_count,
            elapsed_ms = score_started.elapsed().as_secs_f64() * 1000.0,
            "similarity TSED scoring finished"
        );

        let cluster_started = Instant::now();
        let clusters: Vec<_> = cluster_similar_pairs(&score_stats.pairs, self.threshold)
            .into_iter()
            .map(|c| ClusterView::from_domain(corpus, c))
            .collect();
        debug!(
            target: PROFILE_TARGET,
            matched_pair_count = score_stats.pairs.len(),
            cluster_count = clusters.len(),
            cluster_ms = cluster_started.elapsed().as_secs_f64() * 1000.0,
            total_ms = started.elapsed().as_secs_f64() * 1000.0,
            "similarity clusters found"
        );
        clusters
    }
}

#[derive(Debug, Default)]
struct ScoreStats {
    pairs: Vec<(usize, usize, f64)>,
    exact_match_count: usize,
    below_threshold_count: usize,
    diff_filtered_count: usize,
}

impl ScoreStats {
    fn sorted(mut self) -> Self {
        self.pairs.sort_by_key(|(i, j, _)| (*i, *j));
        self
    }

    fn scored_pair_count(&self) -> usize {
        self.pairs.len() + self.below_threshold_count
    }
}

#[derive(Debug)]
struct TreeProfile {
    size: usize,
    subtree_sizes: lens_domain::SubtreeSizes,
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
    fn from_tree(tree: &lens_domain::TreeNode) -> Self {
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

    fn from_tree_for_scoring(tree: &lens_domain::TreeNode) -> Self {
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

fn trees_match_without_distance(
    a: &lens_domain::TreeNode,
    b: &lens_domain::TreeNode,
    compare_values: bool,
) -> bool {
    a.label == b.label
        && (!compare_values || a.value == b.value)
        && a.children.len() == b.children.len()
        && a.children
            .iter()
            .zip(&b.children)
            .all(|(a, b)| trees_match_without_distance(a, b, compare_values))
}

#[derive(Debug)]
struct CandidatePairs {
    pairs: Vec<(usize, usize)>,
    eligible_function_count: usize,
    size_filtered_count: usize,
    label_filtered_count: usize,
    arity_filtered_count: usize,
    shingle_filtered_count: usize,
    strategy: CandidatePairStrategy,
}

impl CandidatePairs {
    fn len(&self) -> usize {
        self.pairs.len()
    }

    fn total_len(&self) -> usize {
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
enum CandidatePairStrategy {
    Cartesian,
    Lsh,
}

impl CandidatePairStrategy {
    fn as_str(&self) -> &'static str {
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
fn candidate_pairs(
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

fn eligible_function_count(corpus: &[OwnedFunction], min_lines: usize) -> usize {
    corpus
        .iter()
        .filter(|function| function.def.line_count() >= min_lines)
        .count()
}

fn similarity_uses_lsh(eligible_count: usize) -> bool {
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
enum CheapFilter {
    Size,
    LabelMultiset,
    RootChildArity,
    PreorderShingle,
}

fn tsed_upper_bound_filter(
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

fn collect_changed_ranges(corpus: &[OwnedFunction]) -> HashMap<PathBuf, Vec<LineRange>> {
    let mut by_file: HashMap<PathBuf, Vec<LineRange>> = HashMap::new();
    for f in corpus {
        if !by_file.contains_key(&f.file) {
            by_file.insert(f.file.clone(), changed_line_ranges(&f.file));
        }
    }
    by_file
}

fn pair_touches_changes(
    a: &OwnedFunction,
    b: &OwnedFunction,
    changed: &HashMap<PathBuf, Vec<LineRange>>,
) -> bool {
    function_touches_changes(a, changed) || function_touches_changes(b, changed)
}

fn function_touches_changes(f: &OwnedFunction, changed: &HashMap<PathBuf, Vec<LineRange>>) -> bool {
    changed.get(&f.file).is_some_and(|ranges| {
        ranges
            .iter()
            .any(|r| r.overlaps(f.def.start_line, f.def.end_line))
    })
}

impl Default for SimilarityAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

/// A single function plus the file it originated from. The corpus that
/// drives pairwise similarity is a flat `Vec<OwnedFunction>` so cross-file
/// pairs are just regular pairs with different `file`s.
#[derive(Debug)]
struct OwnedFunction {
    /// Filesystem path used for `git diff` lookups.
    file: PathBuf,
    /// Display path (relative to the walk root for directory mode).
    rel_path: String,
    def: FunctionDef,
}

fn extract_functions(
    lang: SourceLang,
    source: &str,
    exclude_tests: bool,
) -> Result<Vec<FunctionDef>, AnalyzerError> {
    match lang {
        SourceLang::Rust => extract_rust(source, exclude_tests),
        SourceLang::TypeScript(dialect) => extract_typescript(source, dialect, exclude_tests),
        SourceLang::Python => extract_python(source, exclude_tests),
    }
    .map_err(AnalyzerError::Parse)
}

type ExtractError = Box<dyn std::error::Error + Send + Sync>;

fn extract_rust(source: &str, exclude_tests: bool) -> Result<Vec<FunctionDef>, ExtractError> {
    if exclude_tests {
        extract_functions_excluding_tests(source).map_err(box_err)
    } else {
        RustParser::new().extract_functions(source).map_err(box_err)
    }
}

fn extract_typescript(
    source: &str,
    dialect: lens_ts::Dialect,
    exclude_tests: bool,
) -> Result<Vec<FunctionDef>, ExtractError> {
    if exclude_tests {
        lens_ts::extract_functions_excluding_tests(source, dialect).map_err(box_err)
    } else {
        <lens_ts::TypeScriptParser as lens_domain::LanguageParser>::extract_functions(
            &mut lens_ts::TypeScriptParser::with_dialect(dialect),
            source,
        )
        .map_err(box_err)
    }
}

fn extract_python(source: &str, exclude_tests: bool) -> Result<Vec<FunctionDef>, ExtractError> {
    if exclude_tests {
        lens_py::extract_functions_excluding_tests(source).map_err(box_err)
    } else {
        lens_py::PythonParser::new()
            .extract_functions(source)
            .map_err(box_err)
    }
}

fn box_err<E: std::error::Error + Send + Sync + 'static>(e: E) -> ExtractError {
    Box::new(e)
}

#[derive(Debug, Serialize)]
struct Report<'a> {
    /// Input path: a single source file, or the root directory walked.
    root: String,
    function_count: usize,
    threshold: f64,
    min_lines: usize,
    cluster_count: usize,
    clusters: &'a [ClusterView<'a>],
}

impl<'a> Report<'a> {
    fn new(
        path: &Path,
        threshold: f64,
        min_lines: usize,
        function_count: usize,
        clusters: &'a [ClusterView<'a>],
    ) -> Self {
        Self {
            root: path.display().to_string(),
            function_count,
            threshold,
            min_lines,
            cluster_count: clusters.len(),
            clusters,
        }
    }
}

#[derive(Debug, Serialize)]
struct ClusterView<'a> {
    size: usize,
    min_similarity: f64,
    max_similarity: f64,
    functions: Vec<FunctionRef<'a>>,
}

impl<'a> ClusterView<'a> {
    fn from_domain(corpus: &'a [OwnedFunction], cluster: SimilarCluster) -> Self {
        let functions: Vec<FunctionRef<'a>> = cluster
            .members
            .iter()
            .filter_map(|i| corpus.get(*i).map(FunctionRef::from))
            .collect();
        Self {
            size: functions.len(),
            min_similarity: cluster.min_similarity,
            max_similarity: cluster.max_similarity,
            functions,
        }
    }
}

#[derive(Debug, Serialize)]
struct FunctionRef<'a> {
    file: &'a str,
    name: &'a str,
    start_line: usize,
    end_line: usize,
}

impl<'a> From<&'a OwnedFunction> for FunctionRef<'a> {
    fn from(f: &'a OwnedFunction) -> Self {
        Self {
            file: f.rel_path.as_str(),
            name: f.def.name.as_str(),
            start_line: f.def.start_line,
            end_line: f.def.end_line,
        }
    }
}

fn format_markdown(report: &Report<'_>) -> String {
    let mut out = format!(
        "# Similarity report: {} ({} function(s), threshold {:.2}, min lines {})\n",
        report.root, report.function_count, report.threshold, report.min_lines,
    );
    if report.clusters.is_empty() {
        out.push_str("\n_No similar function clusters at or above threshold._\n");
        return out;
    }
    let _ = writeln!(out, "\n## {} similar cluster(s)", report.cluster_count);
    for cluster in report.clusters {
        // writeln! into a String cannot fail; the result is swallowed
        // deliberately rather than unwrapped to satisfy the workspace's
        // `unwrap_used` lint.
        let _ = writeln!(
            out,
            "\n- {} functions, similarity {:.0}–{:.0}%",
            cluster.size,
            cluster.min_similarity * 100.0,
            cluster.max_similarity * 100.0,
        );
        for f in &cluster.functions {
            let _ = writeln!(
                out,
                "  - {}:`{}` (L{}-{})",
                f.file, f.name, f.start_line, f.end_line,
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::collection::vec;
    use proptest::prelude::*;
    use rstest::rstest;
    use std::io::Write;
    use std::path::PathBuf;

    /// Two near-identical function bodies — guaranteed to score above any
    /// modest threshold. Used by the report-rendering and
    /// threshold-suppression tests so a single source string drives both
    /// success-path checks. Keep each body at >= DEFAULT_MIN_LINES so the
    /// default min-lines filter doesn't suppress them.
    const PAIRED_FUNCTIONS: &str = r#"
fn alpha(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
fn beta(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
"#;

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn arb_tree() -> impl Strategy<Value = lens_domain::TreeNode> {
        let leaf = prop_oneof![
            Just(lens_domain::TreeNode::leaf("A")),
            Just(lens_domain::TreeNode::leaf("B")),
            Just(lens_domain::TreeNode::leaf("C")),
            Just(lens_domain::TreeNode::leaf("D")),
            Just(lens_domain::TreeNode::leaf("E")),
        ];
        leaf.prop_recursive(4, 32, 4, |inner| {
            (
                prop_oneof![Just("A"), Just("B"), Just("C"), Just("D"), Just("E")],
                vec(inner, 0..4),
            )
                .prop_map(|(label, children)| {
                    lens_domain::TreeNode::with_children(label, "", children)
                })
        })
    }

    proptest! {
        #[test]
        fn cheap_tsed_filters_do_not_drop_pairs_that_reach_threshold(
            a in arb_tree(),
            b in arb_tree(),
            threshold in 0.0_f64..1.0,
        ) {
            let opts = TSEDOptions::default();
            let profiles = vec![TreeProfile::from_tree(&a), TreeProfile::from_tree(&b)];
            if let Some(filter) = tsed_upper_bound_filter(&profiles, 0, 1, threshold, &opts) {
                let actual = lens_domain::calculate_tsed(&a, &b, &opts);
                prop_assert!(
                    actual < threshold + 1e-9,
                    "filter {filter:?} dropped pair with TSED {actual} at threshold {threshold}: {a:?} {b:?}",
                );
            }
        }
    }

    #[test]
    fn cheap_filters_prune_structurally_unreachable_pairs() {
        let a = lens_domain::TreeNode::with_children(
            "Block",
            "",
            vec![
                lens_domain::TreeNode::leaf("Let"),
                lens_domain::TreeNode::leaf("Let"),
                lens_domain::TreeNode::leaf("Return"),
            ],
        );
        let b = lens_domain::TreeNode::with_children(
            "Block",
            "",
            vec![
                lens_domain::TreeNode::leaf("If"),
                lens_domain::TreeNode::leaf("While"),
                lens_domain::TreeNode::leaf("Match"),
            ],
        );
        let profiles = vec![TreeProfile::from_tree(&a), TreeProfile::from_tree(&b)];
        let filter = tsed_upper_bound_filter(&profiles, 0, 1, 0.9, &TSEDOptions::default());
        assert!(matches!(
            filter,
            Some(CheapFilter::LabelMultiset | CheapFilter::PreorderShingle)
        ));
    }

    fn assert_json_pair_report(out: &str) {
        let parsed: serde_json::Value = serde_json::from_str(out).unwrap();
        assert_eq!(parsed["function_count"], 2);
        assert!(parsed["cluster_count"].as_u64().unwrap() >= 1);
        let clusters = parsed["clusters"].as_array().unwrap();
        let cluster = &clusters[0];
        assert!(cluster["size"].as_u64().unwrap() >= 2);
        let names: Vec<&str> = cluster["functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
        // Each function reference still carries a file path so cross-file
        // clusters from directory mode are unambiguous; assert the field
        // is present even for single-file input.
        assert!(cluster["functions"][0]["file"].as_str().is_some());
        // The cluster summary stats accompany the members so an agent can
        // judge cohesion without re-deriving from pairs.
        assert!(cluster["min_similarity"].as_f64().is_some());
        assert!(cluster["max_similarity"].as_f64().is_some());
    }

    fn assert_markdown_pair_report(out: &str) {
        assert!(out.contains("Similarity report"));
        assert!(out.contains("similar cluster"));
        assert!(out.contains("alpha"));
        assert!(out.contains("beta"));
    }

    /// Both output formats must surface the matched function names; only the
    /// shape of the rendered report differs.
    #[rstest]
    #[case::json(OutputFormat::Json, assert_json_pair_report)]
    #[case::markdown(OutputFormat::Md, assert_markdown_pair_report)]
    fn report_renders_paired_functions(
        #[case] format: OutputFormat,
        #[case] assert_report: fn(&str),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", PAIRED_FUNCTIONS);
        let out = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, format)
            .unwrap();
        assert_report(&out);
    }

    #[test]
    fn empty_report_when_no_pairs_above_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn alpha() -> i32 {
    let a = 1;
    let b = 2;
    let c = 3;
    a + b + c
}
fn beta(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs {
        if *x > 0 {
            total += x;
        }
    }
    total
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let md = SimilarityAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("No similar function clusters"));
    }

    #[test]
    fn threshold_override_suppresses_all_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", PAIRED_FUNCTIONS);
        let json = SimilarityAnalyzer::new()
            .with_threshold(1.5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cluster_count"], 0);
    }

    #[test]
    fn min_lines_filters_short_functions() {
        // Two parallel one-line bodies form a similar pair only at the
        // permissive default min-lines; raising it past the function's
        // line count drops them from the corpus before TSED runs.
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn alpha(x: i32) -> i32 { x + 1 }
fn beta(x: i32)  -> i32 { x + 1 }
"#;
        let file = write_file(dir.path(), "lib.rs", src);

        let permissive = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_min_lines(1)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&permissive).unwrap();
        assert!(parsed["cluster_count"].as_u64().unwrap() >= 1);

        let strict = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_min_lines(5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&strict).unwrap();
        assert_eq!(parsed["cluster_count"], 0);
    }

    #[test]
    fn exclude_tests_drops_test_module_pairs_from_report() {
        // Two parallel `#[test]` fixtures alongside a single
        // production function. Without `--exclude-tests` the two test
        // bodies form a similar pair; with it they're filtered before
        // similarity is computed and `cluster_count` falls to zero.
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn production(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}

#[cfg(test)]
mod tests {
    fn alpha() -> i32 {
        let a = 1;
        let b = 2;
        let c = 3;
        let d = 4;
        a + b + c + d
    }
    fn beta() -> i32 {
        let a = 1;
        let b = 2;
        let c = 3;
        let d = 4;
        a + b + c + d
    }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);

        let with_tests = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&with_tests).unwrap();
        assert!(parsed["cluster_count"].as_u64().unwrap() >= 1);

        let without_tests = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_exclude_tests(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&without_tests).unwrap();
        assert_eq!(parsed["cluster_count"], 0);
        assert_eq!(parsed["function_count"], 1);
    }

    #[test]
    fn report_renders_paired_python_functions() {
        // Two structurally identical Python functions — guaranteed to
        // score above the 0.5 threshold and exercise the lens-py
        // dispatch added alongside the Rust / TS arms.
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
def alpha(xs):
    total = 0
    for x in xs:
        total += x
    return total

def beta(ys):
    sum_ = 0
    for y in ys:
        sum_ += y
    return sum_
"#;
        let file = write_file(dir.path(), "lib.py", src);
        let json = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["function_count"], 2);
        assert!(parsed["cluster_count"].as_u64().unwrap() >= 1);
        let names: Vec<&str> = parsed["clusters"][0]["functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    #[test]
    fn exclude_tests_drops_python_test_functions_from_report() {
        // pytest-style `test_*` functions form a parallel pair next to
        // a single production function; `--exclude-tests` should drop
        // them via `lens_py::extract_functions_excluding_tests`.
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
def production(xs):
    total = 0
    for x in xs:
        total += x
    return total

def test_alpha():
    a = 1
    b = 2
    c = 3
    assert a + b + c == 6

def test_beta():
    a = 1
    b = 2
    c = 3
    assert a + b + c == 6
"#;
        let file = write_file(dir.path(), "lib.py", src);

        let with_tests = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&with_tests).unwrap();
        assert!(parsed["cluster_count"].as_u64().unwrap() >= 1);

        let without_tests = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_exclude_tests(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&without_tests).unwrap();
        assert_eq!(parsed["cluster_count"], 0);
        assert_eq!(parsed["function_count"], 1);
    }

    #[test]
    fn exclude_tests_drops_typescript_test_functions_from_report() {
        // xUnit-style `test_*` functions form a parallel pair next to
        // a single production function; `--exclude-tests` should drop
        // them via `lens_ts::extract_functions_excluding_tests`.
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
function production(xs: number[]): number {
    let total = 0;
    for (const x of xs) {
        total += x;
    }
    return total;
}

function test_alpha(): void {
    const a = 1;
    const b = 2;
    const c = 3;
    if (a + b + c !== 6) throw new Error("bad");
}

function test_beta(): void {
    const a = 1;
    const b = 2;
    const c = 3;
    if (a + b + c !== 6) throw new Error("bad");
}
"#;
        let file = write_file(dir.path(), "lib.ts", src);

        let with_tests = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&with_tests).unwrap();
        assert!(parsed["cluster_count"].as_u64().unwrap() >= 1);

        let without_tests = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_exclude_tests(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&without_tests).unwrap();
        assert_eq!(parsed["cluster_count"], 0);
        assert_eq!(parsed["function_count"], 1);
    }

    #[test]
    fn directory_mode_reports_cross_file_pairs() {
        // Two near-identical functions split across two files: only
        // visible to the analyzer once it walks the directory.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.rs",
            r#"
fn alpha(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
"#,
        );
        write_file(
            dir.path(),
            "nested/b.rs",
            r#"
fn beta(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
"#,
        );

        let json = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["function_count"], 2);
        assert_eq!(parsed["cluster_count"], 1);
        let cluster = &parsed["clusters"][0];
        assert_eq!(cluster["size"], 2);
        let files: Vec<&str> = cluster["functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["file"].as_str().unwrap())
            .collect();
        assert!(files.contains(&"a.rs"));
        assert!(files.contains(&"nested/b.rs"));
    }

    #[test]
    fn directory_mode_skips_unsupported_extensions_and_gitignored_files() {
        // `.gitignore` should be honoured (the `ignore` walker is
        // gitignore-aware out of the box), and unsupported extensions
        // should be silently skipped.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.rs",
            r#"
fn alpha(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
"#,
        );
        write_file(
            dir.path(),
            "ignored.rs",
            r#"
fn beta(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
"#,
        );
        write_file(dir.path(), "notes.txt", "not a source file");
        write_file(dir.path(), ".gitignore", "ignored.rs\n");

        // The `ignore` crate honours .gitignore only inside a git repo
        // by default; bootstrap one so the test exercises the gitignore
        // path rather than just the extension filter.
        let status = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        let json = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["function_count"], 1, "got {parsed}");
    }

    #[test]
    fn path_filters_apply_to_directory_walks() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", PAIRED_FUNCTIONS);
        write_file(dir.path(), "tests/lib_test.rs", PAIRED_FUNCTIONS);
        write_file(dir.path(), "src/generated.rs", PAIRED_FUNCTIONS);

        let only_tests = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_only_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&only_tests).unwrap();
        assert_eq!(parsed["function_count"], 2);
        let files: Vec<&str> = parsed["clusters"][0]["functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["file"].as_str().unwrap())
            .collect();
        assert!(files.iter().all(|f| *f == "tests/lib_test.rs"));

        let exclude_tests = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_exclude_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exclude_tests).unwrap();
        let files: Vec<&str> = parsed["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["functions"].as_array().unwrap())
            .map(|f| f["file"].as_str().unwrap())
            .collect();
        assert!(!files.contains(&"tests/lib_test.rs"));

        let exclude_generated = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_exclude_patterns(vec!["generated.rs".to_owned()])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exclude_generated).unwrap();
        let files: Vec<&str> = parsed["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["functions"].as_array().unwrap())
            .map(|f| f["file"].as_str().unwrap())
            .collect();
        assert!(!files.contains(&"src/generated.rs"));
    }

    #[test]
    fn diff_only_filters_to_pairs_touching_changed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
fn alpha(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
fn beta(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
"#,
        );
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        run_git(dir.path(), &["add", "lib.rs"]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        write_file(
            dir.path(),
            "lib.rs",
            r#"
fn alpha(x: i32) -> i32 {
    let a = x + 10;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
fn beta(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
"#,
        );

        let json = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_diff_only(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cluster_count"], 1);
        assert_eq!(parsed["clusters"][0]["size"], 2);
    }

    fn run_git(dir: &Path, args: &[&str]) {
        // Mirror the hardened helper in `hotspot.rs`: disable commit /
        // tag signing so the test never asks the host's signing setup
        // to participate. Without this, sandboxes that have a global
        // `commit.gpgsign=true` (and a signing helper that talks to a
        // service which can fail) make the test brittle.
        let status = std::process::Command::new("git")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .arg("-c")
            .arg("tag.gpgsign=false")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }

    fn setup_unsupported_extension(dir: &Path) -> PathBuf {
        write_file(dir, "notes.txt", "hello")
    }

    fn setup_missing_file(_dir: &Path) -> PathBuf {
        PathBuf::from("/definitely/does/not/exist.rs")
    }

    fn setup_invalid_rust(dir: &Path) -> PathBuf {
        write_file(dir, "broken.rs", "fn ??? {")
    }

    /// All recoverable failure modes route through `AnalyzerError`. Rather
    /// than spinning up a dedicated test per variant, drive the same
    /// `analyze` call and assert on the matching enum arm.
    #[rstest]
    #[case::unsupported_extension(
        setup_unsupported_extension,
        |e: &AnalyzerError| matches!(e, AnalyzerError::UnsupportedExtension { .. }),
    )]
    #[case::missing_file(
        setup_missing_file,
        |e: &AnalyzerError| matches!(e, AnalyzerError::Io { .. }),
    )]
    #[case::parse_failure(
        setup_invalid_rust,
        |e: &AnalyzerError| matches!(e, AnalyzerError::Parse(_)),
    )]
    fn analyze_surfaces_error_variants(
        #[case] setup: fn(&Path) -> PathBuf,
        #[case] matches_expected: fn(&AnalyzerError) -> bool,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = setup(dir.path());
        let err = SimilarityAnalyzer::new()
            .with_min_lines(1)
            .analyze(&path, OutputFormat::Json)
            .unwrap_err();
        assert!(matches_expected(&err), "unexpected error variant: {err}");
    }
}
