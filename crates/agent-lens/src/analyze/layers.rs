//! `analyze layers` — inferred levelization with module-cycle and
//! skip-layer call listings.
//!
//! Answers the two vertical questions an agent faces on every addition:
//! *where does this new function belong* and *is it OK for this module to
//! call that one*. Lakos levelization over the **resolved** edges of the
//! shared call graph, run at both granularities:
//!
//! - **Function levels** (`L`) — `level(f) = 1 + max(level of its
//!   callees)`, by a topological pass over the SCC condensation from
//!   [`super::call_graph::algo`], so a call cycle collapses to one node
//!   and its members share a level. Level 1 is leaf code that calls
//!   nothing; the highest level is the entry side.
//! - **Module levels** (`M`) — the same pass over the module graph
//!   induced by the cross-module call edges. Levelizing the module graph
//!   directly, rather than averaging its members' function levels, is
//!   what keeps module levels consistent with module edges: a module
//!   holding both leaf helpers and top-level orchestration would
//!   otherwise land at a median level its own calls contradict.
//!
//! No configuration: both layerings are inferred from the code, nothing
//! is declared.
//!
//! The listings are **structural facts, not judgments** — callbacks,
//! dependency injection, and trait-object dispatch all shape the graph in
//! ways the extracted call facts cannot distinguish from a design error:
//!
//! - **module cycles** — the cross-module calls that make two or more
//!   modules mutually dependent, so no level ordering exists between
//!   them. This is the concrete answer to "is it OK for this module to
//!   call that one": each call site is named with its lines. `analyze
//!   coupling` finds module cycles from imports on a single crate or
//!   entry file; this finds them from call edges, across every supported
//!   language, with the call sites attached.
//! - **skip-level calls** — a downward call passing over at least one
//!   intermediate module level.
//! - **unlevelable functions** — members of a call cycle, whose relative
//!   levels are undefined by construction; `analyze cycles` reports those
//!   tangles in full.
//!
//! Modules also carry the range of function levels their members cover; a
//! module spanning many levels mixes leaf helpers with orchestration,
//! which is a vertical cohesion smell.
//!
//! Only resolved edges are traversable, so levels are lower bounds and a
//! single mis-resolved edge can lift a whole chain: the per-level function
//! and edge counts are the support behind each number, and the per-module
//! resolution confidence is cited at the end.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use super::call_graph::algo::{Condensation, condense};
use super::call_graph::model::{
    ModuleResolutionSummary, NodeVisibility, Resolution, ResolutionMethod,
};
use super::call_graph::{CallGraph, CallGraphBuilder, delegate_call_graph_builders};
use super::format::render_module_confidence;
use super::runner::render_report;
use super::{AnalyzerError, OutputFormat};

const SCHEMA_VERSION: u32 = 1;

/// Markdown listing cap when `--top` is not given. JSON always carries
/// every row.
const DEFAULT_TOP: usize = 20;

/// A module whose members span more than this many function levels gets
/// flagged: it mixes leaf helpers with orchestration.
const WIDE_SPAN_LEVELS: usize = 2;

/// Minimum module-level gap for a downward call to count as skipping a
/// layer. One step down is a direct dependency; two or more steps pass
/// over at least one intermediate level.
const SKIP_MIN_GAP: usize = 2;

/// Module pairs listed per cycle in markdown. JSON carries all of them.
const PAIRS_PER_CYCLE: usize = 5;

/// Concrete call sites listed per module pair in markdown. JSON carries
/// all of them.
const EVIDENCE_PER_PAIR: usize = 3;

/// Module names listed per level in markdown. JSON carries all of them.
const MODULES_PER_LEVEL: usize = 6;

/// Analyzer entry point for `analyze layers`.
#[derive(Debug, Default, Clone)]
pub struct LayersAnalyzer {
    builder: CallGraphBuilder,
    only_tests: bool,
    top: Option<usize>,
}

impl LayersAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    delegate_call_graph_builders! {
        builder,
        only_tests => only_tests,
        exclude_tests,
    }

    /// Cap the markdown listings to the top-N entries. JSON output always
    /// carries every row.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let graph = self.builder.build(path)?;
        let report = Report::build(path, &graph, self.only_tests);
        render_report(&report, format, || format_markdown(&report, self.top))
    }
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    /// Function-level depth buckets, ascending: level 1 (leaves) first.
    levels: Vec<LevelBucket>,
    /// Modules by their level on the module graph, highest (entry side)
    /// first.
    modules: Vec<ModuleLevel>,
    /// Orientation set for reading the map top-down: functions with no
    /// resolved caller that are `main`, exported, or in a language whose
    /// adapter does not extract visibility.
    entry_points: Vec<EntryPoint>,
    /// Groups of mutually dependent modules, with the calls that tie them
    /// together. No level ordering exists inside a group.
    module_cycles: Vec<ModuleCycle>,
    /// Downward cross-module calls jumping over at least one module
    /// level.
    skip_calls: Vec<ModulePair>,
    /// Call cycles, where relative function levels are undefined by
    /// construction.
    unlevelable: Unlevelable,
    /// Per-module call-site resolution counts — the calibration layer: a
    /// module whose edges mostly failed to resolve has under-supported
    /// levels.
    resolution: Vec<ModuleResolutionSummary>,
    summary: Summary,
}

/// One depth bucket of the function-level map.
#[derive(Debug, Serialize)]
struct LevelBucket {
    level: usize,
    function_count: usize,
    module_count: usize,
    /// Resolved call edges leaving this level — the support behind the
    /// level assignment. A level held up by a single edge is one
    /// mis-resolution away from collapsing.
    outgoing_edge_count: usize,
    /// Every module with at least one member at this level.
    modules: Vec<String>,
}

/// One module's vertical position on the module graph, plus the range of
/// function levels its members actually cover.
#[derive(Debug, Serialize)]
struct ModuleLevel {
    module: String,
    /// Level on the module graph, `1 + max(level of the modules it
    /// calls)`, with a module cycle counting as one node.
    level: usize,
    /// This module is part of a module cycle, so its level is shared with
    /// the rest of the cycle rather than derived from it.
    cyclic: bool,
    function_count: usize,
    member_min_level: usize,
    member_max_level: usize,
    member_level_span: usize,
    /// Members span more than [`WIDE_SPAN_LEVELS`] function levels: the
    /// module mixes leaf helpers with orchestration.
    wide_span: bool,
}

#[derive(Debug, Serialize)]
struct EntryPoint {
    id: String,
    qualified_name: String,
    file: String,
    start_line: usize,
    module: String,
    level: usize,
    visibility: NodeVisibility,
    basis: EntryBasis,
}

/// Why a zero-fan-in function is treated as an entry point. Visibility is
/// only extracted for Rust and Go, so TypeScript and Python entries rest
/// on the weaker zero-fan-in evidence alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EntryBasis {
    /// Named `main`: a binary entry point in every supported language,
    /// and never `pub`/exported in Rust or Go.
    Main,
    /// Public or exported, so callers can live outside the analyzed tree.
    Public,
    /// The language adapter does not extract visibility; zero fan-in is
    /// the only evidence.
    VisibilityUnknown,
}

/// One strongly connected group of modules: they call each other directly
/// or transitively, so no level ordering exists between them.
#[derive(Debug, Serialize)]
struct ModuleCycle {
    size: usize,
    /// Shared module level of every member.
    level: usize,
    modules: Vec<String>,
    call_count: usize,
    /// The cross-module calls inside the cycle, chattiest pair first.
    pairs: Vec<ModulePair>,
}

/// One ordered module pair with the concrete call sites behind it.
#[derive(Debug, Serialize)]
struct ModulePair {
    from_module: String,
    from_level: usize,
    to_module: String,
    to_level: usize,
    /// Module levels crossed downward. Always 0 inside a module cycle,
    /// where both ends share a level.
    level_gap: usize,
    call_count: usize,
    /// Of `call_count`, how many were attributed by a name-fallback
    /// heuristic rather than a direct lexical match. A pair that is
    /// entirely fallback-resolved may not exist at all — a bare
    /// `.clone()` resolving to some unrelated `Owner::clone` is the
    /// canonical false edge. Pairs with direct evidence rank first.
    fallback_call_count: usize,
    calls: Vec<CallEvidence>,
}

#[derive(Debug, Serialize)]
struct CallEvidence {
    from: String,
    to: String,
    /// Function levels of the two endpoints, which need not mirror the
    /// module levels: a leaf-heavy module can still hold one high-level
    /// caller.
    from_level: usize,
    to_level: usize,
    /// Every call site behind this caller/callee pair was attributed by a
    /// name-fallback heuristic, so the edge itself is uncertain.
    fallback: bool,
    call_lines: Vec<usize>,
}

/// Functions whose relative level is undefined because they sit in a call
/// cycle.
#[derive(Debug, Serialize)]
struct Unlevelable {
    /// Call cycles (SCCs with 2+ members) in the resolved graph.
    cycle_count: usize,
    /// Functions across those cycles; all members of one cycle share a
    /// level.
    function_count: usize,
    /// Members of the largest cycle (0 when there are none).
    largest: usize,
    /// Resolved call sites between distinct members of a cycle.
    call_count: usize,
}

#[derive(Debug, Serialize)]
struct Summary {
    function_count: usize,
    module_count: usize,
    /// Deepest function level (0 on an empty corpus).
    max_level: usize,
    /// Deepest module level (0 on an empty corpus).
    max_module_level: usize,
    resolved_edge_count: usize,
    entry_point_count: usize,
    wide_span_module_count: usize,
    module_cycle_count: usize,
    cyclic_module_count: usize,
    cyclic_call_count: usize,
    skip_pair_count: usize,
    skip_call_count: usize,
}

impl Report {
    fn build(root: &Path, graph: &CallGraph, only_tests: bool) -> Self {
        let condensation = condense(&graph.resolved_adjacency());
        let node_levels = levels_of(&condensation);
        let module_graph = ModuleGraph::build(graph);

        let edges = EdgeScan::run(graph, &node_levels, &module_graph);
        let levels = level_buckets(graph, &node_levels, &edges.outgoing_edges_by_level);
        let modules = module_levels(graph, &node_levels, &module_graph);
        let entry_points = entry_points(graph, &node_levels, only_tests);
        let module_cycles = module_cycles(&module_graph, edges.cyclic_pairs);

        let summary = Summary {
            function_count: graph.nodes.len(),
            module_count: modules.len(),
            max_level: levels.last().map_or(0, |bucket| bucket.level),
            max_module_level: module_graph.levels.iter().copied().max().unwrap_or(0),
            resolved_edge_count: graph
                .edges
                .iter()
                .filter(|e| e.resolution == Resolution::Resolved)
                .count(),
            entry_point_count: entry_points.len(),
            wide_span_module_count: modules.iter().filter(|m| m.wide_span).count(),
            module_cycle_count: module_cycles.len(),
            cyclic_module_count: module_cycles.iter().map(|c| c.size).sum(),
            cyclic_call_count: module_cycles.iter().map(|c| c.call_count).sum(),
            skip_pair_count: edges.skip.len(),
            skip_call_count: edges.skip.iter().map(|p| p.call_count).sum(),
        };

        Self {
            schema_version: SCHEMA_VERSION,
            root: root.display().to_string(),
            language: graph.language,
            levels,
            modules,
            entry_points,
            module_cycles,
            skip_calls: edges.skip,
            unlevelable: unlevelable(&condensation, edges.unlevelable_call_count),
            resolution: graph.module_summary.clone(),
            summary,
        }
    }
}

/// Lakos levelization on an SCC condensation: `level = 1 + max(level of
/// callees)`, so a component that calls nothing sits at level 1 and every
/// member of a cycle shares one level. Returned per original node.
///
/// [`Condensation::components`] is in reverse topological order — every
/// condensed edge points to a strictly lower component index — so a
/// single ascending pass has all callee levels already final.
fn levels_of(condensation: &Condensation) -> Vec<usize> {
    let mut component_levels = vec![1usize; condensation.components.len()];
    for (component, callees) in condensation.edges.iter().enumerate() {
        component_levels[component] = 1 + callees
            .iter()
            .map(|&callee| component_levels[callee])
            .max()
            .unwrap_or(0);
    }
    condensation
        .component_of
        .iter()
        .map(|&component| component_levels[component])
        .collect()
}

/// The module graph induced by cross-module resolved call edges, already
/// levelized and condensed.
struct ModuleGraph {
    /// Module names by index, ascending.
    names: Vec<String>,
    index: BTreeMap<String, usize>,
    /// Level per module index.
    levels: Vec<usize>,
    condensation: Condensation,
}

impl ModuleGraph {
    fn build(graph: &CallGraph) -> Self {
        let mut names: Vec<String> = graph.nodes.iter().map(|n| n.module.clone()).collect();
        names.sort_unstable();
        names.dedup();
        let index: BTreeMap<String, usize> = names
            .iter()
            .enumerate()
            .map(|(idx, name)| (name.clone(), idx))
            .collect();

        let node_index_by_id = graph.node_index_by_id();
        let mut neighbors: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); names.len()];
        for edge in &graph.edges {
            let Some((from, to)) = resolved_endpoints(edge, &node_index_by_id) else {
                continue;
            };
            let (from_module, to_module) = (&graph.nodes[from].module, &graph.nodes[to].module);
            if from_module == to_module {
                continue;
            }
            if let (Some(&a), Some(&b)) = (index.get(from_module), index.get(to_module)) {
                neighbors[a].insert(b);
            }
        }
        let adjacency: Vec<Vec<usize>> = neighbors
            .into_iter()
            .map(|set| set.into_iter().collect())
            .collect();
        let condensation = condense(&adjacency);

        Self {
            names,
            index,
            levels: levels_of(&condensation),
            condensation,
        }
    }

    fn level_of(&self, module: &str) -> usize {
        self.index.get(module).map_or(0, |&idx| self.levels[idx])
    }

    fn same_cycle(&self, a: &str, b: &str) -> bool {
        match (self.index.get(a), self.index.get(b)) {
            (Some(&a), Some(&b)) => {
                self.condensation.component_of[a] == self.condensation.component_of[b]
            }
            _ => false,
        }
    }
}

/// The two node indices of a resolved edge, or `None` when the edge is
/// unresolved, ambiguous, anonymous, or has an endpoint outside the graph.
fn resolved_endpoints(
    edge: &super::call_graph::model::CallGraphEdge,
    node_index_by_id: &std::collections::HashMap<&str, usize>,
) -> Option<(usize, usize)> {
    if edge.resolution != Resolution::Resolved {
        return None;
    }
    let from = node_index_by_id.get(edge.from.as_deref()?)?;
    let to = node_index_by_id.get(edge.to.as_deref()?)?;
    Some((*from, *to))
}

/// One row per module: its module-graph level plus the range of function
/// levels its members cover. Highest module level first.
fn module_levels(
    graph: &CallGraph,
    node_levels: &[usize],
    module_graph: &ModuleGraph,
) -> Vec<ModuleLevel> {
    let mut member_levels: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (idx, node) in graph.nodes.iter().enumerate() {
        member_levels
            .entry(node.module.as_str())
            .or_default()
            .push(node_levels[idx]);
    }
    let cyclic: Vec<bool> = module_graph
        .condensation
        .component_of
        .iter()
        .map(|&component| module_graph.condensation.components[component].len() >= 2)
        .collect();

    let mut modules: Vec<ModuleLevel> = member_levels
        .into_iter()
        .map(|(module, mut levels)| {
            levels.sort_unstable();
            let member_min_level = levels.first().copied().unwrap_or(0);
            let member_max_level = levels.last().copied().unwrap_or(0);
            let member_level_span = member_max_level - member_min_level;
            let idx = module_graph.index.get(module).copied();
            ModuleLevel {
                module: module.to_owned(),
                level: idx.map_or(0, |idx| module_graph.levels[idx]),
                cyclic: idx.is_some_and(|idx| cyclic[idx]),
                function_count: levels.len(),
                member_min_level,
                member_max_level,
                member_level_span,
                wide_span: member_level_span > WIDE_SPAN_LEVELS,
            }
        })
        .collect();
    modules.sort_by(|a, b| {
        b.level
            .cmp(&a.level)
            .then_with(|| b.function_count.cmp(&a.function_count))
            .then_with(|| a.module.cmp(&b.module))
    });
    modules
}

fn level_buckets(
    graph: &CallGraph,
    node_levels: &[usize],
    outgoing_edges_by_level: &BTreeMap<usize, usize>,
) -> Vec<LevelBucket> {
    let mut by_level: BTreeMap<usize, (usize, BTreeSet<&str>)> = BTreeMap::new();
    for (idx, node) in graph.nodes.iter().enumerate() {
        let bucket = by_level.entry(node_levels[idx]).or_default();
        bucket.0 += 1;
        bucket.1.insert(node.module.as_str());
    }
    by_level
        .into_iter()
        .map(|(level, (function_count, modules))| LevelBucket {
            level,
            function_count,
            module_count: modules.len(),
            outgoing_edge_count: outgoing_edges_by_level.get(&level).copied().unwrap_or(0),
            modules: modules.into_iter().map(ToOwned::to_owned).collect(),
        })
        .collect()
}

/// Zero-fan-in functions that orient the map: `main`, exported symbols
/// (callers may live outside the analyzed tree), and — where the adapter
/// does not extract visibility — every remaining zero-fan-in function.
///
/// Test functions are excluded unless the whole corpus is test code
/// (`--only-tests`): every test is a zero-fan-in root and would drown the
/// production entry points.
fn entry_points(graph: &CallGraph, node_levels: &[usize], only_tests: bool) -> Vec<EntryPoint> {
    let mut entries: Vec<EntryPoint> = graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| (only_tests || !node.is_test) && node.weights.fan_in == 0)
        .filter_map(|(idx, node)| {
            let basis = if node.name == "main" {
                EntryBasis::Main
            } else {
                match node.visibility {
                    NodeVisibility::Public | NodeVisibility::Exported => EntryBasis::Public,
                    NodeVisibility::Unknown => EntryBasis::VisibilityUnknown,
                    NodeVisibility::Restricted
                    | NodeVisibility::Private
                    | NodeVisibility::Unexported => return None,
                }
            };
            Some(EntryPoint {
                id: node.id.clone(),
                qualified_name: node.qualified_name.clone(),
                file: node.file.clone(),
                start_line: node.start_line,
                module: node.module.clone(),
                level: node_levels[idx],
                visibility: node.visibility,
                basis,
            })
        })
        .collect();
    entries.sort_by(|a, b| b.level.cmp(&a.level).then_with(|| a.id.cmp(&b.id)));
    entries
}

fn unlevelable(condensation: &Condensation, call_count: usize) -> Unlevelable {
    let cycles: Vec<usize> = condensation
        .components
        .iter()
        .map(Vec::len)
        .filter(|&size| size >= 2)
        .collect();
    Unlevelable {
        cycle_count: cycles.len(),
        function_count: cycles.iter().sum(),
        largest: cycles.iter().copied().max().unwrap_or(0),
        call_count,
    }
}

/// Fold the cyclic module pairs into one entry per module cycle, largest
/// and chattiest first.
fn module_cycles(module_graph: &ModuleGraph, pairs: Vec<ModulePair>) -> Vec<ModuleCycle> {
    let mut by_component: BTreeMap<usize, Vec<ModulePair>> = BTreeMap::new();
    for pair in pairs {
        let Some(&idx) = module_graph.index.get(&pair.from_module) else {
            continue;
        };
        by_component
            .entry(module_graph.condensation.component_of[idx])
            .or_default()
            .push(pair);
    }
    let mut cycles: Vec<ModuleCycle> = by_component
        .into_iter()
        .map(|(component, mut pairs)| {
            sort_pairs(&mut pairs);
            let members = &module_graph.condensation.components[component];
            ModuleCycle {
                size: members.len(),
                level: members
                    .first()
                    .map_or(0, |&member| module_graph.levels[member]),
                modules: members
                    .iter()
                    .map(|&member| module_graph.names[member].clone())
                    .collect(),
                call_count: pairs.iter().map(|p| p.call_count).sum(),
                pairs,
            }
        })
        .collect();
    cycles.sort_by(|a, b| {
        b.size
            .cmp(&a.size)
            .then_with(|| b.call_count.cmp(&a.call_count))
            .then_with(|| a.modules.cmp(&b.modules))
    });
    cycles
}

/// How one cross-module call sits relative to the two modules' levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PairClass {
    /// Caller and callee modules are mutually dependent: no ordering.
    Cyclic,
    /// Downward call jumping over at least one intermediate module level.
    Skip,
}

/// Accumulated call sites for one ordered module pair, keyed by the
/// concrete `(caller, callee)` node indices behind them.
#[derive(Debug, Default)]
struct PairAccumulator {
    call_count: usize,
    fallback_call_count: usize,
    /// `(caller, callee)` node indices -> (call lines, all fallback).
    calls: BTreeMap<(usize, usize), (Vec<usize>, bool)>,
}

/// Whether the resolver reached this edge's target through a name
/// fallback (last segment, path suffix, crate narrowing) rather than a
/// direct lexical or `self`-method match. Edges with no recorded method
/// are not fallbacks.
fn is_fallback(method: Option<ResolutionMethod>) -> bool {
    matches!(
        method,
        Some(
            ResolutionMethod::LastSegment
                | ResolutionMethod::PathSuffix
                | ResolutionMethod::CrateNarrowed
        )
    )
}

/// One pass over the resolved edges: per-level edge support, the
/// module-pair listings, and the in-cycle call-site count.
struct EdgeScan {
    outgoing_edges_by_level: BTreeMap<usize, usize>,
    cyclic_pairs: Vec<ModulePair>,
    skip: Vec<ModulePair>,
    unlevelable_call_count: usize,
}

impl EdgeScan {
    fn run(graph: &CallGraph, node_levels: &[usize], module_graph: &ModuleGraph) -> Self {
        let node_index_by_id = graph.node_index_by_id();
        let mut outgoing_edges_by_level: BTreeMap<usize, usize> = BTreeMap::new();
        let mut unlevelable_call_count = 0usize;
        let mut pairs: BTreeMap<(PairClass, &str, &str), PairAccumulator> = BTreeMap::new();

        for edge in &graph.edges {
            let Some((from, to)) = resolved_endpoints(edge, &node_index_by_id) else {
                continue;
            };
            *outgoing_edges_by_level
                .entry(node_levels[from])
                .or_default() += 1;
            if from == to {
                // Self-recursion crosses no module boundary and forms no
                // multi-member cycle.
                continue;
            }
            if node_levels[from] == node_levels[to] {
                // Distinct functions on one level are, by construction,
                // members of the same call cycle.
                unlevelable_call_count += edge.call_count;
            }
            let (from_module, to_module) = (
                graph.nodes[from].module.as_str(),
                graph.nodes[to].module.as_str(),
            );
            if from_module == to_module {
                continue;
            }
            let Some(class) = classify(module_graph, from_module, to_module) else {
                continue;
            };
            let fallback = is_fallback(edge.resolution_method);
            let accumulator = pairs.entry((class, from_module, to_module)).or_default();
            accumulator.call_count += edge.call_count;
            if fallback {
                accumulator.fallback_call_count += edge.call_count;
            }
            let entry = accumulator
                .calls
                .entry((from, to))
                .or_insert_with(|| (Vec::new(), true));
            entry.0.extend(&edge.call_lines);
            entry.1 &= fallback;
        }

        let mut cyclic_pairs = Vec::new();
        let mut skip = Vec::new();
        for ((class, from_module, to_module), accumulator) in pairs {
            let from_level = module_graph.level_of(from_module);
            let to_level = module_graph.level_of(to_module);
            let view = ModulePair {
                from_module: from_module.to_owned(),
                from_level,
                to_module: to_module.to_owned(),
                to_level,
                level_gap: from_level.saturating_sub(to_level),
                call_count: accumulator.call_count,
                fallback_call_count: accumulator.fallback_call_count,
                calls: accumulator
                    .calls
                    .into_iter()
                    .map(|((from, to), (mut call_lines, fallback))| {
                        call_lines.sort_unstable();
                        call_lines.dedup();
                        CallEvidence {
                            from: graph.nodes[from].id.clone(),
                            to: graph.nodes[to].id.clone(),
                            from_level: node_levels[from],
                            to_level: node_levels[to],
                            fallback,
                            call_lines,
                        }
                    })
                    .collect(),
            };
            match class {
                PairClass::Cyclic => cyclic_pairs.push(view),
                PairClass::Skip => skip.push(view),
            }
        }
        sort_pairs(&mut skip);

        Self {
            outgoing_edges_by_level,
            cyclic_pairs,
            skip,
            unlevelable_call_count,
        }
    }
}

/// Classify a cross-module call. Mutually dependent modules share a level
/// and are reported as a cycle; otherwise the caller is strictly above the
/// callee by construction, and only descents passing over an intermediate
/// level are worth listing.
fn classify(module_graph: &ModuleGraph, from_module: &str, to_module: &str) -> Option<PairClass> {
    if module_graph.same_cycle(from_module, to_module) {
        return Some(PairClass::Cyclic);
    }
    let gap = module_graph
        .level_of(from_module)
        .saturating_sub(module_graph.level_of(to_module));
    (gap >= SKIP_MIN_GAP).then_some(PairClass::Skip)
}

/// Pairs with direct (non-fallback) call sites first — an entirely
/// fallback-resolved pair may not be a real dependency at all — then the
/// widest level crossing, then the chattiest pair.
fn sort_pairs(pairs: &mut [ModulePair]) {
    fn key(p: &ModulePair) -> PairKey<'_> {
        let direct = p.call_count - p.fallback_call_count;
        (
            direct == 0,
            Reverse(p.level_gap),
            Reverse(direct),
            Reverse(p.call_count),
            p.from_module.as_str(),
            p.to_module.as_str(),
        )
    }
    pairs.sort_by(|a, b| key(a).cmp(&key(b)));
}

/// Ranking key for [`sort_pairs`], ascending: fallback-only pairs last,
/// then widest gap, most direct call sites, most call sites, module names.
type PairKey<'a> = (
    bool,
    Reverse<usize>,
    Reverse<usize>,
    Reverse<usize>,
    &'a str,
    &'a str,
);

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    let summary = &report.summary;
    let mut out = format!(
        "# Layer map: {} ({} function level(s), {} module level(s), {} function(s), \
         {} module(s), {} resolved edge(s))\n",
        report.root,
        summary.max_level,
        summary.max_module_level,
        summary.function_count,
        summary.module_count,
        summary.resolved_edge_count,
    );
    out.push_str(
        "\nLevels are inferred, not declared. `L` is a function level — 1 + max(level of its \
         callees) over resolved call edges, on the SCC condensation, so a call cycle is one \
         node. `M` is the same computation on the module graph induced by cross-module calls, \
         which is why a module's level need not match its members' levels: a leaf-heavy module \
         can still hold one high-level caller. L1/M1 is leaf code that calls nothing; the \
         highest level is the entry side. Resolved edges only, so levels are lower bounds and \
         one mis-resolved edge can lift a whole chain — the per-level counts are the support \
         behind each number, and `name-fallback` marks call sites whose target the resolver \
         picked by a last-segment heuristic — those edges may not exist. The call listings \
         below are structural facts, not architecture errors: callbacks and dependency \
         injection shape the graph in ways the extracted call facts cannot tell apart from a \
         design error.\n",
    );
    if summary.function_count == 0 {
        out.push_str("\n_No functions to analyze._\n");
        return out;
    }

    render_levels(&mut out, &report.levels);
    render_modules(&mut out, &report.modules, limit, summary);
    render_entry_points(&mut out, &report.entry_points, limit);
    render_module_cycles(&mut out, &report.module_cycles, limit, summary);
    render_skip_calls(&mut out, &report.skip_calls, limit, summary);
    render_unlevelable(&mut out, &report.unlevelable);
    render_module_confidence(
        &mut out,
        &report.resolution,
        "Levels in these modules rest on the fewest resolved edges; treat their positions \
         as lower bounds.",
    );
    out
}

fn render_levels(out: &mut String, levels: &[LevelBucket]) {
    out.push_str("\n## Function levels (leaf first)\n\n");
    for bucket in levels {
        let shown: Vec<&str> = bucket
            .modules
            .iter()
            .take(MODULES_PER_LEVEL)
            .map(String::as_str)
            .collect();
        let overflow = bucket.modules.len() - shown.len();
        let _ = writeln!(
            out,
            "- L{}: {} function(s), {} module(s), {} outgoing edge(s) — {}{}",
            bucket.level,
            bucket.function_count,
            bucket.module_count,
            bucket.outgoing_edge_count,
            shown.join(", "),
            if overflow > 0 {
                format!(", +{overflow} more")
            } else {
                String::new()
            },
        );
    }
}

fn render_modules(out: &mut String, modules: &[ModuleLevel], limit: usize, summary: &Summary) {
    let _ = writeln!(
        out,
        "\n## Modules (module level, highest first; top {limit})"
    );
    let _ = writeln!(
        out,
        "\n{} of {} module(s) hold members spanning more than {WIDE_SPAN_LEVELS} function \
         levels, mixing leaf helpers with orchestration — a vertical cohesion smell.\n",
        summary.wide_span_module_count, summary.module_count,
    );
    for module in modules.iter().take(limit) {
        let _ = writeln!(
            out,
            "- M{} `{}`: {} function(s), members L{}-L{} (span {}){}{}",
            module.level,
            module.module,
            module.function_count,
            module.member_min_level,
            module.member_max_level,
            module.member_level_span,
            if module.wide_span {
                " — wide span"
            } else {
                ""
            },
            if module.cyclic { " — in a cycle" } else { "" },
        );
    }
}

fn render_entry_points(out: &mut String, entries: &[EntryPoint], limit: usize) {
    let _ = writeln!(
        out,
        "\n## Entry points ({}, highest function level first; top {limit})",
        entries.len()
    );
    out.push_str(
        "\nFunctions with no resolved caller that are `main`, exported, or written in a \
         language whose adapter does not extract visibility (`visibility_unknown` — the \
         weakest basis). Read the map down from here.\n",
    );
    if entries.is_empty() {
        out.push_str("\n_None._\n");
        return;
    }
    out.push('\n');
    for entry in entries.iter().take(limit) {
        let _ = writeln!(
            out,
            "- L{} `{}` ({}:{}) in `{}`, basis: {}",
            entry.level,
            entry.qualified_name,
            entry.file,
            entry.start_line,
            entry.module,
            basis_label(entry.basis),
        );
    }
}

fn basis_label(basis: EntryBasis) -> &'static str {
    match basis {
        EntryBasis::Main => "main",
        EntryBasis::Public => "public",
        EntryBasis::VisibilityUnknown => "visibility_unknown",
    }
}

fn render_module_cycles(out: &mut String, cycles: &[ModuleCycle], limit: usize, summary: &Summary) {
    let _ = writeln!(
        out,
        "\n## Module cycles ({} cycle(s), {} module(s), {} call site(s); top {limit})",
        summary.module_cycle_count, summary.cyclic_module_count, summary.cyclic_call_count,
    );
    out.push_str(
        "\nThese modules call each other directly or transitively, so no level ordering \
         exists between them and every member shares one module level. Each call site below \
         is a place where the cycle is realised.\n",
    );
    if cycles.is_empty() {
        out.push_str("\n_None._\n");
        return;
    }
    out.push('\n');
    for cycle in cycles.iter().take(limit) {
        let _ = writeln!(
            out,
            "- M{} — {} module(s), {} call site(s): {}",
            cycle.level,
            cycle.size,
            cycle.call_count,
            cycle
                .modules
                .iter()
                .map(|m| format!("`{m}`"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        render_pair_evidence(out, &cycle.pairs, PAIRS_PER_CYCLE, "  ");
    }
}

fn render_skip_calls(out: &mut String, pairs: &[ModulePair], limit: usize, summary: &Summary) {
    let _ = writeln!(
        out,
        "\n## Skip-level calls ({SKIP_MIN_GAP}+ module levels down: {} pair(s), \
         {} call site(s); top {limit})",
        summary.skip_pair_count, summary.skip_call_count,
    );
    out.push_str(
        "\nA downward call passing over at least one intermediate module level. Expected for \
         shared leaf utilities; worth a look when a skipped level owns the same concern.\n",
    );
    if pairs.is_empty() {
        out.push_str("\n_None._\n");
        return;
    }
    out.push('\n');
    render_pair_evidence(out, pairs, limit, "");
}

/// Render module pairs and their call-site evidence as a nested list at
/// `indent`.
fn render_pair_evidence(out: &mut String, pairs: &[ModulePair], limit: usize, indent: &str) {
    for pair in pairs.iter().take(limit) {
        let _ = writeln!(
            out,
            "{indent}- `{}` M{} -> `{}` M{} (gap {}, {} call site(s){})",
            pair.from_module,
            pair.from_level,
            pair.to_module,
            pair.to_level,
            pair.level_gap,
            pair.call_count,
            if pair.fallback_call_count > 0 {
                format!(", {} name-fallback", pair.fallback_call_count)
            } else {
                String::new()
            },
        );
        for call in pair.calls.iter().take(EVIDENCE_PER_PAIR) {
            let _ = writeln!(
                out,
                "{indent}  - {} L{} -> {} L{} (line(s) {}){}",
                call.from,
                call.from_level,
                call.to,
                call.to_level,
                call.call_lines
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                if call.fallback {
                    " [name-fallback]"
                } else {
                    ""
                },
            );
        }
        let overflow = pair.calls.len().saturating_sub(EVIDENCE_PER_PAIR);
        if overflow > 0 {
            let _ = writeln!(out, "{indent}  - +{overflow} more caller/callee pair(s)");
        }
    }
    let overflow = pairs.len().saturating_sub(limit);
    if overflow > 0 {
        let _ = writeln!(out, "{indent}- +{overflow} more module pair(s)");
    }
}

fn render_unlevelable(out: &mut String, unlevelable: &Unlevelable) {
    if unlevelable.cycle_count == 0 {
        return;
    }
    let _ = writeln!(
        out,
        "\n## Unlevelable functions (call cycles)\n\n{} cycle(s) covering {} function(s) \
         (largest {}), {} internal call site(s). Members of a cycle share one level by \
         construction; `analyze cycles` reports them with cut suggestions.",
        unlevelable.cycle_count,
        unlevelable.function_count,
        unlevelable.largest,
        unlevelable.call_count,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use rstest::rstest;
    use serde_json::Value;

    fn analyze_json(path: &Path) -> Value {
        let json = LayersAnalyzer::new()
            .analyze(path, OutputFormat::Json)
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn module_row<'a>(report: &'a Value, module: &str) -> &'a Value {
        report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["module"] == module)
            .unwrap_or_else(|| panic!("no module row for {module}"))
    }

    #[rstest]
    #[case::leaf_only(vec![vec![]], vec![1])]
    #[case::two_chain(vec![vec![1], vec![]], vec![2, 1])]
    #[case::three_chain(vec![vec![1], vec![2], vec![]], vec![3, 2, 1])]
    #[case::diamond_takes_longest_path(
        vec![vec![1, 2], vec![3], vec![3], vec![]],
        vec![3, 2, 2, 1]
    )]
    #[case::cycle_members_share_a_level(vec![vec![1], vec![0, 2], vec![]], vec![2, 2, 1])]
    #[case::self_loop_is_a_leaf(vec![vec![0]], vec![1])]
    #[case::disconnected_nodes_are_all_leaves(vec![vec![], vec![]], vec![1, 1])]
    fn levels_follow_the_longest_call_chain(
        #[case] adjacency: Vec<Vec<usize>>,
        #[case] expected: Vec<usize>,
    ) {
        assert_eq!(levels_of(&condense(&adjacency)), expected);
    }

    #[test]
    fn levels_bucket_a_call_chain_leaf_first() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn top() { mid(); }\nfn mid() { leaf(); }\nfn leaf() {}\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["language"], "rust");
        assert_eq!(report["summary"]["max_level"], 3);
        assert_eq!(report["summary"]["function_count"], 3);

        let levels: Vec<(u64, u64, u64)> = report["levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| {
                (
                    l["level"].as_u64().unwrap(),
                    l["function_count"].as_u64().unwrap(),
                    l["outgoing_edge_count"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(levels, [(1, 1, 0), (2, 1, 1), (3, 1, 1)]);
    }

    #[test]
    fn module_levels_come_from_the_module_graph_not_the_member_levels() {
        let dir = tempfile::tempdir().unwrap();
        // `mixed` holds a leaf (L1) and the top of the whole chain (L4).
        // Averaging its members would drop it below `deep`, contradicting
        // the call that runs mixed -> deep.
        write_file(dir.path(), "src/lib.rs", "mod mixed;\nmod deep;\n");
        write_file(
            dir.path(),
            "src/mixed.rs",
            "pub fn top() { crate::deep::a(); }\npub fn leaf() {}\n",
        );
        write_file(
            dir.path(),
            "src/deep.rs",
            "pub fn a() { b(); }\npub fn b() { c(); }\npub fn c() {}\n",
        );

        let report = analyze_json(dir.path());
        let mixed = module_row(&report, "crate::mixed");
        let deep = module_row(&report, "crate::deep");
        assert_eq!(mixed["level"], 2, "caller module must outrank its callee");
        assert_eq!(deep["level"], 1);
        // Member spans still describe the function levels underneath.
        assert_eq!(mixed["member_min_level"], 1);
        assert_eq!(mixed["member_max_level"], 4);
        assert_eq!(mixed["member_level_span"], 3);
        assert_eq!(mixed["wide_span"], true);
        assert_eq!(report["summary"]["wide_span_module_count"], 1);
        assert_eq!(report["summary"]["max_module_level"], 2);
        // A pure descent of one level is ordinary layering.
        assert_eq!(report["skip_calls"], serde_json::json!([]));
        assert_eq!(report["module_cycles"], serde_json::json!([]));
    }

    #[test]
    fn mutually_calling_modules_are_reported_as_one_cycle_with_call_sites() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "mod a;\nmod b;\n");
        write_file(
            dir.path(),
            "src/a.rs",
            "pub fn f() { crate::b::g(); }\npub fn h() {}\n",
        );
        write_file(dir.path(), "src/b.rs", "pub fn g() { crate::a::h(); }\n");

        let report = analyze_json(dir.path());
        assert_eq!(report["summary"]["module_cycle_count"], 1);
        assert_eq!(report["summary"]["cyclic_module_count"], 2);
        assert_eq!(report["summary"]["cyclic_call_count"], 2);

        let cycle = &report["module_cycles"][0];
        assert_eq!(cycle["size"], 2);
        assert_eq!(
            cycle["modules"],
            serde_json::json!(["crate::a", "crate::b"])
        );
        let pairs: Vec<(&str, &str, u64)> = cycle["pairs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["from_module"].as_str().unwrap(),
                    p["to_module"].as_str().unwrap(),
                    p["level_gap"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            pairs,
            [("crate::a", "crate::b", 0), ("crate::b", "crate::a", 0)],
        );
        let call = &cycle["pairs"][0]["calls"][0];
        assert_eq!(call["from"], "src/a.rs:f:1");
        assert_eq!(call["to"], "src/b.rs:g:1");
        assert_eq!(call["call_lines"], serde_json::json!([1]));
        // Both modules share the cycle's level and are marked as such.
        assert_eq!(module_row(&report, "crate::a")["cyclic"], true);
        assert_eq!(module_row(&report, "crate::b")["cyclic"], true);
        assert_eq!(
            module_row(&report, "crate::a")["level"],
            module_row(&report, "crate::b")["level"],
        );
    }

    #[test]
    fn skip_level_calls_need_a_gap_of_at_least_two() {
        let dir = tempfile::tempdir().unwrap();
        // crate::top (M3) reaches crate::leaf (M1) directly, skipping the
        // intermediate level; the one-step calls must not be listed.
        write_file(dir.path(), "src/lib.rs", "mod top;\nmod mid;\nmod leaf;\n");
        write_file(
            dir.path(),
            "src/top.rs",
            "pub fn f() { crate::mid::f(); crate::leaf::f(); }\n",
        );
        write_file(
            dir.path(),
            "src/mid.rs",
            "pub fn f() { crate::leaf::f(); }\n",
        );
        write_file(dir.path(), "src/leaf.rs", "pub fn f() {}\n");

        let report = analyze_json(dir.path());
        let skips: Vec<(&str, &str, u64)> = report["skip_calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                (
                    p["from_module"].as_str().unwrap(),
                    p["to_module"].as_str().unwrap(),
                    p["level_gap"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(skips, [("crate::top", "crate::leaf", 2)]);
        assert_eq!(report["summary"]["skip_pair_count"], 1);
        assert_eq!(report["summary"]["skip_call_count"], 1);
    }

    #[rstest]
    #[case::direct(Some(ResolutionMethod::Lexical), false)]
    #[case::self_method(Some(ResolutionMethod::SelfMethod), false)]
    #[case::unrecorded(None, false)]
    #[case::last_segment(Some(ResolutionMethod::LastSegment), true)]
    #[case::path_suffix(Some(ResolutionMethod::PathSuffix), true)]
    #[case::crate_narrowed(Some(ResolutionMethod::CrateNarrowed), true)]
    fn fallback_covers_the_name_matching_heuristics(
        #[case] method: Option<ResolutionMethod>,
        #[case] expected: bool,
    ) {
        assert_eq!(is_fallback(method), expected);
    }

    #[test]
    fn name_fallback_call_sites_are_marked_and_ranked_last() {
        let dir = tempfile::tempdir().unwrap();
        // `top::f` reaches `leaf::direct` by an absolute path (a lexical
        // match) and `leaf::guessed` by a bare, unimported name the
        // resolver can only pin down by its last segment.
        write_file(dir.path(), "src/lib.rs", "mod top;\nmod mid;\nmod leaf;\n");
        write_file(
            dir.path(),
            "src/top.rs",
            "pub fn f() { crate::mid::f(); crate::leaf::direct(); guessed(); }\n",
        );
        write_file(
            dir.path(),
            "src/mid.rs",
            "pub fn f() { crate::leaf::direct(); }\n",
        );
        write_file(
            dir.path(),
            "src/leaf.rs",
            "pub fn direct() {}\npub fn guessed() {}\n",
        );

        let report = analyze_json(dir.path());
        let skips = report["skip_calls"].as_array().unwrap();
        assert_eq!(skips.len(), 1, "got {skips:?}");
        let pair = &skips[0];
        assert_eq!(pair["from_module"], "crate::top");
        assert_eq!(pair["to_module"], "crate::leaf");
        assert_eq!(pair["call_count"], 2);
        // Both call sites reach the same module, so the pair keeps its
        // direct evidence while each caller/callee row carries its own
        // provenance.
        let marked: Vec<(&str, bool)> = pair["calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["to"].as_str().unwrap(), c["fallback"].as_bool().unwrap()))
            .collect();
        assert_eq!(
            marked,
            [
                ("src/leaf.rs:direct:1", false),
                ("src/leaf.rs:guessed:2", true)
            ],
        );
    }

    #[test]
    fn cycle_members_share_a_level_and_are_reported_as_unlevelable() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn a() { b(); }\nfn b() { a(); leaf(); }\nfn leaf() {}\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["unlevelable"]["cycle_count"], 1);
        assert_eq!(report["unlevelable"]["function_count"], 2);
        assert_eq!(report["unlevelable"]["largest"], 2);
        assert_eq!(report["unlevelable"]["call_count"], 2);
        assert_eq!(report["summary"]["max_level"], 2);
        // Both cycle members sit at level 2.
        let top = report["levels"].as_array().unwrap().last().unwrap();
        assert_eq!(top["level"], 2);
        assert_eq!(top["function_count"], 2);
    }

    #[test]
    fn entry_points_cover_main_and_public_zero_fan_in_functions() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/main.rs",
            "fn main() { helper(); }\nfn helper() {}\npub fn api() { helper(); }\n",
        );

        let report = analyze_json(dir.path());
        let entries: Vec<(&str, &str)> = report["entry_points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["qualified_name"].as_str().unwrap(),
                    e["basis"].as_str().unwrap(),
                )
            })
            .collect();
        // `helper` is private with a caller, so it is not an entry.
        assert_eq!(entries, [("crate::api", "public"), ("crate::main", "main")]);
        assert_eq!(report["summary"]["entry_point_count"], 2);
    }

    #[test]
    fn private_zero_fan_in_functions_are_not_entry_points() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn orphan() {}\npub fn used() {}\n",
        );

        let report = analyze_json(dir.path());
        let names: Vec<&str> = report["entry_points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["qualified_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["crate::used"]);
    }

    #[test]
    fn typescript_entry_points_fall_back_to_zero_fan_in() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.ts",
            "import { b } from './b';\nexport function a() { b(); }\n",
        );
        write_file(dir.path(), "b.ts", "export function b() {}\n");

        let report = analyze_json(dir.path());
        assert_eq!(report["language"], "typescript");
        let entries: Vec<(&str, &str)> = report["entry_points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["qualified_name"].as_str().unwrap(),
                    e["basis"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(entries.len(), 1, "got {entries:?}");
        assert_eq!(entries[0].1, "visibility_unknown");
    }

    #[test]
    fn test_functions_are_excluded_from_entry_points() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "a.ts", "export function prod() {}\n");
        write_file(
            dir.path(),
            "a.test.ts",
            "import { prod } from './a';\nexport function checksProd() { prod(); }\n",
        );

        let report = analyze_json(dir.path());
        let names: Vec<&str> = report["entry_points"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["qualified_name"].as_str().unwrap())
            .collect();
        assert!(
            names.iter().all(|n| !n.contains("checksProd")),
            "test function leaked into entry points: {names:?}",
        );
    }

    #[test]
    fn exclude_tests_drops_test_files_from_the_level_map() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn prod() {}\n#[cfg(test)]\nmod tests {\n    fn t() { super::prod(); }\n}\n",
        );

        assert_eq!(analyze_json(dir.path())["summary"]["max_level"], 2);

        let json = LayersAnalyzer::new()
            .with_exclude_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let excluded: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(excluded["summary"]["max_level"], 1);
        assert_eq!(excluded["summary"]["function_count"], 1);
    }

    #[test]
    fn exclude_patterns_drop_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/keep.rs", "pub fn keep() {}\n");
        write_file(dir.path(), "src/generated.rs", "pub fn generated() {}\n");

        let json = LayersAnalyzer::new()
            .with_exclude_patterns(vec!["generated.rs".to_owned()])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let report: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(report["summary"]["function_count"], 1);
    }

    #[test]
    fn only_tests_levelizes_the_test_corpus_alone() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn prod() {}\n#[cfg(test)]\nmod tests {\n    fn helper() {}\n    \
             fn t() { helper(); }\n}\n",
        );

        let json = LayersAnalyzer::new()
            .with_only_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let report: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(report["summary"]["function_count"], 2);
        assert_eq!(report["summary"]["max_level"], 2);
        // With the corpus restricted to tests, private test functions are
        // still not exported, so nothing qualifies as an entry point.
        assert_eq!(report["summary"]["entry_point_count"], 0);
    }

    #[test]
    fn markdown_renders_the_map_with_its_framing() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "mod top;\nmod mid;\nmod leaf;\n");
        write_file(
            dir.path(),
            "src/top.rs",
            "pub fn f() { crate::mid::f(); crate::leaf::f(); }\n",
        );
        write_file(
            dir.path(),
            "src/mid.rs",
            "pub fn f() { crate::leaf::f(); }\n",
        );
        write_file(dir.path(), "src/leaf.rs", "pub fn f() {}\n");

        let md = LayersAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Layer map:"), "got: {md}");
        assert!(md.contains("## Function levels (leaf first)"), "got: {md}");
        assert!(md.contains("## Modules (module level"), "got: {md}");
        assert!(md.contains("## Entry points"), "got: {md}");
        assert!(md.contains("## Module cycles (0 cycle(s)"), "got: {md}");
        assert!(
            md.contains("## Skip-level calls (2+ module levels"),
            "got: {md}"
        );
        assert!(
            md.contains("`crate::top` M3 -> `crate::leaf` M1 (gap 2"),
            "got: {md}",
        );
        assert!(
            md.contains("src/top.rs:f:1 L3 -> src/leaf.rs:f:1 L1"),
            "got: {md}"
        );
        assert!(
            md.contains("structural facts, not architecture errors"),
            "got: {md}",
        );
        // No call cycles in this fixture, so the section must be absent
        // rather than rendered with zero counts.
        assert!(!md.contains("## Unlevelable functions"), "got: {md}");
    }

    fn pair(
        from: &str,
        to: &str,
        level_gap: usize,
        call_count: usize,
        fallback: usize,
    ) -> ModulePair {
        ModulePair {
            from_module: from.to_owned(),
            from_level: level_gap + 1,
            to_module: to.to_owned(),
            to_level: 1,
            level_gap,
            call_count,
            fallback_call_count: fallback,
            calls: Vec::new(),
        }
    }

    fn sorted_names(mut pairs: Vec<ModulePair>) -> Vec<String> {
        sort_pairs(&mut pairs);
        pairs.into_iter().map(|p| p.from_module).collect()
    }

    #[test]
    fn sort_pairs_demotes_fallback_only_pairs_below_every_direct_pair() {
        // `guessed` has the widest gap but no directly-resolved call
        // site, so it must still rank below both direct pairs.
        let names = sorted_names(vec![
            pair("guessed", "leaf", 5, 2, 2),
            pair("narrow", "leaf", 1, 1, 0),
            pair("wide", "leaf", 3, 4, 1),
        ]);
        assert_eq!(names, ["wide", "narrow", "guessed"]);
    }

    #[test]
    fn sort_pairs_breaks_gap_ties_by_direct_then_total_then_name() {
        let names = sorted_names(vec![
            pair("zzz", "leaf", 2, 3, 0),
            pair("aaa", "leaf", 2, 3, 0),
            pair("mostly_direct", "leaf", 2, 5, 1),
            pair("chatty_but_guessy", "leaf", 2, 9, 6),
        ]);
        assert_eq!(names, ["mostly_direct", "chatty_but_guessy", "aaa", "zzz"],);
    }

    #[test]
    fn resolved_edge_count_ignores_unresolved_call_sites() {
        let dir = tempfile::tempdir().unwrap();
        // Two resolved edges (a -> b, b -> c) plus one call the resolver
        // cannot attribute at all.
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn a() { b(); }\nfn b() { c(); external(); }\nfn c() {}\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["summary"]["resolved_edge_count"], 2);
    }

    #[test]
    fn fallback_call_counts_are_tallied_per_module_pair() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "mod top;\nmod mid;\nmod leaf;\n");
        write_file(
            dir.path(),
            "src/top.rs",
            "pub fn f() { crate::mid::f(); guessed(); guessed(); }\n",
        );
        write_file(
            dir.path(),
            "src/mid.rs",
            "pub fn f() { crate::leaf::direct(); }\n",
        );
        write_file(
            dir.path(),
            "src/leaf.rs",
            "pub fn direct() {}\npub fn guessed() {}\n",
        );

        let report = analyze_json(dir.path());
        let pair = &report["skip_calls"][0];
        assert_eq!(pair["from_module"], "crate::top");
        assert_eq!(pair["to_module"], "crate::leaf");
        assert_eq!(pair["call_count"], 2);
        assert_eq!(pair["fallback_call_count"], 2);
    }

    #[test]
    fn markdown_truncates_long_listings_with_an_explicit_remainder() {
        let dir = tempfile::tempdir().unwrap();
        // Eight leaf modules (two over MODULES_PER_LEVEL), four callers
        // in `top` reaching the same leaf function (one over
        // EVIDENCE_PER_PAIR), and two skip pairs against `--top 1`.
        let mut root = String::from("mod top;\nmod other;\nmod mid;\n");
        for i in 0..8 {
            let _ = writeln!(root, "mod leaf{i};");
        }
        write_file(dir.path(), "src/lib.rs", &root);
        for i in 0..8 {
            write_file(dir.path(), &format!("src/leaf{i}.rs"), "pub fn f() {}\n");
        }
        write_file(
            dir.path(),
            "src/mid.rs",
            "pub fn f() { crate::leaf0::f(); }\n",
        );
        write_file(
            dir.path(),
            "src/top.rs",
            "pub fn a() { crate::mid::f(); crate::leaf0::f(); }\n\
             pub fn b() { crate::leaf0::f(); }\n\
             pub fn c() { crate::leaf0::f(); }\n\
             pub fn d() { crate::leaf0::f(); }\n",
        );
        write_file(
            dir.path(),
            "src/other.rs",
            "pub fn a() { crate::mid::f(); crate::leaf1::f(); }\n",
        );

        let md = LayersAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        // All eight leaf modules sit on L1; only MODULES_PER_LEVEL are
        // named, the rest are counted.
        assert!(md.contains(", +2 more\n"), "got: {md}");
        // Four distinct callers reach the same callee; three are shown.
        assert!(md.contains("+1 more caller/callee pair(s)"), "got: {md}");
        // Two skip pairs, capped at --top 1.
        assert!(md.contains("+1 more module pair(s)"), "got: {md}");
        assert!(md.contains("basis: public"), "got: {md}");
    }

    #[test]
    fn markdown_omits_remainder_lines_when_nothing_is_truncated() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "mod top;\nmod mid;\nmod leaf;\n");
        write_file(
            dir.path(),
            "src/top.rs",
            "pub fn f() { crate::mid::f(); crate::leaf::f(); }\n",
        );
        write_file(
            dir.path(),
            "src/mid.rs",
            "pub fn f() { crate::leaf::f(); }\n",
        );
        write_file(dir.path(), "src/leaf.rs", "pub fn f() {}\n");

        let md = LayersAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        // All three remainder markers — truncated modules on a level
        // line, truncated call sites, truncated module pairs — render a
        // `+N more`, so nothing may. This also rejects a spurious
        // `+0 more` on an exactly-fitting listing.
        let remainders: Vec<&str> = md
            .lines()
            .filter(|l| l.contains(", +") || l.trim_start().starts_with("- +"))
            .collect();
        assert!(remainders.is_empty(), "got {remainders:?} in: {md}");
    }

    #[test]
    fn markdown_names_every_entry_point_basis() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/main.rs",
            "fn main() { helper(); }\nfn helper() {}\npub fn api() {}\n",
        );

        let rust = LayersAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(rust.contains("basis: main"), "got: {rust}");
        assert!(rust.contains("basis: public"), "got: {rust}");

        let ts_dir = tempfile::tempdir().unwrap();
        write_file(ts_dir.path(), "a.ts", "export function a() {}\n");
        let ts = LayersAnalyzer::new()
            .analyze(ts_dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(ts.contains("basis: visibility_unknown"), "got: {ts}");
    }

    #[test]
    fn markdown_reports_empty_corpora_quietly() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "README.md", "no code here\n");

        let md = LayersAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("_No functions to analyze._"), "got: {md}");
    }

    #[test]
    fn markdown_reports_cycles_and_confidence_sections() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod x { pub fn same() {} }\nmod y { pub fn same() {} }\n\
             fn a() { b(); same(); }\nfn b() { a(); }\n",
        );

        let md = LayersAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("## Unlevelable functions (call cycles)"),
            "got: {md}"
        );
        assert!(
            md.contains("1 cycle(s) covering 2 function(s)"),
            "got: {md}"
        );
        assert!(md.contains("## Resolution confidence"), "got: {md}");
    }

    #[test]
    fn top_caps_the_markdown_listings_only() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "mod a;\nmod b;\nmod c;\n");
        for name in ["a", "b", "c"] {
            write_file(dir.path(), &format!("src/{name}.rs"), "pub fn f() {}\n");
        }

        let json = LayersAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let report: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(report["modules"].as_array().unwrap().len(), 3);

        let md = LayersAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        // Module rows are the only ones carrying a member span.
        assert_eq!(
            md.lines().filter(|l| l.contains("members L")).count(),
            1,
            "got: {md}",
        );
        assert_eq!(
            md.lines().filter(|l| l.contains("basis: ")).count(),
            1,
            "got: {md}",
        );
    }
}
