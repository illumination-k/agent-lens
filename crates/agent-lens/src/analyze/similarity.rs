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
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use lens_domain::{TSEDOptions, calculate_tsed_with_subtree_sizes, cluster_similar_pairs};
use rayon::prelude::*;
use tracing::debug;

use super::{AnalyzePathFilter, AnalyzerError, LineRange, OutputFormat, changed_line_ranges};

mod candidates;
mod corpus;
mod extract;
mod report;

use candidates::{
    CandidatePairs, TreeProfile, candidate_pairs, eligible_function_count, similarity_uses_lsh,
};
#[cfg(test)]
use candidates::{CheapFilter, tsed_upper_bound_filter};
use corpus::{OwnedFunction, collect_corpus};
use report::{ClusterView, Report, format_markdown};

/// Default similarity threshold. Picked to match the cutoff used by the
/// PostToolUse `similarity` hook so the on-demand analyzer reports the
/// same pairs that show up in the hook's transcript message.
pub const DEFAULT_THRESHOLD: f64 = 0.85;

/// Default minimum line count for a function to be considered. Mirrors the
/// `--min-lines` default in `similarity-ts`: tiny functions (one-liners,
/// trivial getters) form too many spurious matches.
pub const DEFAULT_MIN_LINES: usize = 5;

const PROFILE_TARGET: &str = "agent_lens::similarity_profile";
const BODY_SIMILARITY_WEIGHT: f64 = 0.8;
const SIGNATURE_SIMILARITY_WEIGHT: f64 = 0.2;
/// Hard cap for pairs scored by `analyze similarity`.
///
/// Even with LSH enabled, very large corpora can still produce a huge
/// candidate set. Keep a guardrail so runs fail fast with an actionable
/// error instead of spending minutes in pairwise scoring.
///
/// The cap is calibrated from a real benchmark run:
/// `similarity_directory_lsh_1024_functions` measured ~370–384 ms
/// (2026-04-28, local `cargo bench` in this repo). A 1024-function
/// full pair set is 523,776 pairs; scaling that to a practical upper
/// budget around 10 seconds gives a limit around 13M pairs.
const MAX_CANDIDATE_PAIRS: usize = 13_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FunctionSelection {
    All,
    ExcludeTests,
    OnlyTests,
}

impl FunctionSelection {
    pub(super) fn includes(self, is_test: bool) -> bool {
        match self {
            Self::All => true,
            Self::ExcludeTests => !is_test,
            Self::OnlyTests => is_test,
        }
    }
}

/// Analyzer entry point. Holds the threshold and TSED options so per-run
/// configuration can be threaded through `analyze` without changing the
/// CLI surface.
#[derive(Debug, Clone)]
pub struct SimilarityAnalyzer {
    threshold: f64,
    opts: TSEDOptions,
    diff_only: bool,
    exclude_tests: bool,
    only_tests: bool,
    path_filter: AnalyzePathFilter,
    min_lines: usize,
    top: Option<usize>,
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
            only_tests: false,
            path_filter: AnalyzePathFilter::new(),
            min_lines: DEFAULT_MIN_LINES,
            top: None,
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
        self.only_tests = only_tests;
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

    with_setter! {
        /// Cap the markdown report to the top-N clusters. JSON output
        /// always carries the full list.
        fn with_top, top: Option<usize>
    }

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let started = Instant::now();
        let corpus = collect_corpus(path, &self.path_filter, self.function_selection())?;
        let function_count = corpus.len();
        let clusters = self.find_clusters(&corpus)?;
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
            OutputFormat::Md => Ok(format_markdown(&report, self.top)),
        }
    }

    fn function_selection(&self) -> FunctionSelection {
        if self.only_tests {
            FunctionSelection::OnlyTests
        } else if self.exclude_tests {
            FunctionSelection::ExcludeTests
        } else {
            FunctionSelection::All
        }
    }

    /// Pairwise TSED over the corpus, then complete-link clustering. Inlined
    /// rather than calling [`lens_domain::find_similar_pair_indices`] +
    /// [`lens_domain::cluster_similar_pairs`] in two passes so the per-pair
    /// `--diff-only` filter sees the file/line metadata that domain doesn't
    /// know about.
    fn find_clusters<'a>(
        &self,
        corpus: &'a [OwnedFunction],
    ) -> Result<Vec<ClusterView<'a>>, AnalyzerError> {
        let started = Instant::now();
        let changed_by_file = self.changed_ranges_for_run(corpus);
        let profiles = build_tree_profiles(corpus, self.min_lines);
        let candidate_started = Instant::now();
        let candidate_threshold = body_candidate_threshold(self.threshold);
        let candidates = candidate_pairs(
            corpus,
            self.min_lines,
            &profiles,
            candidate_threshold,
            &self.opts,
        );
        log_candidate_stats(corpus.len(), self.min_lines, &candidates, candidate_started);
        let (pairs_to_score, diff_prefiltered_count) =
            self.pairs_to_score(corpus, &candidates, &changed_by_file);
        enforce_candidate_pair_limit(
            candidates.eligible_function_count,
            pairs_to_score.len(),
            MAX_CANDIDATE_PAIRS,
            self.min_lines,
            candidates.strategy.as_str(),
        )?;

        let score_started = Instant::now();
        let mut score_stats = score_candidate_pairs(
            corpus,
            &profiles,
            &pairs_to_score,
            self.threshold,
            &self.opts,
        );
        score_stats.diff_filtered_count = diff_prefiltered_count;
        log_score_stats(&candidates, &score_stats, score_started);

        let cluster_started = Instant::now();
        let domain_pairs: Vec<_> = score_stats
            .pairs
            .iter()
            .map(|pair| (pair.i, pair.j, pair.components.similarity))
            .collect();
        let pair_scores: HashMap<_, _> = score_stats
            .pairs
            .iter()
            .map(|pair| (sorted_pair_key(pair.i, pair.j), pair.components))
            .collect();
        let clusters: Vec<_> = cluster_similar_pairs(&domain_pairs, self.threshold)
            .into_iter()
            .map(|c| ClusterView::from_domain(corpus, c, &pair_scores))
            .collect();
        debug!(
            target: PROFILE_TARGET,
            matched_pair_count = score_stats.pairs.len(),
            cluster_count = clusters.len(),
            cluster_ms = cluster_started.elapsed().as_secs_f64() * 1000.0,
            total_ms = started.elapsed().as_secs_f64() * 1000.0,
            "similarity clusters found"
        );
        Ok(clusters)
    }

    fn changed_ranges_for_run(&self, corpus: &[OwnedFunction]) -> HashMap<PathBuf, Vec<LineRange>> {
        if !self.diff_only {
            return HashMap::new();
        }
        let diff_started = Instant::now();
        let changed = collect_changed_ranges(corpus);
        debug!(
            target: PROFILE_TARGET,
            file_count = changed.len(),
            elapsed_ms = diff_started.elapsed().as_secs_f64() * 1000.0,
            "similarity changed ranges collected"
        );
        changed
    }

    fn pairs_to_score<'a>(
        &self,
        corpus: &[OwnedFunction],
        candidates: &'a CandidatePairs,
        changed_by_file: &HashMap<PathBuf, Vec<LineRange>>,
    ) -> (Cow<'a, [(usize, usize)]>, usize) {
        if !self.diff_only {
            return (Cow::Borrowed(candidates.pairs.as_slice()), 0);
        }
        filter_pairs_touching_changes(corpus, candidates, changed_by_file)
    }
}

fn build_tree_profiles(corpus: &[OwnedFunction], min_lines: usize) -> Vec<TreeProfile> {
    let use_lsh_profiles = similarity_uses_lsh(eligible_function_count(corpus, min_lines));
    if use_lsh_profiles {
        corpus
            .par_iter()
            .map(|f| TreeProfile::from_tree_for_scoring(f.def.body_tree()))
            .collect()
    } else {
        corpus
            .iter()
            .map(|f| TreeProfile::from_tree(f.def.body_tree()))
            .collect()
    }
}

fn body_candidate_threshold(threshold: f64) -> f64 {
    ((threshold - SIGNATURE_SIMILARITY_WEIGHT) / BODY_SIMILARITY_WEIGHT).clamp(0.0, 1.0)
}

fn filter_pairs_touching_changes<'a>(
    corpus: &[OwnedFunction],
    candidates: &'a CandidatePairs,
    changed_by_file: &HashMap<PathBuf, Vec<LineRange>>,
) -> (Cow<'a, [(usize, usize)]>, usize) {
    let mut filtered = 0usize;
    let pairs: Vec<_> = candidates
        .pairs
        .iter()
        .copied()
        .filter(|&(i, j)| {
            let keep = corpus
                .get(i)
                .zip(corpus.get(j))
                .is_some_and(|(a, b)| pair_touches_changes(a, b, changed_by_file));
            if !keep {
                filtered += 1;
            }
            keep
        })
        .collect();
    (Cow::Owned(pairs), filtered)
}

fn log_candidate_stats(
    function_count: usize,
    min_lines: usize,
    candidates: &CandidatePairs,
    started: Instant,
) {
    debug!(
        target: PROFILE_TARGET,
        function_count,
        eligible_function_count = candidates.eligible_function_count,
        min_lines,
        strategy = candidates.strategy.as_str(),
        candidate_count = candidates.total_len(),
        retained_candidate_count = candidates.pairs.len(),
        size_filtered_count = candidates.size_filtered_count,
        label_filtered_count = candidates.label_filtered_count,
        arity_filtered_count = candidates.arity_filtered_count,
        shingle_filtered_count = candidates.shingle_filtered_count,
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "similarity candidates enumerated"
    );
}

fn log_score_stats(candidates: &CandidatePairs, score_stats: &ScoreStats, started: Instant) {
    debug!(
        target: PROFILE_TARGET,
        candidate_count = candidates.total_len(),
        retained_candidate_count = candidates.pairs.len(),
        scored_pair_count = score_stats.scored_pair_count(),
        matched_pair_count = score_stats.pairs.len(),
        exact_match_count = score_stats.exact_match_count,
        size_filtered_count = candidates.size_filtered_count,
        label_filtered_count = candidates.label_filtered_count,
        arity_filtered_count = candidates.arity_filtered_count,
        shingle_filtered_count = candidates.shingle_filtered_count,
        below_threshold_count = score_stats.below_threshold_count,
        diff_filtered_count = score_stats.diff_filtered_count,
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "similarity TSED scoring finished"
    );
}

fn enforce_candidate_pair_limit(
    eligible_function_count: usize,
    candidate_pair_count: usize,
    max_candidate_pairs: usize,
    min_lines: usize,
    strategy: &'static str,
) -> Result<(), AnalyzerError> {
    if candidate_pair_count <= max_candidate_pairs {
        return Ok(());
    }
    let n = eligible_function_count as u128;
    let theoretical_pair_count = n.saturating_mul(n.saturating_sub(1)) / 2;
    Err(AnalyzerError::SimilarityScopeTooBroad {
        eligible_function_count,
        theoretical_pair_count,
        candidate_pair_count,
        max_candidate_pairs,
        min_lines,
        strategy,
    })
}

fn is_exact_match_without_distance(
    profile_a: &TreeProfile,
    profile_b: &TreeProfile,
    a: &lens_domain::TreeNode,
    b: &lens_domain::TreeNode,
    compare_values: bool,
) -> bool {
    if profile_a.size != profile_b.size {
        return false;
    }
    if profile_a.exact_hash(compare_values) != profile_b.exact_hash(compare_values) {
        return false;
    }
    trees_match_without_distance(a, b, compare_values)
}

fn score_candidate_pairs(
    corpus: &[OwnedFunction],
    profiles: &[TreeProfile],
    pairs: &[(usize, usize)],
    threshold: f64,
    opts: &TSEDOptions,
) -> ScoreStats {
    pairs
        .par_iter()
        .fold(ScoreStats::default, |mut stats, &(i, j)| {
            if let Some(score) = score_candidate_pair(corpus, profiles, i, j, opts) {
                stats.record(score, threshold);
            }
            stats
        })
        .reduce(ScoreStats::default, ScoreStats::merge)
        .sorted()
}

fn score_candidate_pair(
    corpus: &[OwnedFunction],
    profiles: &[TreeProfile],
    i: usize,
    j: usize,
    opts: &TSEDOptions,
) -> Option<PairScore> {
    let a = corpus.get(i)?;
    let b = corpus.get(j)?;
    let profile_a = profiles.get(i)?;
    let profile_b = profiles.get(j)?;
    let compare_values = opts.apted.compare_values;
    let body_a = a.def.body_tree();
    let body_b = b.def.body_tree();
    let exact_match =
        is_exact_match_without_distance(profile_a, profile_b, body_a, body_b, compare_values);
    let body_similarity = if exact_match {
        1.0
    } else {
        let sizes_a = profile_a.subtree_sizes(body_a);
        let sizes_b = profile_b.subtree_sizes(body_b);
        calculate_tsed_with_subtree_sizes(
            body_a,
            body_b,
            profile_a.size,
            profile_b.size,
            sizes_a,
            sizes_b,
            opts,
        )
    };
    let signature = signature_components(a.def.signature.as_ref(), b.def.signature.as_ref());
    let signature_similarity = signature.signature_similarity.unwrap_or(1.0);
    let similarity = (BODY_SIMILARITY_WEIGHT * body_similarity)
        + (SIGNATURE_SIMILARITY_WEIGHT * signature_similarity);
    Some(PairScore {
        i,
        j,
        components: SimilarityComponents {
            similarity,
            body_similarity,
            signature_similarity: signature.signature_similarity,
            type_overlap: signature.type_overlap,
            identifier_overlap: signature.identifier_overlap,
        },
        exact_match,
    })
}

#[derive(Debug)]
struct PairScore {
    i: usize,
    j: usize,
    components: SimilarityComponents,
    exact_match: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct SimilarityComponents {
    pub(super) similarity: f64,
    pub(super) body_similarity: f64,
    pub(super) signature_similarity: Option<f64>,
    pub(super) type_overlap: Option<f64>,
    pub(super) identifier_overlap: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct SignatureComponents {
    signature_similarity: Option<f64>,
    type_overlap: Option<f64>,
    identifier_overlap: Option<f64>,
}

fn signature_components(
    a: Option<&lens_domain::FunctionSignature>,
    b: Option<&lens_domain::FunctionSignature>,
) -> SignatureComponents {
    let (Some(a), Some(b)) = (a, b) else {
        return SignatureComponents {
            signature_similarity: None,
            type_overlap: None,
            identifier_overlap: None,
        };
    };

    let identifier_overlap = token_overlap(
        a.name_tokens
            .iter()
            .chain(a.parameter_names.iter())
            .map(String::as_str),
        b.name_tokens
            .iter()
            .chain(b.parameter_names.iter())
            .map(String::as_str),
    );
    let type_overlap = token_overlap(
        a.parameter_type_paths
            .iter()
            .chain(a.return_type_paths.iter())
            .map(String::as_str),
        b.parameter_type_paths
            .iter()
            .chain(b.return_type_paths.iter())
            .map(String::as_str),
    );
    let parameter_name_overlap = token_overlap(
        a.parameter_names.iter().map(String::as_str),
        b.parameter_names.iter().map(String::as_str),
    );
    let generic_overlap = token_overlap(
        a.generics.iter().map(String::as_str),
        b.generics.iter().map(String::as_str),
    );
    let parameter_count = count_similarity(a.parameter_count, b.parameter_count);
    let receiver = if a.receiver == b.receiver { 1.0 } else { 0.0 };
    let signature_similarity = (0.25 * identifier_overlap)
        + (0.10 * parameter_count)
        + (0.05 * parameter_name_overlap)
        + (0.45 * type_overlap)
        + (0.10 * generic_overlap)
        + (0.05 * receiver);

    SignatureComponents {
        signature_similarity: Some(signature_similarity),
        type_overlap: Some(type_overlap),
        identifier_overlap: Some(identifier_overlap),
    }
}

fn token_overlap<'a>(a: impl Iterator<Item = &'a str>, b: impl Iterator<Item = &'a str>) -> f64 {
    let a: HashSet<&str> = a.collect();
    let b: HashSet<&str> = b.collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(&b).count();
    let union = a.union(&b).count();
    if union == 0 {
        1.0
    } else {
        intersection as f64 / union as f64
    }
}

fn count_similarity(a: usize, b: usize) -> f64 {
    let max = a.max(b);
    if max == 0 {
        return 1.0;
    }
    1.0 - (a.abs_diff(b) as f64 / max as f64)
}

fn sorted_pair_key(i: usize, j: usize) -> (usize, usize) {
    if i <= j { (i, j) } else { (j, i) }
}

#[derive(Debug, Default)]
struct ScoreStats {
    pairs: Vec<ScoredPair>,
    exact_match_count: usize,
    below_threshold_count: usize,
    diff_filtered_count: usize,
}

impl ScoreStats {
    fn record(&mut self, score: PairScore, threshold: f64) {
        if score.exact_match {
            self.exact_match_count += 1;
        }
        if score.components.similarity < threshold {
            self.below_threshold_count += 1;
            return;
        }
        self.pairs.push(ScoredPair {
            i: score.i,
            j: score.j,
            components: score.components,
        });
    }

    fn merge(mut a: Self, mut b: Self) -> Self {
        a.below_threshold_count += b.below_threshold_count;
        a.diff_filtered_count += b.diff_filtered_count;
        a.exact_match_count += b.exact_match_count;
        a.pairs.append(&mut b.pairs);
        a
    }

    fn sorted(mut self) -> Self {
        self.pairs.sort_by_key(|pair| (pair.i, pair.j));
        self
    }

    fn scored_pair_count(&self) -> usize {
        self.pairs.len() + self.below_threshold_count
    }
}

#[derive(Debug, Clone)]
struct ScoredPair {
    i: usize,
    j: usize,
    components: SimilarityComponents,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};
    use proptest::collection::vec;
    use proptest::prelude::*;
    use rstest::rstest;
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

    const TWO_CLUSTER_FUNCTIONS: &str = r#"
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
fn gamma(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs {
        total += x;
    }
    total
}
fn delta(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs {
        total += x;
    }
    total
}
"#;

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

    #[test]
    fn exact_match_shortcut_requires_size_hash_and_tree_match() {
        let left = lens_domain::TreeNode::with_children(
            "Block",
            "",
            vec![lens_domain::TreeNode::leaf("Let")],
        );
        let same = left.clone();
        let same_size_different = lens_domain::TreeNode::with_children(
            "Block",
            "",
            vec![lens_domain::TreeNode::leaf("Return")],
        );
        let larger = lens_domain::TreeNode::with_children(
            "Block",
            "",
            vec![
                lens_domain::TreeNode::leaf("Let"),
                lens_domain::TreeNode::leaf("Return"),
            ],
        );

        let left_profile = TreeProfile::from_tree_for_scoring(&left);
        assert!(is_exact_match_without_distance(
            &left_profile,
            &TreeProfile::from_tree_for_scoring(&same),
            &left,
            &same,
            false,
        ));
        assert!(!is_exact_match_without_distance(
            &left_profile,
            &TreeProfile::from_tree_for_scoring(&same_size_different),
            &left,
            &same_size_different,
            false,
        ));
        assert!(!is_exact_match_without_distance(
            &left_profile,
            &TreeProfile::from_tree_for_scoring(&larger),
            &left,
            &larger,
            false,
        ));
    }

    #[test]
    fn exact_match_shortcut_honors_compare_values() {
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
        let left_profile = TreeProfile::from_tree_for_scoring(&left);
        let right_profile = TreeProfile::from_tree_for_scoring(&right);

        assert!(is_exact_match_without_distance(
            &left_profile,
            &right_profile,
            &left,
            &right,
            false,
        ));
        assert!(!is_exact_match_without_distance(
            &left_profile,
            &right_profile,
            &left,
            &right,
            true,
        ));
    }

    fn owned_function(name: &str, start_line: usize, end_line: usize) -> OwnedFunction {
        OwnedFunction {
            file: PathBuf::from("lib.rs"),
            rel_path: "lib.rs".to_owned(),
            is_test: false,
            def: lens_domain::FunctionDef {
                name: name.to_owned(),
                start_line,
                end_line,
                is_test: false,
                signature: None,
                tree: lens_domain::TreeNode::leaf("Block"),
            },
        }
    }

    #[test]
    fn diff_prefilter_keeps_only_pairs_touching_changed_lines() {
        let corpus = vec![
            owned_function("alpha", 1, 5),
            owned_function("beta", 10, 14),
            owned_function("gamma", 20, 24),
        ];
        let candidates = CandidatePairs {
            pairs: vec![(0, 1), (0, 2), (1, 2)],
            eligible_function_count: 3,
            size_filtered_count: 0,
            label_filtered_count: 0,
            arity_filtered_count: 0,
            shingle_filtered_count: 0,
            strategy: candidates::CandidatePairStrategy::Cartesian,
        };
        let changed_by_file = HashMap::from([(
            PathBuf::from("lib.rs"),
            vec![LineRange { start: 20, end: 20 }],
        )]);

        let (pairs, filtered) =
            filter_pairs_touching_changes(&corpus, &candidates, &changed_by_file);

        assert_eq!(pairs.as_ref(), &[(0, 2), (1, 2)]);
        assert_eq!(filtered, 1);
    }

    #[test]
    fn score_stats_record_and_merge_preserve_counts() {
        fn components(similarity: f64) -> SimilarityComponents {
            SimilarityComponents {
                similarity,
                body_similarity: similarity,
                signature_similarity: None,
                type_overlap: None,
                identifier_overlap: None,
            }
        }

        let mut stats = ScoreStats::default();
        stats.record(
            PairScore {
                i: 0,
                j: 1,
                components: components(1.0),
                exact_match: true,
            },
            0.85,
        );
        stats.record(
            PairScore {
                i: 0,
                j: 2,
                components: components(0.25),
                exact_match: false,
            },
            0.85,
        );

        let merged = ScoreStats::merge(
            stats,
            ScoreStats {
                pairs: vec![ScoredPair {
                    i: 2,
                    j: 3,
                    components: components(0.9),
                }],
                exact_match_count: 2,
                below_threshold_count: 3,
                diff_filtered_count: 4,
            },
        );

        let pairs: Vec<_> = merged
            .pairs
            .iter()
            .map(|pair| (pair.i, pair.j, pair.components.similarity))
            .collect();
        assert_eq!(pairs, vec![(0, 1, 1.0), (2, 3, 0.9)]);
        assert_eq!(merged.exact_match_count, 3);
        assert_eq!(merged.below_threshold_count, 4);
        assert_eq!(merged.diff_filtered_count, 4);
    }

    #[test]
    fn score_stats_keeps_scores_equal_to_threshold() {
        let mut stats = ScoreStats::default();
        stats.record(
            PairScore {
                i: 2,
                j: 4,
                components: SimilarityComponents {
                    similarity: 0.85,
                    body_similarity: 1.0,
                    signature_similarity: Some(0.25),
                    type_overlap: Some(0.0),
                    identifier_overlap: Some(0.5),
                },
                exact_match: false,
            },
            0.85,
        );

        assert_eq!(stats.pairs.len(), 1);
        assert_eq!(stats.below_threshold_count, 0);
    }

    #[test]
    fn body_candidate_threshold_reverses_combined_score_formula_and_clamps() {
        assert!((body_candidate_threshold(0.85) - 0.8125).abs() < 1e-9);
        assert_eq!(body_candidate_threshold(0.10), 0.0);
        assert_eq!(body_candidate_threshold(1.50), 1.0);
    }

    #[test]
    fn token_overlap_count_similarity_and_pair_keys_cover_edge_cases() {
        assert_eq!(token_overlap([].into_iter(), ["user"].into_iter()), 0.0);
        assert_eq!(
            token_overlap(["user", "id"].into_iter(), ["id", "order"].into_iter()),
            1.0 / 3.0,
        );
        assert_eq!(count_similarity(0, 0), 1.0);
        assert_eq!(count_similarity(2, 4), 0.5);
        assert_eq!(sorted_pair_key(5, 3), (3, 5));
    }

    fn rust_sig(
        name_tokens: &[&str],
        parameter_names: &[&str],
        parameter_type_paths: &[&str],
        return_type_paths: &[&str],
    ) -> lens_domain::FunctionSignature {
        lens_domain::FunctionSignature {
            name_tokens: name_tokens.iter().map(|s| (*s).to_owned()).collect(),
            parameter_count: parameter_names.len(),
            parameter_names: parameter_names.iter().map(|s| (*s).to_owned()).collect(),
            parameter_type_paths: parameter_type_paths
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            return_type_paths: return_type_paths.iter().map(|s| (*s).to_owned()).collect(),
            generics: Vec::new(),
            receiver: lens_domain::ReceiverShape::None,
        }
    }

    fn rust_sig_with_receiver(
        name_tokens: &[&str],
        parameter_names: &[&str],
        parameter_type_paths: &[&str],
        return_type_paths: &[&str],
        generics: &[&str],
        receiver: lens_domain::ReceiverShape,
    ) -> lens_domain::FunctionSignature {
        let mut sig = rust_sig(
            name_tokens,
            parameter_names,
            parameter_type_paths,
            return_type_paths,
        );
        sig.generics = generics.iter().map(|s| (*s).to_owned()).collect();
        sig.receiver = receiver;
        sig
    }

    #[test]
    fn signature_score_rewards_same_domain_types_over_same_body_different_types() {
        let same_domain_renamed = signature_components(
            Some(&rust_sig(&["validate"], &["id"], &["UserId"], &["bool"])),
            Some(&rust_sig(
                &["validate"],
                &["candidate"],
                &["UserId"],
                &["bool"],
            )),
        )
        .signature_similarity
        .unwrap();
        let different_domain_type = signature_components(
            Some(&rust_sig(&["validate"], &["id"], &["UserId"], &["bool"])),
            Some(&rust_sig(&["validate"], &["id"], &["OrderId"], &["bool"])),
        )
        .signature_similarity
        .unwrap();

        assert!(
            same_domain_renamed > different_domain_type,
            "renamed={same_domain_renamed}, different_type={different_domain_type}",
        );
    }

    #[test]
    fn signature_components_calculates_observable_subscores() {
        let left = rust_sig_with_receiver(
            &["get", "user"],
            &["id"],
            &["UserId"],
            &["User"],
            &["T: Clone"],
            lens_domain::ReceiverShape::Ref,
        );
        let right = rust_sig_with_receiver(
            &["get", "order"],
            &["other"],
            &["OrderId"],
            &["Order"],
            &["E: Clone"],
            lens_domain::ReceiverShape::RefMut,
        );

        let score = signature_components(Some(&left), Some(&right));

        assert_eq!(score.identifier_overlap, Some(0.2));
        assert_eq!(score.type_overlap, Some(0.0));
        assert!((score.signature_similarity.unwrap() - 0.15).abs() < 1e-9);

        let same_receiver = rust_sig_with_receiver(
            &["get", "order"],
            &["other"],
            &["OrderId"],
            &["Order"],
            &["E: Clone"],
            lens_domain::ReceiverShape::Ref,
        );
        let with_receiver_match = signature_components(Some(&left), Some(&same_receiver));
        assert!(
            with_receiver_match.signature_similarity.unwrap() > score.signature_similarity.unwrap()
        );

        let different_parameter_count = signature_components(
            Some(&rust_sig(&[], &["id"], &[], &[])),
            Some(&rust_sig(&[], &["id", "fallback"], &[], &[])),
        );
        assert!((different_parameter_count.signature_similarity.unwrap() - 0.8).abs() < 1e-9);
    }

    #[test]
    fn score_candidate_pair_combines_body_and_signature_scores() {
        let left_body = lens_domain::TreeNode::with_children(
            "Function",
            "",
            vec![
                lens_domain::TreeNode::leaf("FnSignature"),
                lens_domain::TreeNode::leaf("Block"),
            ],
        );
        let right_body = lens_domain::TreeNode::with_children(
            "Function",
            "",
            vec![
                lens_domain::TreeNode::leaf("FnSignature"),
                lens_domain::TreeNode::with_children(
                    "Block",
                    "",
                    vec![lens_domain::TreeNode::leaf("Return")],
                ),
            ],
        );
        let corpus = vec![
            OwnedFunction {
                file: PathBuf::from("lib.rs"),
                rel_path: "lib.rs".to_owned(),
                is_test: false,
                def: lens_domain::FunctionDef {
                    name: "left".to_owned(),
                    start_line: 1,
                    end_line: 5,
                    is_test: false,
                    signature: Some(rust_sig(&["left"], &["id"], &["UserId"], &["User"])),
                    tree: left_body,
                },
            },
            OwnedFunction {
                file: PathBuf::from("lib.rs"),
                rel_path: "lib.rs".to_owned(),
                is_test: false,
                def: lens_domain::FunctionDef {
                    name: "right".to_owned(),
                    start_line: 7,
                    end_line: 11,
                    is_test: false,
                    signature: Some(rust_sig(&["right"], &["id"], &["OrderId"], &["Order"])),
                    tree: right_body,
                },
            },
        ];
        let profiles = build_tree_profiles(&corpus, 1);

        let score =
            score_candidate_pair(&corpus, &profiles, 0, 1, &TSEDOptions::default()).unwrap();

        assert!(score.components.body_similarity < 1.0);
        assert!(score.components.signature_similarity.unwrap() < 0.5);
        assert!(
            (score.components.similarity
                - (BODY_SIMILARITY_WEIGHT * score.components.body_similarity
                    + SIGNATURE_SIMILARITY_WEIGHT
                        * score.components.signature_similarity.unwrap()))
            .abs()
                < 1e-9
        );
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
        let pairs = cluster["pairs"].as_array().unwrap();
        assert!(!pairs.is_empty());
        assert!(pairs[0]["similarity"].as_f64().is_some());
        assert!(pairs[0]["body_similarity"].as_f64().is_some());
    }

    fn assert_markdown_pair_report(out: &str) {
        assert!(out.contains("Similarity report"));
        assert!(out.contains("similar cluster"));
        assert!(out.contains("alpha"));
        assert!(out.contains("beta"));
    }

    /// Paired functions must surface matched names across formats and
    /// language parsers; only the rendered report shape differs.
    #[rstest]
    #[case::rust_json(
        "lib.rs",
        PAIRED_FUNCTIONS,
        OutputFormat::Json,
        assert_json_pair_report
    )]
    #[case::rust_markdown(
        "lib.rs",
        PAIRED_FUNCTIONS,
        OutputFormat::Md,
        assert_markdown_pair_report
    )]
    #[case::python_json(
        "lib.py",
        r#"
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
"#,
        OutputFormat::Json,
        assert_json_pair_report
    )]
    #[case::go_json(
        "lib.go",
        r#"
package p

func alpha(xs []int) int {
    total := 0
    for _, x := range xs {
        total += x
    }
    return total
}

func beta(ys []int) int {
    sum := 0
    for _, y := range ys {
        sum += y
    }
    return sum
}
"#,
        OutputFormat::Json,
        assert_json_pair_report
    )]
    fn report_renders_paired_functions(
        #[case] file_name: &str,
        #[case] src: &str,
        #[case] format: OutputFormat,
        #[case] assert_report: fn(&str),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), file_name, src);
        let out = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, format)
            .unwrap();
        assert_report(&out);
    }

    #[test]
    fn rust_json_pairs_emit_signature_components() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
struct UserId(u64);
struct OrderId(u64);

fn validate_user_id(id: UserId) -> bool {
    let raw = id.0;
    if raw == 0 {
        false
    } else {
        raw > 10
    }
}

fn validate_order_id(id: OrderId) -> bool {
    let raw = id.0;
    if raw == 0 {
        false
    } else {
        raw > 10
    }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);

        let json = SimilarityAnalyzer::new()
            .with_threshold(0.8)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let pair = &parsed["clusters"][0]["pairs"][0];
        let similarity = pair["similarity"].as_f64().unwrap();
        let body_similarity = pair["body_similarity"].as_f64().unwrap();
        let signature_similarity = pair["signature_similarity"].as_f64().unwrap();

        assert!(
            similarity < body_similarity,
            "signature-aware score should lower identical-body domain mismatch: {pair}",
        );
        assert!(signature_similarity < 1.0, "got {pair}");
        assert!(pair["type_overlap"].as_f64().unwrap() < 1.0, "got {pair}");
        assert!(pair["identifier_overlap"].as_f64().is_some());
    }

    #[test]
    fn markdown_top_caps_clusters_without_truncating_json() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", TWO_CLUSTER_FUNCTIONS);

        let full_md = SimilarityAnalyzer::new()
            .with_threshold(0.95)
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert_eq!(
            full_md.matches("\n- 2 functions").count(),
            2,
            "got: {full_md}",
        );

        let top_md = SimilarityAnalyzer::new()
            .with_threshold(0.95)
            .with_top(Some(1))
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(top_md.contains("Top 1 similar cluster(s) of 2 total"));
        assert_eq!(
            top_md.matches("\n- 2 functions").count(),
            1,
            "got: {top_md}",
        );

        let json = SimilarityAnalyzer::new()
            .with_threshold(0.95)
            .with_top(Some(1))
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cluster_count"], 2);
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
    fn only_tests_keeps_test_functions_inside_non_test_rust_files() {
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

        let json = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_only_tests(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["function_count"], 2, "got {parsed}");
        let names: Vec<&str> = parsed["clusters"][0]["functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["alpha", "beta"]);
        assert!(
            parsed["clusters"][0]["functions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["is_test"].as_bool() == Some(true))
        );
    }

    #[test]
    fn all_mode_does_not_compare_test_functions_to_production_functions() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn production(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}

#[test]
fn test_production(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);

        let json = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["function_count"], 2, "got {parsed}");
        assert_eq!(parsed["cluster_count"], 0, "got {parsed}");
    }

    #[rstest]
    #[case::rust_cfg_test_module(
        "lib.rs",
        r#"
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
"#
    )]
    #[case::python_pytest_functions(
        "lib.py",
        r#"
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
"#
    )]
    #[case::go_test_functions(
        "lib.go",
        r#"
package p

import "testing"

func production(xs []int) int {
    total := 0
    for _, x := range xs {
        total += x
    }
    return total
}

func TestAlpha(t *testing.T) {
    a := 1
    b := 2
    c := 3
    if a+b+c != 6 {
        t.Fatal("bad")
    }
}

func TestBeta(t *testing.T) {
    a := 1
    b := 2
    c := 3
    if a+b+c != 6 {
        t.Fatal("bad")
    }
}
"#
    )]
    #[case::typescript_xunit_functions(
        "lib.ts",
        r#"
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
"#
    )]
    fn exclude_tests_drops_test_functions_from_report(#[case] file_name: &str, #[case] src: &str) {
        // Each case has one production function plus two parallel tests.
        // `--exclude-tests` should drop the test pair before similarity runs.
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), file_name, src);

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
        assert!(
            parsed["clusters"][0]["functions"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["is_test"].as_bool() == Some(true))
        );

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

    #[test]
    fn enforce_candidate_pair_limit_surfaces_concrete_numbers() {
        let err = enforce_candidate_pair_limit(20_000, 13_000_001, 13_000_000, 5, "lsh")
            .expect_err("candidate overage should be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("13_000_001") || msg.contains("13000001"),
            "error should include candidate pair count: {msg}"
        );
        assert!(
            msg.contains("199990000"),
            "error should include theoretical pair count: {msg}"
        );
        assert!(
            matches!(err, AnalyzerError::SimilarityScopeTooBroad { .. }),
            "unexpected error variant: {err}"
        );
    }
}
