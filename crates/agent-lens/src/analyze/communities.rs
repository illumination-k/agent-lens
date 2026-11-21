//! `analyze communities` — the module clusters the dependencies form,
//! against the module boundaries the repository declares.
//!
//! `layers` answers *is the dependency direction sane* — levelization,
//! skip-level calls, cycles. This answers the orthogonal question:
//! *is the grouping sane*. A repository declares a partition of its
//! files by filing each one under a directory; the dependency graph
//! forms a partition of its own. Where the two disagree, one of them is
//! wrong, and the disagreement is measurable.
//!
//! The headline is a pair of modularity scores over the same graph: `Q`
//! for the detected partition, and `Q` for the declared one. A declared
//! score close to the detected one means the directory structure is the
//! clustering — the architecture matches reality. The gap between them
//! is what the actionable rows are made of:
//!
//! * **misfiled members** — a file whose community is dominated by a
//!   different declared module, kept only when it has more edge weight
//!   to that module than to the one it is filed under. That is the
//!   concrete move candidate: "`analyze/hotspot.rs` is filed under
//!   `analyze` but its dependency neighbourhood is elsewhere".
//! * **spanning communities** — a cluster spread over several declared
//!   modules with none of them owning a majority: a feature that never
//!   got a home.
//!
//! Ranking is by evidence strength — how lopsided a member's in/out edge
//! counts are — never by community size, so a big cluster does not push
//! a well-evidenced single-file finding off the report.
//!
//! Both granularities read the same substrate as `coupling`: the
//! language-specific module graph from [`super::module_graph`], with
//! each cross-module reference contributing one unit of undirected edge
//! weight. `--granularity file` makes every module-graph node (one
//! source file, or one Go package) a member and its containing directory
//! the declared group; `--granularity module` collapses files into their
//! containing module first, so the members are directories and the
//! declared group is the directory above. On a flat tree the second
//! collapses to a single declared group, which the report says outright
//! rather than dressing up.
//!
//! Three knobs change what is clustered and how:
//!
//! * `--algorithm greedy|leiden` — Clauset–Newman–Moore agglomeration
//!   (the default) or deterministic Leiden, which is faster on large
//!   graphs, never returns a disconnected community, and usually finds a
//!   higher `Q`.
//! * `--resolution γ` — the modularity resolution. Above `1` favours
//!   smaller communities, below `1` larger ones. Both modularity figures
//!   are scored at the same `γ`, so the gap stays comparable.
//! * `--weights static|co-change|both` — what an edge is. `static` is the
//!   module graph above. `co-change` reads git history and weights each
//!   file pair by the Jaccard index of the commits touching them
//!   (`together / (either)`), keeping only pairs with at least
//!   `--min-support` shared commits. `both` normalises each source to a
//!   unit of total weight and blends them, `--co-change-share` deciding
//!   the history half's share.
//!
//! Whenever history is read, the report also partitions the static graph
//! and the co-change graph on their own and compares the two: their
//! normalised mutual information, and the members whose community-mates
//! the two sources disagree on most. A member that clusters with one set
//! of files by its imports and with another by what it is edited
//! alongside is the place an undeclared contract lives.
//!
//! The partition is deterministic by construction — see
//! [`lens_domain::communities`] for why, and for the property test that
//! holds it to it. Determinism is not a nicety here: a report an agent
//! cannot diff against the previous run is not evidence.
//!
//! Limitations, inherited from the graph rather than the algorithm:
//!
//! * Only references the extractors resolve become edges, so an
//!   unresolved import weakens the clustering exactly as it weakens
//!   `coupling` — same caveat, same graph.
//! * Modularity has a resolution limit: a small genuine cluster can
//!   score better absorbed into a larger neighbour. Community sizes are
//!   reported so a reader who knows the codebase can see it happening.
//! * On a small or densely-connected tree nearly everything lands in one
//!   community. That is an honest answer about the graph, and it is
//!   reported as one rather than split into plausible-looking noise.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use lens_domain::{
    Community, CommunityAlgorithm, CommunityEdge, CommunityNode, CommunityParams, CommunityReport,
    DEFAULT_MAX_COMMIT_FILES, DEFAULT_MIN_COMMUNITY, DEFAULT_MIN_SUPPORT, DEFAULT_RESOLUTION,
    DeclaredShare, MisfiledMember, ModulePath, PartitionAgreement, SpanningCommunity,
    compare_partitions, detect_communities, partition_nodes, tally_cochange,
};
use serde::Serialize;

use super::format;
use super::history::churn::ChurnScope;
use super::history::error_from::impl_from_churn_error;
use super::module_graph::{ModuleGraph, build_graph};
use super::module_label::ModuleLabeler;
use super::{AnalyzePathFilter, AnalyzeRoots, CrateAnalyzerError, OutputFormat, PathFilterError};

/// What the report does and does not license. Carried in the output
/// because "these files cluster together" is one sentence away from
/// "move them", and the distance between the two is where an agent would
/// otherwise over-read the result.
const NOTE: &str = "Detected communities are what the edges cluster into — static references, \
     co-change history, or both, per `weights`; the declared partition is the module each member \
     is filed under. `modularity_gap` is the distance between the two — near zero means the \
     declared boundaries already are the ones the edges form. A misfiled row is a move candidate, not a defect: it says a member has more edge \
     weight to another declared module than to its own, and nothing about why. Modularity has a \
     resolution limit, so a small genuine cluster can be absorbed into a larger neighbour — read \
     community sizes before trusting a partition. Only resolved references become edges, the same \
     caveat `coupling` carries.";

/// Communities and misfiled rows listed in markdown when `--top` is not
/// given. JSON always carries every row.
const DEFAULT_TOP: usize = 20;

/// Members named inline in a markdown community row before the rest are
/// summarised as a count. A community can hold most of the graph, and a
/// table cell listing eighty module paths is not a table any more.
const MEMBERS_PER_ROW: usize = 6;

/// What a community member is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Granularity {
    /// One member per module-graph node — a source file for Rust,
    /// TS/JS and Python, a package for Go — with its containing module
    /// as the declared group. This is the granularity that produces
    /// move candidates naming a file.
    #[default]
    File,
    /// Collapse files into their containing module first, so a member is
    /// a directory and the declared group is the directory above it.
    /// Answers whether a subtree is filed under the right parent, and
    /// degenerates to a single declared group on a flat tree.
    Module,
}

impl Granularity {
    fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Module => "module",
        }
    }
}

/// Which partitioner runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Algorithm {
    /// Clauset–Newman–Moore greedy agglomeration: take the best single
    /// merge until none helps.
    #[default]
    Greedy,
    /// Deterministic Leiden: local moving, refinement, aggregation.
    /// Faster on large graphs, never returns a disconnected community,
    /// and usually finds a higher modularity.
    Leiden,
}

impl Algorithm {
    fn as_str(self) -> &'static str {
        match self {
            Self::Greedy => "greedy",
            Self::Leiden => "leiden",
        }
    }

    fn domain(self) -> CommunityAlgorithm {
        match self {
            Self::Greedy => CommunityAlgorithm::Greedy,
            Self::Leiden => CommunityAlgorithm::Leiden,
        }
    }
}

/// What an edge between two members is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WeightSource {
    /// Resolved static references from the module graph, one unit per
    /// distinct reference.
    #[default]
    Static,
    /// Git co-change: the Jaccard index of the commits touching each
    /// file pair. Needs a git working tree.
    CoChange,
    /// Both, each normalised to one unit of total weight and blended by
    /// `--co-change-share`.
    Both,
}

impl WeightSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Static => "static",
            Self::CoChange => "co-change",
            Self::Both => "both",
        }
    }

    fn reads_history(self) -> bool {
        !matches!(self, Self::Static)
    }
}

/// Default share of total edge weight the co-change half carries under
/// `--weights both`.
pub const DEFAULT_CO_CHANGE_SHARE: f64 = 0.5;

/// Errors raised by `analyze communities`.
#[derive(Debug, thiserror::Error)]
pub enum CommunitiesError {
    #[error(transparent)]
    Graph(#[from] CrateAnalyzerError),
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `git` is missing or returned a non-zero exit status.
    #[error("git failed: {}", stderr.trim_end())]
    Git { stderr: String },
    /// Co-change weights were asked for outside a git working tree.
    #[error("{path:?} is not inside a git working tree; co-change weights need history")]
    NotInGitRepo { path: std::path::PathBuf },
    /// `γ` scales the null-model term; zero or below inverts what a
    /// community is, and a non-finite value scores nothing.
    #[error("--resolution must be a finite number above 0; got {value}")]
    ResolutionOutOfRange { value: f64 },
    #[error("--co-change-share must be within [0.0, 1.0]; got {value}")]
    CoChangeShareOutOfRange { value: f64 },
    #[error("failed to serialize report: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error(transparent)]
    PathFilter(#[from] PathFilterError),
    #[error(transparent)]
    UnsupportedFormat(#[from] super::jsonl::UnsupportedFormat),
}

impl_from_churn_error!(CommunitiesError);

/// `analyze communities` flags, and the `[profile.<name>.communities]`
/// table.
///
/// Written out rather than generated by `analyzer_options!` because
/// `min-community` and `granularity` carry real clap defaults, and the
/// generated `Default` would disagree with them.
#[derive(Debug, Clone, clap::Args, serde::Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct CommunitiesOptions {
    /// Cap the markdown listings to the top-N entries. JSON output
    /// always carries the full list.
    #[arg(long)]
    pub top: Option<usize>,
    /// What a community member is: one module-graph node (`file`), or
    /// the containing module those nodes collapse into (`module`).
    #[arg(long, value_enum, default_value_t = Granularity::File)]
    pub granularity: Granularity,
    /// Smallest community that gets reported. Below this a community is
    /// a member the edges gave no home to, which is a fact about that
    /// member rather than a cluster worth naming. The partition — and
    /// therefore both modularity figures — is unaffected.
    #[arg(long, default_value_t = DEFAULT_MIN_COMMUNITY)]
    pub min_community: usize,
    /// Partitioner: greedy agglomeration (`greedy`) or deterministic
    /// Leiden (`leiden`).
    #[arg(long, value_enum, default_value_t = Algorithm::Greedy)]
    pub algorithm: Algorithm,
    /// Modularity resolution `γ`. Above 1 favours smaller communities,
    /// below 1 larger ones; both modularity figures are scored at it.
    #[arg(long, default_value_t = DEFAULT_RESOLUTION)]
    pub resolution: f64,
    /// What an edge is: static references (`static`), git co-change
    /// (`co-change`), or a normalised blend of the two (`both`).
    #[arg(long, value_enum, default_value_t = WeightSource::Static)]
    pub weights: WeightSource,
    /// Under `--weights both`, the share of total edge weight carried by
    /// co-change; the static graph carries the rest.
    #[arg(long, default_value_t = DEFAULT_CO_CHANGE_SHARE)]
    pub co_change_share: f64,
    /// Restrict history to commits in this `--since=` window (git
    /// approxidate, e.g. `90.days.ago`). Only read with co-change
    /// weights.
    #[arg(long)]
    pub since: Option<String>,
    /// Fewest shared commits for a file pair to become a co-change edge.
    /// One shared commit is coincidence, not a pattern.
    #[arg(long, default_value_t = DEFAULT_MIN_SUPPORT)]
    pub min_support: u32,
    /// Commits touching more files than this are dropped whole, so a
    /// repo-wide rename does not couple everything to everything.
    #[arg(long, default_value_t = DEFAULT_MAX_COMMIT_FILES)]
    pub max_commit_files: usize,
}

impl Default for CommunitiesOptions {
    fn default() -> Self {
        Self {
            top: None,
            granularity: Granularity::default(),
            min_community: DEFAULT_MIN_COMMUNITY,
            algorithm: Algorithm::default(),
            resolution: DEFAULT_RESOLUTION,
            weights: WeightSource::default(),
            co_change_share: DEFAULT_CO_CHANGE_SHARE,
            since: None,
            min_support: DEFAULT_MIN_SUPPORT,
            max_commit_files: DEFAULT_MAX_COMMIT_FILES,
        }
    }
}

/// How the history half is read, when it is.
#[derive(Debug, Clone, PartialEq)]
struct HistoryOptions {
    since: Option<String>,
    min_support: u32,
    max_commit_files: usize,
    co_change_share: f64,
}

impl Default for HistoryOptions {
    fn default() -> Self {
        Self {
            since: None,
            min_support: DEFAULT_MIN_SUPPORT,
            max_commit_files: DEFAULT_MAX_COMMIT_FILES,
            co_change_share: DEFAULT_CO_CHANGE_SHARE,
        }
    }
}

/// Analyzer entry point.
#[derive(Debug, Clone)]
pub struct CommunitiesAnalyzer {
    path_filter: AnalyzePathFilter,
    top: Option<usize>,
    granularity: Granularity,
    min_community: usize,
    algorithm: Algorithm,
    resolution: f64,
    weights: WeightSource,
    history: HistoryOptions,
}

impl Default for CommunitiesAnalyzer {
    fn default() -> Self {
        Self {
            path_filter: AnalyzePathFilter::default(),
            top: None,
            granularity: Granularity::default(),
            min_community: DEFAULT_MIN_COMMUNITY,
            algorithm: Algorithm::default(),
            resolution: DEFAULT_RESOLUTION,
            weights: WeightSource::default(),
            history: HistoryOptions::default(),
        }
    }
}

impl CommunitiesAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a whole [`CommunitiesOptions`] group. The CLI flags and the
    /// `[profile.<name>.communities]` table are the same type, so this
    /// is the only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: CommunitiesOptions) -> Self {
        self.with_top(opts.top)
            .with_granularity(opts.granularity)
            .with_min_community(opts.min_community)
            .with_algorithm(opts.algorithm)
            .with_resolution(opts.resolution)
            .with_weights(opts.weights)
            .with_co_change_share(opts.co_change_share)
            .with_since_opt(opts.since)
            .with_min_support(opts.min_support)
            .with_max_commit_files(opts.max_commit_files)
    }

    /// Cap the markdown listings to the top-N rows. JSON output always
    /// carries every row. `None` uses the markdown default of 20.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    pub fn with_granularity(mut self, granularity: Granularity) -> Self {
        self.granularity = granularity;
        self
    }

    pub fn with_min_community(mut self, min_community: usize) -> Self {
        self.min_community = min_community;
        self
    }

    pub fn with_algorithm(mut self, algorithm: Algorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    pub fn with_resolution(mut self, resolution: f64) -> Self {
        self.resolution = resolution;
        self
    }

    pub fn with_weights(mut self, weights: WeightSource) -> Self {
        self.weights = weights;
        self
    }

    pub fn with_co_change_share(mut self, share: f64) -> Self {
        self.history.co_change_share = share;
        self
    }

    pub fn with_min_support(mut self, min_support: u32) -> Self {
        self.history.min_support = min_support;
        self
    }

    /// Restrict the history half to a `--since=` window.
    pub fn with_since_opt(mut self, since: Option<String>) -> Self {
        self.history.since = since;
        self
    }

    pub fn with_max_commit_files(mut self, max_commit_files: usize) -> Self {
        self.history.max_commit_files = max_commit_files;
        self
    }

    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_only_tests(only_tests);
        self
    }

    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_exclude_tests(exclude_tests);
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.path_filter = self.path_filter.with_exclude_patterns(exclude);
        self
    }

    /// Resolve `path`, build the language-specific module graph, read
    /// history when the weights ask for it, detect communities, and
    /// score them against the declared grouping.
    pub fn analyze(
        &self,
        path: impl AsRef<Path>,
        format: OutputFormat,
    ) -> Result<String, CommunitiesError> {
        self.validate()?;
        let path = path.as_ref();
        let mut graph = build_graph(path, &self.path_filter).map_err(CommunitiesError::Graph)?;
        let filter = self.path_filter.compile(&graph.root)?;
        graph.modules.retain(|m| filter.includes_path(&m.file));
        let kept: BTreeSet<&ModulePath> = graph.modules.iter().map(|m| &m.path).collect();
        graph
            .edges
            .retain(|e| kept.contains(&e.from) && kept.contains(&e.to));

        let member = Membership::new(&graph, self.granularity);
        let nodes = member_nodes(&graph, &member);
        let static_edges = static_edges(&graph, &member);
        let history = if self.weights.reads_history() {
            Some(self.read_history(path, &graph, &member)?)
        } else {
            None
        };
        let edges = match (&history, self.weights) {
            (Some(h), WeightSource::CoChange) => h.edges.clone(),
            (Some(h), WeightSource::Both) => {
                blend(&static_edges, &h.edges, self.history.co_change_share)
            }
            _ => static_edges.clone(),
        };
        let params = CommunityParams {
            algorithm: self.algorithm.domain(),
            resolution: self.resolution,
            min_community: self.min_community,
        };
        let report = detect_communities(&nodes, &edges, params);
        let agreement = history.as_ref().map(|h| {
            let algorithm = self.algorithm.domain();
            compare_partitions(
                &partition_nodes(&nodes, &static_edges, algorithm, self.resolution),
                &partition_nodes(&nodes, &h.edges, algorithm, self.resolution),
            )
        });
        let view = ReportView::new(&graph, self, &report, history.as_ref(), agreement.as_ref());
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&view).map_err(CommunitiesError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&view, self.top)),
            OutputFormat::Jsonl => Err(super::jsonl::UnsupportedFormat.into()),
        }
    }

    fn validate(&self) -> Result<(), CommunitiesError> {
        if !(self.resolution.is_finite() && self.resolution > 0.0) {
            return Err(CommunitiesError::ResolutionOutOfRange {
                value: self.resolution,
            });
        }
        let share = self.history.co_change_share;
        if !(0.0..=1.0).contains(&share) {
            return Err(CommunitiesError::CoChangeShareOutOfRange { value: share });
        }
        Ok(())
    }

    /// Read the commit window and turn every co-changing file pair that
    /// lands on two distinct members into a Jaccard-weighted edge.
    fn read_history(
        &self,
        path: &Path,
        graph: &ModuleGraph,
        member: &Membership,
    ) -> Result<History, CommunitiesError> {
        // A crate root or entry file names the graph, not the history:
        // every file the graph read sits under its directory.
        let history_root = if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        let scope = ChurnScope::resolve(&AnalyzeRoots::from(history_root))?;
        let shallow = scope.warn_if_shallow(
            "co-change weights are \
             lower bounds and pairs whose evidence predates the graft are missing. Fetch the \
             full history (`git fetch --unshallow`) for a usable clustering.",
        );
        let commits = scope.collect_commits(self.history.since.as_deref())?;
        let counts = tally_cochange(&commits, self.history.max_commit_files);
        let member_of_file: BTreeMap<String, String> = graph
            .modules
            .iter()
            .map(|m| {
                (
                    scope.key_for_absolute(&m.file),
                    member.of(&m.path).to_string(),
                )
            })
            .collect();
        let mut edges = Vec::new();
        for (a, b, together) in counts.pairs() {
            if together < self.history.min_support {
                continue;
            }
            let (Some(ma), Some(mb)) = (member_of_file.get(a), member_of_file.get(b)) else {
                continue;
            };
            if ma == mb {
                continue;
            }
            let either = counts.commits_for(a) + counts.commits_for(b) - together;
            edges.push(CommunityEdge::new(
                ma.clone(),
                mb.clone(),
                f64::from(together) / f64::from(either.max(1)),
            ));
        }
        let totals = counts.totals();
        Ok(History {
            edges,
            commit_count: totals.commit_count,
            skipped_commit_count: totals.skipped_commit_count,
            shallow,
        })
    }
}

/// The co-change half of the input, and the window it was read from.
struct History {
    edges: Vec<CommunityEdge>,
    commit_count: usize,
    skipped_commit_count: usize,
    shallow: bool,
}

/// Scale each source to one unit of total weight and blend them, the
/// co-change half carrying `share` of the result. A source with no
/// weight at all contributes nothing and leaves the other at full
/// strength, so an empty history does not halve every static edge.
fn blend(
    static_edges: &[CommunityEdge],
    history: &[CommunityEdge],
    share: f64,
) -> Vec<CommunityEdge> {
    let total = |edges: &[CommunityEdge]| -> f64 {
        let mut weights: Vec<f64> = edges.iter().map(|e| e.weight).collect();
        weights.sort_by(f64::total_cmp);
        weights.into_iter().sum()
    };
    let (static_total, history_total) = (total(static_edges), total(history));
    let (static_share, history_share) = match (static_total > 0.0, history_total > 0.0) {
        (true, true) => (1.0 - share, share),
        (true, false) => (1.0, 0.0),
        (false, true) => (0.0, 1.0),
        (false, false) => (0.0, 0.0),
    };
    let scaled = |edges: &[CommunityEdge], factor: f64| -> Vec<CommunityEdge> {
        edges
            .iter()
            .map(|e| CommunityEdge::new(e.a.clone(), e.b.clone(), e.weight * factor))
            .collect::<Vec<_>>()
    };
    // A source with no share scales to zero weight, which the detector
    // drops as uninformative.
    let mut out = scaled(
        static_edges,
        static_share / static_total.max(f64::MIN_POSITIVE),
    );
    out.extend(scaled(
        history,
        history_share / history_total.max(f64::MIN_POSITIVE),
    ));
    out
}

/// Turn the module graph's references into static edges at the
/// requested granularity.
///
/// Edge weight is the number of *distinct* cross-module references, so
/// ten `use` lines naming the same symbol do not outweigh ten naming
/// different ones. The dedup key is the whole edge, which is the same
/// unit `coupling` counts as its Number of Couplings.
fn static_edges(graph: &ModuleGraph, member: &Membership) -> Vec<CommunityEdge> {
    graph
        .edges
        .iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|e| {
            CommunityEdge::new(
                member.of(&e.from).to_string(),
                member.of(&e.to).to_string(),
                1,
            )
        })
        .collect()
}

/// The member population, each with the group it is declared in.
fn member_nodes(graph: &ModuleGraph, member: &Membership) -> Vec<CommunityNode> {
    graph
        .modules
        .iter()
        .map(|m| {
            let id = member.of(&m.path);
            let declared = member.declared(&id);
            (id.to_string(), declared.to_string())
        })
        .collect::<BTreeMap<String, String>>()
        .into_iter()
        .map(|(id, declared)| CommunityNode::new(id, declared))
        .collect()
}

/// Which member a module path belongs to, and which group that member is
/// declared in.
///
/// Two facts do all the work.
///
/// **A member is a file, not a module-tree node.** Rust puts several
/// modules in one file — every `#[cfg(test)] mod tests` is one — and they
/// are not separately filed anywhere, so they fold into the file that
/// holds them and their references become that file's. Without this every
/// `foo` would look like a container of `foo::tests`, and the declared
/// group of `config.rs` would come out as `crate::config` itself. The
/// other three languages already emit one node per file (or per Go
/// package), so the fold is a no-op there.
///
/// **A file member is a container when another file member names it as
/// its parent.** `crate::analyze` holds `crate::analyze::hotspot`, so it is
/// the module those files are declared in — and it is declared in
/// itself, because `analyze/mod.rs` *is* the `analyze` module. A file
/// that holds nothing is declared in its parent.
struct Membership {
    /// Module path → the primary module of the file it was read from.
    file_of: BTreeMap<ModulePath, ModulePath>,
    containers: BTreeSet<ModulePath>,
    granularity: Granularity,
}

impl Membership {
    fn new(graph: &ModuleGraph, granularity: Granularity) -> Self {
        // The smallest path in a file is the one the file is named
        // after: every other module in it is nested inside that one, so
        // the file's own path is their prefix and sorts ahead of them.
        let mut primary: BTreeMap<&Path, &ModulePath> = BTreeMap::new();
        for module in &graph.modules {
            let owner = primary.entry(module.file.as_path()).or_insert(&module.path);
            if module.path < **owner {
                *owner = &module.path;
            }
        }
        let file_of: BTreeMap<ModulePath, ModulePath> = graph
            .modules
            .iter()
            .map(|m| {
                let owner = primary
                    .get(m.file.as_path())
                    .copied()
                    .unwrap_or(&m.path)
                    .clone();
                (m.path.clone(), owner)
            })
            .collect();
        let members: BTreeSet<&ModulePath> = file_of.values().collect();
        let containers = members
            .iter()
            .filter_map(|m| m.parent())
            .filter(|parent| members.contains(parent))
            .collect();
        Self {
            file_of,
            containers,
            granularity,
        }
    }

    /// The member `path` counts as: the file holding it, or — at module
    /// granularity — the module that file is declared in.
    fn of(&self, path: &ModulePath) -> ModulePath {
        let file = self.file_of.get(path).unwrap_or(path);
        match self.granularity {
            Granularity::File => file.clone(),
            Granularity::Module => self.container(file),
        }
    }

    /// The declared group a member sits in: its containing module at
    /// file granularity, its parent at module granularity (where the
    /// member already *is* a container).
    fn declared(&self, member: &ModulePath) -> ModulePath {
        match self.granularity {
            Granularity::File => self.container(member),
            Granularity::Module => member.parent().unwrap_or_else(|| member.clone()),
        }
    }

    /// The module a file is declared in. A container is declared in
    /// itself; anything else in its parent, and a root with no parent in
    /// itself.
    fn container(&self, path: &ModulePath) -> ModulePath {
        if self.containers.contains(path) {
            return path.clone();
        }
        path.parent().unwrap_or_else(|| path.clone())
    }
}

#[derive(Debug, Serialize)]
struct ReportView {
    crate_root: String,
    granularity: &'static str,
    algorithm: &'static str,
    resolution: f64,
    weights: &'static str,
    note: &'static str,
    /// The commit window co-change weights were read from, when they
    /// were.
    #[serde(skip_serializing_if = "Option::is_none")]
    history: Option<HistoryView>,
    /// Module-graph nodes read before members were folded out of them.
    /// Above `node_count` whenever a language puts several modules in
    /// one file, which for Rust is every `#[cfg(test)] mod tests`.
    module_count: usize,
    node_count: usize,
    edge_count: usize,
    total_weight: f64,
    /// Members with no resolved reference either way. They cannot be
    /// clustered, so they argue neither for nor against a boundary.
    isolated_node_count: usize,
    community_count: usize,
    declared_group_count: usize,
    largest_community: usize,
    min_community: usize,
    modularity: ModularityView,
    communities: Vec<CommunityView>,
    misfiled: Vec<MisfiledView>,
    spanning: Vec<SpanningView>,
    /// Static-only against co-change-only partition, when history was
    /// read.
    #[serde(skip_serializing_if = "Option::is_none")]
    agreement: Option<AgreementView>,
}

impl ReportView {
    fn new(
        graph: &ModuleGraph,
        analyzer: &CommunitiesAnalyzer,
        report: &CommunityReport,
        history: Option<&History>,
        agreement: Option<&PartitionAgreement>,
    ) -> Self {
        let labeler = &graph.labeler;
        Self {
            crate_root: graph.root.display().to_string(),
            granularity: analyzer.granularity.as_str(),
            algorithm: analyzer.algorithm.as_str(),
            resolution: analyzer.resolution,
            weights: analyzer.weights.as_str(),
            note: NOTE,
            history: history.map(|h| HistoryView {
                since: analyzer.history.since.clone(),
                commit_count: h.commit_count,
                skipped_commit_count: h.skipped_commit_count,
                co_change_edge_count: h.edges.len(),
                min_support: analyzer.history.min_support,
                co_change_share: (analyzer.weights == WeightSource::Both)
                    .then_some(analyzer.history.co_change_share),
                shallow_clone: h.shallow,
            }),
            module_count: graph.modules.len(),
            node_count: report.node_count,
            edge_count: report.edge_count,
            total_weight: report.total_weight,
            isolated_node_count: report.isolated_node_count,
            community_count: report.community_count,
            declared_group_count: report.declared_group_count,
            largest_community: report.largest_community,
            min_community: analyzer.min_community,
            modularity: ModularityView {
                detected: report.detected_modularity,
                declared: report.declared_modularity,
                gap: report.modularity_gap,
                declared_quality: report.declared_quality,
            },
            communities: report
                .communities
                .iter()
                .map(|c| CommunityView::new(c, labeler))
                .collect(),
            misfiled: report
                .misfiled
                .iter()
                .map(|m| MisfiledView::new(m, labeler))
                .collect(),
            spanning: report
                .spanning
                .iter()
                .map(|s| SpanningView::new(s, labeler))
                .collect(),
            agreement: agreement.map(|a| AgreementView::new(a, labeler)),
        }
    }
}

#[derive(Debug, Serialize)]
struct HistoryView {
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<String>,
    commit_count: usize,
    skipped_commit_count: usize,
    /// Member pairs the history contributed an edge for.
    co_change_edge_count: usize,
    min_support: u32,
    /// Share of total weight carried by co-change under `both`.
    #[serde(skip_serializing_if = "Option::is_none")]
    co_change_share: Option<f64>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    shallow_clone: bool,
}

/// The static-only partition against the co-change-only one.
#[derive(Debug, Serialize)]
struct AgreementView {
    /// Members placed by both sources (non-isolated in each).
    compared: usize,
    /// Normalised mutual information; `1.0` for identical groupings.
    #[serde(skip_serializing_if = "Option::is_none")]
    nmi: Option<f64>,
    /// Members whose community-mates overlap below half between the two
    /// sources, least overlap first.
    divergent: Vec<DivergentView>,
}

#[derive(Debug, Serialize)]
struct DivergentView {
    member: String,
    static_mates: usize,
    co_change_mates: usize,
    shared_mates: usize,
    jaccard: f64,
}

impl AgreementView {
    fn new(agreement: &PartitionAgreement, labeler: &ModuleLabeler) -> Self {
        Self {
            compared: agreement.compared,
            nmi: agreement.nmi,
            divergent: agreement
                .divergent
                .iter()
                .map(|d| DivergentView {
                    member: label(&d.node, labeler),
                    static_mates: d.first_mates,
                    co_change_mates: d.second_mates,
                    shared_mates: d.shared_mates,
                    jaccard: d.jaccard,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ModularityView {
    /// `Q_γ` for the partition the edges support.
    detected: f64,
    /// `Q_γ` for the declared partition, computed over the same
    /// graph so the two are comparable.
    declared: f64,
    /// `detected - declared`. Zero means the declared boundaries already
    /// are the detected ones.
    gap: f64,
    /// `declared / detected`, omitted when the graph has no community
    /// structure to compare against. Above `1.0` when the declared
    /// grouping beats the one the search found, which neither
    /// partitioner rules out — each finds a good partition, not the
    /// optimal one.
    #[serde(skip_serializing_if = "Option::is_none")]
    declared_quality: Option<f64>,
}

#[derive(Debug, Serialize)]
struct ShareView {
    declared: String,
    members: usize,
}

impl ShareView {
    fn list(shares: &[DeclaredShare], labeler: &ModuleLabeler) -> Vec<Self> {
        shares
            .iter()
            .map(|share| Self {
                declared: label(&share.declared, labeler),
                members: share.members,
            })
            .collect()
    }
}

#[derive(Debug, Serialize)]
struct CommunityView {
    id: usize,
    size: usize,
    dominant_declared: String,
    declared_group_count: usize,
    internal_weight: f64,
    external_weight: f64,
    breakdown: Vec<ShareView>,
    members: Vec<String>,
}

impl CommunityView {
    fn new(community: &Community, labeler: &ModuleLabeler) -> Self {
        Self {
            id: community.id,
            size: community.size,
            dominant_declared: label(&community.dominant_declared, labeler),
            declared_group_count: community.breakdown.len(),
            internal_weight: community.internal_weight,
            external_weight: community.external_weight,
            breakdown: ShareView::list(&community.breakdown, labeler),
            members: community
                .members
                .iter()
                .map(|m| label(m, labeler))
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct MisfiledView {
    member: String,
    declared: String,
    suggested: String,
    community: usize,
    weight_to_declared: f64,
    weight_to_suggested: f64,
    evidence: f64,
}

impl MisfiledView {
    fn new(row: &MisfiledMember, labeler: &ModuleLabeler) -> Self {
        Self {
            member: label(&row.node, labeler),
            declared: label(&row.declared, labeler),
            suggested: label(&row.suggested, labeler),
            community: row.community,
            weight_to_declared: row.weight_to_declared,
            weight_to_suggested: row.weight_to_suggested,
            evidence: row.evidence,
        }
    }
}

#[derive(Debug, Serialize)]
struct SpanningView {
    community: usize,
    size: usize,
    declared_group_count: usize,
    breakdown: Vec<ShareView>,
}

impl SpanningView {
    fn new(row: &SpanningCommunity, labeler: &ModuleLabeler) -> Self {
        Self {
            community: row.community,
            size: row.size,
            declared_group_count: row.declared_group_count,
            breakdown: ShareView::list(&row.breakdown, labeler),
        }
    }
}

/// Render a canonical module path in the analyzed language's own
/// spelling. The graph keeps one shape for every language; only the
/// report learns about per-language syntax.
fn label(path: &str, labeler: &ModuleLabeler) -> String {
    labeler.label(&ModulePath::new(path))
}

fn format_markdown(view: &ReportView, top: Option<usize>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    let mut out = format!(
        "# Communities report: {} ({} member(s), {} edge(s), {} communit(ies) at {} granularity)\n",
        view.crate_root, view.node_count, view.edge_count, view.community_count, view.granularity,
    );
    let _ = writeln!(
        &mut out,
        "\nalgorithm: {}, resolution: {}, weights: {}",
        view.algorithm,
        weight(view.resolution),
        view.weights,
    );
    if let Some(history) = &view.history {
        render_history(&mut out, history);
    }
    let _ = writeln!(&mut out, "\n{}", view.note);
    if view.node_count == 0 {
        out.push_str("\n_No modules discovered._\n");
        return out;
    }
    render_modularity(&mut out, view);
    render_misfiled(&mut out, &view.misfiled, limit);
    render_communities(&mut out, view, limit);
    render_spanning(&mut out, &view.spanning, limit);
    if let Some(agreement) = &view.agreement {
        render_agreement(&mut out, agreement, limit);
    }
    out
}

/// A weight rendered as an integer when it is one — static counts stay
/// counts — and to three decimals otherwise.
fn weight(value: f64) -> String {
    if (value - value.round()).abs() < 1e-9 {
        format!("{value:.0}")
    } else {
        format!("{value:.3}")
    }
}

fn render_history(out: &mut String, history: &HistoryView) {
    let mut line = format!(
        "history: {} commit(s) ({} skipped as oversized), {} co-change edge(s) at min-support {}",
        history.commit_count,
        history.skipped_commit_count,
        history.co_change_edge_count,
        history.min_support,
    );
    if let Some(since) = &history.since {
        let _ = write!(line, ", since {since}");
    }
    if let Some(share) = history.co_change_share {
        let _ = write!(line, ", co-change share {share:.2}");
    }
    if history.shallow_clone {
        line.push_str(", shallow clone: counts are lower bounds");
    }
    let _ = writeln!(out, "{line}");
}

fn render_agreement(out: &mut String, agreement: &AgreementView, limit: usize) {
    let _ = writeln!(out, "\n## Static vs co-change partitions\n");
    match agreement.nmi {
        Some(nmi) => {
            let _ = writeln!(
                out,
                "- NMI {nmi:.3} over {} member(s) both sources place",
                agreement.compared,
            );
        }
        None => {
            let _ = writeln!(
                out,
                "- {} member(s) placed by both sources: too few to compare",
                agreement.compared,
            );
            return;
        }
    }
    if agreement.divergent.is_empty() {
        out.push_str(
            "\n_No member's community-mates differ by more than half between the two sources._\n",
        );
        return;
    }
    let _ = writeln!(
        out,
        "\n| member | static mates | co-change mates | shared | jaccard |"
    );
    let _ = writeln!(out, "| --- | ---: | ---: | ---: | ---: |");
    for row in agreement.divergent.iter().take(limit) {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {:.2} |",
            row.member, row.static_mates, row.co_change_mates, row.shared_mates, row.jaccard,
        );
    }
    render_overflow(out, agreement.divergent.len(), limit, "divergent member(s)");
}

fn render_modularity(out: &mut String, view: &ReportView) {
    let _ = writeln!(out, "\n## Modularity\n");
    let _ = writeln!(out, "| partition | Q | groups |");
    let _ = writeln!(out, "| --- | ---: | ---: |");
    let _ = writeln!(
        out,
        "| detected | {:.3} | {} |",
        view.modularity.detected, view.community_count,
    );
    let _ = writeln!(
        out,
        "| declared | {:.3} | {} |",
        view.modularity.declared, view.declared_group_count,
    );
    let _ = writeln!(out, "\n- gap: {:.3}{}", view.modularity.gap, quality(view));
    let _ = writeln!(
        out,
        "- largest community: {} of {} member(s)",
        view.largest_community, view.node_count,
    );
    if view.module_count > view.node_count {
        let _ = writeln!(
            out,
            "- {} module(s) folded into {} member(s): several modules share one file",
            view.module_count, view.node_count,
        );
    }
    if view.isolated_node_count > 0 {
        let _ = writeln!(
            out,
            "- {} member(s) have no resolved reference either way and cluster with nothing",
            view.isolated_node_count,
        );
    }
    if view.community_count <= 1 {
        out.push_str(
            "\n_One community holds the whole graph: at this size and density the dependencies \
             carry no boundary the declared structure could disagree with._\n",
        );
    }
}

/// The gap alone is hard to read without knowing what `Q` the graph can
/// support at all, so it is followed by the ratio whenever there is a
/// detected partition to take a ratio against.
fn quality(view: &ReportView) -> String {
    let Some(ratio) = view.modularity.declared_quality else {
        return " (no community structure to compare against)".to_owned();
    };
    if view.modularity.declared < 0.0 {
        // A negative `Q` is not a small fraction of a positive one, and
        // printing the ratio anyway ("scores -1.02 of the detected one")
        // reads as a bug. Say what a negative score means instead.
        return "; the declared partition scores below zero — its groups share fewer edges than                 chance would give them"
            .to_owned();
    }
    format!("; the declared partition scores {ratio:.2} of the detected one")
}

fn render_misfiled(out: &mut String, rows: &[MisfiledView], limit: usize) {
    let _ = writeln!(out, "\n## Misfiled members (by evidence, top {limit})\n");
    if rows.is_empty() {
        out.push_str(
            "_None: every member has at least as much edge weight to its own declared module as \
             to the one its community is named after._\n",
        );
        return;
    }
    let _ = writeln!(
        out,
        "| member | declared | clusters with | →suggested | →declared | evidence |"
    );
    let _ = writeln!(out, "| --- | --- | --- | ---: | ---: | ---: |");
    for row in rows.iter().take(limit) {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} |",
            row.member,
            row.declared,
            row.suggested,
            weight(row.weight_to_suggested),
            weight(row.weight_to_declared),
            weight(row.evidence),
        );
    }
    render_overflow(out, rows.len(), limit, "misfiled member(s)");
}

fn render_communities(out: &mut String, view: &ReportView, limit: usize) {
    if view.communities.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "\n## Communities (size >= {}, top {limit})\n",
        view.min_community,
    );
    let _ = writeln!(
        out,
        "| # | size | dominant | groups | internal | external | members |"
    );
    let _ = writeln!(out, "| ---: | ---: | --- | ---: | ---: | ---: | --- |");
    for c in view.communities.iter().take(limit) {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} |",
            c.id,
            c.size,
            c.dominant_declared,
            c.declared_group_count,
            weight(c.internal_weight),
            weight(c.external_weight),
            members_cell(&c.members),
        );
    }
    render_overflow(out, view.communities.len(), limit, "communities");
}

fn members_cell(members: &[String]) -> String {
    let shown = members
        .iter()
        .take(MEMBERS_PER_ROW)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let rest = members.len().saturating_sub(MEMBERS_PER_ROW);
    if rest == 0 {
        shown
    } else {
        format!("{shown}, +{rest} more")
    }
}

fn render_spanning(out: &mut String, rows: &[SpanningView], limit: usize) {
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## Spanning communities (top {limit})\n");
    for row in rows.iter().take(limit) {
        let breakdown = row
            .breakdown
            .iter()
            .map(|s| format!("{} ({})", s.declared, s.members))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "- #{} — {} member(s) across {} declared module(s): {breakdown}",
            row.community, row.size, row.declared_group_count,
        );
    }
    render_overflow(out, rows.len(), limit, "spanning communities");
}

/// [`format::render_overflow`] with the cap clamped to what exists, as
/// every listing here passes the raw `--top`.
fn render_overflow(out: &mut String, total: usize, limit: usize, unit: &str) {
    format::render_overflow(out, total, total.min(limit), unit);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};
    use rstest::rstest;
    use std::path::PathBuf;

    /// A crate with a planted misfile: `a::stray` is filed under `a` but
    /// every reference it makes runs into `b`, while the rest of `a` and
    /// the rest of `b` each keep to themselves.
    fn planted_crate(dir: &Path) -> PathBuf {
        let lib = write_file(dir, "lib.rs", "pub mod a;\npub mod b;\n");
        write_file(
            dir,
            "a/mod.rs",
            "pub mod one;\npub mod two;\npub mod stray;\n",
        );
        write_file(dir, "a/one.rs", "pub struct One;\npub fn one() {}\n");
        write_file(
            dir,
            "a/two.rs",
            "use crate::a::one::One;\npub fn two(_o: One) { crate::a::one::one(); }\n",
        );
        write_file(dir, "b/mod.rs", "pub mod p;\npub mod q;\n");
        write_file(dir, "b/p.rs", "pub struct P;\npub fn p() {}\n");
        write_file(
            dir,
            "b/q.rs",
            "use crate::b::p::P;\npub fn q(_p: P) { crate::b::p::p(); }\n",
        );
        // The finding: filed under `a`, wired entirely into `b`.
        write_file(
            dir,
            "a/stray.rs",
            "use crate::b::p::P;\nuse crate::b::q::q;\npub fn stray(p: P) { q(p); crate::b::p::p(); }\n",
        );
        lib
    }

    fn report(path: &Path, analyzer: CommunitiesAnalyzer) -> serde_json::Value {
        let json = analyzer.analyze(path, OutputFormat::Json).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn a_planted_misfiled_module_is_reported_with_its_edge_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let json = report(&lib, CommunitiesAnalyzer::new());

        let rows = json["misfiled"].as_array().unwrap();
        let stray = rows
            .iter()
            .find(|r| r["member"] == "crate::a::stray")
            .unwrap_or_else(|| panic!("no stray row in {json:#}"));
        assert_eq!(stray["declared"], "crate::a", "got {json:#}");
        assert_eq!(stray["suggested"], "crate::b", "got {json:#}");
        assert!(
            stray["weight_to_suggested"].as_f64().unwrap()
                > stray["weight_to_declared"].as_f64().unwrap(),
            "got {json:#}",
        );
        assert!(stray["evidence"].as_f64().unwrap() >= 1.0, "got {json:#}");
    }

    /// The modules that keep to their own subtree must not be reported:
    /// a misfiled listing that names half the crate is noise, not a
    /// finding.
    #[test]
    fn modules_wired_within_their_own_declared_module_are_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let json = report(&lib, CommunitiesAnalyzer::new());
        let named: Vec<&str> = json["misfiled"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["member"].as_str())
            .collect();
        for kept in ["crate::a::two", "crate::b::q", "crate::b::p"] {
            assert!(!named.contains(&kept), "{kept} reported in {json:#}");
        }
    }

    /// Both modularity figures describe the whole graph, so raising
    /// `--min-community` may empty the listings but must not move the
    /// headline numbers.
    #[test]
    fn min_community_bounds_the_listings_not_the_modularity() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let wide = report(&lib, CommunitiesAnalyzer::new().with_min_community(2));
        let narrow = report(&lib, CommunitiesAnalyzer::new().with_min_community(99));

        assert_eq!(wide["modularity"], narrow["modularity"]);
        assert_eq!(wide["community_count"], narrow["community_count"]);
        assert!(!wide["communities"].as_array().unwrap().is_empty());
        assert!(narrow["communities"].as_array().unwrap().is_empty());
    }

    /// At module granularity the members are directories, so `crate::a`
    /// and `crate::b` are the nodes and their declared group is `crate`.
    #[test]
    fn module_granularity_reports_directories_as_members() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let json = report(
            &lib,
            CommunitiesAnalyzer::new().with_granularity(Granularity::Module),
        );
        assert_eq!(json["granularity"], "module", "got {json:#}");
        let members: BTreeSet<&str> = json["communities"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["members"].as_array().unwrap())
            .filter_map(|m| m.as_str())
            .collect();
        assert!(members.contains("crate::a"), "got {json:#}");
        assert!(members.contains("crate::b"), "got {json:#}");
        assert!(!members.contains("crate::a::stray"), "got {json:#}");
        // Everything is filed directly under the crate root here, so the
        // declared partition has exactly one group — the degenerate case
        // this granularity has on a flat tree, reported rather than hidden.
        assert_eq!(json["declared_group_count"], 1, "got {json:#}");
    }

    /// A file's containing module is itself when it holds submodules
    /// (`a/mod.rs` lives in `a/`, not in the crate root), which is what
    /// keeps `crate::a` from being counted as a member of `crate`.
    #[test]
    fn a_module_holding_submodules_is_declared_in_itself() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let graph = build_graph(&lib, &AnalyzePathFilter::new()).unwrap();
        let member = Membership::new(&graph, Granularity::File);

        assert_eq!(
            member.declared(&ModulePath::new("crate::a")),
            ModulePath::new("crate::a"),
        );
        assert_eq!(
            member.declared(&ModulePath::new("crate::a::stray")),
            ModulePath::new("crate::a"),
        );
        assert_eq!(
            member.declared(&ModulePath::new("crate")),
            ModulePath::new("crate"),
        );
    }

    /// Determinism at the analyzer boundary, not just in the domain: two
    /// runs over the same tree must produce byte-identical output, or a
    /// report cannot be diffed against the previous one.
    #[rstest]
    #[case(OutputFormat::Json)]
    #[case(OutputFormat::Md)]
    fn repeated_runs_produce_identical_output(#[case] format: OutputFormat) {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let analyzer = CommunitiesAnalyzer::new();
        assert_eq!(
            analyzer.analyze(&lib, format).unwrap(),
            analyzer.analyze(&lib, format).unwrap(),
        );
    }

    #[test]
    fn markdown_carries_the_modularity_comparison_and_the_findings() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let md = CommunitiesAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Communities report"), "got {md}");
        assert!(md.contains("## Modularity"), "got {md}");
        assert!(md.contains("| detected |"), "got {md}");
        assert!(md.contains("| declared |"), "got {md}");
        assert!(md.contains("## Misfiled members"), "got {md}");
        assert!(md.contains("crate::a::stray"), "got {md}");
    }

    /// A declared partition can score below zero — its groups share
    /// fewer edges than chance would give them — and a ratio against the
    /// detected score would then print as "-1.02 of the detected one",
    /// which reads as a bug rather than as the finding it is.
    #[test]
    fn a_declared_partition_below_zero_is_described_rather_than_ratioed() {
        // Two declared modules whose every reference crosses between
        // them: the declared partition captures no edge at all, which is
        // worse than chance and therefore a negative Q.
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "pub mod x;\npub mod y;\n");
        write_file(dir.path(), "x/mod.rs", "pub mod a;\npub mod c;\n");
        write_file(dir.path(), "y/mod.rs", "pub mod b;\npub mod d;\n");
        write_file(dir.path(), "x/a.rs", "pub struct A;\n");
        write_file(dir.path(), "x/c.rs", "pub struct C;\n");
        write_file(
            dir.path(),
            "y/b.rs",
            "use crate::x::a::A;\npub fn b(_a: A) {}\n",
        );
        write_file(
            dir.path(),
            "y/d.rs",
            "use crate::x::c::C;\npub fn d(_c: C) {}\n",
        );

        let json = report(&lib, CommunitiesAnalyzer::new());
        let declared = json["modularity"]["declared"].as_f64().unwrap();
        assert!(declared < 0.0, "got {json:#}");

        let md = CommunitiesAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("scores below zero"), "got {md}");
        assert!(!md.contains("scores -"), "got {md}");
    }

    /// A cluster split evenly between two declared modules is owned by
    /// neither, which is the spanning listing's whole subject — so the
    /// section has to appear, with the breakdown that argues for it.
    #[test]
    fn markdown_reports_a_cluster_no_declared_module_owns() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "pub mod x;\npub mod y;\n");
        write_file(dir.path(), "x/mod.rs", "pub mod one;\n");
        write_file(dir.path(), "x/one.rs", "pub struct X;\npub fn x() {}\n");
        write_file(dir.path(), "y/mod.rs", "pub mod one;\n");
        write_file(
            dir.path(),
            "y/one.rs",
            "use crate::x::one::X;\npub fn y(_x: X) { crate::x::one::x(); }\n",
        );

        let json = report(&lib, CommunitiesAnalyzer::new());
        assert_eq!(
            json["spanning"].as_array().map(Vec::len),
            Some(1),
            "got {json:#}"
        );
        assert_eq!(
            json["spanning"][0]["declared_group_count"], 2,
            "got {json:#}"
        );

        let md = CommunitiesAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("## Spanning communities"), "got {md}");
        assert!(md.contains("declared module(s)"), "got {md}");
        assert!(md.contains("crate::x (1)"), "got {md}");
    }

    /// A tree with no cross-module reference has no boundary to
    /// disagree with, and the report has to say so rather than inventing
    /// clusters.
    #[test]
    fn an_unconnected_crate_reports_no_structure() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "pub mod a;\npub mod b;\n");
        write_file(dir.path(), "a.rs", "pub fn a() {}\n");
        write_file(dir.path(), "b.rs", "pub fn b() {}\n");

        let json = report(&lib, CommunitiesAnalyzer::new());
        assert_eq!(json["edge_count"], 0, "got {json:#}");
        assert_eq!(json["modularity"]["detected"], 0.0, "got {json:#}");
        assert!(
            json["modularity"].get("declared_quality").is_none(),
            "got {json:#}",
        );
        assert!(json["misfiled"].as_array().unwrap().is_empty());

        let md = CommunitiesAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("no resolved reference"), "got {md}");
    }

    /// Excluding a module must drop it from the population *and* from
    /// every edge, or a filtered run reports members that are not in it.
    #[test]
    fn excluded_modules_leave_the_graph_entirely() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let json = report(
            &lib,
            CommunitiesAnalyzer::new().with_exclude_patterns(vec!["b/**".to_owned()]),
        );
        let members: Vec<String> = json["communities"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["members"].as_array().unwrap())
            .filter_map(|m| m.as_str().map(str::to_owned))
            .collect();
        assert!(
            members.iter().all(|m| !m.starts_with("crate::b")),
            "got {json:#}",
        );
        assert!(
            json["misfiled"].as_array().unwrap().is_empty(),
            "got {json:#}"
        );
    }

    /// Every builder has to reach the analyzer. `with_options` is
    /// checked against the equivalent chain elsewhere, which cannot see
    /// a builder that quietly drops its argument — both sides would drop
    /// it — so each one is pinned to an observable effect here.
    #[test]
    fn with_top_bounds_the_markdown_listings() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let capped = CommunitiesAnalyzer::new()
            .with_top(Some(1))
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        let uncapped = CommunitiesAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(capped.contains("not shown"), "got {capped}");
        assert!(capped.contains("top 1"), "got {capped}");
        assert!(!uncapped.contains("not shown"), "got {uncapped}");
    }

    #[rstest]
    #[case::exclude_tests(true, false)]
    #[case::only_tests(false, true)]
    fn the_test_filters_reach_the_member_set(
        #[case] exclude_tests: bool,
        #[case] only_tests: bool,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "pub mod a;\npub mod tests;\n");
        write_file(dir.path(), "a.rs", "pub struct A;\n");
        write_file(
            dir.path(),
            "tests.rs",
            "use crate::a::A;\npub fn t(_a: A) {}\n",
        );

        let json = report(
            &lib,
            CommunitiesAnalyzer::new()
                .with_exclude_tests(exclude_tests)
                .with_only_tests(only_tests),
        );
        let unfiltered = report(&lib, CommunitiesAnalyzer::new());
        assert!(
            json["module_count"].as_u64() < unfiltered["module_count"].as_u64(),
            "filter did not reach the walk: {json:#} vs {unfiltered:#}",
        );
    }

    /// An excluded module has to leave the *edges* as well as the node
    /// list, and both endpoints have to be checked.
    ///
    /// At file granularity a half-excluded edge is harmless — its
    /// surviving endpoint names no member, so the detector drops it
    /// anyway. At module granularity it is not: an excluded file's
    /// module path still resolves to its containing module, so keeping
    /// the edge would credit the parent with weight from a file the
    /// caller asked to leave out.
    #[test]
    fn excluding_a_module_drops_the_edges_that_touch_it() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let module = || CommunitiesAnalyzer::new().with_granularity(Granularity::Module);
        let full = report(&lib, module());
        let filtered = report(
            &lib,
            module().with_exclude_patterns(vec!["b/p.rs".to_owned()]),
        );
        assert!(
            filtered["total_weight"].as_f64().unwrap() < full["total_weight"].as_f64().unwrap(),
            "weight from an excluded file leaked into its parent: {filtered:#} vs {full:#}",
        );
    }

    /// Rust puts `#[cfg(test)] mod tests` in the same file as the module
    /// it tests. Those are not separately filed anywhere, so they fold
    /// into the file — which is what keeps a module from looking like
    /// the container of its own test module.
    #[test]
    fn modules_sharing_a_file_fold_into_one_member() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "pub mod a;\npub mod b;\n");
        write_file(dir.path(), "a.rs", "pub struct A;\n");
        write_file(
            dir.path(),
            "b.rs",
            "use crate::a::A;\npub fn b(_a: A) {}\n\n#[cfg(test)]\nmod tests {\n    use crate::a::A;\n    #[test]\n    fn t() { let _ = A; }\n}\n",
        );

        let json = report(&lib, CommunitiesAnalyzer::new());
        assert!(
            json["module_count"].as_u64() > json["node_count"].as_u64(),
            "the inline test module should have folded: {json:#}",
        );
        let members: BTreeSet<&str> = json["communities"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["members"].as_array().unwrap())
            .filter_map(|m| m.as_str())
            .collect();
        assert!(members.contains("crate::b"), "got {json:#}");
        assert!(!members.contains("crate::b::tests"), "got {json:#}");
        // The fold is also why `crate::b` is filed in `crate` rather
        // than in itself.
        assert!(
            json["misfiled"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["declared"] != "crate::b"),
            "got {json:#}",
        );
    }

    /// Each community carries the declared groups it is made of, not
    /// just the winner: that breakdown is the evidence a reader weighs
    /// the dominant name against.
    #[test]
    fn a_community_carries_its_declared_breakdown() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let json = report(&lib, CommunitiesAnalyzer::new());
        let mixed = json["communities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["declared_group_count"].as_u64().unwrap() >= 2)
            .unwrap_or_else(|| panic!("no mixed community in {json:#}"));
        let breakdown = mixed["breakdown"].as_array().unwrap();
        assert!(!breakdown.is_empty(), "got {json:#}");
        assert_eq!(
            breakdown[0]["declared"], mixed["dominant_declared"],
            "the breakdown leads with the dominant group: {json:#}",
        );
        let counted: u64 = breakdown
            .iter()
            .map(|s| s["members"].as_u64().unwrap())
            .sum();
        assert_eq!(counted, mixed["size"].as_u64().unwrap(), "got {json:#}");
    }

    /// The markdown carries the whole report, and each line below is a
    /// fact a reader acts on: how many members folded, how many cluster
    /// with nothing, which members a community holds, and how many rows
    /// the cap hid.
    #[test]
    fn markdown_reports_the_fold_the_isolates_and_the_members() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(
            dir.path(),
            "lib.rs",
            "pub mod a;\npub mod b;\npub mod lonely;\n",
        );
        write_file(dir.path(), "a.rs", "pub struct A;\n");
        write_file(
            dir.path(),
            "b.rs",
            "use crate::a::A;\npub fn b(_a: A) {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
        );
        write_file(dir.path(), "lonely.rs", "pub fn lonely() {}\n");

        let md = CommunitiesAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("module(s) folded into"), "got {md}");
        assert!(md.contains("no resolved reference"), "got {md}");
        assert!(md.contains("## Communities"), "got {md}");
        assert!(md.contains("crate::a"), "got {md}");
    }

    /// Two sections only exist when there is something to put in them,
    /// and the overflow line only when the cap actually hid a row —
    /// "+0 more" reads as truncation that never happened.
    #[test]
    fn markdown_omits_what_the_report_does_not_have() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(
            dir.path(),
            "lib.rs",
            "pub mod a;\npub mod b;\nuse crate::a::A;\npub fn top(x: A) { crate::b::b(x); }\n",
        );
        write_file(dir.path(), "a.rs", "pub struct A;\npub fn a() {}\n");
        write_file(
            dir.path(),
            "b.rs",
            "use crate::a::A;\npub fn b(_a: A) { crate::a::a(); }\n",
        );

        let md = CommunitiesAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(!md.contains("not shown"), "nothing was capped: {md}");
        assert!(!md.contains("folded into"), "nothing folded: {md}");
        assert!(
            !md.contains("no resolved reference"),
            "every member is connected: {md}",
        );
        // One community holding the whole graph is the honest answer at
        // this size, and the report says so instead of splitting noise.
        assert!(
            md.contains("One community holds the whole graph"),
            "got {md}"
        );
        assert!(!md.contains("## Spanning communities"), "got {md}");
    }

    /// A community that outgrows the inline member list summarises the
    /// rest rather than printing a table cell of eighty module paths.
    #[test]
    fn a_large_community_summarises_the_members_it_does_not_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut decls = String::new();
        for i in 0..MEMBERS_PER_ROW + 3 {
            decls.push_str(&format!("pub mod m{i};\n"));
            write_file(
                dir.path(),
                &format!("m{i}.rs"),
                &format!("pub struct S{i};\npub fn f{i}() {{}}\n"),
            );
        }
        // Wire every module to `m0` so they form one community.
        let lib = write_file(dir.path(), "lib.rs", &decls);
        for i in 1..MEMBERS_PER_ROW + 3 {
            write_file(
                dir.path(),
                &format!("m{i}.rs"),
                &format!(
                    "use crate::m0::S0;\npub struct S{i};\npub fn f{i}(_s: S0) {{ crate::m0::f0(); }}\n"
                ),
            );
        }

        let md = CommunitiesAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(md.contains(" more |"), "members should be summarised: {md}");
        assert!(md.contains("crate::m0"), "got {md}");
    }

    /// A declared partition of exactly one group scores `Q = 0` by
    /// construction — every edge is internal and every degree is in it.
    /// Zero is not below zero, so it takes the ratio wording.
    #[test]
    fn a_declared_partition_scoring_zero_still_gets_a_ratio() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(
            dir.path(),
            "lib.rs",
            "pub mod a;\npub mod b;\npub mod c;\npub mod d;\n",
        );
        write_file(dir.path(), "a.rs", "pub struct A;\n");
        write_file(dir.path(), "b.rs", "use crate::a::A;\npub fn b(_a: A) {}\n");
        write_file(dir.path(), "c.rs", "pub struct C;\n");
        write_file(dir.path(), "d.rs", "use crate::c::C;\npub fn d(_c: C) {}\n");

        let json = report(&lib, CommunitiesAnalyzer::new());
        assert_eq!(json["declared_group_count"], 1, "got {json:#}");
        assert_eq!(json["modularity"]["declared"], 0.0, "got {json:#}");

        let md = CommunitiesAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("scores 0.00 of the detected one"), "got {md}");
        assert!(!md.contains("scores below zero"), "got {md}");
    }

    #[test]
    fn options_carry_the_documented_defaults() {
        let opts = CommunitiesOptions::default();
        assert_eq!(opts.min_community, DEFAULT_MIN_COMMUNITY);
        assert_eq!(opts.granularity, Granularity::File);
        assert_eq!(opts.top, None);
    }

    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
    }

    fn commit(dir: &Path, message: &str, files: &[(&str, &str)]) {
        for (path, contents) in files {
            write_file(dir, path, contents);
        }
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-q", "-m", message]);
    }

    /// Static edges pair `a::one`–`a::two` and `b::p`–`b::q`; history
    /// pairs them the other way round: `a::one` is always edited with
    /// `b::p`, and `a::two` with `b::q`.
    fn crossed_history(dir: &Path) -> PathBuf {
        init_repo(dir);
        commit(
            dir,
            "init",
            &[
                ("lib.rs", "pub mod a;\npub mod b;\n"),
                ("a/mod.rs", "pub mod one;\npub mod two;\n"),
                ("a/one.rs", "pub fn one() -> u8 { 0 }\n"),
                (
                    "a/two.rs",
                    "pub fn two() -> u8 { crate::a::one::one() + 0 }\n",
                ),
                ("b/mod.rs", "pub mod p;\npub mod q;\n"),
                ("b/p.rs", "pub fn p() -> u8 { 0 }\n"),
                ("b/q.rs", "pub fn q() -> u8 { crate::b::p::p() + 0 }\n"),
            ],
        );
        for i in 1..=4 {
            commit(
                dir,
                &format!("one+p {i}"),
                &[
                    ("a/one.rs", &format!("pub fn one() -> u8 {{ {i} }}\n")),
                    ("b/p.rs", &format!("pub fn p() -> u8 {{ {i} }}\n")),
                ],
            );
            commit(
                dir,
                &format!("two+q {i}"),
                &[
                    (
                        "a/two.rs",
                        &format!("pub fn two() -> u8 {{ crate::a::one::one() + {i} }}\n"),
                    ),
                    (
                        "b/q.rs",
                        &format!("pub fn q() -> u8 {{ crate::b::p::p() + {i} }}\n"),
                    ),
                ],
            );
        }
        dir.join("lib.rs")
    }

    fn community_of<'a>(json: &'a serde_json::Value, member: &str) -> &'a serde_json::Value {
        json["communities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["members"].as_array().unwrap().iter().any(|m| m == member))
            .unwrap_or_else(|| panic!("no community holds {member} in {json:#}"))
    }

    #[rstest]
    #[case::greedy(Algorithm::Greedy)]
    #[case::leiden(Algorithm::Leiden)]
    fn co_change_weights_cluster_what_is_edited_together(#[case] algorithm: Algorithm) {
        let dir = tempfile::tempdir().unwrap();
        let lib = crossed_history(dir.path());
        let json = report(
            &lib,
            CommunitiesAnalyzer::new()
                .with_weights(WeightSource::CoChange)
                .with_algorithm(algorithm),
        );
        let holding_one = community_of(&json, "crate::a::one");
        let members = holding_one["members"].as_array().unwrap();
        assert!(members.iter().any(|m| m == "crate::b::p"), "got {json:#}");
        assert!(members.iter().all(|m| m != "crate::a::two"), "got {json:#}");
        assert_eq!(json["weights"], "co-change");
        assert_eq!(json["history"]["co_change_edge_count"], 2, "got {json:#}");
        // Each crossed pair moved together in all five commits either
        // file appears in: Jaccard 5 / (5 + 5 - 5) = 1.
        assert_eq!(json["total_weight"], 2.0, "got {json:#}");
    }

    /// Pairs below `--min-support` contribute no edge. The initial commit
    /// touches every file once, so at 1 it pairs everything; the crossed
    /// pairs co-change five times, so at 6 nothing survives.
    #[test]
    fn co_change_pairs_below_min_support_contribute_no_edge() {
        let dir = tempfile::tempdir().unwrap();
        let lib = crossed_history(dir.path());
        let json = report(
            &lib,
            CommunitiesAnalyzer::new()
                .with_weights(WeightSource::CoChange)
                .with_min_support(1),
        );
        assert!(
            json["history"]["co_change_edge_count"].as_u64().unwrap() > 2,
            "got {json:#}",
        );
        let strict = report(
            &lib,
            CommunitiesAnalyzer::new()
                .with_weights(WeightSource::CoChange)
                .with_min_support(6),
        );
        assert_eq!(
            strict["history"]["co_change_edge_count"], 0,
            "got {strict:#}"
        );
    }

    /// Static and co-change disagree completely on this history, and the
    /// agreement section says so member by member.
    #[test]
    fn crossed_sources_are_reported_as_divergent() {
        let dir = tempfile::tempdir().unwrap();
        let lib = crossed_history(dir.path());
        let json = report(
            &lib,
            CommunitiesAnalyzer::new().with_weights(WeightSource::Both),
        );
        let agreement = &json["agreement"];
        assert_eq!(agreement["compared"], 4, "got {json:#}");
        assert!(agreement["nmi"].as_f64().unwrap() < 0.5, "got {json:#}");
        let divergent: Vec<&str> = agreement["divergent"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["member"].as_str().unwrap())
            .collect();
        assert!(divergent.contains(&"crate::a::one"), "got {json:#}");
        assert_eq!(json["history"]["co_change_share"], 0.5, "got {json:#}");
        // Both sources are normalised to one unit and blended.
        let total = json["total_weight"].as_f64().unwrap();
        assert!((total - 1.0).abs() < 1e-9, "got {json:#}");
    }

    #[test]
    fn static_weights_read_no_history() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let json = report(&lib, CommunitiesAnalyzer::new());
        assert!(json.get("history").is_none(), "got {json:#}");
        assert!(json.get("agreement").is_none(), "got {json:#}");
        assert_eq!(json["weights"], "static");
        assert_eq!(json["algorithm"], "greedy");
    }

    #[test]
    fn co_change_weights_outside_a_repository_are_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let err = CommunitiesAnalyzer::new()
            .with_weights(WeightSource::CoChange)
            .analyze(&lib, OutputFormat::Json)
            .unwrap_err();
        assert!(
            matches!(err, CommunitiesError::NotInGitRepo { .. }),
            "got {err:?}"
        );
    }

    #[rstest]
    #[case(0.0)]
    #[case(-1.0)]
    #[case(f64::NAN)]
    #[case(f64::INFINITY)]
    fn a_resolution_outside_the_positive_reals_is_rejected(#[case] resolution: f64) {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let err = CommunitiesAnalyzer::new()
            .with_resolution(resolution)
            .analyze(&lib, OutputFormat::Json)
            .unwrap_err();
        assert!(
            matches!(err, CommunitiesError::ResolutionOutOfRange { .. }),
            "got {err:?}"
        );
    }

    #[rstest]
    #[case(-0.1)]
    #[case(1.5)]
    fn a_co_change_share_outside_the_unit_interval_is_rejected(#[case] share: f64) {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let err = CommunitiesAnalyzer::new()
            .with_co_change_share(share)
            .analyze(&lib, OutputFormat::Json)
            .unwrap_err();
        assert!(
            matches!(err, CommunitiesError::CoChangeShareOutOfRange { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn leiden_reports_the_planted_misfile_too() {
        let dir = tempfile::tempdir().unwrap();
        let lib = planted_crate(dir.path());
        let json = report(
            &lib,
            CommunitiesAnalyzer::new().with_algorithm(Algorithm::Leiden),
        );
        assert_eq!(json["algorithm"], "leiden");
        let rows = json["misfiled"].as_array().unwrap();
        assert!(
            rows.iter().any(|r| r["member"] == "crate::a::stray"),
            "got {json:#}",
        );
    }

    #[test]
    fn markdown_carries_the_run_settings_and_the_agreement() {
        let dir = tempfile::tempdir().unwrap();
        let lib = crossed_history(dir.path());
        let md = CommunitiesAnalyzer::new()
            .with_weights(WeightSource::Both)
            .with_algorithm(Algorithm::Leiden)
            .with_resolution(1.5)
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("algorithm: leiden, resolution: 1.500, weights: both"),
            "got {md}"
        );
        assert!(md.contains("history: 9 commit(s)"), "got {md}");
        assert!(md.contains("co-change share 0.50"), "got {md}");
        assert!(md.contains("## Static vs co-change partitions"), "got {md}");
        assert!(md.contains("| crate::a::one |"), "got {md}");
    }

    fn edge(a: &str, b: &str, w: f64) -> CommunityEdge {
        CommunityEdge::new(a, b, w)
    }

    fn total(edges: &[CommunityEdge]) -> f64 {
        edges.iter().map(|e| e.weight).sum()
    }

    /// Each source is scaled to its share of one unit of weight, so the
    /// blend does not depend on how big either source's raw numbers are.
    #[test]
    fn blend_gives_each_source_its_share_of_one_unit() {
        let statics = vec![edge("a", "b", 3.0), edge("b", "c", 1.0)];
        let history = vec![edge("a", "c", 0.2)];
        let out = blend(&statics, &history, 0.25);
        assert!((total(&out) - 1.0).abs() < 1e-12, "got {out:?}");
        let ac = out.iter().find(|e| e.a == "a" && e.b == "c").unwrap();
        assert!((ac.weight - 0.25).abs() < 1e-12, "got {out:?}");
        let ab = out.iter().find(|e| e.a == "a" && e.b == "b").unwrap();
        assert!((ab.weight - 0.75 * 0.75).abs() < 1e-12, "got {out:?}");
    }

    /// An empty source leaves the other at full strength rather than
    /// scaling it down to its share.
    #[rstest]
    #[case::no_history(vec![edge("a", "b", 2.0)], vec![])]
    #[case::no_static(vec![], vec![edge("a", "b", 0.4)])]
    fn blend_with_one_empty_source_keeps_the_other_whole(
        #[case] statics: Vec<CommunityEdge>,
        #[case] history: Vec<CommunityEdge>,
    ) {
        let out = blend(&statics, &history, 0.5);
        assert_eq!(out.len(), 1);
        assert!((total(&out) - 1.0).abs() < 1e-12, "got {out:?}");
    }

    #[test]
    fn blend_of_nothing_is_nothing() {
        assert!(blend(&[], &[], 0.5).is_empty());
    }

    #[rstest]
    #[case(3.0, "3")]
    #[case(0.0, "0")]
    #[case(0.25, "0.250")]
    #[case(1.0 / 3.0, "0.333")]
    fn weights_render_as_counts_when_whole(#[case] value: f64, #[case] expected: &str) {
        assert_eq!(weight(value), expected);
    }

    #[rstest]
    #[case(Algorithm::Greedy, CommunityAlgorithm::Greedy)]
    #[case(Algorithm::Leiden, CommunityAlgorithm::Leiden)]
    fn the_algorithm_flag_selects_the_domain_partitioner(
        #[case] flag: Algorithm,
        #[case] expected: CommunityAlgorithm,
    ) {
        assert_eq!(flag.domain(), expected);
    }

    #[test]
    fn the_since_window_reaches_the_history_options() {
        let analyzer = CommunitiesAnalyzer::new().with_since_opt(Some("90.days.ago".to_owned()));
        assert_eq!(analyzer.history.since.as_deref(), Some("90.days.ago"));
    }

    /// A source with no share contributes only zero-weight edges, which
    /// the detector ignores.
    #[test]
    fn blend_at_full_share_leaves_the_other_source_weightless() {
        let statics = vec![edge("a", "b", 3.0)];
        let history = vec![edge("a", "c", 0.2)];
        let out = blend(&statics, &history, 1.0);
        let ab = out.iter().find(|e| e.b == "b").map(|e| e.weight);
        assert_eq!(ab, Some(0.0), "got {out:?}");
        assert!((total(&out) - 1.0).abs() < 1e-12, "got {out:?}");
    }
}
