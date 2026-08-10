//! `analyze impact` — blast radius of a diff via reverse reachability
//! on the function call graph.
//!
//! Targets the core agent failure mode: a locally-correct edit that
//! breaks distant, unread callers. Seeds default to the functions whose
//! spans intersect the unstaged working-tree diff (`git diff -U0`);
//! `--function <symbol>` seeds a pre-edit query instead. From each seed
//! the analyzer walks callers backwards over **resolved edges only**,
//! on the SCC condensation of the graph (so a call cycle counts as one
//! hop and cannot inflate depths), up to `--depth` hops (default 5).
//!
//! Output shape per changed function: depth-1 callers verbatim, deeper
//! callers folded to per-depth per-module counts, reachable test
//! functions listed as a verification checklist, and the transitive
//! caller count (VFI) with modules spanned. Counts are bounds, and the
//! report says so: heuristic over-resolution can inflate them (upper
//! bound), while ambiguous call sites naming impact members as
//! candidates and call sites whose caller could not be attributed are
//! excluded entirely (lower bound) — both exclusion counts are
//! reported per seed.
//!
//! Symbols follow the same matching rules as `analyze graph-query`
//! (`::`-segment suffix on the qualified name, or an exact node id);
//! ambiguous matches are listed, never guessed.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;

use serde::Serialize;

use super::call_graph::algo::{bfs, condense, reverse_adjacency};
use super::call_graph::model::{CallGraphNode, Resolution};
use super::call_graph::{CallGraph, CallGraphBuilder, delegate_call_graph_builders, match_symbol};
use super::options::analyzer_options;
use super::runner::render_report;
use super::{AnalyzeRoots, AnalyzerError, DiffScope, OutputFormat, overlaps_any};

const SCHEMA_VERSION: u32 = 1;

/// Default reverse-traversal depth cap, in condensation hops. Bounding
/// the walk is a load-bearing noise mitigation: one over-resolved edge
/// into a popular name would otherwise fabricate impact across the
/// whole repo.
pub const DEFAULT_IMPACT_DEPTH: usize = 5;

/// Markdown list cap when `--top` is not given. JSON always carries
/// every row.
const DEFAULT_TOP: usize = 20;

/// `impact_explosion` fires when the depth-2 caller count reaches this
/// floor and at least [`EXPLOSION_RATIO`] times the depth-1 count.
const EXPLOSION_FLOOR: usize = 10;
const EXPLOSION_RATIO: usize = 3;

analyzer_options! {
    /// `analyze impact` flags, and the `[profile.<name>.impact]` table.
    pub struct ImpactOptions {
        @shared(ranking);
        /// Seed the query from this function instead of the working-tree
        /// diff: a `::`-segment suffix of its qualified name (e.g. `foo`,
        /// `module::foo`, `Owner::method`) or an exact node id
        /// (`file:name:line`, as listed on ambiguity). Repeatable.
        #[arg(long = "function", value_name = "SYMBOL")]
        pub function: Vec<String>,
        /// Reverse-traversal depth cap in call hops (cycles count as one).
        /// Callers beyond the cap are counted, not listed.
        #[arg(long)]
        pub depth: Option<usize>,
        /// Seed from the given git revision range instead of the
        /// working-tree diff, as `git diff -U0 <range>` (`HEAD~1..HEAD`,
        /// `main...topic`) — the blast radius of a commit that already
        /// landed. Ignored when `--function` is given.
        #[arg(
            long,
            value_name = "RANGE",
            value_parser = crate::analyze::parse_diff_range,
        )]
        pub diff_range: Option<String>,
    }
}

/// Analyzer entry point for `analyze impact`.
#[derive(Debug, Clone)]
pub struct ImpactAnalyzer {
    builder: CallGraphBuilder,
    functions: Vec<String>,
    depth: Option<usize>,
    top: Option<usize>,
    diff: DiffScope,
}

/// Unlike the analyzers where a diff gate is opt-in, impact *starts*
/// from a diff — an unseeded run means "what does my pending work
/// reach?". So the default scope is the working tree, not
/// [`DiffScope::Disabled`], which would seed nothing at all.
impl Default for ImpactAnalyzer {
    fn default() -> Self {
        Self {
            builder: CallGraphBuilder::default(),
            functions: Vec::new(),
            depth: None,
            top: None,
            diff: DiffScope::WorkingTree,
        }
    }
}

impl ImpactAnalyzer {
    /// Apply a whole [`ImpactOptions`] group. The CLI flags and the
    /// `[profile.<name>.impact]` table are the same type, so this is the
    /// only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: ImpactOptions) -> Self {
        self.with_functions(opts.function)
            .with_depth(opts.depth)
            .with_top(opts.top)
            .with_diff_scope(DiffScope::new(true, opts.diff_range))
    }

    /// Which diff seeds the query when no `--function` is given.
    /// Defaults to the working tree; a range seeds from a commit that
    /// already landed.
    pub fn with_diff_scope(mut self, diff: DiffScope) -> Self {
        self.diff = diff;
        self
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the query from these symbols instead of the working-tree
    /// diff. Each must match exactly one function; ambiguous matches
    /// are listed, never guessed.
    pub fn with_functions(mut self, functions: Vec<String>) -> Self {
        self.functions = functions;
        self
    }

    /// Reverse-traversal depth cap in condensation hops (default
    /// [`DEFAULT_IMPACT_DEPTH`]). Callers reachable beyond the cap are
    /// counted in `beyond_depth_count`, not silently dropped.
    pub fn with_depth(mut self, depth: Option<usize>) -> Self {
        self.depth = depth;
        self
    }

    /// Cap the markdown caller and test lists to the top-N rows. JSON
    /// output always carries every row.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    delegate_call_graph_builders! {
        builder,
        only_tests,
        exclude_tests,
    }

    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, AnalyzerError> {
        let roots = roots.into();
        let graph = self.builder.build(&roots)?;
        let seeds = self.resolve_seeds(&roots, &graph)?;
        let report = Report::build(&roots, &graph, self, seeds);
        render_report(&report, format, || format_markdown(&report, self.top))
    }

    /// Seed node indices: explicit `--function` symbols when given,
    /// otherwise functions overlapping the unstaged diff.
    fn resolve_seeds(
        &self,
        roots: &AnalyzeRoots,
        graph: &CallGraph,
    ) -> Result<Seeds, AnalyzerError> {
        if !self.functions.is_empty() {
            let mut seeds = Vec::new();
            for symbol in &self.functions {
                match match_symbol(graph, symbol).as_slice() {
                    [] => {
                        return Ok(Seeds::SymbolFailure {
                            symbol: symbol.clone(),
                            status: ImpactStatus::SymbolNotFound,
                            candidates: Vec::new(),
                        });
                    }
                    [unique] => seeds.push(*unique),
                    matches => {
                        return Ok(Seeds::SymbolFailure {
                            symbol: symbol.clone(),
                            status: ImpactStatus::SymbolAmbiguous,
                            candidates: matches.to_vec(),
                        });
                    }
                }
            }
            seeds.sort_unstable();
            seeds.dedup();
            return Ok(Seeds::Resolved(seeds));
        }
        let changed = self
            .builder
            .changed_line_ranges_by_display_path(roots, &self.diff)?;
        let seeds: Vec<usize> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| {
                changed
                    .get(&node.file)
                    .is_some_and(|ranges| overlaps_any(node.start_line, node.end_line, ranges))
            })
            .map(|(idx, _)| idx)
            .collect();
        Ok(Seeds::Resolved(seeds))
    }

    fn seed_source(&self) -> SeedSource {
        if self.functions.is_empty() {
            SeedSource::Diff
        } else {
            SeedSource::Functions
        }
    }
}

/// Outcome of seed resolution.
enum Seeds {
    Resolved(Vec<usize>),
    /// A `--function` symbol failed to match uniquely; the report
    /// carries the failure instead of guessing.
    SymbolFailure {
        symbol: String,
        status: ImpactStatus,
        candidates: Vec<usize>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SeedSource {
    Diff,
    Functions,
}

/// How the analysis terminated. Anything but `ok` carries no impact
/// results; `symbol_ambiguous` lists the candidates so the caller can
/// re-run with a longer suffix or an exact node id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ImpactStatus {
    Ok,
    /// Diff mode found no function overlapping an unstaged change.
    NoChanges,
    SymbolNotFound,
    SymbolAmbiguous,
}

/// One function, as cited in seeds, caller lists, and checklists.
#[derive(Debug, Clone, Serialize)]
struct FunctionRef {
    id: String,
    qualified_name: String,
    file: String,
    line: usize,
    end_line: usize,
    module: String,
    is_test: bool,
}

impl FunctionRef {
    fn from_node(node: &CallGraphNode) -> Self {
        Self {
            id: node.id.clone(),
            qualified_name: node.qualified_name.clone(),
            file: node.file.clone(),
            line: node.start_line,
            end_line: node.end_line,
            module: node.module.clone(),
            is_test: node.is_test,
        }
    }
}

/// Callers at one condensation depth, folded to module counts.
#[derive(Debug, Serialize)]
struct DepthBucket {
    depth: usize,
    count: usize,
    modules: BTreeMap<String, usize>,
}

#[derive(Debug, Serialize)]
struct TransitiveView {
    /// Per-depth module fold over the capped impact set. The depth-1
    /// count can exceed `direct_callers.len()` when a call cycle sits
    /// one hop up: cycles are folded to one condensation unit, so all
    /// members land at the same depth.
    by_depth: Vec<DepthBucket>,
    /// Distinct impacted functions within the depth cap (== `vfi`).
    total: usize,
    /// Distinct modules among the capped impact set.
    modules_spanned: usize,
}

/// Blast radius of one changed function.
#[derive(Debug, Serialize)]
struct ChangedFunction {
    function: FunctionRef,
    /// Members of the seed's own call cycle (depth 0): mutually
    /// recursive with the changed function, impacted at every depth.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    cycle_members: Vec<FunctionRef>,
    /// Functions with a resolved call edge into the changed function.
    direct_callers: Vec<FunctionRef>,
    transitive: TransitiveView,
    /// Test functions inside the capped impact set — the verification
    /// checklist for this edit.
    reachable_tests: Vec<FunctionRef>,
    /// Visible fan-in: distinct transitive callers within the depth
    /// cap. Same value as `transitive.total`, named per the plan.
    vfi: usize,
    /// Callers reachable only beyond the depth cap (not in any list or
    /// count above). Raise `--depth` to include them.
    beyond_depth_count: usize,
    /// Advisory shotgun-surgery signal: depth-2 caller count is at
    /// least 10 and at least 3x the depth-1 count.
    impact_explosion: bool,
    /// Ambiguous call edges naming the seed or an impact member among
    /// their candidates. Each is a potential caller excluded from every
    /// count above — the lower-bound direction.
    excluded_ambiguous_edge_count: usize,
    /// Resolved call edges into the impact set whose caller could not
    /// be attributed to a function. Also excluded — lower bound.
    unattributed_caller_edge_count: usize,
}

#[derive(Debug, Serialize)]
struct Summary {
    changed_count: usize,
    /// Distinct functions impacted by any seed, within the depth cap.
    union_impacted_count: usize,
    union_reachable_test_count: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    depth_limit: usize,
    seed_source: SeedSource,
    status: ImpactStatus,
    /// The `--function` symbol that failed to resolve uniquely
    /// (`status` says how).
    #[serde(skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    /// Candidate matches for an ambiguous symbol.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    candidates: Vec<FunctionRef>,
    changed: Vec<ChangedFunction>,
    summary: Summary,
    /// Direction of every count in this report: resolved edges only.
    note: &'static str,
}

const BOUNDS_NOTE: &str = "Counts follow resolved call edges only: heuristic over-resolution can \
     inflate them (upper bound), while ambiguous and caller-unattributed call sites are excluded \
     entirely (lower bound; see the per-function excluded counts).";

impl Report {
    fn build(roots: &AnalyzeRoots, graph: &CallGraph, spec: &ImpactAnalyzer, seeds: Seeds) -> Self {
        let mut report = Self {
            schema_version: SCHEMA_VERSION,
            root: roots.display(),
            language: graph.language,
            depth_limit: spec.depth.unwrap_or(DEFAULT_IMPACT_DEPTH),
            seed_source: spec.seed_source(),
            status: ImpactStatus::Ok,
            symbol: None,
            candidates: Vec::new(),
            changed: Vec::new(),
            summary: Summary {
                changed_count: 0,
                union_impacted_count: 0,
                union_reachable_test_count: 0,
            },
            note: BOUNDS_NOTE,
        };

        let seeds = match seeds {
            Seeds::Resolved(seeds) => seeds,
            Seeds::SymbolFailure {
                symbol,
                status,
                candidates,
            } => {
                report.status = status;
                report.symbol = Some(symbol);
                report.candidates = candidates
                    .iter()
                    .map(|&idx| FunctionRef::from_node(&graph.nodes[idx]))
                    .collect();
                return report;
            }
        };
        if seeds.is_empty() {
            report.status = ImpactStatus::NoChanges;
            return report;
        }

        let adjacency = graph.resolved_adjacency();
        let reversed = reverse_adjacency(&adjacency);
        let condensation = condense(&adjacency);
        let reversed_condensation = reverse_adjacency(&condensation.edges);

        let mut union_impacted: BTreeSet<usize> = BTreeSet::new();
        let mut union_tests: BTreeSet<usize> = BTreeSet::new();
        for &seed in &seeds {
            let changed = Self::changed_function(
                graph,
                &reversed,
                &condensation.component_of,
                &condensation.components,
                &reversed_condensation,
                seed,
                report.depth_limit,
            );
            report.changed.push(changed.view);
            union_impacted.extend(changed.impacted);
            union_tests.extend(changed.tests);
        }
        report.summary = Summary {
            changed_count: seeds.len(),
            union_impacted_count: union_impacted.len(),
            union_reachable_test_count: union_tests.len(),
        };
        report
    }

    fn changed_function(
        graph: &CallGraph,
        reversed: &[Vec<usize>],
        component_of: &[usize],
        components: &[Vec<usize>],
        reversed_condensation: &[Vec<usize>],
        seed: usize,
        depth_limit: usize,
    ) -> ChangedResult {
        // Reverse BFS over the condensation: every visit is one SCC, so
        // a call cycle counts as a single hop and depths cannot loop.
        let visits = bfs(reversed_condensation, &[component_of[seed]]);
        let mut impacted: Vec<(usize, usize)> = Vec::new();
        let mut beyond_depth_count = 0usize;
        for visit in &visits {
            for &node in &components[visit.node] {
                if node == seed {
                    continue;
                }
                if visit.depth <= depth_limit {
                    impacted.push((visit.depth, node));
                } else {
                    beyond_depth_count += 1;
                }
            }
        }
        impacted.sort_unstable();

        let mut by_depth: BTreeMap<usize, DepthBucket> = BTreeMap::new();
        for &(depth, node) in impacted.iter().filter(|&&(depth, _)| depth >= 1) {
            let bucket = by_depth.entry(depth).or_insert_with(|| DepthBucket {
                depth,
                count: 0,
                modules: BTreeMap::new(),
            });
            bucket.count += 1;
            *bucket
                .modules
                .entry(graph.nodes[node].module.clone())
                .or_default() += 1;
        }
        let depth_count = |depth: usize| by_depth.get(&depth).map_or(0, |bucket| bucket.count);
        let impact_explosion =
            depth_count(2) >= EXPLOSION_FLOOR && depth_count(2) >= EXPLOSION_RATIO * depth_count(1);

        let modules_spanned = impacted
            .iter()
            .map(|&(_, node)| graph.nodes[node].module.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let reachable_tests: Vec<FunctionRef> = impacted
            .iter()
            .filter(|&&(_, node)| graph.nodes[node].is_test)
            .map(|&(_, node)| FunctionRef::from_node(&graph.nodes[node]))
            .collect();

        // Everything a would-be caller could target: the seed plus its
        // capped impact set. Ambiguous edges naming any of these as a
        // candidate are excluded potential callers.
        let closed_ids: HashSet<&str> = impacted
            .iter()
            .map(|&(_, node)| graph.nodes[node].id.as_str())
            .chain(std::iter::once(graph.nodes[seed].id.as_str()))
            .collect();
        let mut excluded_ambiguous_edge_count = 0usize;
        let mut unattributed_caller_edge_count = 0usize;
        for edge in &graph.edges {
            match edge.resolution {
                Resolution::Ambiguous => {
                    if edge
                        .candidates
                        .iter()
                        .any(|candidate| closed_ids.contains(candidate.as_str()))
                    {
                        excluded_ambiguous_edge_count += 1;
                    }
                }
                Resolution::Resolved => {
                    if edge.from.is_none()
                        && edge.to.as_deref().is_some_and(|to| closed_ids.contains(to))
                    {
                        unattributed_caller_edge_count += 1;
                    }
                }
                Resolution::Unresolved | Resolution::Anonymous => {}
            }
        }

        let view = ChangedFunction {
            function: FunctionRef::from_node(&graph.nodes[seed]),
            cycle_members: components[component_of[seed]]
                .iter()
                .filter(|&&node| node != seed)
                .map(|&node| FunctionRef::from_node(&graph.nodes[node]))
                .collect(),
            direct_callers: reversed[seed]
                .iter()
                .filter(|&&caller| caller != seed)
                .map(|&caller| FunctionRef::from_node(&graph.nodes[caller]))
                .collect(),
            transitive: TransitiveView {
                by_depth: by_depth.into_values().collect(),
                total: impacted.len(),
                modules_spanned,
            },
            reachable_tests,
            vfi: impacted.len(),
            beyond_depth_count,
            impact_explosion,
            excluded_ambiguous_edge_count,
            unattributed_caller_edge_count,
        };
        ChangedResult {
            impacted: impacted.iter().map(|&(_, node)| node).collect(),
            tests: impacted
                .iter()
                .filter(|&&(_, node)| graph.nodes[node].is_test)
                .map(|&(_, node)| node)
                .collect(),
            view,
        }
    }
}

/// One seed's view plus the raw index sets feeding the union summary.
struct ChangedResult {
    view: ChangedFunction,
    impacted: Vec<usize>,
    tests: Vec<usize>,
}

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    let mut out = format!(
        "# Impact: {} ({} changed function(s), union blast radius {} within depth {})\n",
        report.root,
        report.summary.changed_count,
        report.summary.union_impacted_count,
        report.depth_limit,
    );
    match report.status {
        ImpactStatus::Ok => {}
        ImpactStatus::NoChanges => {
            out.push_str(
                "\n_No unstaged change overlaps a function. Pass `--function <symbol>` \
                 for a pre-edit query._\n",
            );
            return out;
        }
        ImpactStatus::SymbolNotFound => {
            let symbol = report.symbol.as_deref().unwrap_or_default();
            let _ = writeln!(
                out,
                "\n_No function matches `{symbol}` (matching is by `::`-segment suffix \
                 on the qualified name, or an exact node id)._",
            );
            return out;
        }
        ImpactStatus::SymbolAmbiguous => {
            let symbol = report.symbol.as_deref().unwrap_or_default();
            let _ = writeln!(
                out,
                "\n`{symbol}` matches {} function(s). Not guessing — re-run with a longer \
                 qualified-name suffix or an exact node id:\n",
                report.candidates.len(),
            );
            for row in &report.candidates {
                let _ = writeln!(
                    out,
                    "- `{}` ({}:{}) id `{}`",
                    row.qualified_name, row.file, row.line, row.id,
                );
            }
            return out;
        }
    }

    let _ = writeln!(out, "\n{}", report.note);
    for changed in &report.changed {
        render_changed(&mut out, changed, limit);
    }
    out
}

fn render_changed(out: &mut String, changed: &ChangedFunction, limit: usize) {
    let f = &changed.function;
    let _ = writeln!(
        out,
        "\n## `{}` ({}:{}-{}, module `{}`)",
        f.qualified_name, f.file, f.line, f.end_line, f.module,
    );
    let _ = writeln!(
        out,
        "- blast radius: {} function(s) across {} module(s)",
        changed.vfi, changed.transitive.modules_spanned,
    );
    if !changed.cycle_members.is_empty() {
        let members = changed
            .cycle_members
            .iter()
            .map(|m| format!("`{}`", m.id))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "- same call cycle ({}): {members} — mutually recursive, impacted at every depth",
            changed.cycle_members.len(),
        );
    }
    let _ = writeln!(out, "- direct callers ({}):", changed.direct_callers.len());
    render_rows(out, &changed.direct_callers, limit);
    for bucket in &changed.transitive.by_depth {
        if bucket.depth < 2 {
            continue;
        }
        // Dominant module first so the fold reads as "where the blast
        // lands", not an alphabetical inventory.
        let mut modules: Vec<(&str, usize)> = bucket
            .modules
            .iter()
            .map(|(module, &count)| (module.as_str(), count))
            .collect();
        modules.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let fold = modules
            .iter()
            .map(|(module, count)| format!("{module} {count}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "- depth {}: {} function(s) — {fold}",
            bucket.depth, bucket.count,
        );
    }
    if changed.beyond_depth_count > 0 {
        let _ = writeln!(
            out,
            "- beyond the depth cap: {} function(s) (raise --depth to include them)",
            changed.beyond_depth_count,
        );
    }
    if changed.reachable_tests.is_empty() {
        out.push_str("- no test reaches this function over resolved edges\n");
    } else {
        let _ = writeln!(
            out,
            "- verification checklist — {} test(s) reach this function:",
            changed.reachable_tests.len(),
        );
        render_rows(out, &changed.reachable_tests, limit);
    }
    if changed.excluded_ambiguous_edge_count > 0 || changed.unattributed_caller_edge_count > 0 {
        let _ = writeln!(
            out,
            "- excluded from all counts: {} ambiguous edge(s) naming impact members as \
             candidates, {} caller-unattributed edge(s)",
            changed.excluded_ambiguous_edge_count, changed.unattributed_caller_edge_count,
        );
    }
    if changed.impact_explosion {
        let depth_count = |depth: usize| {
            changed
                .transitive
                .by_depth
                .iter()
                .find(|bucket| bucket.depth == depth)
                .map_or(0, |bucket| bucket.count)
        };
        let _ = writeln!(
            out,
            "- impact explodes at depth 2 ({} vs {} at depth 1) — hidden shotgun-surgery signal",
            depth_count(2),
            depth_count(1),
        );
    }
}

fn render_rows(out: &mut String, rows: &[FunctionRef], limit: usize) {
    for row in rows.iter().take(limit) {
        let _ = writeln!(
            out,
            "  - `{}`{}",
            row.id,
            if row.is_test { " (test)" } else { "" },
        );
    }
    if rows.len() > limit {
        let _ = writeln!(out, "  - … and {} more (see JSON)", rows.len() - limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};
    use rstest::rstest;
    use serde_json::Value;
    use std::path::Path;

    /// A four-hop chain with a test at the top:
    /// `db_insert <- repo_save <- service_save <- api_save <- tests::saves`.
    const CHAIN: &str = "fn db_insert() {}\n\
                         fn repo_save() { db_insert(); }\n\
                         fn service_save() { repo_save(); }\n\
                         fn api_save() { service_save(); }\n\
                         #[cfg(test)]\nmod tests { fn saves() { crate::api_save(); } }\n";

    fn analyze_json(analyzer: &ImpactAnalyzer, path: &Path) -> Value {
        let json = analyzer.analyze(path, OutputFormat::Json).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn ids(rows: &Value) -> Vec<&str> {
        rows.as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn function_seed_reports_direct_callers_and_folded_transitive() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", CHAIN);

        let report = analyze_json(
            &ImpactAnalyzer::new().with_functions(vec!["db_insert".to_owned()]),
            dir.path(),
        );
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["language"], "rust");
        assert_eq!(report["status"], "ok");
        assert_eq!(report["seed_source"], "functions");
        assert_eq!(report["depth_limit"], DEFAULT_IMPACT_DEPTH);

        let changed = &report["changed"][0];
        assert_eq!(changed["function"]["qualified_name"], "crate::db_insert");
        assert_eq!(ids(&changed["direct_callers"]), ["src/lib.rs:repo_save:2"]);
        assert_eq!(changed["vfi"], 4);
        assert_eq!(changed["transitive"]["total"], 4);
        assert_eq!(changed["transitive"]["modules_spanned"], 2);
        assert_eq!(changed["beyond_depth_count"], 0);
        assert_eq!(changed["impact_explosion"], false);
        assert_eq!(changed["excluded_ambiguous_edge_count"], 0);
        assert_eq!(changed["unattributed_caller_edge_count"], 0);

        let by_depth = changed["transitive"]["by_depth"].as_array().unwrap();
        let folded: Vec<(u64, u64)> = by_depth
            .iter()
            .map(|b| (b["depth"].as_u64().unwrap(), b["count"].as_u64().unwrap()))
            .collect();
        assert_eq!(folded, [(1, 1), (2, 1), (3, 1), (4, 1)]);
        assert_eq!(by_depth[1]["modules"]["crate"], 1);

        assert_eq!(
            ids(&changed["reachable_tests"]),
            ["src/lib.rs:saves:6"],
            "the test caller is the verification checklist",
        );
        assert_eq!(report["summary"]["changed_count"], 1);
        assert_eq!(report["summary"]["union_impacted_count"], 4);
        assert_eq!(report["summary"]["union_reachable_test_count"], 1);
        assert!(
            report["note"].as_str().unwrap().contains("lower bound"),
            "bounds are stated in the output itself",
        );
    }

    #[test]
    fn depth_cap_folds_remainder_into_beyond_depth_count() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", CHAIN);

        let report = analyze_json(
            &ImpactAnalyzer::new()
                .with_functions(vec!["db_insert".to_owned()])
                .with_depth(Some(1)),
            dir.path(),
        );
        let changed = &report["changed"][0];
        assert_eq!(report["depth_limit"], 1);
        assert_eq!(changed["vfi"], 1);
        assert_eq!(changed["beyond_depth_count"], 3);
        assert_eq!(
            changed["transitive"]["by_depth"].as_array().unwrap().len(),
            1
        );
        assert_eq!(
            changed["reachable_tests"].as_array().unwrap().len(),
            0,
            "the test caller sits beyond the cap",
        );
    }

    #[test]
    fn diff_mode_seeds_functions_overlapping_unstaged_changes() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(dir.path(), "src/lib.rs", CHAIN);
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);
        write_file(
            dir.path(),
            "src/lib.rs",
            &CHAIN.replace(
                "fn repo_save() { db_insert(); }",
                "fn repo_save() { let _guard = 1; db_insert(); }",
            ),
        );

        let report = analyze_json(&ImpactAnalyzer::new(), dir.path());
        assert_eq!(report["status"], "ok");
        assert_eq!(report["seed_source"], "diff");
        let changed = report["changed"].as_array().unwrap();
        assert_eq!(changed.len(), 1, "got {changed:?}");
        assert_eq!(changed[0]["function"]["qualified_name"], "crate::repo_save");
        assert_eq!(
            ids(&changed[0]["direct_callers"]),
            ["src/lib.rs:service_save:3"],
        );
    }

    #[test]
    fn diff_mode_without_changes_reports_no_changes() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(dir.path(), "src/lib.rs", CHAIN);
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let report = analyze_json(&ImpactAnalyzer::new(), dir.path());
        assert_eq!(report["status"], "no_changes");
        assert_eq!(report["changed"].as_array().unwrap().len(), 0);
        assert_eq!(report["summary"]["changed_count"], 0);
    }

    #[test]
    fn ambiguous_function_symbol_lists_candidates_instead_of_guessing() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn dup() {} }\nmod b { pub fn dup() {} }\nfn caller() { a::dup(); }\n",
        );

        let report = analyze_json(
            &ImpactAnalyzer::new().with_functions(vec!["dup".to_owned()]),
            dir.path(),
        );
        assert_eq!(report["status"], "symbol_ambiguous");
        assert_eq!(report["symbol"], "dup");
        let candidates: Vec<&str> = report["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["qualified_name"].as_str().unwrap())
            .collect();
        assert_eq!(candidates, ["crate::a::dup", "crate::b::dup"]);
        assert_eq!(report["changed"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn unmatched_function_symbol_reports_not_found() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", CHAIN);

        let report = analyze_json(
            &ImpactAnalyzer::new().with_functions(vec!["nope".to_owned()]),
            dir.path(),
        );
        assert_eq!(report["status"], "symbol_not_found");
        assert_eq!(report["symbol"], "nope");
        assert_eq!(report["changed"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn call_cycles_fold_to_one_hop_and_surface_cycle_members() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn ping() { pong(); }\nfn pong() { ping(); }\nfn outer() { ping(); }\n",
        );

        let report = analyze_json(
            &ImpactAnalyzer::new().with_functions(vec!["ping".to_owned()]),
            dir.path(),
        );
        let changed = &report["changed"][0];
        assert_eq!(ids(&changed["cycle_members"]), ["src/lib.rs:pong:2"]);
        assert_eq!(
            ids(&changed["direct_callers"]),
            ["src/lib.rs:pong:2", "src/lib.rs:outer:3"],
        );
        assert_eq!(changed["vfi"], 2, "pong (depth 0) plus outer (depth 1)");
        let by_depth = changed["transitive"]["by_depth"].as_array().unwrap();
        assert_eq!(by_depth.len(), 1);
        assert_eq!(by_depth[0]["depth"], 1);
        assert_eq!(by_depth[0]["count"], 1);
    }

    #[test]
    fn self_recursion_is_not_its_own_caller() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn looping(n: u32) { if n > 0 { looping(n - 1); } }\n",
        );

        let report = analyze_json(
            &ImpactAnalyzer::new().with_functions(vec!["looping".to_owned()]),
            dir.path(),
        );
        let changed = &report["changed"][0];
        assert_eq!(changed["vfi"], 0);
        assert_eq!(changed["direct_callers"].as_array().unwrap().len(), 0);
        assert_eq!(changed.get("cycle_members"), None);
    }

    #[test]
    fn duplicate_function_symbols_dedupe_to_one_seed() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", CHAIN);

        let report = analyze_json(
            &ImpactAnalyzer::new()
                .with_functions(vec!["db_insert".to_owned(), "crate::db_insert".to_owned()]),
            dir.path(),
        );
        assert_eq!(report["changed"].as_array().unwrap().len(), 1);
        assert_eq!(report["summary"]["changed_count"], 1);
    }

    #[test]
    fn multiple_seeds_report_union_summary() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", CHAIN);

        let report = analyze_json(
            &ImpactAnalyzer::new()
                .with_functions(vec!["db_insert".to_owned(), "service_save".to_owned()]),
            dir.path(),
        );
        let changed = report["changed"].as_array().unwrap();
        assert_eq!(changed.len(), 2);
        // db_insert impacts 4; service_save impacts {api_save, tests::saves},
        // both already inside db_insert's set — the union stays 4.
        assert_eq!(report["summary"]["union_impacted_count"], 4);
        assert_eq!(report["summary"]["union_reachable_test_count"], 1);
    }

    #[test]
    fn ambiguous_edges_naming_impact_members_are_counted_as_excluded() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn dup() {} }\nmod b { pub fn dup() {} }\nfn caller() { dup(); }\n",
        );

        let report = analyze_json(
            &ImpactAnalyzer::new().with_functions(vec!["a::dup".to_owned()]),
            dir.path(),
        );
        let changed = &report["changed"][0];
        assert_eq!(changed["vfi"], 0, "the ambiguous call is not traversed");
        assert_eq!(
            changed["excluded_ambiguous_edge_count"], 1,
            "but it is reported as an excluded potential caller",
        );
    }

    #[test]
    fn impact_explosion_flags_wide_second_level_fanout() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::from("fn seed() {}\nfn funnel() { seed(); }\n");
        for i in 0..12 {
            src.push_str(&format!("fn caller_{i}() {{ funnel(); }}\n"));
        }
        write_file(dir.path(), "src/lib.rs", &src);

        let report = analyze_json(
            &ImpactAnalyzer::new().with_functions(vec!["seed".to_owned()]),
            dir.path(),
        );
        let changed = &report["changed"][0];
        assert_eq!(changed["impact_explosion"], true);
        assert_eq!(changed["vfi"], 13);
    }

    #[rstest]
    #[case::shallow_fanout_is_not_an_explosion(2)]
    #[case::wide_but_direct_fanout_is_not_an_explosion(0)]
    fn impact_explosion_stays_off_for_narrow_or_direct_fanout(#[case] second_level: usize) {
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::from("fn seed() {}\nfn funnel() { seed(); }\n");
        for i in 0..second_level {
            src.push_str(&format!("fn caller_{i}() {{ funnel(); }}\n"));
        }
        for i in 0..12 {
            src.push_str(&format!("fn direct_{i}() {{ seed(); }}\n"));
        }
        write_file(dir.path(), "src/lib.rs", &src);

        let report = analyze_json(
            &ImpactAnalyzer::new().with_functions(vec!["seed".to_owned()]),
            dir.path(),
        );
        assert_eq!(report["changed"][0]["impact_explosion"], false);
    }

    #[test]
    fn impact_explosion_requires_the_ratio_not_just_the_floor() {
        // Depth-1 fan-in of 5 and depth-2 fan-in of 12: the floor (>= 10)
        // is met but the ratio (>= 3x depth-1 = 15) is not.
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::from("fn seed() {}\n");
        for i in 0..5 {
            src.push_str(&format!("fn funnel_{i}() {{ seed(); }}\n"));
        }
        for i in 0..12 {
            src.push_str(&format!("fn caller_{i}() {{ funnel_{}(); }}\n", i % 5));
        }
        write_file(dir.path(), "src/lib.rs", &src);

        let report = analyze_json(
            &ImpactAnalyzer::new().with_functions(vec!["seed".to_owned()]),
            dir.path(),
        );
        let changed = &report["changed"][0];
        assert_eq!(changed["transitive"]["by_depth"][0]["count"], 5);
        assert_eq!(changed["transitive"]["by_depth"][1]["count"], 12);
        assert_eq!(changed["impact_explosion"], false);
    }

    #[test]
    fn static_initializer_calls_are_counted_as_unattributed() {
        // A call site outside any function body has no caller to
        // attribute: it must be excluded from the radius but reported.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "const fn seed_const() -> i32 { 1 }\nstatic VALUE: i32 = seed_const();\n",
        );

        let report = analyze_json(
            &ImpactAnalyzer::new().with_functions(vec!["seed_const".to_owned()]),
            dir.path(),
        );
        let changed = &report["changed"][0];
        assert_eq!(changed["vfi"], 0);
        assert_eq!(changed["unattributed_caller_edge_count"], 1);
        assert_eq!(changed["excluded_ambiguous_edge_count"], 0);
    }

    #[test]
    fn analysis_is_deterministic_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", CHAIN);

        let analyzer = ImpactAnalyzer::new().with_functions(vec!["db_insert".to_owned()]);
        let a = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        let b = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn markdown_renders_headline_callers_fold_and_checklist() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", CHAIN);

        let md = ImpactAnalyzer::new()
            .with_functions(vec!["db_insert".to_owned()])
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Impact:"), "got: {md}");
        assert!(
            md.contains("## `crate::db_insert` (src/lib.rs:1-1, module `crate`)"),
            "got: {md}",
        );
        assert!(
            md.contains("- blast radius: 4 function(s) across 2 module(s)"),
            "got: {md}",
        );
        assert!(md.contains("- direct callers (1):"), "got: {md}");
        assert!(md.contains("  - `src/lib.rs:repo_save:2`"), "got: {md}");
        assert!(
            md.contains("- depth 2: 1 function(s) — crate 1"),
            "got: {md}",
        );
        assert!(
            md.contains("- depth 3: 1 function(s) — crate 1"),
            "got: {md}"
        );
        assert!(
            md.contains("- depth 4: 1 function(s) — crate::tests 1"),
            "got: {md}",
        );
        assert!(
            !md.contains("- depth 1:"),
            "depth 1 is the verbatim caller list, never a fold: {md}",
        );
        assert!(
            md.contains("verification checklist — 1 test(s)"),
            "got: {md}",
        );
        assert!(md.contains("  - `src/lib.rs:saves:6` (test)"), "got: {md}");
        assert!(md.contains("lower bound"), "got: {md}");
        // Zero-count annotations stay silent instead of adding noise.
        assert!(!md.contains("same call cycle"), "got: {md}");
        assert!(!md.contains("beyond the depth cap"), "got: {md}");
        assert!(!md.contains("excluded from all counts"), "got: {md}");
        assert!(!md.contains("impact explodes"), "got: {md}");
    }

    #[test]
    fn markdown_renders_beyond_depth_note_when_the_cap_hides_callers() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", CHAIN);

        let md = ImpactAnalyzer::new()
            .with_functions(vec!["db_insert".to_owned()])
            .with_depth(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("- beyond the depth cap: 3 function(s) (raise --depth to include them)"),
            "got: {md}",
        );
    }

    #[test]
    fn markdown_renders_cycle_members() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn ping() { pong(); }\nfn pong() { ping(); }\n",
        );

        let md = ImpactAnalyzer::new()
            .with_functions(vec!["ping".to_owned()])
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("- same call cycle (1): `src/lib.rs:pong:2` — mutually recursive"),
            "got: {md}",
        );
    }

    #[test]
    fn markdown_renders_excluded_edge_counts() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn dup() {} }\nmod b { pub fn dup() {} }\nfn caller() { dup(); }\n",
        );

        let md = ImpactAnalyzer::new()
            .with_functions(vec!["a::dup".to_owned()])
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains(
                "- excluded from all counts: 1 ambiguous edge(s) naming impact members as \
                 candidates, 0 caller-unattributed edge(s)"
            ),
            "got: {md}",
        );
    }

    #[test]
    fn markdown_renders_excluded_counts_for_unattributed_edges_alone() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "const fn seed_const() -> i32 { 1 }\nstatic VALUE: i32 = seed_const();\n",
        );

        let md = ImpactAnalyzer::new()
            .with_functions(vec!["seed_const".to_owned()])
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains(
                "- excluded from all counts: 0 ambiguous edge(s) naming impact members as \
                 candidates, 1 caller-unattributed edge(s)"
            ),
            "got: {md}",
        );
    }

    #[test]
    fn markdown_renders_the_explosion_signal_with_both_counts() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::from("fn seed() {}\nfn funnel() { seed(); }\n");
        for i in 0..12 {
            src.push_str(&format!("fn caller_{i}() {{ funnel(); }}\n"));
        }
        write_file(dir.path(), "src/lib.rs", &src);

        let md = ImpactAnalyzer::new()
            .with_functions(vec!["seed".to_owned()])
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("- impact explodes at depth 2 (12 vs 1 at depth 1)"),
            "got: {md}",
        );
    }

    #[rstest]
    #[case::limit_below_rows_folds_the_rest(2, Some("  - … and 3 more (see JSON)"))]
    #[case::limit_equal_to_rows_is_not_truncation(5, None)]
    fn markdown_caps_caller_lists_at_top(#[case] top: usize, #[case] fold_line: Option<&str>) {
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::from("fn seed() {}\n");
        for i in 0..5 {
            src.push_str(&format!("fn caller_{i}() {{ seed(); }}\n"));
        }
        write_file(dir.path(), "src/lib.rs", &src);

        let md = ImpactAnalyzer::new()
            .with_functions(vec!["seed".to_owned()])
            .with_top(Some(top))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("- direct callers (5):"), "got: {md}");
        match fold_line {
            Some(line) => assert!(md.contains(line), "got: {md}"),
            None => assert!(!md.contains("more (see JSON)"), "got: {md}"),
        }
    }

    #[test]
    fn markdown_no_changes_points_at_function_flag() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", CHAIN);

        let md = ImpactAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("No unstaged change overlaps a function"),
            "got: {md}"
        );
        assert!(md.contains("--function"), "got: {md}");
    }

    #[test]
    fn markdown_ambiguity_lists_ids_for_reruns() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn dup() {} }\nmod b { pub fn dup() {} }\n",
        );

        let md = ImpactAnalyzer::new()
            .with_functions(vec!["dup".to_owned()])
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("`dup` matches 2 function(s)"), "got: {md}");
        assert!(md.contains("Not guessing"), "got: {md}");
        assert!(md.contains("id `src/lib.rs:dup:1`"), "got: {md}");
    }

    #[test]
    fn markdown_reports_missing_test_coverage_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn seed() {}\nfn caller() { seed(); }\n",
        );

        let md = ImpactAnalyzer::new()
            .with_functions(vec!["seed".to_owned()])
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("- no test reaches this function over resolved edges"),
            "got: {md}",
        );
    }

    #[test]
    fn typescript_graphs_are_supported() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.ts",
            "import { helper } from './b';\nexport function entry() { helper(); }\n",
        );
        write_file(dir.path(), "b.ts", "export function helper() {}\n");

        let report = analyze_json(
            &ImpactAnalyzer::new().with_functions(vec!["helper".to_owned()]),
            dir.path(),
        );
        assert_eq!(report["language"], "typescript");
        assert_eq!(report["status"], "ok");
        let changed = &report["changed"][0];
        assert_eq!(changed["vfi"], 1);
        assert!(
            ids(&changed["direct_callers"])[0].contains("entry"),
            "got {report}",
        );
    }

    #[test]
    fn exclude_tests_drops_test_callers_from_the_radius() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", CHAIN);

        let report = analyze_json(
            &ImpactAnalyzer::new()
                .with_functions(vec!["db_insert".to_owned()])
                .with_exclude_tests(true),
            dir.path(),
        );
        let changed = &report["changed"][0];
        assert_eq!(changed["vfi"], 3, "the test caller is out of the graph");
        assert_eq!(changed["reachable_tests"].as_array().unwrap().len(), 0);
    }
}
