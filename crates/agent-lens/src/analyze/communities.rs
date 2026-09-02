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
//!   concrete move candidate: "`analyze/churn.rs` is filed under
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
    Community, CommunityEdge, CommunityNode, CommunityReport, DEFAULT_MIN_COMMUNITY, DeclaredShare,
    MisfiledMember, ModulePath, SpanningCommunity, detect_communities,
};
use serde::Serialize;

use super::module_graph::{GraphPolicy, ModuleGraph, build_graph};
use super::module_label::ModuleLabeler;
use super::{AnalyzePathFilter, CrateAnalyzerError, OutputFormat};

/// What the report does and does not license. Carried in the output
/// because "these files cluster together" is one sentence away from
/// "move them", and the distance between the two is where an agent would
/// otherwise over-read the result.
const NOTE: &str = "Detected communities are what the dependency edges cluster into; the declared \
     partition is the module each member is filed under. `modularity_gap` is the distance between \
     the two — near zero means the declared boundaries already are the ones the dependencies \
     form. A misfiled row is a move candidate, not a defect: it says a member has more edge \
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
}

impl Default for CommunitiesOptions {
    fn default() -> Self {
        Self {
            top: None,
            granularity: Granularity::default(),
            min_community: DEFAULT_MIN_COMMUNITY,
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
}

impl Default for CommunitiesAnalyzer {
    fn default() -> Self {
        Self {
            path_filter: AnalyzePathFilter::default(),
            top: None,
            granularity: Granularity::default(),
            min_community: DEFAULT_MIN_COMMUNITY,
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

    /// Resolve `path`, build the language-specific module graph, detect
    /// its communities, and score them against the declared grouping.
    pub fn analyze(
        &self,
        path: impl AsRef<Path>,
        format: OutputFormat,
    ) -> Result<String, CrateAnalyzerError> {
        let mut graph = build_graph(path.as_ref(), GraphPolicy::COUPLING, &self.path_filter)?;
        let filter = self.path_filter.compile(&graph.root)?;
        graph.modules.retain(|m| filter.includes_path(&m.file));
        let kept: BTreeSet<&ModulePath> = graph.modules.iter().map(|m| &m.path).collect();
        graph
            .edges
            .retain(|e| kept.contains(&e.from) && kept.contains(&e.to));

        let (nodes, edges) = graph_input(&graph, self.granularity);
        let report = detect_communities(&nodes, &edges, self.min_community);
        let view = ReportView::new(&graph, self.granularity, self.min_community, &report);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&view).map_err(CrateAnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&view, self.top)),
        }
    }
}

/// Turn the module graph into the node and edge lists the detector
/// takes, at the requested granularity.
///
/// Edge weight is the number of *distinct* cross-module references, so
/// ten `use` lines naming the same symbol do not outweigh ten naming
/// different ones. The dedup key is the whole edge, which is the same
/// unit `coupling` counts as its Number of Couplings.
fn graph_input(
    graph: &ModuleGraph,
    granularity: Granularity,
) -> (Vec<CommunityNode>, Vec<CommunityEdge>) {
    let member = Membership::new(graph, granularity);
    let nodes: BTreeMap<String, String> = graph
        .modules
        .iter()
        .map(|m| {
            let id = member.of(&m.path);
            let declared = member.declared(&id);
            (id.to_string(), declared.to_string())
        })
        .collect();
    let edges = graph
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
        .collect();
    (
        nodes
            .into_iter()
            .map(|(id, declared)| CommunityNode::new(id, declared))
            .collect(),
        edges,
    )
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
/// its parent.** `crate::analyze` holds `crate::analyze::churn`, so it is
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
    note: &'static str,
    /// Module-graph nodes read before members were folded out of them.
    /// Above `node_count` whenever a language puts several modules in
    /// one file, which for Rust is every `#[cfg(test)] mod tests`.
    module_count: usize,
    node_count: usize,
    edge_count: usize,
    total_weight: u64,
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
}

impl ReportView {
    fn new(
        graph: &ModuleGraph,
        granularity: Granularity,
        min_community: usize,
        report: &CommunityReport,
    ) -> Self {
        let labeler = &graph.labeler;
        Self {
            crate_root: graph.root.display().to_string(),
            granularity: granularity.as_str(),
            note: NOTE,
            module_count: graph.modules.len(),
            node_count: report.node_count,
            edge_count: report.edge_count,
            total_weight: report.total_weight,
            isolated_node_count: report.isolated_node_count,
            community_count: report.community_count,
            declared_group_count: report.declared_group_count,
            largest_community: report.largest_community,
            min_community,
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
        }
    }
}

#[derive(Debug, Serialize)]
struct ModularityView {
    /// Newman `Q` for the partition the edges support.
    detected: f64,
    /// Newman `Q` for the declared partition, computed over the same
    /// graph so the two are comparable.
    declared: f64,
    /// `detected - declared`. Zero means the declared boundaries already
    /// are the detected ones.
    gap: f64,
    /// `declared / detected`, omitted when the graph has no community
    /// structure to compare against. Above `1.0` when the declared
    /// grouping beats the one the search found, which greedy
    /// agglomeration allows — it finds a good partition, not the
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
    fn new(share: &DeclaredShare, labeler: &ModuleLabeler) -> Self {
        Self {
            declared: label(&share.declared, labeler),
            members: share.members,
        }
    }

    fn list(shares: &[DeclaredShare], labeler: &ModuleLabeler) -> Vec<Self> {
        shares.iter().map(|s| Self::new(s, labeler)).collect()
    }
}

#[derive(Debug, Serialize)]
struct CommunityView {
    id: usize,
    size: usize,
    dominant_declared: String,
    declared_group_count: usize,
    internal_weight: u64,
    external_weight: u64,
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
    weight_to_declared: u64,
    weight_to_suggested: u64,
    evidence: u64,
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
    let _ = writeln!(&mut out, "\n{}", view.note);
    if view.node_count == 0 {
        out.push_str("\n_No modules discovered._\n");
        return out;
    }
    render_modularity(&mut out, view);
    render_misfiled(&mut out, &view.misfiled, limit);
    render_communities(&mut out, view, limit);
    render_spanning(&mut out, &view.spanning, limit);
    out
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
            row.weight_to_suggested,
            row.weight_to_declared,
            row.evidence,
        );
    }
    render_overflow(out, rows.len(), limit, "misfiled member");
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
            c.internal_weight,
            c.external_weight,
            members_cell(&c.members),
        );
    }
    render_overflow(out, view.communities.len(), limit, "community");
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
    render_overflow(out, rows.len(), limit, "spanning community");
}

fn render_overflow(out: &mut String, total: usize, limit: usize, unit: &str) {
    let omitted = total.saturating_sub(limit);
    if omitted > 0 {
        let _ = writeln!(
            out,
            "\n_{omitted} more {unit}(s) not shown; raise --top or use --format json._",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
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
            stray["weight_to_suggested"].as_u64().unwrap()
                > stray["weight_to_declared"].as_u64().unwrap(),
            "got {json:#}",
        );
        assert!(stray["evidence"].as_u64().unwrap() >= 1, "got {json:#}");
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
        let graph = build_graph(&lib, GraphPolicy::COUPLING, &AnalyzePathFilter::new()).unwrap();
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
            filtered["total_weight"].as_u64().unwrap() < full["total_weight"].as_u64().unwrap(),
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
}
