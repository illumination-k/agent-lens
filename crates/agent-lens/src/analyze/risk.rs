//! `analyze risk` — churn × blast-radius ranking of files.
//!
//! The "how carefully should I treat this edit?" prior. [`super::hotspot`]
//! ranks `commits × cognitive_max`, which cannot separate a file that is
//! *hot but leaf* (changes often, nothing depends on it — low stakes)
//! from one that is *hot and load-bearing* (changes often, half the
//! codebase calls into it — where a defect propagates). Network measures
//! of a dependency graph predict defects better than intra-function
//! complexity metrics (Zimmermann & Nagappan), so this analyzer swaps
//! the complexity axis for a centrality axis.
//!
//! This is a join, not new analysis. Both inputs already exist:
//!
//! * **Churn** — [`super::churn`], the same `git log --name-only` pass
//!   `analyze hotspot` runs, `--since` window included.
//! * **Centrality** — the [`super::call_graph`] substrate, running the
//!   same PageRank pass `analyze hubs` reports (damping
//!   [`PAGERANK_DAMPING`], fixed [`PAGERANK_ITERATIONS`] iterations,
//!   call-count-weighted, resolved edges only), rolled up per file as
//!   the max and sum over the file's functions. Transitive caller counts
//!   (VFI, the graph-wide form of what `analyze impact` reports per seed)
//!   ride along as a second raw component.
//!
//! The composite is a **rank product** — see [`lens_domain::risk`] for
//! why ranks rather than raw values, and note that *lower is riskier*.
//! Every raw component travels with it: a ranking an agent cannot audit
//! reads as an oracle.
//!
//! Conventions inherited from the graph-analyzer family: resolved edges
//! only (so centrality is a lower bound, and ambiguous call sites cannot
//! inflate a common utility name into a hub), test functions excluded
//! from centrality unless `--only-tests`, and per-module resolution
//! confidence cited in the output.
//!
//! Granularity is per **file**, and the report says so: git attributes
//! commits to files, not functions. Function-level churn needs
//! diff-to-function mapping over history and is deliberately not
//! guessed at here.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use lens_domain::{FileCentrality, RiskEntry, compute_risk};
use serde::Serialize;

use super::call_graph::algo::{
    PAGERANK_DAMPING, PAGERANK_ITERATIONS, pagerank, percentile_buckets, transitive_caller_counts,
};
use super::call_graph::model::{CallGraphNode, ModuleResolutionSummary, Resolution};
use super::call_graph::{CallGraph, CallGraphBuilder, delegate_call_graph_builders};
use super::churn::ChurnScope;
use super::error_from::impl_from_churn_error;
use super::format::render_module_confidence;
use super::options::analyzer_options;
use super::runner::render_report;
use super::{AnalyzeRoots, AnalyzerError, OutputFormat};

const SCHEMA_VERSION: u32 = 1;

/// Markdown ranking cap when `--top` is not given. JSON always carries
/// every ranked file.
const DEFAULT_TOP: usize = 20;

/// Largest condensation the exact transitive-caller closure is attempted
/// on. The closure costs `components²/8` bytes of ancestor bitsets, so
/// past this point the report drops VFI and says so rather than
/// allocating gigabytes or silently substituting an approximation. The
/// composite never depended on VFI, so nothing else changes.
const VFI_MAX_COMPONENTS: usize = 10_000;

/// Errors raised while running the risk analyzer.
#[derive(Debug, thiserror::Error)]
pub enum RiskError {
    #[error(transparent)]
    Analyze(#[from] AnalyzerError),
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `git` is missing or returned a non-zero exit status.
    #[error("git failed: {}", stderr.trim_end())]
    Git { stderr: String },
    /// The provided path is not inside any git working tree, so there is
    /// no history to rank churn against.
    #[error("{path:?} is not inside a git working tree")]
    NotInGitRepo { path: PathBuf },
}

impl_from_churn_error!(RiskError);

analyzer_options! {
    /// `analyze risk` flags, and the `[profile.<name>.risk]` table.
    pub struct RiskOptions {
        @shared(ranking);
        /// Restrict the churn axis to commits in this `--since=` window.
        /// Accepts anything git's approxidate parser does (e.g.
        /// `90.days.ago`, `2024-01-01`). Centrality is a property of the
        /// current source and is unaffected.
        #[arg(long)]
        pub since: Option<String>,
    }
}

/// Analyzer entry point for `analyze risk`.
#[derive(Debug, Default, Clone)]
pub struct RiskAnalyzer {
    builder: CallGraphBuilder,
    only_tests: bool,
    since: Option<String>,
    top: Option<usize>,
}

impl RiskAnalyzer {
    /// Apply a whole [`RiskOptions`] group. The CLI flags and the
    /// `[profile.<name>.risk]` table are the same type, so this is the
    /// only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: RiskOptions) -> Self {
        self.with_top(opts.top).with_since_opt(opts.since)
    }

    pub fn new() -> Self {
        Self::default()
    }

    delegate_call_graph_builders! {
        builder,
        /// Rank test files instead: churn stays per file, and centrality
        /// is measured over the test corpus itself.
        only_tests => only_tests,
        exclude_tests,
    }

    /// Restrict the churn axis to commits in the given git `--since=`
    /// window (`"90.days.ago"`, `"2024-01-01"`, …). Centrality is a
    /// property of the current source and is unaffected, so a narrow
    /// window re-ranks *which* load-bearing files are also moving.
    pub fn with_since(mut self, since: impl Into<String>) -> Self {
        self.since = Some(since.into());
        self
    }

    /// [`Self::with_since`] for callers threading an optional CLI flag:
    /// `None` leaves the window unchanged.
    pub fn with_since_opt(mut self, since: Option<String>) -> Self {
        if let Some(s) = since {
            self.since = Some(s);
        }
        self
    }

    /// Cap the markdown table to the top-N files. JSON output always
    /// carries every ranked file.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    /// Walk `roots`, join their churn with call-graph centrality, and
    /// produce a report in `format`. Accepts a single path or several —
    /// see [`AnalyzeRoots`]; every root must sit in the same working tree.
    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, RiskError> {
        let roots = roots.into();
        let scope = ChurnScope::resolve(&roots)?;
        let churn = scope.collect(self.since.as_deref())?;
        let graph = self.builder.build(&roots)?;
        let centrality = Centrality::compute(&graph, self.only_tests);
        let files = centrality.roll_up_by_file(&graph.nodes, &scope);

        let report = Report::build(self, &roots, &scope, &graph, &centrality, churn, files);
        Ok(render_report(&report, format, || {
            format_markdown(&report, self.top)
        })?)
    }
}

/// Per-function centrality over the candidate subgraph: the PageRank
/// pass plus, when the graph is small enough, the exact transitive
/// caller closure.
struct Centrality {
    /// Node indices eligible for centrality, in node order.
    candidates: Vec<usize>,
    /// PageRank importance per candidate.
    pagerank: Vec<f64>,
    /// Percentile bucket (1–100) of each candidate's PageRank within the
    /// candidate set — the same bucketing `analyze hubs` reports, so the
    /// two outputs line up.
    percentile: Vec<u32>,
    /// Transitive resolved-caller count per candidate, or `None` when
    /// the graph exceeded [`VFI_MAX_COMPONENTS`].
    vfi: Option<Vec<usize>>,
}

impl Centrality {
    fn compute(graph: &CallGraph, only_tests: bool) -> Self {
        let candidates: Vec<usize> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| only_tests || !node.is_test)
            .map(|(idx, _)| idx)
            .collect();
        let mut candidate_of: Vec<Option<usize>> = vec![None; graph.nodes.len()];
        for (candidate_idx, &node_idx) in candidates.iter().enumerate() {
            candidate_of[node_idx] = Some(candidate_idx);
        }
        let index_by_id = graph.node_index_by_id();

        // Test scaffolding must not make production code look
        // load-bearing, so the whole pass runs on the
        // candidate-candidate subgraph — the same restriction
        // `analyze hubs` applies.
        let mut weighted: Vec<Vec<(usize, f64)>> = vec![Vec::new(); candidates.len()];
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); candidates.len()];
        for edge in &graph.edges {
            if edge.resolution != Resolution::Resolved {
                continue;
            }
            let (Some(from), Some(to)) = (edge.from.as_deref(), edge.to.as_deref()) else {
                continue;
            };
            let (Some(&from_idx), Some(&to_idx)) = (index_by_id.get(from), index_by_id.get(to))
            else {
                continue;
            };
            let (Some(from_candidate), Some(to_candidate)) =
                (candidate_of[from_idx], candidate_of[to_idx])
            else {
                continue;
            };
            if from_candidate == to_candidate {
                // Self-recursion is not incoming traffic from anywhere.
                continue;
            }
            weighted[from_candidate].push((to_candidate, edge.call_count as f64));
            adjacency[from_candidate].push(to_candidate);
        }
        for (weighted_edges, edges) in weighted.iter_mut().zip(&mut adjacency) {
            weighted_edges.sort_by_key(|edge| edge.0);
            edges.sort_unstable();
            edges.dedup();
        }

        let pagerank = pagerank(&weighted, PAGERANK_DAMPING, PAGERANK_ITERATIONS);
        let percentile = percentile_buckets(&pagerank);
        let vfi = transitive_caller_counts(&adjacency, VFI_MAX_COMPONENTS);
        Self {
            candidates,
            pagerank,
            percentile,
            vfi,
        }
    }

    /// Fold per-function centrality into per-file rollups keyed in git's
    /// path space, alongside the presentation-only extras (the hottest
    /// member and its percentile) the pure ranking layer has no use for.
    fn roll_up_by_file(
        &self,
        nodes: &[CallGraphNode],
        scope: &ChurnScope,
    ) -> BTreeMap<String, FileRollup> {
        let mut files: BTreeMap<String, FileRollup> = BTreeMap::new();
        for (candidate_idx, &node_idx) in self.candidates.iter().enumerate() {
            let node = &nodes[node_idx];
            let score = self.pagerank[candidate_idx];
            let vfi = self.vfi.as_ref().map(|vfi| vfi[candidate_idx]);
            let rollup = files.entry(scope.key_for_display(&node.file)).or_default();
            rollup.function_count += 1;
            rollup.loc += node.weights.loc;
            rollup.pagerank_sum += score;
            if let Some(vfi) = vfi {
                rollup.vfi_sum = Some(rollup.vfi_sum.unwrap_or(0) + vfi);
                rollup.vfi_max = Some(rollup.vfi_max.unwrap_or(0).max(vfi));
            }
            // Ties go to the earlier node, and nodes are already in
            // (file, line, name) order, so the pick is deterministic.
            if rollup.hottest.is_none() || score > rollup.pagerank_max {
                rollup.pagerank_max = score;
                rollup.hottest = Some(HotFunction {
                    qualified_name: node.qualified_name.clone(),
                    start_line: node.start_line,
                    module: node.module.clone(),
                    pagerank: score,
                    pagerank_percentile: self.percentile[candidate_idx],
                    vfi,
                });
            }
        }
        files
    }
}

/// One file's centrality rollup: the ranked components plus the
/// evidence rendered next to them.
#[derive(Debug, Default, Clone)]
struct FileRollup {
    function_count: usize,
    loc: usize,
    pagerank_max: f64,
    pagerank_sum: f64,
    vfi_max: Option<usize>,
    vfi_sum: Option<usize>,
    hottest: Option<HotFunction>,
}

/// The file's most important function — the concrete answer to "why is
/// this file central?".
#[derive(Debug, Clone, Serialize)]
struct HotFunction {
    qualified_name: String,
    start_line: usize,
    module: String,
    pagerank: f64,
    pagerank_percentile: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    vfi: Option<usize>,
}

/// Whether the transitive-caller closure ran. Serialized so a consumer
/// can tell "no callers" from "not measured".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum VfiStatus {
    Computed,
    /// The condensation exceeded [`VFI_MAX_COMPONENTS`]; every `vfi_*`
    /// field is absent.
    SkippedLargeGraph,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    repo_root: String,
    language: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<String>,
    /// Ranking granularity. Fixed at `"file"`: git attributes commits to
    /// files, so a quiet function in a churning file inherits that churn.
    granularity: &'static str,
    /// All graph nodes, including test functions.
    node_count: usize,
    /// Nodes eligible for centrality (non-test, unless `--only-tests`).
    candidate_count: usize,
    resolved_edge_count: usize,
    vfi: VfiStatus,
    file_count: usize,
    summary: Summary,
    /// Ranked files, riskiest (lowest rank product) first.
    files: Vec<FileView>,
    /// Per-module call-site resolution counts: a module whose edges are
    /// mostly unresolved has its centrality understated, so its files
    /// rank lower than they should.
    modules: Vec<ModuleResolutionSummary>,
}

impl Report {
    fn build(
        analyzer: &RiskAnalyzer,
        roots: &AnalyzeRoots,
        scope: &ChurnScope,
        graph: &CallGraph,
        centrality: &Centrality,
        churn: Vec<lens_domain::FileChurn>,
        mut files: BTreeMap<String, FileRollup>,
    ) -> Self {
        let inputs: Vec<FileCentrality> = files
            .iter()
            .map(|(path, rollup)| FileCentrality {
                path: path.clone(),
                function_count: rollup.function_count,
                loc: rollup.loc,
                pagerank_max: rollup.pagerank_max,
                pagerank_sum: rollup.pagerank_sum,
                vfi_max: rollup.vfi_max,
                vfi_sum: rollup.vfi_sum,
            })
            .collect();
        let ranked = compute_risk(churn, inputs);
        let views: Vec<FileView> = ranked
            .into_iter()
            .map(|entry| {
                let hottest = files.get_mut(&entry.path).and_then(|r| r.hottest.take());
                FileView::new(entry, hottest)
            })
            .collect();

        Self {
            schema_version: SCHEMA_VERSION,
            root: roots.display(),
            repo_root: scope.repo_root().display().to_string(),
            language: graph.language,
            since: analyzer.since.clone(),
            granularity: "file",
            node_count: graph.nodes.len(),
            candidate_count: centrality.candidates.len(),
            resolved_edge_count: graph
                .edges
                .iter()
                .filter(|e| e.resolution == Resolution::Resolved)
                .count(),
            vfi: if centrality.vfi.is_some() {
                VfiStatus::Computed
            } else {
                VfiStatus::SkippedLargeGraph
            },
            file_count: views.len(),
            summary: Summary::from_files(&views),
            files: views,
            modules: graph.module_summary.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
struct Summary {
    commits_max: u32,
    pagerank_max: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    vfi_max: Option<usize>,
}

impl Summary {
    fn from_files(files: &[FileView]) -> Self {
        Self {
            commits_max: files.iter().map(|f| f.commits).max().unwrap_or(0),
            pagerank_max: files
                .iter()
                .map(|f| f.pagerank_max)
                .max_by(f64::total_cmp)
                .unwrap_or(0.0),
            vfi_max: files.iter().filter_map(|f| f.vfi_max).max(),
        }
    }
}

#[derive(Debug, Serialize)]
struct FileView {
    path: String,
    /// `churn_rank × centrality_rank`. **Lower is riskier.**
    rank_product: u64,
    churn_rank: usize,
    centrality_rank: usize,
    commits: u32,
    function_count: usize,
    loc: usize,
    pagerank_max: f64,
    pagerank_sum: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    vfi_max: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    vfi_sum: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hottest_function: Option<HotFunction>,
}

impl FileView {
    fn new(entry: RiskEntry, hottest: Option<HotFunction>) -> Self {
        Self {
            path: entry.path,
            rank_product: entry.rank_product,
            churn_rank: entry.churn_rank,
            centrality_rank: entry.centrality_rank,
            commits: entry.commits,
            function_count: entry.function_count,
            loc: entry.loc,
            pagerank_max: entry.pagerank_max,
            pagerank_sum: entry.pagerank_sum,
            vfi_max: entry.vfi_max,
            vfi_sum: entry.vfi_sum,
            hottest_function: hottest,
        }
    }

    fn percentile(&self) -> String {
        self.hottest_function
            .as_ref()
            .map_or_else(|| "-".to_owned(), |f| format!("p{}", f.pagerank_percentile))
    }

    fn hottest_cell(&self) -> String {
        self.hottest_function.as_ref().map_or_else(
            || "-".to_owned(),
            |f| format!("`{}`:{}", f.qualified_name, f.start_line),
        )
    }
}

fn format_optional_usize(value: Option<usize>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let scope = report
        .since
        .as_deref()
        .map_or_else(String::new, |s| format!(", since {s}"));
    let mut out = format!(
        "# Risk report: {} ({} file(s) ranked{scope})\n",
        report.root, report.file_count,
    );
    out.push_str(
        "\nRank product of churn and call-graph centrality: \
         `rank_product = churn_rank * centrality_rank`, and **lower is riskier** \
         (rank 1 is the top of an axis). This ranks blast radius, not defects: a \
         high row means \"check callers and tests before editing\", not \"fix this\". \
         Churn is per-file commit counts from `git log`; centrality is the highest \
         PageRank importance among the file's functions, the same pass `analyze hubs` \
         reports — so `PR p` percentiles are comparable between the two. Both ranks \
         are competition ranks over the files listed here, so they are relative to \
         this scope, not absolute.\n",
    );
    out.push_str(
        "\nGranularity is per file: git attributes commits to files, so a quiet \
         function in a churning file inherits that churn. Centrality follows \
         resolved call edges only, so it is a lower bound — ambiguous and unresolved \
         call sites are invisible to it, and the module confidence list below says \
         where that bites hardest.\n",
    );

    if report.files.is_empty() {
        out.push_str("\n_No files matched._\n");
        return out;
    }

    let _ = writeln!(
        &mut out,
        "\n## Summary\n\
         - files_ranked: {}\n\
         - functions_scored: {} of {} graph node(s)\n\
         - commits_max: {}\n\
         - pagerank_max: {:.6}\n\
         - vfi: {}",
        report.file_count,
        report.candidate_count,
        report.node_count,
        report.summary.commits_max,
        report.summary.pagerank_max,
        match report.vfi {
            VfiStatus::Computed => format!(
                "max {} transitive caller(s)",
                format_optional_usize(report.summary.vfi_max),
            ),
            VfiStatus::SkippedLargeGraph => format!(
                "not computed (graph exceeds {VFI_MAX_COMPONENTS} components); \
                 the ranking does not use it"
            ),
        },
    );

    let limit = top.unwrap_or(DEFAULT_TOP);
    let _ = writeln!(
        &mut out,
        "\n## Top {limit} by risk (rank product, lower is riskier)\n"
    );
    let _ = writeln!(
        &mut out,
        "| file | rp | churn_rk | cent_rk | commits | PR max | PR p | VFI max | fns | loc | \
         hottest function |"
    );
    let _ = writeln!(
        &mut out,
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |"
    );
    for f in report.files.iter().take(limit) {
        let _ = writeln!(
            &mut out,
            "| {} | {} | {} | {} | {} | {:.6} | {} | {} | {} | {} | {} |",
            f.path,
            f.rank_product,
            f.churn_rank,
            f.centrality_rank,
            f.commits,
            f.pagerank_max,
            f.percentile(),
            format_optional_usize(f.vfi_max),
            f.function_count,
            f.loc,
            f.hottest_cell(),
        );
    }

    render_module_confidence(
        &mut out,
        &report.modules,
        "Centrality in these modules is the most undercounted, so their files rank \
         lower here than the code warrants.",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};
    use serde_json::Value;
    use std::path::Path;

    /// A repo where churn and centrality disagree, which is the only
    /// interesting case: `leaf` churns hardest but nothing calls it,
    /// while `core::sink` is called by every module and churns almost as
    /// hard. Hotspot-style `churn × complexity` would crown `leaf`.
    fn init_split_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        write_file(
            dir,
            "src/lib.rs",
            "pub mod core;\npub mod leaf;\npub mod a;\n",
        );
        write_file(dir, "src/core.rs", "pub fn sink() {}\n");
        write_file(dir, "src/leaf.rs", "pub fn alone() {}\n");
        write_file(
            dir,
            "src/a.rs",
            "pub fn one() { crate::core::sink(); }\n\
             pub fn two() { crate::core::sink(); }\n\
             pub fn three() { crate::core::sink(); }\n",
        );
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", "initial"]);

        // leaf.rs: three more commits. core.rs: two more.
        for i in 0..3 {
            write_file(
                dir,
                "src/leaf.rs",
                &format!("pub fn alone() {{ let _ = {i}; }}\n"),
            );
            run_git(dir, &["add", "."]);
            run_git(dir, &["commit", "-q", "-m", "churn leaf"]);
        }
        for i in 0..2 {
            write_file(
                dir,
                "src/core.rs",
                &format!("pub fn sink() {{ let _ = {i}; }}\n"),
            );
            run_git(dir, &["add", "."]);
            run_git(dir, &["commit", "-q", "-m", "churn core"]);
        }
    }

    fn analyze_json(analyzer: &RiskAnalyzer, path: &Path) -> Value {
        let json = analyzer.analyze(path, OutputFormat::Json).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn file_entry<'a>(report: &'a Value, path: &str) -> &'a Value {
        report["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == path)
            .unwrap_or_else(|| panic!("no file entry for {path} in {report}"))
    }

    #[test]
    fn load_bearing_file_outranks_the_churnier_leaf() {
        let dir = tempfile::tempdir().unwrap();
        init_split_repo(dir.path());

        let report = analyze_json(&RiskAnalyzer::new(), dir.path());
        let core = file_entry(&report, "src/core.rs");
        let leaf = file_entry(&report, "src/leaf.rs");
        assert!(
            leaf["commits"].as_u64().unwrap() > core["commits"].as_u64().unwrap(),
            "fixture must give the leaf more churn: {report}",
        );
        assert_eq!(core["centrality_rank"], 1);
        assert!(
            core["rank_product"].as_u64().unwrap() < leaf["rank_product"].as_u64().unwrap(),
            "the load-bearing file must outrank the churnier leaf: {report}",
        );
        assert_eq!(report["files"][0]["path"], "src/core.rs");
    }

    #[test]
    fn raw_components_travel_with_the_composite() {
        let dir = tempfile::tempdir().unwrap();
        init_split_repo(dir.path());

        let core = analyze_json(&RiskAnalyzer::new(), dir.path());
        let core = file_entry(&core, "src/core.rs").clone();
        assert_eq!(
            core["rank_product"].as_u64().unwrap(),
            core["churn_rank"].as_u64().unwrap() * core["centrality_rank"].as_u64().unwrap(),
        );
        assert!(core["commits"].as_u64().unwrap() >= 3);
        assert!(core["pagerank_max"].as_f64().unwrap() > 0.0);
        assert_eq!(core["vfi_max"], 3, "three callers reach the sink");
        assert_eq!(
            core["hottest_function"]["qualified_name"],
            "crate::core::sink"
        );
        assert_eq!(core["hottest_function"]["pagerank_percentile"], 100);
    }

    /// The integration hazard the analyzer exists to get right: churn
    /// paths are repo-root-relative while graph node ids embed
    /// analyze-root-relative paths. A sub-directory target must still
    /// join, i.e. commits must be non-zero.
    #[test]
    fn subdirectory_target_still_joins_churn_onto_graph_files() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(
            dir.path(),
            "crates/app/src/lib.rs",
            "pub fn sink() {}\npub fn caller() { sink(); }\n",
        );
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let report = analyze_json(&RiskAnalyzer::new(), &dir.path().join("crates/app"));
        let entry = file_entry(&report, "crates/app/src/lib.rs");
        assert!(
            entry["commits"].as_u64().unwrap() >= 1,
            "the join must survive path-space normalization: {report}",
        );
    }

    #[test]
    fn single_file_target_joins_on_its_repo_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        init_split_repo(dir.path());

        let report = analyze_json(&RiskAnalyzer::new(), &dir.path().join("src/core.rs"));
        assert_eq!(report["file_count"], 1);
        let entry = file_entry(&report, "src/core.rs");
        assert!(entry["commits"].as_u64().unwrap() >= 3, "got {report}");
    }

    /// The rollup keeps the file's *most* important member, not the
    /// first one it meets: `quiet` is declared above `hub` and has no
    /// callers at all.
    #[test]
    fn hottest_function_is_the_file_maximum_not_the_first_member() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn quiet() {}\n\
             pub fn hub() {}\n\
             pub fn c1() { hub(); }\n\
             pub fn c2() { hub(); }\n\
             pub fn c3() { hub(); }\n",
        );
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let report = analyze_json(&RiskAnalyzer::new(), dir.path());
        let entry = file_entry(&report, "src/lib.rs");
        assert_eq!(entry["hottest_function"]["qualified_name"], "crate::hub");
        assert_eq!(entry["hottest_function"]["vfi"], 3);
        assert_eq!(entry["function_count"], 5);
        assert_eq!(entry["loc"], 5, "five one-line functions: {report}");
        assert_eq!(entry["vfi_max"], 3);
        assert_eq!(
            entry["vfi_sum"], 3,
            "only `hub` has callers, so the file's total is its own: {report}",
        );
        assert_eq!(
            report["resolved_edge_count"], 3,
            "the three calls into `hub` are the resolved edges: {report}",
        );
        assert_eq!(report["node_count"], 5);
        assert_eq!(report["candidate_count"], 5);
        assert_eq!(
            entry["pagerank_max"], entry["hottest_function"]["pagerank"],
            "the ranked max must be the named function's score: {report}",
        );
        assert!(
            entry["pagerank_sum"].as_f64().unwrap() > entry["pagerank_max"].as_f64().unwrap(),
            "five functions must sum above any single one: {report}",
        );
    }

    /// `alpha` and `beta` are called identically, so their PageRank is
    /// symmetric and the pick is a pure tie-break. Nodes arrive in
    /// (file, line, name) order and the earlier one has to win, or the
    /// named evidence would drift between runs.
    #[test]
    fn tied_members_resolve_to_the_earlier_function() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn alpha() {}\n\
             pub fn beta() {}\n\
             pub fn caller() { alpha(); beta(); }\n",
        );
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let report = analyze_json(&RiskAnalyzer::new(), dir.path());
        let entry = file_entry(&report, "src/lib.rs");
        assert_eq!(entry["hottest_function"]["qualified_name"], "crate::alpha");
        assert_eq!(entry["hottest_function"]["start_line"], 1);
    }

    #[test]
    fn since_option_is_applied_only_when_present() {
        let dir = tempfile::tempdir().unwrap();
        init_split_repo(dir.path());

        let applied = analyze_json(
            &RiskAnalyzer::new().with_since_opt(Some("2099-01-01".to_owned())),
            dir.path(),
        );
        assert_eq!(applied["since"], "2099-01-01");
        assert!(
            applied["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["commits"] == 0),
            "got {applied}",
        );

        let untouched = analyze_json(&RiskAnalyzer::new().with_since_opt(None), dir.path());
        assert_eq!(untouched.get("since"), None, "got {untouched}");
        assert!(
            untouched["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|f| f["commits"].as_u64().unwrap() > 0),
            "got {untouched}",
        );
    }

    #[test]
    fn files_absent_from_history_rank_on_centrality_alone() {
        let dir = tempfile::tempdir().unwrap();
        init_split_repo(dir.path());
        // Never committed, so churn has nothing for it.
        write_file(dir.path(), "src/fresh.rs", "pub fn brand_new() {}\n");

        let report = analyze_json(&RiskAnalyzer::new(), dir.path());
        let fresh = file_entry(&report, "src/fresh.rs");
        assert_eq!(fresh["commits"], 0);
        assert!(fresh["centrality_rank"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn since_window_narrows_the_churn_axis_only() {
        let dir = tempfile::tempdir().unwrap();
        init_split_repo(dir.path());

        let report = analyze_json(&RiskAnalyzer::new().with_since("2099-01-01"), dir.path());
        let files = report["files"].as_array().unwrap();
        assert!(
            files.iter().all(|f| f["commits"] == 0),
            "no commit is inside the window: {report}",
        );
        assert!(
            files
                .iter()
                .any(|f| f["pagerank_max"].as_f64().unwrap() > 0.0),
            "centrality must survive an empty churn window: {report}",
        );
        assert_eq!(report["since"], "2099-01-01");
    }

    #[test]
    fn test_functions_do_not_make_a_file_load_bearing() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn helper() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 fn t1() { crate::helper(); }\n\
                 fn t2() { crate::helper(); }\n\
                 fn t3() { crate::helper(); }\n\
             }\n",
        );
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let report = analyze_json(&RiskAnalyzer::new(), dir.path());
        let entry = file_entry(&report, "src/lib.rs");
        assert_eq!(entry["function_count"], 1, "only prod functions score");
        assert_eq!(
            entry["vfi_max"], 0,
            "test callers are outside the candidate subgraph: {report}",
        );
    }

    #[test]
    fn analysis_is_deterministic_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        init_split_repo(dir.path());
        let analyzer = RiskAnalyzer::new();
        let a = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        let b = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn excluded_paths_leave_the_ranking() {
        let dir = tempfile::tempdir().unwrap();
        init_split_repo(dir.path());

        let report = analyze_json(
            &RiskAnalyzer::new().with_exclude_patterns(vec!["**/leaf.rs".to_owned()]),
            dir.path(),
        );
        assert!(
            report["files"]
                .as_array()
                .unwrap()
                .iter()
                .all(|f| f["path"] != "src/leaf.rs"),
            "excluded file leaked into the ranking: {report}",
        );
    }

    #[test]
    fn markdown_states_the_direction_granularity_and_bounds() {
        let dir = tempfile::tempdir().unwrap();
        init_split_repo(dir.path());

        let md = RiskAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Risk report:"), "got: {md}");
        // Agents act on wording literally: the inverted direction, the
        // file-level granularity, and the lower-bound caveat all have to
        // be stated or the table reads as an oracle.
        assert!(md.contains("lower is riskier"), "got: {md}");
        assert!(md.contains("not defects"), "got: {md}");
        assert!(md.contains("Granularity is per file"), "got: {md}");
        assert!(md.contains("lower bound"), "got: {md}");
        assert!(md.contains("Top 1 by risk"), "got: {md}");
        // The row is the evidence, so every cell that explains the rank
        // has to be rendered: the percentile, the VFI, and the named
        // function that carried the file.
        assert!(
            md.contains("| src/core.rs | 2 | 2 | 1 | "),
            "rp=2 from churn rank 2 and centrality rank 1 — second on churn, \
             first on blast radius, and still the riskiest row: {md}",
        );
        assert!(md.contains("| p100 |"), "got: {md}");
        assert!(md.contains("`crate::core::sink`:1"), "got: {md}");
        assert!(!md.contains("| src/leaf.rs |"), "top 1 must cap: {md}");
    }

    /// With `--top 0` nothing is rendered but the caveats still are, so
    /// an empty table can never be mistaken for "no risk".
    #[test]
    fn markdown_summary_survives_a_zero_cap() {
        let dir = tempfile::tempdir().unwrap();
        init_split_repo(dir.path());

        let md = RiskAnalyzer::new()
            .with_top(Some(0))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("- files_ranked: 3"), "got: {md}");
        assert!(md.contains("transitive caller(s)"), "got: {md}");
        assert!(!md.contains("| src/core.rs |"), "got: {md}");
    }

    #[test]
    fn markdown_reports_empty_input_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(dir.path(), "README.md", "no source here\n");
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let md = RiskAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("_No files matched._"), "got: {md}");
    }

    #[test]
    fn vfi_is_reported_as_computed_for_ordinary_graphs() {
        let dir = tempfile::tempdir().unwrap();
        init_split_repo(dir.path());
        let report = analyze_json(&RiskAnalyzer::new(), dir.path());
        assert_eq!(report["vfi"], "computed");
        assert_eq!(report["granularity"], "file");
        assert_eq!(report["schema_version"], 1);
    }

    #[test]
    fn target_outside_a_git_working_tree_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "lone.rs", "fn x() {}\n");
        let err = RiskAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, RiskError::NotInGitRepo { .. }), "got {err:?}");
    }

    #[test]
    fn missing_path_surfaces_an_io_error() {
        let err = RiskAnalyzer::new()
            .analyze(Path::new("/definitely/does/not/exist"), OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, RiskError::Io { .. }), "got {err:?}");
    }

    #[test]
    fn risk_error_displays_carry_their_diagnostic() {
        use std::error::Error as _;

        let io = RiskError::Io {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        };
        assert!(io.to_string().contains("/tmp/x"), "got {io}");
        assert!(io.source().is_some());

        let git = RiskError::Git {
            stderr: "fatal: not a git repo\n".to_owned(),
        };
        assert!(
            git.to_string().contains("fatal: not a git repo"),
            "got {git}"
        );
        assert!(!git.to_string().ends_with('\n'));
        assert!(git.source().is_none());

        let missing = RiskError::NotInGitRepo {
            path: PathBuf::from("/tmp/lonely"),
        };
        assert!(
            missing
                .to_string()
                .contains("not inside a git working tree"),
            "got {missing}",
        );
    }

    #[test]
    fn optional_usize_renders_a_dash_when_absent() {
        assert_eq!(format_optional_usize(Some(7)), "7");
        assert_eq!(format_optional_usize(None), "-");
    }
}
