//! `analyze hidden-coupling` — the static-dependency × co-change
//! differential.
//!
//! Two analyzers here already answer half of it. [`super::coupling`],
//! [`super::layers`] and the call graph say what the code declares
//! *should* be coupled; [`super::co_change`] says what the repository
//! actually experienced. Neither number is new on its own. The
//! **difference** between them is, and it is the one signal no
//! single-view tool can produce:
//!
//! | static edge | co-change | verdict |
//! | --- | --- | --- |
//! | no | high | **hidden coupling** — an undeclared contract |
//! | yes | ~none in the window | **suspect dependency** — vestigial, or simply stable |
//! | yes | high | expected; not reported |
//! | no | none | not reported |
//!
//! The two buckets are never merged into one score. They carry
//! different confidence and license different actions: a hidden pair is
//! an implicit contract to find and make explicit (a shared literal, a
//! serialization format, a duplicated constant, a generated file with no
//! regeneration step), while a suspect dependency is only a question —
//! over one window a stable, correct dependency looks exactly like a
//! dead one, so those rows rank below the hidden ones and the report
//! says why.
//!
//! Three classifications decide whether the report is useful or is
//! mostly noise, and each gets its own bucket rather than a silent drop:
//!
//! * **Test ↔ subject pairs** co-change by construction, so a pair with
//!   a test-like path on either side is never hidden coupling. It is
//!   still reported, because a test that stopped moving with its subject
//!   is worth seeing.
//! * **Files outside the static view** — `.md`, `.toml`, workflow YAML,
//!   fixtures — have no static edge *by construction*: no language
//!   backend reads them. That is not a missing dependency, it is no
//!   static view at all, and scoring it as hidden coupling would fill
//!   the report with documentation. The bucket is still useful: a doc
//!   that always moves with a source file is a real contract.
//! * **Unresolved call sites** already weaken the call graph, so
//!   "no static path" is an **upper bound** — the same caveat every
//!   graph analyzer here emits, cited per module at the end of the
//!   report.

use std::fmt::Write as _;
use std::path::PathBuf;

use lens_domain::{
    CoChangeCounts, CoChangePair, CoChangeThresholds, CoChangeTotals, rank_cochange_pairs,
    tally_cochange,
};
use serde::Serialize;
use tracing::warn;

use super::call_graph::model::{ModuleResolutionSummary, Resolution};
use super::call_graph::{CallGraph, CallGraphBuilder};
use super::churn::ChurnScope;
use super::co_change::{CoChangeOptions, retain_reportable_paths};
use super::error_from::impl_from_churn_error;
use super::format::render_module_confidence;
use super::runner::render_report;
use super::static_file_graph::{
    Direction, ModuleGraphCoverage, StaticFileGraph, StaticFileGraphBuilder, StaticRelation,
    StaticVerdict, add_module_graphs,
};
use super::{
    AnalyzePathFilter, AnalyzeRoots, AnalyzerError, CompiledPathFilter, OutputFormat,
    PathFilterError, collect_source_files,
};

/// # Schema history
///
/// * `schema_version: 1` — initial shape.
const SCHEMA_VERSION: u32 = 1;

/// Rows listed per markdown table when `--top` is not given. JSON always
/// carries every row of every bucket.
const DEFAULT_TOP: usize = 20;

/// What the differential does and does not license, stated in the output
/// because both halves are easy to over-read on their own.
const NOTE: &str = "The difference between what the code declares and what history did, not a \
     finding on its own. `hidden_coupling` is a pair that co-changed with no file-level \
     dependency between them either way: look for the implicit contract (a shared literal, a \
     serialization format, a duplicated constant, a generated file with no regeneration step) \
     and make it explicit or delete it. `suspect_dependencies` is the weaker half and ranks \
     below it: over one window a stable, correct dependency is indistinguishable from a dead \
     one, so a row is a question about whether the edge is still load-bearing, never a verdict. \
     The static side is the file-level projection of the module graph plus resolved call edges, \
     so it is a lower bound — an unresolved call site is an edge nobody can see, and \
     `relation: no_path` is therefore an upper bound on \"undeclared\". Test pairs and pairs \
     with a file no language backend reads are kept in their own buckets: both have no static \
     edge by construction and neither is a missing dependency.";

/// Errors raised while running the hidden-coupling analyzer.
#[derive(Debug, thiserror::Error)]
pub enum HiddenCouplingError {
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
    /// no history to subtract the static graph from.
    #[error("{path:?} is not inside a git working tree")]
    NotInGitRepo { path: PathBuf },
    /// A confidence is a probability, so a cut outside `[0.0, 1.0]` can
    /// never be met. Left unchecked it empties the co-change side, which
    /// reads as "nothing here is hidden" — the opposite of what
    /// happened. Rejected the same way `co-change` rejects it.
    #[error(
        "--min-confidence must be within [0.0, 1.0]; got {value} — no pair's confidence can reach it"
    )]
    MinConfidenceOutOfRange { value: f64 },
    #[error("failed to serialize report: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error(transparent)]
    PathFilter(#[from] PathFilterError),
}

impl_from_churn_error!(HiddenCouplingError);

/// Analyzer entry point for `analyze hidden-coupling`.
///
/// The option surface *is* [`CoChangeOptions`]: every knob here scopes
/// the history half, and the two analyzers must read the same window
/// with the same thresholds or their answers stop being comparable. A
/// separate byte-identical struct would only be a second place for the
/// defaults to drift.
#[derive(Debug, Clone, Default)]
pub struct HiddenCouplingAnalyzer {
    builder: CallGraphBuilder,
    since: Option<String>,
    top: Option<usize>,
    thresholds: CoChangeThresholds,
    path_filter: AnalyzePathFilter,
}

impl HiddenCouplingAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a whole [`CoChangeOptions`] group. The CLI flags and the
    /// `[profile.<name>.hidden-coupling]` table are the same type, so
    /// this is the only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: CoChangeOptions) -> Self {
        self.with_top(opts.top)
            .with_since_opt(opts.since)
            .with_min_support(opts.min_support)
            .with_min_confidence(opts.min_confidence)
            .with_max_commit_files(opts.max_commit_files)
    }

    /// Restrict history to commits made in the given git `--since=`
    /// window. The static graph reflects the current source either way.
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

    /// Cap each markdown bucket to the top-N rows. JSON always carries
    /// every row. `None` uses the markdown default of 20.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    /// Minimum co-change count, in both directions: the bar a pair must
    /// clear to count as co-changing, and the bar a declared dependency
    /// must fall under — on both endpoints' own commit counts too — to
    /// count as suspect.
    pub fn with_min_support(mut self, min_support: u32) -> Self {
        self.thresholds.min_support = min_support;
        self
    }

    pub fn with_min_confidence(mut self, min_confidence: f64) -> Self {
        self.thresholds.min_confidence = min_confidence;
        self
    }

    pub fn with_max_commit_files(mut self, max_commit_files: usize) -> Self {
        self.thresholds.max_commit_files = max_commit_files;
        self
    }

    // Each filter knob is set twice on purpose. The call graph judges
    // files it walked, so it compiles its own filter against the
    // analysis base; the history side only ever holds git's
    // repo-relative strings. One flag, two path spaces — and a pair
    // filtered out of one side but not the other would be classified
    // against a graph that cannot see it.
    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_only_tests(only_tests);
        self.builder = self.builder.with_only_tests(only_tests);
        self
    }

    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_exclude_tests(exclude_tests);
        self.builder = self.builder.with_exclude_tests(exclude_tests);
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.path_filter = self.path_filter.with_exclude_patterns(exclude.clone());
        self.builder = self.builder.with_exclude_patterns(exclude);
        self
    }

    /// Read `roots`' history, project the static graphs onto files, and
    /// report where the two disagree. Accepts a single path or several —
    /// see [`AnalyzeRoots`]; every root must sit in the same working
    /// tree.
    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, HiddenCouplingError> {
        let confidence = self.thresholds.min_confidence;
        if !(0.0..=1.0).contains(&confidence) {
            return Err(HiddenCouplingError::MinConfidenceOutOfRange { value: confidence });
        }
        let roots = roots.into();
        let scope = ChurnScope::resolve(&roots)?;
        // Compiled against the repo root, not the analysis base: git
        // reports repo-relative paths, and both sides of this
        // differential are keyed in that space.
        let filter = self.path_filter.compile(scope.repo_root())?;

        let shallow = scope.is_shallow();
        if shallow {
            warn!(
                "shallow clone: `git log` sees a truncated history, so every co-change count is \
                 a lower bound. Pairs whose evidence predates the graft look like declared \
                 dependencies that never move, which is exactly the suspect bucket's failure \
                 mode. Fetch the full history (`git fetch --unshallow`) first."
            );
        }

        let mut commits = scope.collect_commits(self.since.as_deref())?;
        retain_reportable_paths(&mut commits, &filter, scope.repo_root());
        let counts = tally_cochange(&commits, self.thresholds.max_commit_files);
        let cochanging = rank_cochange_pairs(&counts, self.thresholds);

        let call_graph = self.builder.build(&roots)?;
        let static_view = self.build_static_graph(&scope, &filter, &call_graph)?;

        let report = Report::build(
            self,
            ReportInputs {
                roots: &roots,
                scope: &scope,
                shallow_clone: shallow,
                counts: &counts,
                cochanging: &cochanging,
                call_graph: &call_graph,
                static_view: &static_view,
                filter: &filter,
            },
        );
        Ok(render_report(&report, format, || {
            format_markdown(&report, self.top.unwrap_or(DEFAULT_TOP))
        })?)
    }

    /// The file-level static view: every file a language backend reads,
    /// the resolved call edges between them, and every module graph the
    /// scope contains.
    ///
    /// The walk runs on the canonicalized targets so file keys land in
    /// git's path space directly, the same join `hotspot` makes. It is
    /// what separates "this file has no declared dependency" from "no
    /// backend reads this file": the call graph only has nodes for files
    /// that declare a function, so a `.rs` file holding nothing but a
    /// macro or a constant would otherwise look like a `.md`.
    fn build_static_graph(
        &self,
        scope: &ChurnScope,
        filter: &CompiledPathFilter,
        call_graph: &CallGraph,
    ) -> Result<StaticGraphView, HiddenCouplingError> {
        let targets = AnalyzeRoots::new(scope.targets().to_vec());
        let files = collect_source_files(&targets, filter)?;

        let mut builder = StaticFileGraphBuilder::new();
        for file in &files {
            builder.observe_file(scope.key_for_absolute(&file.path));
        }
        builder.add_call_graph(call_graph, scope);
        let paths: Vec<PathBuf> = files.into_iter().map(|file| file.path).collect();
        let module_graph = add_module_graphs(&mut builder, scope, &paths);
        Ok(StaticGraphView {
            graph: builder.finish(),
            module_graph,
        })
    }
}

/// The assembled static view plus how much of it the module half
/// contributed.
struct StaticGraphView {
    graph: StaticFileGraph,
    module_graph: ModuleGraphCoverage,
}

/// Which bucket a co-changing pair belongs in.
///
/// The order is the report's order, and it is deliberate: the two
/// "by construction" explanations are checked before any static verdict
/// is consulted, so neither can be counted as a missing dependency.
enum Bucket {
    /// At least one side is a test-like path.
    TestPair(Vec<String>),
    /// At least one side is a file no language backend reads.
    NoStaticView(Vec<String>),
    /// Both sides are in the static view and neither declares a
    /// dependency on the other.
    Hidden(StaticVerdict),
    /// A declared dependency that co-changes: expected, not reported.
    Expected,
}

fn classify(pair: &CoChangePair, graph: &StaticFileGraph, filter: &CompiledPathFilter) -> Bucket {
    let tests: Vec<String> = [&pair.a, &pair.b]
        .into_iter()
        .filter(|path| filter.is_test_relative(path))
        .cloned()
        .collect();
    if !tests.is_empty() {
        return Bucket::TestPair(tests);
    }
    let outside: Vec<String> = [&pair.a, &pair.b]
        .into_iter()
        .filter(|path| !graph.contains(path))
        .cloned()
        .collect();
    if !outside.is_empty() {
        return Bucket::NoStaticView(outside);
    }
    // Both sides are in the view, so the graph always has a verdict.
    match graph.verdict(&pair.a, &pair.b) {
        Some(verdict) if verdict.relation != StaticRelation::Direct => Bucket::Hidden(verdict),
        _ => Bucket::Expected,
    }
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    target: String,
    repo_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<String>,
    note: &'static str,
    /// Set when `git log` is reading a truncated history: the suspect
    /// bucket's failure mode, since evidence before the graft is simply
    /// missing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    shallow_clone: bool,
    thresholds: ThresholdsView,
    history: HistoryView,
    static_view: StaticViewSummary,
    /// The highest-value bucket: co-changed, with no declared dependency
    /// either way. `no_path` rows first, then the ones that only relate
    /// through intermediates.
    hidden_coupling: Vec<HiddenRow>,
    /// Declared dependencies whose endpoints both moved in the window
    /// but not together. Weaker by construction — see [`NOTE`].
    suspect_dependencies: Vec<SuspectRow>,
    /// Co-changing pairs with a test-like path on either side. Coupled
    /// by construction, kept separate so they cannot be read as hidden.
    test_pairs: Vec<TestPairRow>,
    /// Co-changing pairs where no language backend reads one side. No
    /// static view rather than no dependency.
    no_static_view: Vec<OutsideRow>,
    /// Per-module call-site resolution counts: a module whose calls
    /// mostly go unresolved has edges missing from the static side, so
    /// its pairs are over-reported as hidden.
    modules: Vec<ModuleResolutionSummary>,
}

#[derive(Debug, Serialize)]
struct ThresholdsView {
    min_support: u32,
    min_confidence: f64,
    max_commit_files: usize,
}

#[derive(Debug, Serialize)]
struct HistoryView {
    /// The window's size: commits counted and skipped, files seen, and
    /// pairs that co-changed at all.
    #[serde(flatten)]
    totals: TotalsView,
    /// Pairs that cleared the thresholds — the population the buckets
    /// partition.
    cochanging_pair_count: usize,
}

/// [`CoChangeTotals`] as report fields. The domain type is not
/// `Serialize` (it is arithmetic, not a schema), and naming the four
/// keys here is what pins them to this analyzer's JSON shape.
#[derive(Debug, Serialize)]
struct TotalsView {
    commit_count: usize,
    skipped_commit_count: usize,
    file_count: usize,
    candidate_pair_count: usize,
}

impl From<CoChangeTotals> for TotalsView {
    fn from(t: CoChangeTotals) -> Self {
        Self {
            commit_count: t.commit_count,
            skipped_commit_count: t.skipped_commit_count,
            file_count: t.file_count,
            candidate_pair_count: t.candidate_pair_count,
        }
    }
}

#[derive(Debug, Serialize)]
struct StaticViewSummary {
    /// Files a language backend reads. A pair with a file outside this
    /// set lands in `no_static_view`.
    file_count: usize,
    /// Directed file-level edges from both projections.
    edge_count: usize,
    language: &'static str,
    resolved_call_edge_count: usize,
    /// Call sites the resolver could not attribute. Every one of these
    /// is an edge the static side is missing.
    unresolved_call_site_count: usize,
    module_graph: ModuleGraphCoverage,
}

/// The co-change half of a row, identical in every bucket so the four
/// tables read the same way.
#[derive(Debug, Serialize)]
struct PairView {
    a: String,
    b: String,
    cochanges: u32,
    commits_a: u32,
    commits_b: u32,
    confidence_a_to_b: f64,
    confidence_b_to_a: f64,
    lift: f64,
    score: f64,
    last_cochange: String,
    last_cochange_commits_ago: usize,
}

impl From<&CoChangePair> for PairView {
    fn from(p: &CoChangePair) -> Self {
        Self {
            a: p.a.clone(),
            b: p.b.clone(),
            cochanges: p.cochanges,
            commits_a: p.commits_a,
            commits_b: p.commits_b,
            confidence_a_to_b: p.confidence_a_to_b,
            confidence_b_to_a: p.confidence_b_to_a,
            lift: p.lift,
            score: p.score,
            last_cochange: p.last_cochange.clone(),
            last_cochange_commits_ago: p.last_cochange_commits_ago,
        }
    }
}

#[derive(Debug, Serialize)]
struct HiddenRow {
    #[serde(flatten)]
    pair: PairView,
    #[serde(rename = "static")]
    verdict: StaticVerdict,
}

#[derive(Debug, Serialize)]
struct SuspectRow {
    a: String,
    b: String,
    direction: Direction,
    /// Which projection declared the edge: `call`, `import`, or both.
    edge: &'static str,
    cochanges: u32,
    commits_a: u32,
    commits_b: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_cochange: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_cochange_commits_ago: Option<usize>,
}

#[derive(Debug, Serialize)]
struct TestPairRow {
    #[serde(flatten)]
    pair: PairView,
    /// Which side(s) the test/production split classes as tests.
    tests: Vec<String>,
}

#[derive(Debug, Serialize)]
struct OutsideRow {
    #[serde(flatten)]
    pair: PairView,
    /// Which side(s) no language backend reads.
    outside: Vec<String>,
}

/// Everything the report is folded from, so building it stays one
/// argument plus the analyzer whose options it restates.
struct ReportInputs<'a> {
    roots: &'a AnalyzeRoots,
    scope: &'a ChurnScope,
    shallow_clone: bool,
    counts: &'a CoChangeCounts,
    cochanging: &'a [CoChangePair],
    call_graph: &'a CallGraph,
    static_view: &'a StaticGraphView,
    filter: &'a CompiledPathFilter,
}

impl Report {
    fn build(analyzer: &HiddenCouplingAnalyzer, input: ReportInputs<'_>) -> Self {
        let ReportInputs {
            roots,
            scope,
            shallow_clone,
            counts,
            cochanging,
            call_graph,
            static_view,
            filter,
        } = input;
        let mut hidden = Vec::new();
        let mut test_pairs = Vec::new();
        let mut no_static_view = Vec::new();
        for pair in cochanging {
            match classify(pair, &static_view.graph, filter) {
                Bucket::TestPair(tests) => test_pairs.push(TestPairRow {
                    pair: pair.into(),
                    tests,
                }),
                Bucket::NoStaticView(outside) => no_static_view.push(OutsideRow {
                    pair: pair.into(),
                    outside,
                }),
                Bucket::Hidden(verdict) => hidden.push(HiddenRow {
                    pair: pair.into(),
                    verdict,
                }),
                Bucket::Expected => {}
            }
        }
        // `no_path` above `transitive`: a pair with no chain at all is
        // the undeclared contract, while one relating through four
        // intermediates is only weakly explained. Within each, the
        // co-change ranking decides.
        hidden.sort_by(|x, y| {
            relation_rank(x.verdict.relation)
                .cmp(&relation_rank(y.verdict.relation))
                .then_with(|| y.pair.score.total_cmp(&x.pair.score))
                .then_with(|| y.pair.cochanges.cmp(&x.pair.cochanges))
                .then_with(|| x.pair.a.cmp(&y.pair.a))
                .then_with(|| x.pair.b.cmp(&y.pair.b))
        });

        Self {
            schema_version: SCHEMA_VERSION,
            target: roots.display(),
            repo_root: scope.repo_root().display().to_string(),
            since: analyzer.since.clone(),
            note: NOTE,
            shallow_clone,
            thresholds: ThresholdsView {
                min_support: analyzer.thresholds.min_support,
                min_confidence: analyzer.thresholds.min_confidence,
                max_commit_files: analyzer.thresholds.max_commit_files,
            },
            history: HistoryView {
                totals: counts.totals().into(),
                cochanging_pair_count: cochanging.len(),
            },
            static_view: StaticViewSummary {
                file_count: static_view.graph.file_count(),
                edge_count: static_view.graph.edge_count(),
                language: call_graph.language,
                resolved_call_edge_count: call_graph
                    .edges
                    .iter()
                    .filter(|e| e.resolution == Resolution::Resolved)
                    .count(),
                unresolved_call_site_count: call_graph
                    .module_summary
                    .iter()
                    .map(|m| m.total_call_count - m.calls.resolved_call_count)
                    .sum(),
                module_graph: static_view.module_graph,
            },
            hidden_coupling: hidden,
            suspect_dependencies: suspect_dependencies(
                &static_view.graph,
                counts,
                filter,
                analyzer.thresholds.min_support,
            ),
            test_pairs,
            no_static_view,
            modules: call_graph.module_summary.clone(),
        }
    }
}

fn relation_rank(relation: StaticRelation) -> u8 {
    match relation {
        StaticRelation::NoPath => 0,
        StaticRelation::Transitive => 1,
        StaticRelation::Direct => 2,
    }
}

/// Declared dependencies the window has no evidence for.
///
/// Both endpoints must have moved at least `min_support` times inside
/// the window: a dependency between two files that barely changed says
/// nothing at all, and reporting it would bury the rows where both sides
/// were busy and still never moved together. Test pairs are excluded for
/// the same reason they are excluded above — a test that declares a
/// dependency on its subject is not evidence about the subject.
fn suspect_dependencies(
    graph: &StaticFileGraph,
    counts: &lens_domain::CoChangeCounts,
    filter: &CompiledPathFilter,
    min_support: u32,
) -> Vec<SuspectRow> {
    let mut rows: Vec<SuspectRow> = graph
        .direct_edges()
        .into_iter()
        .filter(|edge| !filter.is_test_relative(edge.a) && !filter.is_test_relative(edge.b))
        .filter_map(|edge| {
            let commits_a = counts.commits_for(edge.a);
            let commits_b = counts.commits_for(edge.b);
            if commits_a < min_support || commits_b < min_support {
                return None;
            }
            let support = counts.support(edge.a, edge.b);
            let cochanges = support.map_or(0, |s| s.cochanges);
            if cochanges >= min_support {
                return None;
            }
            Some(SuspectRow {
                a: edge.a.to_owned(),
                b: edge.b.to_owned(),
                direction: edge.direction,
                edge: edge.source.label(),
                cochanges,
                commits_a,
                commits_b,
                last_cochange: support.map(|s| s.last_cochange.to_owned()),
                last_cochange_commits_ago: support.map(|s| s.last_cochange_commits_ago),
            })
        })
        .collect();
    // The busier both endpoints were, the more the window had to say and
    // the louder its silence is.
    rows.sort_by(|x, y| {
        y.commits_a
            .min(y.commits_b)
            .cmp(&x.commits_a.min(x.commits_b))
            .then_with(|| x.cochanges.cmp(&y.cochanges))
            .then_with(|| x.a.cmp(&y.a))
            .then_with(|| x.b.cmp(&y.b))
    });
    rows
}

fn format_markdown(report: &Report, top: usize) -> String {
    let scope = report
        .since
        .as_deref()
        .map_or_else(String::new, |s| format!(", since {s}"));
    let mut out = format!(
        "# Hidden coupling report: {} ({} hidden, {} suspect over {} commit(s){scope})\n",
        report.target,
        report.hidden_coupling.len(),
        report.suspect_dependencies.len(),
        report.history.totals.commit_count,
    );
    let _ = writeln!(&mut out, "\n{NOTE}");
    if report.shallow_clone {
        out.push_str(
            "\n**Shallow clone**: the log is truncated, so co-change evidence before the graft \
             is missing entirely — every declared dependency whose history predates it looks \
             suspect. Run `git fetch --unshallow` before trusting this report.\n",
        );
    }
    render_summary(&mut out, report);
    render_hidden(&mut out, report, top);
    render_suspect(&mut out, report, top);
    render_test_pairs(&mut out, report, top);
    render_no_static_view(&mut out, report, top);
    render_module_confidence(
        &mut out,
        &report.modules,
        "Unresolved call sites are static edges nobody can see, so a pair inside these modules \
         is more likely to be reported as hidden than it deserves.",
    );
    out
}

fn render_summary(out: &mut String, report: &Report) {
    let _ = writeln!(
        out,
        "\n## Summary\n\
         - history: {} commit(s) counted, {} skipped over `--max-commit-files {}`; {} file(s); \
         {} of {} co-changing pair(s) cleared the thresholds\n\
         - static view: {} file(s), {} directed edge(s) from {} module graph(s) and {} resolved \
         call edge(s); {} call site(s) unresolved\n\
         - thresholds: support >= {}, max confidence >= {:.2}\n\
         - buckets: {} hidden, {} suspect, {} test pair(s), {} outside the static view",
        report.history.totals.commit_count,
        report.history.totals.skipped_commit_count,
        report.thresholds.max_commit_files,
        report.history.totals.file_count,
        report.history.cochanging_pair_count,
        report.history.totals.candidate_pair_count,
        report.static_view.file_count,
        report.static_view.edge_count,
        report.static_view.module_graph.roots,
        report.static_view.resolved_call_edge_count,
        report.static_view.unresolved_call_site_count,
        report.thresholds.min_support,
        report.thresholds.min_confidence,
        report.hidden_coupling.len(),
        report.suspect_dependencies.len(),
        report.test_pairs.len(),
        report.no_static_view.len(),
    );
}

/// The co-change columns every bucket table shares, and the cells that
/// fill them. The three buckets carrying a pair are the same evidence
/// under three different explanations, so they read as one table shape
/// with at most one extra column.
const PAIR_COLUMNS: &str = "a | b | co | a→b | b→a | lift | last";
const PAIR_RULE: &str = "--- | --- | ---: | ---: | ---: | ---: | ---";

fn render_pair_cells(pair: &PairView) -> String {
    format!(
        "{} | {} | {} | {:.2} | {:.2} | {:.2} | {} ({} ago)",
        pair.a,
        pair.b,
        pair.cochanges,
        pair.confidence_a_to_b,
        pair.confidence_b_to_a,
        pair.lift,
        pair.last_cochange,
        pair.last_cochange_commits_ago,
    )
}

/// Write a bucket's table header, and its rows through `cells`.
fn render_pair_table<R>(out: &mut String, rows: &[R], top: usize, cells: impl Fn(&R) -> String) {
    let _ = writeln!(out, "| {PAIR_COLUMNS} |");
    let _ = writeln!(out, "| {PAIR_RULE} |");
    for row in rows.iter().take(top) {
        let _ = writeln!(out, "| {} |", cells(row));
    }
    render_overflow(out, rows.len(), top, "pair(s)");
}

/// Close a capped table, naming what was left out. Nothing is written
/// when nothing was capped: "+0 more" reads as truncation that never
/// happened.
fn render_overflow(out: &mut String, total: usize, shown: usize, unit: &str) {
    let overflow = total.saturating_sub(shown);
    if overflow > 0 {
        let _ = writeln!(
            out,
            "\n+{overflow} more {unit} not shown (raise `--top`; JSON carries every row)."
        );
    }
}

fn render_hidden(out: &mut String, report: &Report, top: usize) {
    let _ = writeln!(
        out,
        "\n## Hidden coupling ({} pair(s): co-changed, nothing declares the dependency)\n",
        report.hidden_coupling.len(),
    );
    if report.hidden_coupling.is_empty() {
        out.push_str(
            "_Every co-changing pair is either declared, a test pair, or outside the static \
             view._\n",
        );
        return;
    }
    let _ = writeln!(out, "| {PAIR_COLUMNS} | static |");
    let _ = writeln!(out, "| {PAIR_RULE} | --- |");
    for row in report.hidden_coupling.iter().take(top) {
        let _ = writeln!(
            out,
            "| {} | {} |",
            render_pair_cells(&row.pair),
            render_verdict(&row.verdict),
        );
    }
    render_overflow(out, report.hidden_coupling.len(), top, "pair(s)");
}

fn render_verdict(verdict: &StaticVerdict) -> String {
    match (verdict.relation, verdict.distance) {
        (StaticRelation::NoPath, _) => "no path".to_owned(),
        (StaticRelation::Direct, _) => "direct".to_owned(),
        (StaticRelation::Transitive, Some(distance)) => format!("{distance} hops"),
        (StaticRelation::Transitive, None) => "transitive".to_owned(),
    }
}

fn render_suspect(out: &mut String, report: &Report, top: usize) {
    let _ = writeln!(
        out,
        "\n## Suspect dependencies ({} edge(s): declared, but the window never moved them \
         together)\n\n\
         Weaker than the bucket above, by construction: over one window a stable, correct \
         dependency and a dead one look the same. Both endpoints changed at least \
         `--min-support` times here, which is what makes the silence worth a question — is the \
         edge still load-bearing?\n",
        report.suspect_dependencies.len(),
    );
    if report.suspect_dependencies.is_empty() {
        out.push_str("_No declared dependency between two files this busy went unexercised._\n");
        return;
    }
    let _ = writeln!(out, "| a | b | edge | dir | co | commits_a | commits_b |");
    let _ = writeln!(out, "| --- | --- | --- | --- | ---: | ---: | ---: |");
    for row in report.suspect_dependencies.iter().take(top) {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} |",
            row.a,
            row.b,
            row.edge,
            render_direction(row.direction),
            row.cochanges,
            row.commits_a,
            row.commits_b,
        );
    }
    render_overflow(out, report.suspect_dependencies.len(), top, "edge(s)");
}

fn render_direction(direction: Direction) -> &'static str {
    match direction {
        Direction::AToB => "a→b",
        Direction::BToA => "b→a",
        Direction::Both => "both",
    }
}

fn render_test_pairs(out: &mut String, report: &Report, top: usize) {
    if report.test_pairs.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "\n## Test pairs ({} pair(s): coupled by construction, not a finding)\n",
        report.test_pairs.len(),
    );
    render_pair_table(out, &report.test_pairs, top, |row| {
        render_pair_cells(&row.pair)
    });
}

fn render_no_static_view(out: &mut String, report: &Report, top: usize) {
    if report.no_static_view.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "\n## Outside the static view ({} pair(s): no language backend reads one side)\n\n\
         Not a missing dependency — there is no static view of these files to miss one. A doc, \
         a manifest, or a fixture that always moves with a source file is still a real \
         contract, and the only place it is written down is here.\n",
        report.no_static_view.len(),
    );
    render_pair_table(out, &report.no_static_view, top, |row| {
        render_pair_cells(&row.pair)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};
    use rstest::rstest;
    use std::path::Path;

    /// One crate whose history is built to land a pair in every bucket
    /// at once, so a classification that leaks into the wrong bucket
    /// shows up as two failures rather than none.
    ///
    /// Static shape (`use` edges, and therefore module-graph edges):
    ///
    /// * `a -> b`, `f -> g`, `h -> mid -> i`
    /// * `c`, `d`, `e` depend on nothing and nothing depends on them
    ///
    /// History, on top of one initial commit touching everything:
    ///
    /// | pair | commits | expected bucket |
    /// | --- | --- | --- |
    /// | `c` + `d` | 4 | hidden, no path |
    /// | `h` + `i` | 4 | hidden, transitive at 2 |
    /// | `f` + `g` | 4 | expected — declared *and* co-changing |
    /// | `a` + `tests/a_test.rs` | 4 | test pair |
    /// | `e` + `notes.md` | 4 | outside the static view |
    /// | `a`, then `b`, separately | 4 each | suspect: declared, never together |
    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);

        write_file(
            dir,
            "src/lib.rs",
            "pub mod a;\npub mod b;\npub mod c;\npub mod d;\npub mod e;\npub mod f;\n\
             pub mod g;\npub mod h;\npub mod i;\npub mod mid;\n",
        );
        write_file(
            dir,
            "src/mid.rs",
            "use crate::i::leaf;\npub fn step() -> u8 { leaf() }\n",
        );
        for path in SOURCES {
            write_file(dir, path, &body(path, 0));
        }
        write_file(dir, "tests/a_test.rs", "// case 0\n");
        write_file(dir, "notes.md", "# notes\n");
        commit(dir, "initial");

        for revision in 1..=4 {
            touch(dir, &["src/c.rs", "src/d.rs"], revision);
            touch(dir, &["src/h.rs", "src/i.rs"], revision);
            touch(dir, &["src/f.rs", "src/g.rs"], revision);
            touch(dir, &["src/a.rs"], revision);
            touch(dir, &["src/b.rs"], revision);
            // A test that moves with its subject, and a doc that moves
            // with a source file no static view can relate it to.
            write_file(dir, "src/a.rs", &body("src/a.rs", revision + 10));
            write_file(dir, "tests/a_test.rs", &format!("// case {revision}\n"));
            commit(dir, "test");
            write_file(dir, "src/e.rs", &body("src/e.rs", revision));
            write_file(dir, "notes.md", &format!("# notes {revision}\n"));
            commit(dir, "docs");
        }
    }

    /// The files the history rewrites. Everything else in the fixture is
    /// written once and never moves again, so it can pair with nothing.
    const SOURCES: &[&str] = &[
        "src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/e.rs", "src/f.rs", "src/g.rs",
        "src/h.rs", "src/i.rs",
    ];

    /// One file's contents at `revision`. The `use` line and the call it
    /// makes are part of every revision: the static shape has to survive
    /// the whole history, or a pair's classification would depend on
    /// which commit the working tree happens to sit at.
    fn body(path: &str, revision: usize) -> String {
        match path {
            "src/a.rs" => {
                format!("use crate::b::work;\npub fn run() -> u8 {{ work() + {revision} }}\n")
            }
            "src/f.rs" => {
                format!("use crate::g::helper;\npub fn call() -> u8 {{ helper() + {revision} }}\n")
            }
            "src/h.rs" => {
                format!("use crate::mid::step;\npub fn top() -> u8 {{ step() + {revision} }}\n")
            }
            "src/b.rs" => format!("pub fn work() -> u8 {{ {revision} }}\n"),
            "src/g.rs" => format!("pub fn helper() -> u8 {{ {revision} }}\n"),
            "src/i.rs" => format!("pub fn leaf() -> u8 {{ {revision} }}\n"),
            other => {
                let name = other.trim_start_matches("src/").trim_end_matches(".rs");
                format!("pub fn {name}() -> u8 {{ {revision} }}\n")
            }
        }
    }

    /// Rewrite each named file at `revision` and commit them as one
    /// change.
    fn touch(dir: &Path, files: &[&str], revision: usize) {
        for path in files {
            write_file(dir, path, &body(path, revision));
        }
        commit(dir, "edit");
    }

    fn commit(dir: &Path, message: &str) {
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-q", "-m", message]);
    }

    fn json(analyzer: &HiddenCouplingAnalyzer, dir: &Path) -> serde_json::Value {
        let out = analyzer.analyze(dir, OutputFormat::Json).unwrap();
        serde_json::from_str(&out).unwrap()
    }

    /// Pair spellings in one bucket, as `"a b"` strings.
    fn pairs(report: &serde_json::Value, bucket: &str) -> Vec<String> {
        report[bucket]
            .as_array()
            .unwrap_or_else(|| panic!("no {bucket} in {report}"))
            .iter()
            .map(|row| {
                format!(
                    "{} {}",
                    row["a"].as_str().unwrap(),
                    row["b"].as_str().unwrap()
                )
            })
            .collect()
    }

    #[test]
    fn every_bucket_gets_only_the_pair_it_explains() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let report = json(&HiddenCouplingAnalyzer::new(), dir.path());

        assert_eq!(
            pairs(&report, "hidden_coupling"),
            ["src/c.rs src/d.rs", "src/h.rs src/i.rs"],
            "got {report}",
        );
        assert_eq!(
            pairs(&report, "test_pairs"),
            ["src/a.rs tests/a_test.rs"],
            "got {report}",
        );
        assert_eq!(
            pairs(&report, "no_static_view"),
            ["notes.md src/e.rs"],
            "got {report}",
        );
        assert_eq!(
            pairs(&report, "suspect_dependencies"),
            ["src/a.rs src/b.rs"],
            "got {report}",
        );
    }

    /// The pair the differential exists to *not* report: declared and
    /// co-changing is the expected case, and it must appear in no bucket
    /// at all.
    #[test]
    fn a_declared_dependency_that_co_changes_is_reported_nowhere() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let report = json(&HiddenCouplingAnalyzer::new(), dir.path());
        for bucket in [
            "hidden_coupling",
            "suspect_dependencies",
            "test_pairs",
            "no_static_view",
        ] {
            assert!(
                !pairs(&report, bucket).contains(&"src/f.rs src/g.rs".to_owned()),
                "the expected pair leaked into {bucket}: {report}",
            );
        }
    }

    /// The two hidden rows carry different evidence and must say so:
    /// nothing relates `c` and `d` at all, while `h` reaches `i` through
    /// one intermediate. `no_path` ranks first because it is the
    /// stronger finding.
    #[test]
    fn a_hidden_row_states_whether_there_is_no_path_or_only_a_long_one() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let report = json(&HiddenCouplingAnalyzer::new(), dir.path());
        let hidden = report["hidden_coupling"].as_array().unwrap();

        assert_eq!(hidden[0]["static"]["relation"], "no_path", "got {report}");
        assert!(
            hidden[0]["static"].get("distance").is_none(),
            "got {report}"
        );
        assert_eq!(
            hidden[1]["static"],
            serde_json::json!({
                "relation": "transitive",
                "distance": 2,
                "direction": "a_to_b",
            }),
            "got {report}",
        );
    }

    /// Every bucket row carries the co-change evidence it was classified
    /// from, so a reader can weigh the row without a second run.
    #[test]
    fn a_hidden_row_carries_both_sides_of_the_evidence() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let report = json(&HiddenCouplingAnalyzer::new(), dir.path());
        let row = &report["hidden_coupling"][0];
        assert_eq!(row["cochanges"], 5, "got {report}");
        assert_eq!(row["commits_a"], 5);
        assert_eq!(row["commits_b"], 5);
        assert!((row["confidence_a_to_b"].as_f64().unwrap() - 1.0).abs() < 1e-9);
        assert!(row["lift"].as_f64().unwrap() > 1.0);
        assert_eq!(row["last_cochange"].as_str().unwrap().len(), 10);
    }

    /// A suspect row is only worth reading when the window had something
    /// to say about both files. Raising the bar past `b`'s own commit
    /// count must empty the bucket rather than keep a row nothing
    /// supports.
    #[test]
    fn a_suspect_dependency_needs_both_endpoints_to_have_moved() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let quiet = json(
            &HiddenCouplingAnalyzer::new().with_min_support(6),
            dir.path(),
        );
        assert!(
            pairs(&quiet, "suspect_dependencies").is_empty(),
            "`b` moved 5 times, under the bar: {quiet}",
        );
    }

    /// The suspect row names what declared the edge: `a` imports from
    /// `b` and calls into it, so both projections saw it.
    #[test]
    fn a_suspect_row_names_the_projection_that_declared_the_edge() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let report = json(&HiddenCouplingAnalyzer::new(), dir.path());
        let row = &report["suspect_dependencies"][0];
        assert_eq!(row["edge"], "call+import", "got {report}");
        assert_eq!(row["direction"], "a_to_b");
        assert_eq!(row["cochanges"], 1, "only the initial commit: {report}");
        assert_eq!(row["commits_a"], 9);
        assert_eq!(row["commits_b"], 5);
    }

    /// `--exclude-tests` drops the test half of the history, so the
    /// bucket that exists to hold it empties instead of its rows
    /// reappearing somewhere they would read as findings.
    #[test]
    fn excluding_tests_empties_the_test_bucket_rather_than_moving_it() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let report = json(
            &HiddenCouplingAnalyzer::new().with_exclude_tests(true),
            dir.path(),
        );
        assert!(pairs(&report, "test_pairs").is_empty(), "got {report}");
        assert!(
            !pairs(&report, "hidden_coupling")
                .iter()
                .any(|pair| pair.contains("tests/")),
            "got {report}",
        );
    }

    /// An `--exclude` glob has to reach both halves: a file dropped from
    /// the history but kept in the static view would still be counted as
    /// a declared dependency nothing exercises.
    #[test]
    fn an_exclude_glob_reaches_the_history_and_the_static_graph() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let report = json(
            &HiddenCouplingAnalyzer::new().with_exclude_patterns(vec!["src/b.rs".to_owned()]),
            dir.path(),
        );
        assert!(
            pairs(&report, "suspect_dependencies").is_empty(),
            "the excluded endpoint still carried its edge: {report}",
        );
    }

    #[test]
    fn the_report_states_the_size_of_both_views_it_subtracted() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let report = json(&HiddenCouplingAnalyzer::new(), dir.path());
        assert_eq!(report["history"]["commit_count"], 29, "got {report}");
        // `c/d`, `h/i`, `f/g`, `a`/its test, and `e`/`notes.md`.
        assert_eq!(
            report["history"]["cochanging_pair_count"], 5,
            "got {report}"
        );
        assert!(report["history"]["candidate_pair_count"].as_u64().unwrap() > 5);
        // Eleven `.rs` files plus the integration test; `notes.md` is
        // the one tracked file no backend reads.
        assert_eq!(report["static_view"]["file_count"], 12, "got {report}");
        assert!(report["static_view"]["edge_count"].as_u64().unwrap() >= 4);
        assert_eq!(report["static_view"]["module_graph"]["roots"], 1);
        assert_eq!(report["schema_version"], SCHEMA_VERSION);
    }

    #[test]
    fn a_since_window_scopes_the_history_side_only() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let report = json(
            &HiddenCouplingAnalyzer::new().with_since("2099-01-01"),
            dir.path(),
        );
        assert_eq!(report["since"], "2099-01-01");
        assert_eq!(report["history"]["commit_count"], 0);
        assert!(pairs(&report, "hidden_coupling").is_empty(), "got {report}");
        assert!(
            report["static_view"]["file_count"].as_u64().unwrap() > 0,
            "the static graph is a property of the current source: {report}",
        );
    }

    #[test]
    fn since_option_is_applied_when_present() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let report = json(
            &HiddenCouplingAnalyzer::new().with_since_opt(Some("2099-01-01".to_owned())),
            dir.path(),
        );
        assert_eq!(report["since"], "2099-01-01");
    }

    #[rstest]
    #[case::heading("# Hidden coupling report:")]
    #[case::note("not a finding on its own")]
    #[case::summary("## Summary")]
    #[case::hidden("## Hidden coupling (2 pair(s)")]
    #[case::hidden_row("| src/c.rs | src/d.rs |")]
    #[case::hidden_verdict("| no path |")]
    #[case::transitive_verdict("| 2 hops |")]
    #[case::suspect("## Suspect dependencies (1 edge(s)")]
    #[case::suspect_row("| src/a.rs | src/b.rs | call+import |")]
    #[case::tests("## Test pairs (1 pair(s)")]
    #[case::outside("## Outside the static view (1 pair(s)")]
    fn markdown_keeps_every_bucket_distinct(#[case] needle: &str) {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let md = HiddenCouplingAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains(needle), "missing {needle}\ngot {md}");
    }

    #[test]
    fn markdown_caps_each_bucket_and_says_what_is_left() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let md = HiddenCouplingAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("+1 more pair(s) not shown"), "got {md}");
        assert!(!md.contains("| src/h.rs | src/i.rs |"), "got {md}");
    }

    /// A cap nothing exceeded must not carry an overflow line: "+0 more"
    /// reads as truncation that never happened.
    #[test]
    fn markdown_says_nothing_about_overflow_when_nothing_was_capped() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let md = HiddenCouplingAnalyzer::new()
            .with_top(Some(20))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(!md.contains("more pair(s) not shown"), "got {md}");
        assert!(!md.contains("more edge(s) not shown"), "got {md}");
    }

    /// A repository whose history explains every co-changing pair still
    /// has to render both bucket headings, or an empty section reads as
    /// a section that failed to run.
    #[test]
    fn an_empty_bucket_says_so_rather_than_disappearing() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(dir.path(), "src/lib.rs", "pub fn only() -> u8 { 0 }\n");
        commit(dir.path(), "initial");

        let md = HiddenCouplingAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("_Every co-changing pair is either declared, a test pair, or outside"),
            "got {md}",
        );
        assert!(
            md.contains("_No declared dependency between two files this busy went unexercised._"),
            "got {md}",
        );
        assert!(!md.contains("## Test pairs"), "got {md}");
        assert!(!md.contains("## Outside the static view"), "got {md}");
    }

    /// The same reason `co-change` rejects it: an unreachable cut
    /// empties the history side, and an empty report would read as
    /// "nothing here is hidden" rather than "you asked for something
    /// impossible".
    #[rstest]
    #[case::above_one(1.5)]
    #[case::negative(-0.1)]
    fn an_unreachable_min_confidence_is_rejected(#[case] value: f64) {
        let dir = tempfile::tempdir().unwrap();
        let err = HiddenCouplingAnalyzer::new()
            .with_min_confidence(value)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        let HiddenCouplingError::MinConfidenceOutOfRange { value: reported } = err else {
            panic!("expected MinConfidenceOutOfRange, got {err:?}");
        };
        assert!((reported - value).abs() < f64::EPSILON);
    }

    #[test]
    fn target_directory_outside_git_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "pub fn f() {}\n");
        let err = HiddenCouplingAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(
            matches!(err, HiddenCouplingError::NotInGitRepo { .. }),
            "{err:?}",
        );
    }

    #[test]
    fn missing_path_surfaces_io_error() {
        let err = HiddenCouplingAnalyzer::new()
            .analyze(Path::new("/definitely/does/not/exist"), OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, HiddenCouplingError::Io { .. }), "{err:?}");
    }

    #[test]
    fn an_invalid_exclude_glob_surfaces_a_path_filter_error() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let err = HiddenCouplingAnalyzer::new()
            .with_exclude_patterns(vec!["[".to_owned()])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, HiddenCouplingError::PathFilter(_)), "{err:?}",);
    }

    #[rstest]
    #[case::io(
        HiddenCouplingError::Io {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        },
        &["/tmp/x", "missing", "failed to read"],
    )]
    #[case::git(
        HiddenCouplingError::Git { stderr: "fatal: not a git repo\n".to_owned() },
        &["fatal: not a git repo"],
    )]
    #[case::not_in_git_repo(
        HiddenCouplingError::NotInGitRepo { path: PathBuf::from("/tmp/lonely") },
        &["/tmp/lonely", "not inside a git working tree"],
    )]
    #[case::min_confidence_out_of_range(
        HiddenCouplingError::MinConfidenceOutOfRange { value: 1.5 },
        &["--min-confidence", "1.5", "[0.0, 1.0]"],
    )]
    fn error_display_carries_the_diagnostic(
        #[case] err: HiddenCouplingError,
        #[case] needles: &[&str],
    ) {
        let msg = err.to_string();
        for needle in needles {
            assert!(msg.contains(needle), "missing {needle} in {msg}");
        }
        assert!(!msg.ends_with('\n'), "trailing newline in {msg}");
    }
}
