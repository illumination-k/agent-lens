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
use std::path::{Path, PathBuf};
use std::time::Instant;

use lens_domain::{
    FunctionDef, LanguageParser, SimilarCluster, TSEDOptions, calculate_tsed_with_subtree_sizes,
    cluster_similar_pairs,
};
use lens_rust::{RustParser, extract_functions_excluding_tests};
use rayon::prelude::*;
use serde::Serialize;
use tracing::debug;

use super::{
    AnalyzePathFilter, AnalyzerError, LineRange, OutputFormat, SourceFile, SourceLang,
    changed_line_ranges, collect_source_files, read_source,
};

mod candidates;

#[cfg(test)]
use candidates::{CheapFilter, tsed_upper_bound_filter};
use candidates::{TreeProfile, candidate_pairs, eligible_function_count, similarity_uses_lsh};

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
        let started = Instant::now();
        let files = collect_source_files(path, &filter)?;

        let parsed: Vec<Vec<OwnedFunction>> = files
            .par_iter()
            .map(|source_file| self.collect_file(source_file))
            .collect::<Result<_, _>>()?;

        let out: Vec<_> = parsed.into_iter().flatten().collect();
        let file_count = files.len();
        debug!(
            target: PROFILE_TARGET,
            root = %path.display(),
            file_count,
            function_count = out.len(),
            elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
            "similarity corpus directory collected"
        );
        Ok(out)
    }

    fn collect_file(&self, file: &SourceFile) -> Result<Vec<OwnedFunction>, AnalyzerError> {
        let started = Instant::now();
        let (lang, source) = read_source(&file.path)?;
        let funcs = extract_functions(lang, &source, self.exclude_tests)?;
        let out: Vec<_> = funcs
            .into_iter()
            .map(|def| OwnedFunction {
                file: file.path.clone(),
                rel_path: file.display_path.clone(),
                def,
            })
            .collect();
        debug!(
            target: PROFILE_TARGET,
            path = %file.path.display(),
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
        let profiles: Vec<TreeProfile> = if use_lsh_profiles {
            corpus
                .par_iter()
                .map(|f| TreeProfile::from_tree_for_scoring(&f.def.tree))
                .collect()
        } else {
            corpus
                .iter()
                .map(|f| TreeProfile::from_tree(&f.def.tree))
                .collect()
        };
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
                let compare_values = self.opts.apted.compare_values;
                let similarity = if is_exact_match_without_distance(
                    profile_a,
                    profile_b,
                    &a.def.tree,
                    &b.def.tree,
                    compare_values,
                ) {
                    stats.exact_match_count += 1;
                    1.0
                } else {
                    let sizes_a = profile_a.subtree_sizes(&a.def.tree);
                    let sizes_b = profile_b.subtree_sizes(&b.def.tree);
                    calculate_tsed_with_subtree_sizes(
                        &a.def.tree,
                        &b.def.tree,
                        profile_a.size,
                        profile_b.size,
                        sizes_a,
                        sizes_b,
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
        SourceLang::Go => extract_go(source, exclude_tests),
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

fn extract_go(source: &str, exclude_tests: bool) -> Result<Vec<FunctionDef>, ExtractError> {
    if exclude_tests {
        lens_golang::extract_functions_excluding_tests(source).map_err(box_err)
    } else {
        lens_golang::GoParser::new()
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
    fn report_renders_paired_go_functions() {
        // Two structurally identical Go functions — guaranteed to score
        // above the 0.5 threshold and exercise the lens-golang dispatch
        // added alongside the Rust / TS / Python arms.
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
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
"#;
        let file = write_file(dir.path(), "lib.go", src);
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
    fn exclude_tests_drops_go_test_functions_from_report() {
        // `go test`-style `Test*` functions form a parallel pair next
        // to a single production function; `--exclude-tests` should
        // drop them via `lens_golang::extract_functions_excluding_tests`.
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
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
"#;
        let file = write_file(dir.path(), "lib.go", src);

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
