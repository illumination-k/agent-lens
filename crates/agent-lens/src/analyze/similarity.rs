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

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use ignore::WalkBuilder;
use lens_domain::{
    CandidateStrategy, FunctionDef, LanguageParser, SimilarCluster, TSEDOptions,
    calculate_tsed_with_subtree_sizes, cluster_similar_pairs, collect_subtree_sizes,
    lsh_candidate_pairs,
};
use lens_rust::{RustParser, extract_functions_excluding_tests};
use rayon::prelude::*;
use serde::Serialize;
use tracing::debug;

use super::{AnalyzerError, LineRange, OutputFormat, SourceLang, changed_line_ranges, read_source};

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

    with_setter! {
        /// Drop test scaffolding before computing similarity. Filters out
        /// `#[test]` / `#[rstest]` / `#[<runner>::test]` functions and items
        /// inside `#[cfg(test)] mod` blocks. Table-driven tests otherwise
        /// dominate the noise floor with parallel-but-distinct fixtures
        /// that aren't refactor candidates.
        fn with_exclude_tests, exclude_tests: bool
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
        if path.is_dir() {
            self.collect_directory(path)
        } else {
            self.collect_file(path, None)
        }
    }

    fn collect_directory(&self, root: &Path) -> Result<Vec<OwnedFunction>, AnalyzerError> {
        let started = Instant::now();
        let mut out = Vec::new();
        let mut file_count = 0usize;
        for entry in WalkBuilder::new(root).build() {
            let entry = entry.map_err(|e| AnalyzerError::Io {
                path: root.to_path_buf(),
                source: std::io::Error::other(e),
            })?;
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let p = entry.path();
            if SourceLang::from_path(p).is_none() {
                continue;
            }
            // Display paths relative to the walk root so cross-file pair
            // reports stay readable when the analyzer is pointed at a
            // deep directory.
            let rel = p
                .strip_prefix(root)
                .unwrap_or(p)
                .display()
                .to_string()
                .replace('\\', "/");
            file_count += 1;
            out.extend(self.collect_file(p, Some(rel))?);
        }
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
        let profiles: Vec<TreeProfile> = corpus
            .iter()
            .map(|f| TreeProfile::from_tree(&f.def.tree))
            .collect();
        let candidate_started = Instant::now();
        let candidates = candidate_pairs(
            corpus,
            self.min_lines,
            &profiles,
            self.threshold,
            self.opts.size_penalty,
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
            elapsed_ms = candidate_started.elapsed().as_secs_f64() * 1000.0,
            "similarity candidates enumerated"
        );

        let score_started = Instant::now();
        let score_stats = candidates
            .pairs
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
                if self.diff_only && !pair_touches_changes(a, b, &changed_by_file) {
                    stats.diff_filtered_count += 1;
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
        debug!(
            target: PROFILE_TARGET,
            candidate_count = candidates.total_len(),
            retained_candidate_count = candidates.len(),
            scored_pair_count = score_stats.scored_pair_count(),
            matched_pair_count = score_stats.pairs.len(),
            exact_match_count = score_stats.exact_match_count,
            size_filtered_count = candidates.size_filtered_count,
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
        self.pairs.len() + self.below_threshold_count + self.diff_filtered_count
    }
}

#[derive(Debug)]
struct TreeProfile {
    size: usize,
    subtree_sizes: lens_domain::SubtreeSizes,
}

impl TreeProfile {
    fn from_tree(tree: &lens_domain::TreeNode) -> Self {
        let subtree_sizes = collect_subtree_sizes(tree);
        let size = subtree_sizes
            .get(&(std::ptr::from_ref::<lens_domain::TreeNode>(tree) as usize))
            .copied()
            .unwrap_or(0);
        Self {
            size,
            subtree_sizes,
        }
    }
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
    strategy: CandidatePairStrategy,
}

impl CandidatePairs {
    fn len(&self) -> usize {
        self.pairs.len()
    }

    fn total_len(&self) -> usize {
        self.pairs.len() + self.size_filtered_count
    }
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
    size_penalty: bool,
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
    let use_lsh = strategy
        .lsh_min_functions
        .is_some_and(|min_n| eligible_indices.len() >= min_n);
    let (pairs, size_filtered_count) = if use_lsh {
        let funcs: Vec<FunctionDef> = eligible_indices
            .iter()
            .filter_map(|&i| corpus.get(i).map(|f| f.def.clone()))
            .collect();
        filter_size_compatible_pairs(
            lsh_candidate_pairs(&funcs, &strategy.lsh)
                .into_iter()
                .filter_map(|(i, j)| {
                    let a = eligible_indices.get(i).copied()?;
                    let b = eligible_indices.get(j).copied()?;
                    Some((a, b))
                }),
            profiles,
            threshold,
            size_penalty,
        )
    } else {
        filter_size_compatible_pairs(
            eligible_indices
                .iter()
                .enumerate()
                .flat_map(|(pos, &i)| eligible_indices[pos + 1..].iter().map(move |&j| (i, j))),
            profiles,
            threshold,
            size_penalty,
        )
    };
    CandidatePairs {
        pairs,
        eligible_function_count: eligible_indices.len(),
        size_filtered_count,
        strategy: if use_lsh {
            CandidatePairStrategy::Lsh
        } else {
            CandidatePairStrategy::Cartesian
        },
    }
}

fn filter_size_compatible_pairs(
    pairs: impl IntoIterator<Item = (usize, usize)>,
    profiles: &[TreeProfile],
    threshold: f64,
    size_penalty: bool,
) -> (Vec<(usize, usize)>, usize) {
    let mut out = Vec::new();
    let mut filtered = 0usize;
    for (i, j) in pairs {
        if size_upper_bound_below_threshold(profiles, i, j, threshold, size_penalty) {
            filtered += 1;
        } else {
            out.push((i, j));
        }
    }
    (out, filtered)
}

fn size_upper_bound_below_threshold(
    profiles: &[TreeProfile],
    i: usize,
    j: usize,
    threshold: f64,
    size_penalty: bool,
) -> bool {
    if !size_penalty {
        return false;
    }
    let Some(size_a) = profiles.get(i).map(|p| p.size) else {
        return false;
    };
    let Some(size_b) = profiles.get(j).map(|p| p.size) else {
        return false;
    };
    let max_size = size_a.max(size_b);
    if max_size == 0 {
        return false;
    }
    let min_size = size_a.min(size_b) as f64;
    let upper_bound = (min_size / max_size as f64).sqrt();
    upper_bound < threshold
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
