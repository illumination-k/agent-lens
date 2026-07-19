//! `analyze hubs` — hub and misplacement smells on the function call
//! graph.
//!
//! Single pass over the shared [`super::call_graph::CallGraph`]
//! substrate. Four findings, each invisible to intra-function
//! complexity metrics:
//!
//! - **God functions** — outlier fan-out; empirically defect-prone.
//! - **Load-bearing functions** — outlier fan-in. This is a
//!   blast-radius signal, *not* a defect signal: the report tells the
//!   agent to check callers before editing, never to "fix" the
//!   function.
//! - **Bottlenecks** — Henry–Kafura information flow spikes,
//!   `loc × (fan_in × fan_out)²` (the module-level pattern from
//!   `lens-domain/src/coupling.rs`, applied per function).
//! - **Misplaced functions** — cross-module pull: the fraction of a
//!   function's resolved call traffic (incoming + outgoing) that lands
//!   in a different module, with the dominant foreign module named.
//!   This is the deterministic, clustering-free fragment of "feature
//!   envy".
//!
//! Conventions shared with the rest of the graph-analyzer family:
//! resolved edges only (degrees are lower bounds; per-node non-resolved
//! call counts and the per-module confidence summary are cited in the
//! output), outlier flagging by a robust quartile rule on log-scaled
//! metrics (never absolute thresholds), and deterministic output — the
//! PageRank importance pass runs a fixed 100 iterations with no
//! epsilon so scores are bit-stable across runs.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use super::call_graph::algo::pagerank;
use super::call_graph::model::{
    CallGraphNode, ModuleResolutionSummary, Resolution, ResolutionMethod,
};
use super::call_graph::{CallGraph, CallGraphBuilder};
use super::{AnalyzerError, OutputFormat};

const SCHEMA_VERSION: u32 = 1;

/// Markdown ranking cap when `--top` is not given. JSON always carries
/// every flagged entry.
const DEFAULT_TOP: usize = 20;

/// PageRank damping factor (standard value).
const PAGERANK_DAMPING: f64 = 0.85;

/// Fixed PageRank iteration count. No epsilon-based early exit: a fixed
/// count is what makes the scores bit-stable across runs.
const PAGERANK_ITERATIONS: usize = 100;

/// Cross-module pull above which a function lands on the "misplaced?"
/// list.
const MISPLACED_PULL_THRESHOLD: f64 = 0.7;

/// Minimum resolved incident call sites before cross-module pull is
/// judged at all. A helper with a single foreign call site has pull 1.0
/// on no evidence; three sites is the least support worth reporting.
const MISPLACED_MIN_CALL_SITES: usize = 3;

/// Minimum share of the *foreign* traffic the dominant foreign module
/// must hold. "Misplaced" means "this belongs in module B"; a shared
/// utility whose foreign traffic scatters thinly across many modules
/// has a high pull but no module B to move to.
const MISPLACED_MIN_DOMINANT_SHARE: f64 = 0.5;

/// Analyzer entry point for `analyze hubs`.
#[derive(Debug, Default, Clone)]
pub struct HubsAnalyzer {
    builder: CallGraphBuilder,
    only_tests: bool,
    top: Option<usize>,
}

impl HubsAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.only_tests = only_tests;
        self.builder = self.builder.with_only_tests(only_tests);
        self
    }

    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.builder = self.builder.with_exclude_tests(exclude_tests);
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.builder = self.builder.with_exclude_patterns(exclude);
        self
    }

    /// Cap the markdown rankings to the top-N entries. JSON output
    /// always carries every flagged entry.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let graph = self.builder.build(path)?;
        let report = Report::build(path, &graph, self.only_tests);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).map_err(AnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&report, self.top)),
        }
    }
}

/// One function's hub metrics. The same shape backs every table so raw
/// components always travel with the composite that flagged them.
#[derive(Debug, Clone, Serialize)]
struct HubEntry {
    id: String,
    qualified_name: String,
    file: String,
    start_line: usize,
    module: String,
    loc: usize,
    /// Distinct resolved callers outside tests. With `--only-tests`
    /// the corpus itself is test code, so test callers count here.
    fan_in: usize,
    /// Distinct resolved callers inside tests (informational; never
    /// drives ranking).
    test_fan_in: usize,
    /// Distinct resolved callees.
    fan_out: usize,
    /// Henry–Kafura information flow: `loc × (fan_in × fan_out)²`.
    /// Size-confounded by construction — read it next to `loc`.
    henry_kafura: u64,
    /// Percentile bucket (1–100) of this node's PageRank importance on
    /// the resolved call graph, call-count-weighted. Buckets, not raw
    /// scores: the distribution is heavy-tailed.
    pagerank_percentile: u32,
    /// Fraction of resolved incident call sites (in + out) landing in
    /// a different module. `None` when the node has no resolved
    /// incident traffic.
    #[serde(skip_serializing_if = "Option::is_none")]
    cross_module_pull: Option<f64>,
    /// Foreign module receiving the most incident traffic, with its
    /// call-site count. Only present when there is foreign traffic.
    #[serde(skip_serializing_if = "Option::is_none")]
    dominant_foreign_module: Option<ForeignModule>,
    /// Resolved incident call sites (in + out), the denominator behind
    /// `cross_module_pull`.
    incident_call_count: usize,
    /// Incident call sites whose other endpoint lives in a different
    /// module — the numerator behind `cross_module_pull`.
    foreign_call_count: usize,
    /// Share of incoming resolved call sites attributed by the
    /// last-segment fallback family (`last_segment`, `path_suffix`,
    /// `crate_narrowed`) rather than direct lexical/self resolution.
    /// High values mean the fan-in itself is less certain. `None` when
    /// nothing resolved inward.
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback_fan_in_fraction: Option<f64>,
    /// Outgoing call sites the resolver could not attribute — the
    /// per-node honesty signal: high counts mean fan-out is
    /// undercounted.
    unresolved_outgoing_call_count: usize,
    ambiguous_outgoing_call_count: usize,
}

impl HubEntry {
    /// Share of foreign traffic held by the dominant foreign module,
    /// or 0.0 when there is none.
    fn dominant_foreign_share(&self) -> f64 {
        if self.foreign_call_count == 0 {
            return 0.0;
        }
        self.dominant_foreign_module
            .as_ref()
            .map_or(0.0, |dominant| {
                dominant.call_count as f64 / self.foreign_call_count as f64
            })
    }
}

#[derive(Debug, Clone, Serialize)]
struct ForeignModule {
    module: String,
    call_count: usize,
}

/// The raw-domain outlier cutoffs the robust rule produced, `None`
/// when a metric had no positive values to fit. A value is flagged
/// when it is strictly greater than the cutoff.
#[derive(Debug, Serialize)]
struct Cutoffs {
    #[serde(skip_serializing_if = "Option::is_none")]
    fan_out: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fan_in: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    henry_kafura: Option<f64>,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    /// All graph nodes, including test functions.
    node_count: usize,
    /// Nodes eligible for flagging (non-test, unless `--only-tests`).
    candidate_count: usize,
    resolved_edge_count: usize,
    /// Outlier cutoffs: flag when metric > cutoff
    /// (Q3 + 1.5·IQR on the log-scaled metric, back-transformed).
    cutoffs: Cutoffs,
    god_functions: Vec<HubEntry>,
    load_bearing: Vec<HubEntry>,
    bottlenecks: Vec<HubEntry>,
    misplaced: Vec<HubEntry>,
    /// Every analyzed function with its raw hub metrics, in node
    /// (file, line) order — the full-fidelity surface behind the
    /// flagged lists above. `analyze risk` consumes the PageRank
    /// percentiles from here.
    functions: Vec<HubEntry>,
    /// Per-module call-site resolution counts — the calibration layer:
    /// a module whose edges are mostly unresolved should have its
    /// hub metrics read as lower bounds.
    modules: Vec<ModuleResolutionSummary>,
}

impl Report {
    fn build(root: &Path, graph: &CallGraph, only_tests: bool) -> Self {
        let metrics = NodeMetrics::compute(graph, only_tests);
        let entries: Vec<HubEntry> = metrics.to_entries(&graph.nodes);

        let fan_out_cutoff = log_outlier_cutoff(entries.iter().map(|e| e.fan_out as f64));
        let fan_in_cutoff = log_outlier_cutoff(entries.iter().map(|e| e.fan_in as f64));
        let hk_cutoff = log_outlier_cutoff(entries.iter().map(|e| e.henry_kafura as f64));

        let mut god_functions: Vec<HubEntry> = entries
            .iter()
            .filter(|e| exceeds(e.fan_out as f64, fan_out_cutoff))
            .cloned()
            .collect();
        god_functions.sort_by(|a, b| b.fan_out.cmp(&a.fan_out).then_with(|| a.id.cmp(&b.id)));

        let mut load_bearing: Vec<HubEntry> = entries
            .iter()
            .filter(|e| exceeds(e.fan_in as f64, fan_in_cutoff))
            .cloned()
            .collect();
        load_bearing.sort_by(|a, b| b.fan_in.cmp(&a.fan_in).then_with(|| a.id.cmp(&b.id)));

        let mut bottlenecks: Vec<HubEntry> = entries
            .iter()
            .filter(|e| exceeds(e.henry_kafura as f64, hk_cutoff))
            .cloned()
            .collect();
        bottlenecks.sort_by(|a, b| {
            b.henry_kafura
                .cmp(&a.henry_kafura)
                .then_with(|| a.id.cmp(&b.id))
        });

        let mut misplaced: Vec<HubEntry> = entries
            .iter()
            .filter(|e| {
                e.incident_call_count >= MISPLACED_MIN_CALL_SITES
                    && e.cross_module_pull
                        .is_some_and(|pull| pull > MISPLACED_PULL_THRESHOLD)
                    && e.dominant_foreign_share() >= MISPLACED_MIN_DOMINANT_SHARE
            })
            .cloned()
            .collect();
        misplaced.sort_by(|a, b| {
            // `filter` above guarantees the pull is present.
            let pull = |e: &HubEntry| e.cross_module_pull.unwrap_or(0.0);
            pull(b)
                .total_cmp(&pull(a))
                .then_with(|| b.incident_call_count.cmp(&a.incident_call_count))
                .then_with(|| a.id.cmp(&b.id))
        });

        Self {
            schema_version: SCHEMA_VERSION,
            root: root.display().to_string(),
            language: graph.language,
            node_count: graph.nodes.len(),
            candidate_count: entries.len(),
            resolved_edge_count: graph
                .edges
                .iter()
                .filter(|e| e.resolution == Resolution::Resolved)
                .count(),
            cutoffs: Cutoffs {
                fan_out: fan_out_cutoff.map(f64::exp),
                fan_in: fan_in_cutoff.map(f64::exp),
                henry_kafura: hk_cutoff.map(f64::exp),
            },
            god_functions,
            load_bearing,
            bottlenecks,
            misplaced,
            functions: entries,
            modules: graph.module_summary.clone(),
        }
    }
}

/// Per-candidate accumulators over the resolved edge set.
#[derive(Debug, Default, Clone)]
struct NodeAccumulator {
    caller_nodes: Vec<usize>,
    test_caller_nodes: Vec<usize>,
    callee_nodes: Vec<usize>,
    same_module_call_count: usize,
    foreign_call_counts: BTreeMap<String, usize>,
    direct_incoming_call_count: usize,
    fallback_incoming_call_count: usize,
}

struct NodeMetrics {
    /// `candidate_of[node_idx]` maps into the candidate-indexed vecs
    /// below, or `None` for excluded (test) nodes.
    candidates: Vec<usize>,
    accumulators: Vec<NodeAccumulator>,
    pagerank_percentiles: Vec<u32>,
}

impl NodeMetrics {
    fn compute(graph: &CallGraph, only_tests: bool) -> Self {
        let is_candidate = |node: &CallGraphNode| only_tests || !node.is_test;
        let candidates: Vec<usize> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| is_candidate(node))
            .map(|(idx, _)| idx)
            .collect();
        let mut candidate_of: Vec<Option<usize>> = vec![None; graph.nodes.len()];
        for (candidate_idx, &node_idx) in candidates.iter().enumerate() {
            candidate_of[node_idx] = Some(candidate_idx);
        }
        let index_by_id = graph.node_index_by_id();

        let mut accumulators = vec![NodeAccumulator::default(); candidates.len()];
        let mut weighted_adjacency: Vec<Vec<(usize, f64)>> = vec![Vec::new(); candidates.len()];
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
            if from_idx == to_idx {
                // Self-recursion is not hub traffic: it would grant
                // every recursive function fan_in = fan_out = 1 and a
                // phantom HK score of its own LOC.
                continue;
            }
            let from_node = &graph.nodes[from_idx];
            let to_node = &graph.nodes[to_idx];

            // Test callers feed the informational split even when the
            // caller is not a candidate.
            if let Some(to_candidate) = candidate_of[to_idx]
                && from_node.is_test
            {
                accumulators[to_candidate].test_caller_nodes.push(from_idx);
            }

            // Everything else — degrees, traffic, PageRank — lives on
            // the candidate-candidate subgraph so test scaffolding
            // cannot inflate prod hub metrics.
            let (Some(from_candidate), Some(to_candidate)) =
                (candidate_of[from_idx], candidate_of[to_idx])
            else {
                continue;
            };
            accumulators[from_candidate].callee_nodes.push(to_idx);
            accumulators[to_candidate].caller_nodes.push(from_idx);

            let fallback = matches!(
                edge.resolution_method,
                Some(
                    ResolutionMethod::LastSegment
                        | ResolutionMethod::PathSuffix
                        | ResolutionMethod::CrateNarrowed
                )
            );
            if fallback {
                accumulators[to_candidate].fallback_incoming_call_count += edge.call_count;
            } else {
                accumulators[to_candidate].direct_incoming_call_count += edge.call_count;
            }

            if from_node.module == to_node.module {
                accumulators[from_candidate].same_module_call_count += edge.call_count;
                accumulators[to_candidate].same_module_call_count += edge.call_count;
            } else {
                *accumulators[from_candidate]
                    .foreign_call_counts
                    .entry(to_node.module.clone())
                    .or_default() += edge.call_count;
                *accumulators[to_candidate]
                    .foreign_call_counts
                    .entry(from_node.module.clone())
                    .or_default() += edge.call_count;
            }

            weighted_adjacency[from_candidate].push((to_candidate, edge.call_count as f64));
        }
        for (accumulator, edges) in accumulators.iter_mut().zip(&mut weighted_adjacency) {
            accumulator.caller_nodes.sort_unstable();
            accumulator.caller_nodes.dedup();
            accumulator.test_caller_nodes.sort_unstable();
            accumulator.test_caller_nodes.dedup();
            accumulator.callee_nodes.sort_unstable();
            accumulator.callee_nodes.dedup();
            edges.sort_by(|a, b| a.0.cmp(&b.0));
        }

        let scores = pagerank(&weighted_adjacency, PAGERANK_DAMPING, PAGERANK_ITERATIONS);
        let pagerank_percentiles = percentile_buckets(&scores);

        Self {
            candidates,
            accumulators,
            pagerank_percentiles,
        }
    }

    fn to_entries(&self, nodes: &[CallGraphNode]) -> Vec<HubEntry> {
        self.candidates
            .iter()
            .enumerate()
            .map(|(candidate_idx, &node_idx)| {
                let node = &nodes[node_idx];
                let acc = &self.accumulators[candidate_idx];
                let fan_in = acc.caller_nodes.len();
                let fan_out = acc.callee_nodes.len();
                let flow = (fan_in as u64).saturating_mul(fan_out as u64);
                let henry_kafura =
                    (node.weights.loc as u64).saturating_mul(flow.saturating_mul(flow));
                let foreign_call_count: usize = acc.foreign_call_counts.values().sum();
                let incident_call_count = acc.same_module_call_count + foreign_call_count;
                let cross_module_pull = (incident_call_count > 0)
                    .then(|| foreign_call_count as f64 / incident_call_count as f64);
                // BTreeMap iteration is name-ordered, so on tied counts
                // the lexicographically smallest module wins.
                let dominant_foreign_module = acc
                    .foreign_call_counts
                    .iter()
                    .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
                    .map(|(module, &call_count)| ForeignModule {
                        module: module.clone(),
                        call_count,
                    });
                let incoming_calls =
                    acc.direct_incoming_call_count + acc.fallback_incoming_call_count;
                let fallback_fan_in_fraction = (incoming_calls > 0)
                    .then(|| acc.fallback_incoming_call_count as f64 / incoming_calls as f64);
                HubEntry {
                    id: node.id.clone(),
                    qualified_name: node.qualified_name.clone(),
                    file: node.file.clone(),
                    start_line: node.start_line,
                    module: node.module.clone(),
                    loc: node.weights.loc,
                    fan_in,
                    test_fan_in: acc.test_caller_nodes.len(),
                    fan_out,
                    henry_kafura,
                    pagerank_percentile: self.pagerank_percentiles[candidate_idx],
                    cross_module_pull,
                    dominant_foreign_module,
                    incident_call_count,
                    foreign_call_count,
                    fallback_fan_in_fraction,
                    unresolved_outgoing_call_count: node.outgoing_calls.unresolved_call_count,
                    ambiguous_outgoing_call_count: node.outgoing_calls.ambiguous_call_count,
                }
            })
            .collect()
    }
}

/// Percentile bucket (1–100) of each score within the whole score set:
/// the share of scores at or below it. Ties share a bucket, so the
/// output is independent of node order.
fn percentile_buckets(scores: &[f64]) -> Vec<u32> {
    let mut sorted: Vec<f64> = scores.to_vec();
    sorted.sort_by(f64::total_cmp);
    scores
        .iter()
        .map(|score| {
            let at_or_below = sorted.partition_point(|s| s.total_cmp(score).is_le());
            ((at_or_below * 100) / scores.len().max(1)) as u32
        })
        .collect()
}

/// Robust outlier cutoff: `Q3 + 1.5·IQR` over the natural logs of the
/// positive values, returned in the log domain. Returns `None` when no
/// value is positive. Callers flag values *strictly greater* than the
/// cutoff (compared in the log domain, so no `exp`/`ln` round-trip
/// error), which means an all-equal distribution (IQR = 0) flags
/// nothing.
fn log_outlier_cutoff(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut logs: Vec<f64> = values.filter(|&v| v > 0.0).map(f64::ln).collect();
    if logs.is_empty() {
        return None;
    }
    logs.sort_by(f64::total_cmp);
    let q1 = percentile_f64(&logs, 25);
    let q3 = percentile_f64(&logs, 75);
    Some(q3 + 1.5 * (q3 - q1))
}

fn exceeds(value: f64, log_cutoff: Option<f64>) -> bool {
    value > 0.0 && log_cutoff.is_some_and(|cutoff| value.ln() > cutoff)
}

/// Nearest-rank percentile over a pre-sorted slice, the f64 sibling of
/// the helper in `analyze::complexity`.
fn percentile_f64(sorted: &[f64], p: usize) -> f64 {
    let idx = ((p.min(100) * sorted.len()).div_ceil(100)).saturating_sub(1);
    sorted[idx]
}

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    let mut out = format!(
        "# Hub report: {} ({} function(s), {} analyzed, {} resolved edge(s))\n",
        report.root, report.node_count, report.candidate_count, report.resolved_edge_count,
    );
    out.push_str(
        "\nDegrees count resolved call edges only, so every number is a lower bound; \
         per-node `unres out` and the module confidence list below say how much is missing. \
         Outliers are flagged by a robust rule (> Q3 + 1.5*IQR on the log-scaled metric), \
         not absolute thresholds. `PR` is the PageRank-importance percentile on the \
         call-count-weighted resolved graph.\n",
    );
    if report.candidate_count == 0 {
        out.push_str("\n_No functions to analyze._\n");
        return out;
    }

    let _ = writeln!(out, "\n## God functions (outlier fan-out, top {limit})");
    out.push_str("\nHigh fan-out coordinates many callees and is empirically defect-prone.\n");
    render_entries(&mut out, &report.god_functions, limit, |e| {
        format!(
            "fan_out={}, fan_in={}(+{} test), loc={}, PR=p{}, unres out={}",
            e.fan_out,
            e.fan_in,
            e.test_fan_in,
            e.loc,
            e.pagerank_percentile,
            e.unresolved_outgoing_call_count + e.ambiguous_outgoing_call_count,
        )
    });

    let _ = writeln!(
        out,
        "\n## Load-bearing functions (outlier fan-in, top {limit})"
    );
    out.push_str(
        "\nHigh fan-in is a blast-radius signal, not a defect signal: check callers \
         before editing these functions. `fallback` is the share of incoming call \
         sites attributed by name-fallback heuristics — higher means the fan-in \
         itself is less certain.\n",
    );
    render_entries(&mut out, &report.load_bearing, limit, |e| {
        format!(
            "fan_in={}(+{} test), fan_out={}, loc={}, PR=p{}, fallback={}",
            e.fan_in,
            e.test_fan_in,
            e.fan_out,
            e.loc,
            e.pagerank_percentile,
            format_fraction(e.fallback_fan_in_fraction),
        )
    });

    let _ = writeln!(out, "\n## Bottlenecks (outlier Henry-Kafura, top {limit})");
    out.push_str(
        "\nHK = loc * (fan_in * fan_out)^2 spikes where wide traffic flows through \
         one function. HK is size-confounded, so read it next to loc.\n",
    );
    render_entries(&mut out, &report.bottlenecks, limit, |e| {
        format!(
            "HK={}, fan_in={}, fan_out={}, loc={}, PR=p{}",
            e.henry_kafura, e.fan_in, e.fan_out, e.loc, e.pagerank_percentile,
        )
    });

    let _ = writeln!(
        out,
        "\n## Misplaced? (cross-module pull > {MISPLACED_PULL_THRESHOLD}, \
         >= {MISPLACED_MIN_CALL_SITES} resolved incident call sites)"
    );
    out.push_str(
        "\nMost of the function's resolved call traffic lands in a different module \
         than its own — the deterministic fragment of \"feature envy\". Consider \
         whether it belongs in the dominant module instead.\n",
    );
    render_entries(&mut out, &report.misplaced, limit, |e| {
        let pull = e.cross_module_pull.unwrap_or(0.0);
        let dominant = e.dominant_foreign_module.as_ref().map_or_else(
            || "-".to_owned(),
            |f| {
                format!(
                    "{} ({}/{} call sites)",
                    f.module, f.call_count, e.incident_call_count
                )
            },
        );
        format!(
            "pull={pull:.2}, dominant: {dominant}, own module: {}",
            e.module
        )
    });

    render_module_confidence(&mut out, &report.modules);
    out
}

fn render_entries(
    out: &mut String,
    entries: &[HubEntry],
    limit: usize,
    detail: impl Fn(&HubEntry) -> String,
) {
    if entries.is_empty() {
        out.push_str("\n_No outliers._\n");
        return;
    }
    out.push('\n');
    for entry in entries.iter().take(limit) {
        let _ = writeln!(
            out,
            "- `{}` ({}:{}): {}",
            entry.qualified_name,
            entry.file,
            entry.start_line,
            detail(entry),
        );
    }
}

fn format_fraction(fraction: Option<f64>) -> String {
    fraction.map_or_else(|| "-".to_owned(), |f| format!("{:.0}%", f * 100.0))
}

/// Cite the graph-confidence calibration: the modules whose call sites
/// resolved worst, i.e. where hub metrics are most undercounted.
fn render_module_confidence(out: &mut String, modules: &[ModuleResolutionSummary]) {
    const TOP_MODULES: usize = 5;
    let mut worst: Vec<&ModuleResolutionSummary> = modules
        .iter()
        .filter(|m| m.total_call_count > 0 && m.calls.resolved_call_count < m.total_call_count)
        .collect();
    if worst.is_empty() {
        return;
    }
    let unresolved_share = |m: &ModuleResolutionSummary| {
        (m.total_call_count - m.calls.resolved_call_count) as f64 / m.total_call_count as f64
    };
    worst.sort_by(|a, b| {
        unresolved_share(b)
            .total_cmp(&unresolved_share(a))
            .then_with(|| b.total_call_count.cmp(&a.total_call_count))
            .then_with(|| a.module.cmp(&b.module))
    });
    out.push_str(
        "\n## Resolution confidence (worst modules)\n\
         \nHub metrics in these modules are the most undercounted; treat their \
         degrees as loose lower bounds.\n\n",
    );
    for m in worst.iter().take(TOP_MODULES) {
        let unresolved = m.total_call_count - m.calls.resolved_call_count;
        let _ = writeln!(
            out,
            "- `{}`: {}/{} call sites not resolved ({:.0}%)",
            m.module,
            unresolved,
            m.total_call_count,
            unresolved_share(m) * 100.0,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use rstest::rstest;
    use serde_json::Value;

    fn analyze_json(path: &Path) -> Value {
        let json = HubsAnalyzer::new()
            .analyze(path, OutputFormat::Json)
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn function_entry<'a>(report: &'a Value, name_suffix: &str) -> &'a Value {
        report["functions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e["qualified_name"]
                    .as_str()
                    .is_some_and(|q| q.ends_with(name_suffix))
            })
            .unwrap_or_else(|| panic!("no function entry ending in {name_suffix}"))
    }

    /// One god function calling twelve helpers, each helper forwarding
    /// to one shared sink. The fan-out outlier rule must single out the
    /// god function, and the fan-in rule the sink.
    fn hub_fixture_source() -> String {
        let mut src = String::from("fn sink() {}\n");
        let mut god_body = String::new();
        for i in 0..12 {
            let _ = writeln!(src, "fn helper_{i}() {{ sink(); }}");
            let _ = write!(god_body, "helper_{i}(); ");
        }
        let _ = writeln!(src, "fn god() {{ {god_body}}}");
        src
    }

    #[test]
    fn god_function_is_flagged_by_fan_out_outlier_rule() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", &hub_fixture_source());

        let report = analyze_json(dir.path());
        let god_functions = report["god_functions"].as_array().unwrap();
        assert_eq!(god_functions.len(), 1, "got {god_functions:?}");
        assert_eq!(god_functions[0]["qualified_name"], "crate::god");
        assert_eq!(god_functions[0]["fan_out"], 12);
        assert_eq!(god_functions[0]["fan_in"], 0);
    }

    #[test]
    fn load_bearing_sink_is_flagged_by_fan_in_outlier_rule() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", &hub_fixture_source());

        let report = analyze_json(dir.path());
        let load_bearing = report["load_bearing"].as_array().unwrap();
        assert_eq!(load_bearing.len(), 1, "got {load_bearing:?}");
        assert_eq!(load_bearing[0]["qualified_name"], "crate::sink");
        assert_eq!(load_bearing[0]["fan_in"], 12);
        assert_eq!(load_bearing[0]["test_fan_in"], 0);
    }

    #[test]
    fn pagerank_percentile_puts_the_sink_at_the_top() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", &hub_fixture_source());

        let report = analyze_json(dir.path());
        let sink_pct = function_entry(&report, "::sink")["pagerank_percentile"]
            .as_u64()
            .unwrap();
        let god_pct = function_entry(&report, "::god")["pagerank_percentile"]
            .as_u64()
            .unwrap();
        assert_eq!(
            sink_pct, 100,
            "the heavily-called sink must top the ranking"
        );
        assert!(god_pct < sink_pct, "god={god_pct} sink={sink_pct}");
    }

    #[test]
    fn analysis_is_deterministic_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", &hub_fixture_source());

        let analyzer = HubsAnalyzer::new();
        let a = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        let b = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn test_callers_are_split_out_of_ranked_fan_in() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn helper() {}\n\
             pub fn caller() { helper(); }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 fn t1() { crate::helper(); }\n\
                 fn t2() { crate::helper(); }\n\
                 fn t3() { crate::helper(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let helper = function_entry(&report, "::helper");
        assert_eq!(helper["fan_in"], 1, "prod fan-in only");
        assert_eq!(helper["test_fan_in"], 3);
        // Test nodes are not candidates, so only prod functions are analyzed.
        assert_eq!(report["candidate_count"], 2);
        assert_eq!(report["node_count"], 5);
    }

    #[test]
    fn misplaced_function_names_dominant_foreign_module() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn envious() {} }\n\
             mod b {\n\
                 fn b1() { crate::a::envious(); }\n\
                 fn b2() { crate::a::envious(); }\n\
                 fn b3() { crate::a::envious(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let misplaced = report["misplaced"].as_array().unwrap();
        assert_eq!(misplaced.len(), 1, "got {misplaced:?}");
        assert_eq!(misplaced[0]["qualified_name"], "crate::a::envious");
        assert_eq!(misplaced[0]["cross_module_pull"], 1.0);
        assert_eq!(
            misplaced[0]["dominant_foreign_module"]["module"],
            "crate::b"
        );
        assert_eq!(misplaced[0]["dominant_foreign_module"]["call_count"], 3);
        assert_eq!(misplaced[0]["incident_call_count"], 3);
        assert_eq!(misplaced[0]["foreign_call_count"], 3);
    }

    #[test]
    fn scattered_foreign_traffic_is_not_misplaced() {
        // Four different modules call the utility once each: high pull,
        // but no dominant module to move it to.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod util { pub fn shared() {} }\n\
             mod a { fn f() { crate::util::shared(); } }\n\
             mod b { fn f() { crate::util::shared(); } }\n\
             mod c { fn f() { crate::util::shared(); } }\n\
             mod d { fn f() { crate::util::shared(); } }\n",
        );

        let report = analyze_json(dir.path());
        let shared = function_entry(&report, "util::shared");
        assert_eq!(shared["cross_module_pull"], 1.0);
        assert!(
            report["misplaced"].as_array().unwrap().is_empty(),
            "scattered utility must not be flagged: {:?}",
            report["misplaced"],
        );
    }

    #[test]
    fn own_module_traffic_keeps_pull_below_threshold() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a {\n\
                 pub fn settled() {}\n\
                 fn a1() { settled(); }\n\
                 fn a2() { settled(); }\n\
                 fn a3() { settled(); }\n\
             }\n\
             mod b { fn f() { crate::a::settled(); } }\n",
        );

        let report = analyze_json(dir.path());
        let settled = function_entry(&report, "a::settled");
        assert_eq!(settled["cross_module_pull"], 0.25);
        assert!(report["misplaced"].as_array().unwrap().is_empty());
    }

    #[test]
    fn henry_kafura_spike_is_flagged_as_bottleneck() {
        // `funnel` sits between six callers and six callees; three
        // small pass-through chains give the outlier rule a baseline.
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::new();
        let mut funnel_body = String::new();
        for i in 0..6 {
            let _ = writeln!(src, "fn target_{i}() {{}}");
            let _ = writeln!(src, "fn caller_{i}() {{ funnel(); }}");
            let _ = write!(funnel_body, "target_{i}(); ");
        }
        let _ = writeln!(src, "fn funnel() {{ {funnel_body}}}");
        for i in 0..3 {
            let _ = writeln!(src, "fn tail_{i}() {{}}");
            let _ = writeln!(src, "fn mid_{i}() {{ tail_{i}(); }}");
            let _ = writeln!(src, "fn head_{i}() {{ mid_{i}(); }}");
        }
        write_file(dir.path(), "src/lib.rs", &src);

        let report = analyze_json(dir.path());
        let bottlenecks = report["bottlenecks"].as_array().unwrap();
        assert_eq!(bottlenecks.len(), 1, "got {bottlenecks:?}");
        assert_eq!(bottlenecks[0]["qualified_name"], "crate::funnel");
        // loc = 1, fan_in = 6, fan_out = 6 -> 1 * (6*6)^2.
        assert_eq!(bottlenecks[0]["henry_kafura"], 1296);
        assert_eq!(bottlenecks[0]["loc"], 1);
    }

    #[test]
    fn fallback_resolved_fan_in_is_reported_as_uncertain() {
        // `self.inner.helper()` resolves through the last-segment
        // fallback, so helper's fan-in provenance must say 100%
        // fallback.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub struct Inner;\n\
             impl Inner { pub fn helper(&self) {} }\n\
             pub struct S { inner: Inner }\n\
             impl S { pub fn caller(&self) { self.inner.helper(); } }\n",
        );

        let report = analyze_json(dir.path());
        let helper = function_entry(&report, "Inner::helper");
        assert_eq!(helper["fallback_fan_in_fraction"], 1.0);
        let direct = function_entry(&report, "S::caller");
        assert_eq!(direct.get("fallback_fan_in_fraction"), None);
    }

    #[test]
    fn self_recursion_is_not_hub_traffic() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "fn looping() { looping(); }\n");

        let report = analyze_json(dir.path());
        let entry = function_entry(&report, "::looping");
        assert_eq!(entry["fan_in"], 0);
        assert_eq!(entry["fan_out"], 0);
        assert_eq!(entry["henry_kafura"], 0);
    }

    #[test]
    fn unresolved_outgoing_calls_surface_per_function() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn caller() { external_call(); another_external(); }\n",
        );

        let report = analyze_json(dir.path());
        let caller = function_entry(&report, "::caller");
        assert_eq!(caller["unresolved_outgoing_call_count"], 2);
        assert_eq!(caller["ambiguous_outgoing_call_count"], 0);
    }

    #[test]
    fn only_tests_mode_analyzes_test_functions() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn prod() {}\n\
             #[cfg(test)]\n\
             mod tests { fn t_helper() {} fn t_caller() { t_helper(); } }\n",
        );

        let json = HubsAnalyzer::new()
            .with_only_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let report: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(report["candidate_count"], 2);
        let helper = function_entry(&report, "::t_helper");
        assert_eq!(
            helper["fan_in"], 1,
            "test callers rank in --only-tests mode"
        );
    }

    #[test]
    fn exclude_tests_drops_test_functions_entirely() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn helper() {}\n\
             #[cfg(test)]\n\
             mod tests { fn t() { crate::helper(); } }\n",
        );

        let json = HubsAnalyzer::new()
            .with_exclude_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let report: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(report["node_count"], 1);
        let helper = function_entry(&report, "::helper");
        assert_eq!(
            helper["test_fan_in"], 0,
            "test callers are gone from the graph"
        );
    }

    #[test]
    fn markdown_states_signal_direction_and_caps_rankings() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", &hub_fixture_source());

        let md = HubsAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Hub report:"), "got: {md}");
        // Agents act on wording literally: fan-in must read as blast
        // radius, never as a defect.
        assert!(md.contains("not a defect signal"), "got: {md}");
        assert!(md.contains("check callers"), "got: {md}");
        assert!(md.contains("lower bound"), "got: {md}");
        assert!(md.contains("top 1"), "got: {md}");
        assert!(md.contains("`crate::god`"), "got: {md}");
        assert!(md.contains("`crate::sink`"), "got: {md}");
    }

    #[test]
    fn markdown_reports_empty_input_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "README.md", "no source here\n");

        let md = HubsAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("_No functions to analyze._"), "got: {md}");
    }

    #[test]
    fn markdown_cites_module_resolution_confidence() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod shaky { fn caller() { external_one(); external_two(); local(); } fn local() {} }\n",
        );

        let md = HubsAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("## Resolution confidence"), "got: {md}");
        assert!(
            md.contains("`crate::shaky`: 2/3 call sites not resolved (67%)"),
            "got: {md}",
        );
    }

    #[rstest]
    #[case::all_equal_values_flag_nothing(vec![3.0, 3.0, 3.0, 3.0], 3.0, false)]
    #[case::single_value_flags_nothing(vec![5.0], 5.0, false)]
    #[case::spike_over_flat_baseline_is_flagged(
        vec![1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0, 12.0],
        12.0,
        true
    )]
    #[case::wide_spread_absorbs_moderate_values(
        vec![1.0, 2.0, 4.0, 8.0, 16.0],
        16.0,
        false
    )]
    fn log_outlier_rule_flags_expected_values(
        #[case] values: Vec<f64>,
        #[case] probe: f64,
        #[case] expected: bool,
    ) {
        let cutoff = log_outlier_cutoff(values.iter().copied());
        assert_eq!(
            exceeds(probe, cutoff),
            expected,
            "values={values:?} probe={probe} cutoff={cutoff:?}",
        );
    }

    #[test]
    fn log_outlier_cutoff_ignores_zeros_and_handles_empty() {
        assert_eq!(log_outlier_cutoff(std::iter::empty()), None);
        assert_eq!(log_outlier_cutoff([0.0, 0.0].into_iter()), None);
    }

    #[rstest]
    #[case::unique_scores(vec![0.1, 0.4, 0.2], vec![33, 100, 66])]
    #[case::all_tied(vec![0.5, 0.5], vec![100, 100])]
    #[case::single(vec![0.9], vec![100])]
    fn percentile_buckets_rank_scores(#[case] scores: Vec<f64>, #[case] expected: Vec<u32>) {
        assert_eq!(percentile_buckets(&scores), expected);
    }
}
