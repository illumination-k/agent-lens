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

use super::runner::{FilterConfig, delegate_filter_builders, render_report};
use super::{AnalyzerError, LineRange, OutputFormat, changed_line_ranges};

mod candidates;
mod corpus;
mod doc;
mod extract;
mod paired;
mod report;
mod token;

use candidates::{
    CandidatePairs, TreeProfile, candidate_pairs, eligible_function_count, similarity_uses_lsh,
};
#[cfg(test)]
use candidates::{CheapFilter, tsed_upper_bound_filter};
use corpus::{OwnedFunction, collect_corpus};
pub use paired::PairKey;
use paired::{PairedCandidate, name_matched_pairs};
use report::{
    ClusterView, PairedReport, PairedReportInputs, Report, ScoredMatch, build_drift_groups,
    format_markdown, format_paired_markdown,
};
use token::TokenProfile;

/// Default similarity threshold. Picked to match the cutoff used by the
/// PostToolUse `similarity` hook so the on-demand analyzer reports the
/// same pairs that show up in the hook's transcript message.
pub const DEFAULT_THRESHOLD: f64 = 0.85;

/// Default minimum line count for a function to be considered. Mirrors the
/// `--min-lines` default in `similarity-ts`: tiny functions (one-liners,
/// trivial getters) form too many spurious matches.
pub const DEFAULT_MIN_LINES: usize = 5;

/// Default floor for `--paired-by`. A name-matched pair scoring below
/// this shares a name and essentially nothing else, which in practice
/// means two unrelated functions that happen to be called the same thing
/// (every analyzer's own `format_markdown`, every type's `new`) rather
/// than one implementation that drifted from another. Drift lives in the
/// band between this floor and the threshold; below it the report would
/// be dominated by namesakes and the ascending sort would put them first.
/// Pass `--drift-floor 0` to see them anyway.
pub const DEFAULT_DRIFT_FLOOR: f64 = 0.3;

const PROFILE_TARGET: &str = "agent_lens::similarity_profile";
const BODY_SIMILARITY_WEIGHT: f64 = 0.8;
const SIGNATURE_SIMILARITY_WEIGHT: f64 = 0.2;
/// Hard cap for pairs scored by `analyze similarity`.
///
/// Even with LSH enabled, very large corpora can still produce a huge
/// candidate set. Keep a guardrail so runs fail fast with an actionable
/// error instead of spending minutes in pairwise scoring.
///
/// The cap is calibrated from a historical dense benchmark run before
/// signature component reporting:
/// `similarity_directory_lsh_dense_1024_functions` measured ~370–384 ms
/// (2026-04-28, local `cargo bench` in this repo). A 1024-function full
/// pair set is 523,776 pairs; scaling that to a practical upper budget
/// around 10 seconds gives a limit around 13M pairs.
const MAX_CANDIDATE_PAIRS: usize = 13_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FunctionSelection {
    #[default]
    All,
    ExcludeTests,
    OnlyTests,
}

impl FunctionSelection {
    /// Build a selection from the same `(only_tests, exclude_tests)`
    /// CLI args that drive the path-level filter. The CLI is the
    /// canonical caller — passing both flags from one place lets path
    /// and function-level test filtering stay in lock-step without the
    /// analyzer having to read them back from the path filter.
    pub fn from_args(only_tests: bool, exclude_tests: bool) -> Self {
        if only_tests {
            Self::OnlyTests
        } else if exclude_tests {
            Self::ExcludeTests
        } else {
            Self::All
        }
    }

    pub(super) fn includes(self, is_test: bool) -> bool {
        match self {
            Self::All => true,
            Self::ExcludeTests => !is_test,
            Self::OnlyTests => is_test,
        }
    }
}

/// Algorithm used to score how similar two function bodies are.
///
/// Both methods feed the same `0.8 * body + 0.2 * signature` blend and
/// the same clustering, so only the body score differs. The choice is a
/// recall/speed vs precision trade-off, not a different report shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SimilarityMethod {
    /// APTED tree-edit distance over the body AST. Precise and the most
    /// faithful to structural change, but the costliest to compute.
    #[default]
    Tsed,
    /// Weighted Jaccard overlap of the body's preorder token k-grams.
    /// Cheaper than TSED and more tolerant of reordered code, at the
    /// cost of some precision.
    Token,
}

impl SimilarityMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Tsed => "tsed",
            Self::Token => "token",
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
    filter: FilterConfig,
    selection: FunctionSelection,
    min_lines: usize,
    top: Option<usize>,
    method: SimilarityMethod,
    sweep: Option<Vec<f64>>,
    doc_overlap: bool,
    paired_by: Option<PairKey>,
    drift_floor: f64,
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
            filter: FilterConfig::default(),
            selection: FunctionSelection::All,
            min_lines: DEFAULT_MIN_LINES,
            top: None,
            method: SimilarityMethod::default(),
            sweep: None,
            doc_overlap: false,
            paired_by: None,
            drift_floor: DEFAULT_DRIFT_FLOOR,
        }
    }

    with_setter! {
        /// Override the similarity threshold. Callers passing a non-default
        /// value via `--threshold` go through here.
        fn with_threshold, threshold: f64
    }

    delegate_filter_builders!(filter);

    with_setter! {
        /// Function-level test filter. Path-level test filtering is set
        /// independently via `with_only_tests` / `with_exclude_tests`;
        /// the CLI keeps both in sync by deriving this value from the
        /// same `(only_tests, exclude_tests)` args via
        /// [`FunctionSelection::from_args`].
        fn with_function_selection, selection: FunctionSelection
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

    with_setter! {
        /// Pick the body-scoring algorithm. Defaults to
        /// [`SimilarityMethod::Tsed`]; [`SimilarityMethod::Token`] swaps
        /// in the cheaper token k-gram score.
        fn with_method, method: SimilarityMethod
    }

    with_setter! {
        /// Surface the doc-comment overlap in the markdown report. It is
        /// a diagnostic component that never feeds `similarity`, and the
        /// JSON report always carries it per pair, so this only controls
        /// whether markdown rolls it up per cluster.
        fn with_doc_overlap, doc_overlap: bool
    }

    with_setter! {
        /// Switch to name-anchored pairing: match functions by the given
        /// [`PairKey`] first, score second, and report every matched pair
        /// regardless of threshold. `None` (the default) keeps the
        /// threshold-clustering report.
        fn with_paired_by, paired_by: Option<PairKey>
    }

    with_setter! {
        /// Drop name-matched pairs scoring below this from the
        /// `--paired-by` report. Defaults to [`DEFAULT_DRIFT_FLOOR`];
        /// `0.0` reports every match. Ignored outside paired mode.
        fn with_drift_floor, drift_floor: f64
    }

    /// Enable multi-threshold sweep mode. Pairs are scored and clustered
    /// once at the lowest rung of `ladder` (the floor), and every reported
    /// cluster is annotated with the highest rung at which its complete-link
    /// structure survives intact. This turns the "run at 0.85, see nothing,
    /// re-run at 0.75, re-run at 0.6" workflow into a single pass that
    /// distinguishes verbatim clones from merely structural parallels.
    ///
    /// The ladder is sorted ascending and de-duplicated; an empty ladder is
    /// treated as "no sweep" and leaves the single-`--threshold` behaviour
    /// untouched. When set, the sweep floor supersedes `--threshold` as the
    /// clustering cut.
    pub fn with_sweep(mut self, ladder: Option<Vec<f64>>) -> Self {
        self.sweep = ladder.and_then(|mut rungs| {
            rungs.retain(|r| r.is_finite());
            rungs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            rungs.dedup();
            (!rungs.is_empty()).then_some(rungs)
        });
        self
    }

    /// Clustering cut for this run: the sweep floor when sweeping, otherwise
    /// the plain `--threshold`. Candidate generation, scoring, and the
    /// complete-link cut all key off this single value so sweep mode simply
    /// lowers the cut and lets the per-cluster annotation carry the rest.
    fn cluster_threshold(&self) -> f64 {
        self.sweep
            .as_deref()
            .and_then(|ladder| ladder.first().copied())
            .unwrap_or(self.threshold)
    }

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let started = Instant::now();
        let corpus = collect_corpus(path, &self.filter.path_filter(), self.selection)?;
        let function_count = corpus.len();
        if let Some(key) = self.paired_by {
            return self.analyze_paired(path, &corpus, key, format, started);
        }
        let clusters = self.find_clusters(&corpus)?;
        let report = Report::new(
            path,
            self.method.as_str(),
            self.cluster_threshold(),
            self.min_lines,
            function_count,
            self.sweep.as_deref(),
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
        render_report(&report, format, || {
            format_markdown(&report, self.top, self.doc_overlap)
        })
    }

    /// Name-anchored counterpart to [`Self::find_clusters`]: pair first
    /// by name key, score second, and report every matched pair whether
    /// or not it clears the threshold.
    ///
    /// The threshold stops being a filter here and becomes a label —
    /// clustering can only report pairs that are *still* similar, so it
    /// structurally cannot surface a pair of siblings that has drifted
    /// apart, which is the pair most likely to be a missed sync.
    fn analyze_paired(
        &self,
        path: &Path,
        corpus: &[OwnedFunction],
        key: PairKey,
        format: OutputFormat,
        started: Instant,
    ) -> Result<String, AnalyzerError> {
        let candidates = name_matched_pairs(corpus, self.min_lines, key);
        let changed_by_file = self.changed_ranges_for_run(corpus);
        let matched = self.paired_pairs_to_score(corpus, &candidates.pairs, &changed_by_file);
        enforce_candidate_pair_limit(
            candidates.eligible_function_count,
            matched.len(),
            MAX_CANDIDATE_PAIRS,
            self.min_lines,
            key.as_str(),
        )?;

        let profiles = match self.method {
            SimilarityMethod::Tsed => build_tree_profiles(corpus, self.min_lines),
            SimilarityMethod::Token => Vec::new(),
        };
        let index_pairs: Vec<(usize, usize)> = matched.iter().map(|c| (c.i, c.j)).collect();
        // Threshold 0 keeps every scored pair: in this mode the cut
        // labels drift instead of filtering the report.
        let mut score_stats = self.score_pairs(corpus, &profiles, &index_pairs, 0.0);
        annotate_doc_overlap(corpus, &mut score_stats.pairs);

        let key_by_pair: HashMap<(usize, usize), usize> = matched
            .iter()
            .map(|c| (sorted_pair_key(c.i, c.j), c.key))
            .collect();
        let kept: Vec<ScoredMatch> = score_stats
            .pairs
            .iter()
            .filter(|pair| pair.components.similarity >= self.drift_floor)
            .filter_map(|pair| {
                Some(ScoredMatch {
                    key: *key_by_pair.get(&sorted_pair_key(pair.i, pair.j))?,
                    i: pair.i,
                    j: pair.j,
                    components: pair.components,
                })
            })
            .collect();
        let below_floor_count = score_stats.pairs.len() - kept.len();
        let groups = build_drift_groups(corpus, &candidates.keys, &kept, self.threshold);

        let report = PairedReport::new(
            PairedReportInputs {
                path,
                method: self.method.as_str(),
                paired_by: key.as_str(),
                threshold: self.threshold,
                drift_floor: self.drift_floor,
                min_lines: self.min_lines,
                function_count: corpus.len(),
                same_file_pair_count: candidates.same_file_pair_count,
                below_floor_count,
            },
            groups,
        );
        debug!(
            target: PROFILE_TARGET,
            path = %path.display(),
            paired_by = key.as_str(),
            function_count = corpus.len(),
            name_matched_pair_count = matched.len(),
            same_file_pair_count = candidates.same_file_pair_count,
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "analyze similarity --paired-by finished"
        );
        render_report(&report, format, || {
            format_paired_markdown(&report, self.top)
        })
    }

    /// `--diff-only` for the paired path. Same rule as the clustering
    /// path: keep a pair when either side overlaps a changed line range.
    fn paired_pairs_to_score(
        &self,
        corpus: &[OwnedFunction],
        pairs: &[PairedCandidate],
        changed_by_file: &HashMap<PathBuf, Vec<LineRange>>,
    ) -> Vec<PairedCandidate> {
        if !self.filter.diff_only() {
            return pairs.to_vec();
        }
        pairs
            .iter()
            .copied()
            .filter(|c| {
                corpus
                    .get(c.i)
                    .zip(corpus.get(c.j))
                    .is_some_and(|(a, b)| pair_touches_changes(a, b, changed_by_file))
            })
            .collect()
    }

    /// Pairwise scoring over the corpus (TSED or token, per
    /// [`SimilarityMethod`]), then complete-link clustering. Inlined
    /// rather than calling [`lens_domain::find_similar_pair_indices`] +
    /// [`lens_domain::cluster_similar_pairs`] in two passes so the per-pair
    /// `--diff-only` filter sees the file/line metadata that domain doesn't
    /// know about.
    fn find_clusters<'a>(
        &self,
        corpus: &'a [OwnedFunction],
    ) -> Result<Vec<ClusterView<'a>>, AnalyzerError> {
        let started = Instant::now();
        let threshold = self.cluster_threshold();
        let changed_by_file = self.changed_ranges_for_run(corpus);
        // TSED scoring and its cheap candidate filters both run off the
        // tree profiles; the token method needs neither, so skip the work.
        let profiles = match self.method {
            SimilarityMethod::Tsed => build_tree_profiles(corpus, self.min_lines),
            SimilarityMethod::Token => Vec::new(),
        };
        let candidate_started = Instant::now();
        let candidate_threshold = body_candidate_threshold(threshold);
        let candidates = candidate_pairs(
            corpus,
            self.min_lines,
            &profiles,
            candidate_threshold,
            &self.opts,
            self.method,
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
        let mut score_stats = self.score_pairs(corpus, &profiles, &pairs_to_score, threshold);
        score_stats.diff_filtered_count = diff_prefiltered_count;
        log_score_stats(&candidates, &score_stats, score_started, self.method);
        annotate_doc_overlap(corpus, &mut score_stats.pairs);

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
        let mut clusters: Vec<_> = cluster_similar_pairs(&domain_pairs, threshold)
            .into_iter()
            .map(|c| ClusterView::from_domain(corpus, c, &pair_scores))
            .collect();
        if let Some(ladder) = self.sweep.as_deref() {
            report::annotate_sweep_survival(&mut clusters, ladder);
        }
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
        if !self.filter.diff_only() {
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
        if !self.filter.diff_only() {
            return (Cow::Borrowed(candidates.pairs.as_slice()), 0);
        }
        filter_pairs_touching_changes(corpus, candidates, changed_by_file)
    }

    /// Score `pairs` with the configured [`SimilarityMethod`]. TSED reads
    /// the prebuilt tree `profiles`; the token method builds its own
    /// flattened token profiles, which `profiles` does not carry.
    fn score_pairs(
        &self,
        corpus: &[OwnedFunction],
        profiles: &[TreeProfile],
        pairs: &[(usize, usize)],
        threshold: f64,
    ) -> ScoreStats {
        match self.method {
            SimilarityMethod::Tsed => {
                score_candidate_pairs(corpus, profiles, pairs, threshold, &self.opts)
            }
            SimilarityMethod::Token => {
                let token_profiles = build_token_profiles(corpus, self.opts.apted.compare_values);
                score_token_candidate_pairs(corpus, &token_profiles, pairs, threshold)
            }
        }
    }
}

fn build_tree_profiles(corpus: &[OwnedFunction], min_lines: usize) -> Vec<TreeProfile> {
    let use_lsh_profiles = similarity_uses_lsh(eligible_function_count(corpus, min_lines));
    if use_lsh_profiles {
        corpus
            .par_iter()
            .map(|f| TreeProfile::from_tree_for_scoring(f.body_tree()))
            .collect()
    } else {
        corpus
            .iter()
            .map(|f| TreeProfile::from_tree(f.body_tree()))
            .collect()
    }
}

fn build_token_profiles(corpus: &[OwnedFunction], compare_values: bool) -> Vec<TokenProfile> {
    corpus
        .par_iter()
        .map(|f| TokenProfile::from_tree(f.body_tree(), compare_values))
        .collect()
}

fn body_candidate_threshold(threshold: f64) -> f64 {
    ((threshold - SIGNATURE_SIMILARITY_WEIGHT) / BODY_SIMILARITY_WEIGHT).clamp(0.0, 1.0)
}

/// Fill [`SimilarityComponents::doc_overlap`] on the pairs that survived
/// threshold filtering. Runs outside the scoring hot path: only reported
/// pairs pay for doc tokenization, so corpora with millions of candidate
/// pairs see no extra scoring cost.
fn annotate_doc_overlap(corpus: &[OwnedFunction], pairs: &mut [ScoredPair]) {
    for pair in pairs {
        let (Some(a), Some(b)) = (corpus.get(pair.i), corpus.get(pair.j)) else {
            continue;
        };
        pair.components.doc_overlap = doc::doc_overlap(a.doc(), b.doc());
    }
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

fn log_score_stats(
    candidates: &CandidatePairs,
    score_stats: &ScoreStats,
    started: Instant,
    method: SimilarityMethod,
) {
    debug!(
        target: PROFILE_TARGET,
        method = method.as_str(),
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
        "similarity scoring finished"
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
    let body_a = a.body_tree();
    let body_b = b.body_tree();
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
    let signature = signature_components(a.signature(), b.signature());
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
            doc_overlap: None,
        },
        exact_match,
    })
}

fn score_token_candidate_pairs(
    corpus: &[OwnedFunction],
    token_profiles: &[TokenProfile],
    pairs: &[(usize, usize)],
    threshold: f64,
) -> ScoreStats {
    pairs
        .par_iter()
        .fold(ScoreStats::default, |mut stats, &(i, j)| {
            if let Some(score) = score_token_candidate_pair(corpus, token_profiles, i, j) {
                stats.record(score, threshold);
            }
            stats
        })
        .reduce(ScoreStats::default, ScoreStats::merge)
        .sorted()
}

fn score_token_candidate_pair(
    corpus: &[OwnedFunction],
    token_profiles: &[TokenProfile],
    i: usize,
    j: usize,
) -> Option<PairScore> {
    let a = corpus.get(i)?;
    let b = corpus.get(j)?;
    let body_similarity = token::token_similarity(token_profiles.get(i)?, token_profiles.get(j)?);
    let signature = signature_components(a.signature(), b.signature());
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
            doc_overlap: None,
        },
        exact_match: body_similarity >= 1.0,
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
    /// Word-level overlap of the two functions' doc comments. A
    /// diagnostic component only — it does not feed `similarity` — and
    /// filled by [`annotate_doc_overlap`] after threshold filtering so
    /// the scoring hot path never tokenizes doc prose. `None` unless
    /// both sides carry doc text.
    pub(super) doc_overlap: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct SignatureComponents {
    signature_similarity: Option<f64>,
    type_overlap: Option<f64>,
    identifier_overlap: Option<f64>,
}

fn signature_components(
    a: Option<&lens_domain::SignatureShape>,
    b: Option<&lens_domain::SignatureShape>,
) -> SignatureComponents {
    let (Some(a), Some(b)) = (a, b) else {
        return SignatureComponents {
            signature_similarity: None,
            type_overlap: None,
            identifier_overlap: None,
        };
    };

    let identifier_overlap = token_overlap(
        a.name_tokens().chain(a.parameter_names()),
        b.name_tokens().chain(b.parameter_names()),
    );
    let type_overlap = token_overlap(
        a.parameter_type_paths()
            .chain(a.return_type_paths.iter().map(String::as_str)),
        b.parameter_type_paths()
            .chain(b.return_type_paths.iter().map(String::as_str)),
    );
    let parameter_name_overlap = token_overlap(a.parameter_names(), b.parameter_names());
    let generic_overlap = token_overlap(a.generics(), b.generics());
    let parameter_count = count_similarity(a.parameter_count(), b.parameter_count());
    let receiver = if a.receiver_kind() == b.receiver_kind() {
        1.0
    } else {
        0.0
    };
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
            .any(|r| r.overlaps(f.start_line(), f.end_line()))
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

    /// Two structurally identical functions whose doc comments differ by
    /// exactly one word. Drives the `doc_overlap` checks on both the JSON
    /// and the markdown side so a single source pins the expected 4/6.
    const DOCUMENTED_PAIR: &str = r#"
/// Validate the user id before persisting.
fn validate_user(id: u64) -> bool {
    let raw = id;
    if raw == 0 {
        false
    } else {
        raw > 10
    }
}

/// Validate the order id before persisting.
fn validate_order(id: u64) -> bool {
    let raw = id;
    if raw == 0 {
        false
    } else {
        raw > 10
    }
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
            shape: lens_domain::FunctionShape::from(lens_domain::FunctionDef {
                name: name.to_owned(),
                start_line,
                end_line,
                is_test: false,
                signature: None,
                doc: None,
                tree: lens_domain::TreeNode::leaf("Block"),
            }),
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
    fn function_touches_changes_uses_actual_start_line() {
        let function = owned_function("target", 10, 14);
        let at_start = HashMap::from([(
            PathBuf::from("lib.rs"),
            vec![LineRange { start: 10, end: 10 }],
        )]);
        let before_start = HashMap::from([(
            PathBuf::from("lib.rs"),
            vec![LineRange { start: 9, end: 9 }],
        )]);

        assert!(function_touches_changes(&function, &at_start));
        assert!(!function_touches_changes(&function, &before_start));
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
                doc_overlap: None,
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
                    doc_overlap: None,
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
    fn function_selection_from_args_maps_each_combo() {
        // CLI exposes `only_tests` and `exclude_tests` as mutually
        // exclusive flags (clap `conflicts_with`). The mapping pinned
        // here is the contract the trait impl in `cli.rs` relies on.
        assert_eq!(
            FunctionSelection::from_args(false, false),
            FunctionSelection::All
        );
        assert_eq!(
            FunctionSelection::from_args(true, false),
            FunctionSelection::OnlyTests
        );
        assert_eq!(
            FunctionSelection::from_args(false, true),
            FunctionSelection::ExcludeTests
        );
        // Both true is impossible via the CLI but the mapping still
        // needs a deterministic answer; only_tests wins.
        assert_eq!(
            FunctionSelection::from_args(true, true),
            FunctionSelection::OnlyTests
        );
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
    ) -> lens_domain::SignatureShape {
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
        .into()
    }

    fn rust_sig_with_receiver(
        name_tokens: &[&str],
        parameter_names: &[&str],
        parameter_type_paths: &[&str],
        return_type_paths: &[&str],
        generics: &[&str],
        receiver: lens_domain::ReceiverShape,
    ) -> lens_domain::SignatureShape {
        let mut sig = lens_domain::FunctionSignature {
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
        };
        sig.generics = generics.iter().map(|s| (*s).to_owned()).collect();
        sig.receiver = receiver;
        sig.into()
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
                shape: lens_domain::FunctionShape::from(lens_domain::FunctionDef {
                    name: "left".to_owned(),
                    start_line: 1,
                    end_line: 5,
                    is_test: false,
                    signature: Some(lens_domain::FunctionSignature {
                        name_tokens: vec!["left".to_owned()],
                        parameter_count: 1,
                        parameter_names: vec!["id".to_owned()],
                        parameter_type_paths: vec!["UserId".to_owned()],
                        return_type_paths: vec!["User".to_owned()],
                        generics: Vec::new(),
                        receiver: lens_domain::ReceiverShape::None,
                    }),
                    doc: None,
                    tree: left_body,
                }),
            },
            OwnedFunction {
                file: PathBuf::from("lib.rs"),
                rel_path: "lib.rs".to_owned(),
                is_test: false,
                shape: lens_domain::FunctionShape::from(lens_domain::FunctionDef {
                    name: "right".to_owned(),
                    start_line: 7,
                    end_line: 11,
                    is_test: false,
                    signature: Some(lens_domain::FunctionSignature {
                        name_tokens: vec!["right".to_owned()],
                        parameter_count: 1,
                        parameter_names: vec!["id".to_owned()],
                        parameter_type_paths: vec!["OrderId".to_owned()],
                        return_type_paths: vec!["Order".to_owned()],
                        generics: Vec::new(),
                        receiver: lens_domain::ReceiverShape::None,
                    }),
                    doc: None,
                    tree: right_body,
                }),
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
        // Neither function is documented, so the diagnostic doc component
        // must be absent rather than reported as 0.
        assert!(pair["doc_overlap"].is_null(), "got {pair}");
    }

    #[test]
    fn rust_json_pairs_emit_doc_overlap_when_both_sides_documented() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", DOCUMENTED_PAIR);

        let json = SimilarityAnalyzer::new()
            .with_threshold(0.8)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let pair = &parsed["clusters"][0]["pairs"][0];
        let doc_overlap = pair["doc_overlap"].as_f64().unwrap();
        // Docs differ by exactly one word out of a 7-word union
        // ({validate, user, order, id, before, persisting} after
        // stopword removal: user vs order unique, 4 shared → 4/6).
        assert!(
            (doc_overlap - 4.0 / 6.0).abs() < 1e-9,
            "got {doc_overlap}: {pair}"
        );
        // The blended similarity must be unaffected by doc text: it is
        // exactly the body/signature blend, doc_overlap is diagnostic.
        let similarity = pair["similarity"].as_f64().unwrap();
        let body = pair["body_similarity"].as_f64().unwrap();
        let signature = pair["signature_similarity"].as_f64().unwrap();
        assert!(
            (similarity
                - (BODY_SIMILARITY_WEIGHT * body + SIGNATURE_SIMILARITY_WEIGHT * signature))
                .abs()
                < 1e-9,
            "doc text leaked into the blended score: {pair}"
        );
    }

    /// Markdown stays silent about the doc component until asked, while
    /// JSON carries it either way — the flag controls rendering, not
    /// computation.
    #[test]
    fn markdown_reports_doc_overlap_only_under_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", DOCUMENTED_PAIR);

        let plain = SimilarityAnalyzer::new()
            .with_threshold(0.8)
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(!plain.contains("doc overlap"), "got: {plain}");

        let annotated = SimilarityAnalyzer::new()
            .with_threshold(0.8)
            .with_doc_overlap(true)
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        // 4/6 shared doc words, on the cluster's single pair.
        assert!(
            annotated.contains("doc overlap 67–67% (1/1 pairs documented)"),
            "got: {annotated}",
        );

        // The flag is markdown-only: JSON is byte-identical with and
        // without it, and already carries the per-pair value.
        let with = SimilarityAnalyzer::new()
            .with_threshold(0.8)
            .with_doc_overlap(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let without = SimilarityAnalyzer::new()
            .with_threshold(0.8)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        assert_eq!(with, without);
    }

    /// An undocumented cluster must say so rather than silently omit the
    /// annotation the caller explicitly asked for — "no doc text" and
    /// "flag not set" are different facts.
    #[test]
    fn markdown_doc_overlap_marks_undocumented_clusters() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", PAIRED_FUNCTIONS);

        let md = SimilarityAnalyzer::new()
            .with_doc_overlap(true)
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("doc overlap n/a (0/1 pairs documented)"),
            "got: {md}",
        );
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

    /// A verbatim pair (alpha/beta) and a looser structural pair
    /// (gamma/delta) sit in different similarity bands. Used by the sweep
    /// tests so one source string exercises both the high and low rungs.
    const SWEEP_FUNCTIONS: &str = r#"
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
        if *x > 0 {
            total += x;
        }
    }
    total
}
fn delta(ys: &[i64]) -> i64 {
    let mut sum = 0;
    for y in ys {
        if *y > 1 {
            sum += y;
        }
    }
    sum
}
"#;

    #[test]
    fn sweep_annotates_each_cluster_with_its_highest_surviving_rung() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", SWEEP_FUNCTIONS);
        let json = SimilarityAnalyzer::new()
            .with_sweep(Some(vec![0.6, 0.75, 0.85]))
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        // The floor (lowest rung) is the clustering cut, and the ladder is
        // echoed back for the consumer.
        assert!((parsed["threshold"].as_f64().unwrap() - 0.6).abs() < 1e-9);
        assert_eq!(parsed["sweep"], serde_json::json!([0.6, 0.75, 0.85]));
        assert_eq!(parsed["cluster_count"], 2);

        // Verbatim clones survive the top rung and sort first; the looser
        // structural pair only survives the middle rung.
        let survivals: Vec<f64> = parsed["clusters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["survives_at_threshold"].as_f64().unwrap())
            .collect();
        assert_eq!(survivals, vec![0.85, 0.75]);
    }

    #[test]
    fn sweep_floor_surfaces_pairs_a_plain_threshold_would_drop() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", SWEEP_FUNCTIONS);

        // At the default 0.85 the structural gamma/delta pair scores below
        // the cut and vanishes; only the verbatim cluster survives.
        let strict = SimilarityAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let strict: serde_json::Value = serde_json::from_str(&strict).unwrap();
        assert_eq!(strict["cluster_count"], 1);

        // Sweeping down to a 0.6 floor recovers it in the same run.
        let swept = SimilarityAnalyzer::new()
            .with_sweep(Some(vec![0.6, 0.75, 0.85]))
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let swept: serde_json::Value = serde_json::from_str(&swept).unwrap();
        assert_eq!(swept["cluster_count"], 2);
    }

    #[test]
    fn sweep_markdown_header_lists_the_ladder_and_tags_clusters() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", SWEEP_FUNCTIONS);
        let md = SimilarityAnalyzer::new()
            .with_sweep(Some(vec![0.6, 0.75, 0.85]))
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("sweep [0.60, 0.75, 0.85]"), "got {md}");
        assert!(md.contains("[survives ≥0.85]"), "got {md}");
        assert!(md.contains("[survives ≥0.75]"), "got {md}");
    }

    #[test]
    fn with_sweep_sorts_dedups_and_treats_empty_as_no_sweep() {
        // Out-of-order, duplicated input: the floor (and thus the
        // clustering cut) is the smallest distinct rung.
        let messy = SimilarityAnalyzer::new().with_sweep(Some(vec![0.85, 0.6, 0.75, 0.6]));
        assert_eq!(messy.sweep.as_deref(), Some([0.6, 0.75, 0.85].as_slice()));
        assert!((messy.cluster_threshold() - 0.6).abs() < 1e-9);

        // An empty ladder leaves the plain `--threshold` path intact.
        let none = SimilarityAnalyzer::new()
            .with_threshold(0.9)
            .with_sweep(Some(Vec::new()));
        assert!(none.sweep.is_none());
        assert!((none.cluster_threshold() - 0.9).abs() < 1e-9);
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
            .with_function_selection(FunctionSelection::OnlyTests)
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
            .with_function_selection(FunctionSelection::ExcludeTests)
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
        |e: &AnalyzerError| matches!(e, AnalyzerError::PathNotFound { .. }),
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
    fn report_records_the_scoring_method() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", PAIRED_FUNCTIONS);

        let tsed = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&tsed).unwrap();
        assert_eq!(parsed["method"], "tsed");

        let token = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_method(SimilarityMethod::Token)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&token).unwrap();
        assert_eq!(parsed["method"], "token");
    }

    /// The token method must surface the same near-duplicate pair the
    /// TSED method does, across language parsers and output formats.
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
    fn token_method_reports_paired_functions(
        #[case] file_name: &str,
        #[case] src: &str,
        #[case] format: OutputFormat,
        #[case] assert_report: fn(&str),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), file_name, src);
        let out = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_method(SimilarityMethod::Token)
            .analyze(&file, format)
            .unwrap();
        assert_report(&out);
    }

    #[test]
    fn token_method_markdown_header_names_the_method() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", PAIRED_FUNCTIONS);
        let md = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_method(SimilarityMethod::Token)
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("token method"), "got: {md}");
    }

    #[test]
    fn score_token_candidate_pair_combines_body_and_signature_scores() {
        let corpus = vec![
            OwnedFunction {
                file: PathBuf::from("lib.rs"),
                rel_path: "lib.rs".to_owned(),
                is_test: false,
                shape: lens_domain::FunctionShape::from(lens_domain::FunctionDef {
                    name: "left".to_owned(),
                    start_line: 1,
                    end_line: 5,
                    is_test: false,
                    signature: Some(lens_domain::FunctionSignature {
                        name_tokens: vec!["left".to_owned()],
                        parameter_count: 1,
                        parameter_names: vec!["id".to_owned()],
                        parameter_type_paths: vec!["UserId".to_owned()],
                        return_type_paths: vec!["User".to_owned()],
                        generics: Vec::new(),
                        receiver: lens_domain::ReceiverShape::None,
                    }),
                    doc: None,
                    tree: lens_domain::TreeNode::with_children(
                        "Block",
                        "",
                        vec![
                            lens_domain::TreeNode::leaf("Let"),
                            lens_domain::TreeNode::leaf("If"),
                            lens_domain::TreeNode::leaf("Return"),
                        ],
                    ),
                }),
            },
            OwnedFunction {
                file: PathBuf::from("lib.rs"),
                rel_path: "lib.rs".to_owned(),
                is_test: false,
                shape: lens_domain::FunctionShape::from(lens_domain::FunctionDef {
                    name: "right".to_owned(),
                    start_line: 7,
                    end_line: 11,
                    is_test: false,
                    signature: Some(lens_domain::FunctionSignature {
                        name_tokens: vec!["right".to_owned()],
                        parameter_count: 1,
                        parameter_names: vec!["id".to_owned()],
                        parameter_type_paths: vec!["OrderId".to_owned()],
                        return_type_paths: vec!["Order".to_owned()],
                        generics: Vec::new(),
                        receiver: lens_domain::ReceiverShape::None,
                    }),
                    doc: None,
                    tree: lens_domain::TreeNode::with_children(
                        "Block",
                        "",
                        vec![
                            lens_domain::TreeNode::leaf("Let"),
                            lens_domain::TreeNode::leaf("If"),
                            lens_domain::TreeNode::leaf("Return"),
                        ],
                    ),
                }),
            },
        ];
        let token_profiles = build_token_profiles(&corpus, false);

        let score = score_token_candidate_pair(&corpus, &token_profiles, 0, 1).unwrap();

        // Identical body token streams, divergent signature types.
        assert_eq!(score.components.body_similarity, 1.0);
        assert!(score.exact_match);
        assert!(score.components.signature_similarity.unwrap() < 1.0);
        assert!(
            (score.components.similarity
                - (BODY_SIMILARITY_WEIGHT * score.components.body_similarity
                    + SIGNATURE_SIMILARITY_WEIGHT
                        * score.components.signature_similarity.unwrap()))
            .abs()
                < 1e-9
        );
    }

    #[test]
    fn score_token_candidate_pair_weights_a_partial_body_overlap() {
        // Bodies overlap only partially, so `body_similarity` lands
        // strictly between 0 and 1 — the case that pins down the body
        // weight as a multiplier rather than any other operator.
        fn body(third: &str) -> lens_domain::TreeNode {
            lens_domain::TreeNode::with_children(
                "Block",
                "",
                vec![
                    lens_domain::TreeNode::leaf("Let"),
                    lens_domain::TreeNode::leaf("Let"),
                    lens_domain::TreeNode::leaf(third),
                ],
            )
        }
        fn function(name: &str, third: &str) -> OwnedFunction {
            OwnedFunction {
                file: PathBuf::from("lib.rs"),
                rel_path: "lib.rs".to_owned(),
                is_test: false,
                shape: lens_domain::FunctionShape::from(lens_domain::FunctionDef {
                    name: name.to_owned(),
                    start_line: 1,
                    end_line: 5,
                    is_test: false,
                    signature: None,
                    doc: None,
                    tree: body(third),
                }),
            }
        }
        let corpus = vec![function("left", "Let"), function("right", "Call")];
        let token_profiles = build_token_profiles(&corpus, false);

        let score = score_token_candidate_pair(&corpus, &token_profiles, 0, 1).unwrap();

        let body = score.components.body_similarity;
        assert!(
            body > 0.0 && body < 1.0,
            "expected a partial overlap: {body}"
        );
        assert!(!score.exact_match);
        // No signatures, so the signature term contributes its 1.0 default.
        assert!(score.components.signature_similarity.is_none());
        let expected = BODY_SIMILARITY_WEIGHT * body + SIGNATURE_SIMILARITY_WEIGHT * 1.0;
        assert!(
            (score.components.similarity - expected).abs() < 1e-9,
            "similarity {} should be {expected}",
            score.components.similarity,
        );
    }

    /// The NAPI side of the drift fixture: a conversion carrying four
    /// fields.
    const NAPI_SIBLING: &str = r#"
pub struct Summary;

impl Summary {
    pub fn from_raw(raw: &Raw) -> Summary {
        let title = raw.title.clone();
        let authors = raw.authors.clone();
        let keywords = raw.keywords.clone();
        let year = raw.year;
        Summary { title, authors, keywords, year }
    }
}
"#;

    /// The WASM mirror of [`NAPI_SIBLING`], already drifted: same role,
    /// same name modulo the binding prefix, two fields dropped. This is
    /// the pair threshold clustering cannot report — the drift that made
    /// it dangerous is the same drift that pushes it under the cut.
    const WASM_SIBLING: &str = r#"
pub struct JsSummary;

impl JsSummary {
    pub fn from_raw(raw: &Raw) -> JsSummary {
        let title = raw.title.clone();
        let year = raw.year;
        JsSummary { title, year }
    }
}
"#;

    fn drift_fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "napi.rs", NAPI_SIBLING);
        write_file(dir.path(), "wasm.rs", WASM_SIBLING);
        dir
    }

    /// The regression the mode exists for. Clustering at the default
    /// threshold reports nothing, because the pair drifted below the cut;
    /// name-anchored pairing reports it, because it matched on the name
    /// before it ever looked at the score.
    #[test]
    fn paired_mode_reports_siblings_that_clustering_drops() {
        let dir = drift_fixture();

        let clustered: serde_json::Value = serde_json::from_str(
            &SimilarityAnalyzer::new()
                .analyze(dir.path(), OutputFormat::Json)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(clustered["cluster_count"], 0);

        let paired: serde_json::Value = serde_json::from_str(
            &SimilarityAnalyzer::new()
                .with_paired_by(Some(PairKey::Qualified))
                .analyze(dir.path(), OutputFormat::Json)
                .unwrap(),
        )
        .unwrap();

        assert_eq!(paired["paired_by"], "qualified");
        assert_eq!(paired["key_count"], 1);
        assert_eq!(paired["pair_count"], 1);
        assert_eq!(paired["drifted_pair_count"], 1);
        let group = &paired["groups"][0];
        // The `Js` prefix is stripped, so the two mirror types match.
        assert_eq!(group["key"], "summary::from_raw");
        assert_eq!(group["size"], 2);
        let files: Vec<&str> = group["functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["file"].as_str().unwrap())
            .collect();
        assert_eq!(files, vec!["napi.rs", "wasm.rs"]);
        let similarity = group["pairs"][0]["similarity"].as_f64().unwrap();
        assert!(
            (DEFAULT_DRIFT_FLOOR..DEFAULT_THRESHOLD).contains(&similarity),
            "fixture should drift within the reportable band, got {similarity}",
        );
        // The per-pair score components carry over from the clustering
        // path unchanged, so a consumer reads one pair shape either way.
        assert!(group["pairs"][0]["body_similarity"].as_f64().is_some());
        assert!(group["pairs"][0]["signature_similarity"].as_f64().is_some());
    }

    #[test]
    fn paired_markdown_names_the_key_and_both_siblings() {
        let dir = drift_fixture();

        let out = SimilarityAnalyzer::new()
            .with_paired_by(Some(PairKey::Qualified))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();

        assert!(out.contains("paired by qualified"), "{out}");
        assert!(out.contains("`summary::from_raw`"), "{out}");
        assert!(out.contains("napi.rs:`Summary::from_raw`"), "{out}");
        assert!(out.contains("wasm.rs:`JsSummary::from_raw`"), "{out}");
        assert!(out.contains("pair(s) drifted"), "{out}");
    }

    /// A floor at 1.0 admits nothing, so the same fixture must come back
    /// empty *and* say the match was dropped rather than never found.
    #[test]
    fn drift_floor_drops_matches_and_counts_them() {
        let dir = drift_fixture();

        let json: serde_json::Value = serde_json::from_str(
            &SimilarityAnalyzer::new()
                .with_paired_by(Some(PairKey::Qualified))
                .with_drift_floor(1.0)
                .analyze(dir.path(), OutputFormat::Json)
                .unwrap(),
        )
        .unwrap();

        assert_eq!(json["pair_count"], 0);
        assert_eq!(json["key_count"], 0);
        assert_eq!(json["below_floor_count"], 1);
    }

    /// The partial case, which the all-or-nothing one above cannot pin:
    /// three functions share the `method` key, one of them shares nothing
    /// else with the other two. The kept pair and the dropped ones have
    /// to be accounted for separately.
    #[test]
    fn drift_floor_counts_only_the_matches_it_dropped() {
        let dir = drift_fixture();
        write_file(
            dir.path(),
            "unrelated.rs",
            r#"
pub struct Counter;

impl Counter {
    pub fn from_raw(raw: &Raw) -> Counter {
        for entry in raw.entries.iter() {
            if entry.enabled {
                return Counter::new(entry.id);
            }
        }
        Counter::empty()
    }
}
"#,
        );

        let json: serde_json::Value = serde_json::from_str(
            &SimilarityAnalyzer::new()
                .with_paired_by(Some(PairKey::Method))
                .analyze(dir.path(), OutputFormat::Json)
                .unwrap(),
        )
        .unwrap();

        // Three functions share `from_raw`, so three pairs were scored:
        // the drifted mirror pair survives, the two involving the
        // unrelated namesake do not.
        assert_eq!(json["pair_count"], 1);
        assert_eq!(json["below_floor_count"], 2);
    }

    /// The loose key drops the owner, so two conversions on unrelated
    /// types still pair. Same fixture, and `from_raw` is the only name
    /// either file defines, so the difference is purely the key.
    #[test]
    fn method_key_matches_across_unrelated_owners() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "napi.rs", NAPI_SIBLING);
        write_file(
            dir.path(),
            "wasm.rs",
            &WASM_SIBLING.replace("JsSummary", "WebArticle"),
        );

        let qualified: serde_json::Value = serde_json::from_str(
            &SimilarityAnalyzer::new()
                .with_paired_by(Some(PairKey::Qualified))
                .analyze(dir.path(), OutputFormat::Json)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(qualified["pair_count"], 0);

        let method: serde_json::Value = serde_json::from_str(
            &SimilarityAnalyzer::new()
                .with_paired_by(Some(PairKey::Method))
                .analyze(dir.path(), OutputFormat::Json)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(method["pair_count"], 1);
        assert_eq!(method["groups"][0]["key"], "from_raw");
    }

    /// Siblings are a cross-file pattern; two same-named functions in one
    /// file are not drift. They must be excluded and counted, so an empty
    /// report can say which of the two reasons it is empty for.
    #[test]
    fn paired_mode_skips_and_counts_same_file_namesakes() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "lib.rs",
            &format!("{NAPI_SIBLING}{}", WASM_SIBLING.replace("Js", "Wasm")),
        );

        let json: serde_json::Value = serde_json::from_str(
            &SimilarityAnalyzer::new()
                .with_paired_by(Some(PairKey::Qualified))
                .analyze(dir.path(), OutputFormat::Json)
                .unwrap(),
        )
        .unwrap();

        assert_eq!(json["pair_count"], 0);
        assert_eq!(json["same_file_pair_count"], 1);
    }

    /// `--diff-only` narrows the paired report the same way it narrows
    /// the clustering one: a pair survives only when an edit touched one
    /// of its two sides.
    #[test]
    fn paired_mode_honors_diff_only() {
        let dir = drift_fixture();
        write_file(
            dir.path(),
            "other.rs",
            &NAPI_SIBLING.replace("Summary", "Note"),
        );
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let analyzer = SimilarityAnalyzer::new().with_paired_by(Some(PairKey::Method));
        let before: serde_json::Value =
            serde_json::from_str(&analyzer.analyze(dir.path(), OutputFormat::Json).unwrap())
                .unwrap();
        assert_eq!(before["pair_count"], 3);

        // Touch one side of one pair only.
        write_file(
            dir.path(),
            "wasm.rs",
            &WASM_SIBLING.replace("let year = raw.year;", "let year = raw.year + 1;"),
        );

        let json: serde_json::Value = serde_json::from_str(
            &analyzer
                .clone()
                .with_diff_only(true)
                .analyze(dir.path(), OutputFormat::Json)
                .unwrap(),
        )
        .unwrap();

        assert_eq!(json["pair_count"], 2);
        let files: Vec<&str> = json["groups"][0]["functions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["file"].as_str().unwrap())
            .collect();
        assert!(files.contains(&"wasm.rs"), "{files:?}");
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
