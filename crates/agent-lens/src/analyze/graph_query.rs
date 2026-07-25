//! `analyze graph-query` — canned traversal primitives on the function
//! call graph.
//!
//! Answers the structural questions agents ask mid-task ("who calls X?",
//! "what does X reach?", "is there a call chain from A to B?") with a
//! deliberately closed vocabulary of four verbs over the shared
//! [`super::call_graph::CallGraph`] substrate:
//!
//! - **callers** — reverse BFS: functions that reach the symbol.
//! - **callees** — forward BFS: functions the symbol reaches.
//! - **neighborhood** — the ego graph around the symbol (`--direction
//!   in|out|both`, default both).
//! - **path** — shortest call chain from `--symbol` to `--to`, reported
//!   as one witness chain with call-line evidence per hop.
//!
//! The vocabulary stays at these four verbs on purpose; a general query
//! language is explicit scope creep. Symbols are matched by
//! `::`-segment suffix on `qualified_name` (or an exact node id);
//! ambiguous matches are listed, never guessed.
//!
//! Conventions shared with the graph-analyzer family: traversal follows
//! resolved edges only, so every result set is a lower bound — each row
//! carries the node's counts of unresolved and ambiguous outgoing call
//! sites as the honesty signal (a per-hop resolution tag would be
//! vacuous: every traversable hop is resolved by construction). Output
//! is capped by node count (`--limit`, default 50), not just hops, and
//! is deterministic: BFS visits in (depth, file, line) order. Markdown
//! adapts its verbosity: at most 3 results render with qualified name,
//! span, and module detail; larger sets fold to id + depth rows.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use super::call_graph::algo::{self, BfsVisit};
use super::call_graph::model::{CallGraphNode, Resolution};
use super::call_graph::{CallGraph, CallGraphBuilder, match_symbol};
use super::{AnalyzerError, OutputFormat};

const SCHEMA_VERSION: u32 = 1;

/// Default output cap, counted in result nodes (not hops).
pub const DEFAULT_GRAPH_QUERY_LIMIT: usize = 50;

/// Default traversal depth for `callers` / `callees` / `neighborhood`.
pub const DEFAULT_GRAPH_QUERY_DEPTH: usize = 1;

/// Result-set size at or below which markdown renders per-node detail
/// (qualified name, span, module) instead of compact id rows.
const DETAIL_THRESHOLD: usize = 3;

/// The four traversal verbs. This vocabulary is intentionally closed —
/// new structural questions should compose these, not grow a query
/// language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GraphQueryKind {
    /// Functions that (transitively) call the symbol.
    Callers,
    /// Functions the symbol (transitively) calls.
    Callees,
    /// The ego graph around the symbol, both directions by default.
    Neighborhood,
    /// Shortest call chain from `--symbol` to `--to`.
    Path,
}

impl GraphQueryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Callers => "callers",
            Self::Callees => "callees",
            Self::Neighborhood => "neighborhood",
            Self::Path => "path",
        }
    }
}

/// Traversal direction for `neighborhood` (and the effective direction
/// reported for every verb).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GraphDirection {
    In,
    Out,
    Both,
}

impl GraphDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
            Self::Both => "both",
        }
    }
}

/// Analyzer entry point for `analyze graph-query`.
#[derive(Debug, Clone)]
pub struct GraphQueryAnalyzer {
    builder: CallGraphBuilder,
    query: GraphQueryKind,
    symbol: String,
    to: Option<String>,
    depth: Option<usize>,
    direction: Option<GraphDirection>,
    limit: Option<usize>,
}

impl GraphQueryAnalyzer {
    pub fn new(query: GraphQueryKind, symbol: impl Into<String>) -> Self {
        Self {
            builder: CallGraphBuilder::new(),
            query,
            symbol: symbol.into(),
            to: None,
            depth: None,
            direction: None,
            limit: None,
        }
    }

    /// Destination symbol for `path` queries.
    pub fn with_to(mut self, to: Option<String>) -> Self {
        self.to = to;
        self
    }

    /// Traversal depth cap. Defaults to 1 for `callers` / `callees` /
    /// `neighborhood`; for `path` it caps the search (default
    /// unbounded).
    pub fn with_depth(mut self, depth: Option<usize>) -> Self {
        self.depth = depth;
        self
    }

    /// Traversal direction for `neighborhood` (default both).
    pub fn with_direction(mut self, direction: Option<GraphDirection>) -> Self {
        self.direction = direction;
        self
    }

    /// Output cap by node count (default
    /// [`DEFAULT_GRAPH_QUERY_LIMIT`]). The witness chain of a `path`
    /// query is never truncated — a partial chain is not evidence.
    pub fn with_limit(mut self, limit: Option<usize>) -> Self {
        self.limit = limit;
        self
    }

    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
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

    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        self.validate()?;
        let graph = self.builder.build(path)?;
        let report = Report::build(path, &graph, self);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).map_err(AnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&report)),
        }
    }

    /// Reject flag combinations that would otherwise be silently
    /// ignored — an agent passing `--to` to `callers` almost certainly
    /// meant `--query path`.
    fn validate(&self) -> Result<(), AnalyzerError> {
        let invalid = |message: &str| {
            Err(AnalyzerError::InvalidQuery {
                message: message.to_owned(),
            })
        };
        match self.query {
            GraphQueryKind::Path => {
                if self.to.is_none() {
                    return invalid("`--query path` requires `--to <symbol>`");
                }
            }
            _ if self.to.is_some() => {
                return invalid("`--to` only applies to `--query path`");
            }
            _ => {}
        }
        if self.direction.is_some() && self.query != GraphQueryKind::Neighborhood {
            return invalid("`--direction` only applies to `--query neighborhood`");
        }
        Ok(())
    }

    /// Effective traversal direction, also reported in the output.
    fn effective_direction(&self) -> GraphDirection {
        match self.query {
            GraphQueryKind::Callers => GraphDirection::In,
            GraphQueryKind::Callees | GraphQueryKind::Path => GraphDirection::Out,
            GraphQueryKind::Neighborhood => self.direction.unwrap_or(GraphDirection::Both),
        }
    }

    /// Effective depth cap: `None` means an unbounded `path` search.
    fn effective_depth(&self) -> Option<usize> {
        match self.query {
            GraphQueryKind::Path => self.depth,
            _ => Some(self.depth.unwrap_or(DEFAULT_GRAPH_QUERY_DEPTH)),
        }
    }
}

/// How the query terminated. Anything but `ok` carries no traversal
/// results; `symbol_ambiguous` / `to_ambiguous` list the candidates so
/// the caller can re-run with a longer suffix or an exact node id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum QueryStatus {
    Ok,
    SymbolNotFound,
    SymbolAmbiguous,
    ToNotFound,
    ToAmbiguous,
    NoPath,
}

/// One function, as cited in seeds, candidate lists, and result rows.
#[derive(Debug, Clone, Serialize)]
struct NodeRow {
    id: String,
    qualified_name: String,
    file: String,
    line: usize,
    end_line: usize,
    module: String,
    is_test: bool,
    /// Outgoing call sites the resolver could not attribute — the
    /// per-node honesty signal: high counts mean this node's edges are
    /// undercounted.
    unresolved_outgoing_call_count: usize,
    ambiguous_outgoing_call_count: usize,
}

impl NodeRow {
    fn from_node(node: &CallGraphNode) -> Self {
        Self {
            id: node.id.clone(),
            qualified_name: node.qualified_name.clone(),
            file: node.file.clone(),
            line: node.start_line,
            end_line: node.end_line,
            module: node.module.clone(),
            is_test: node.is_test,
            unresolved_outgoing_call_count: node.outgoing_calls.unresolved_call_count,
            ambiguous_outgoing_call_count: node.outgoing_calls.ambiguous_call_count,
        }
    }
}

/// One node reached by a traversal verb.
#[derive(Debug, Clone, Serialize)]
struct ResultRow {
    #[serde(flatten)]
    node: NodeRow,
    /// Minimum edge distance from the seed.
    depth: usize,
    /// How the node was reached, present only for `neighborhood
    /// --direction both`: `in` (a caller), `out` (a callee), or `both`.
    #[serde(skip_serializing_if = "Option::is_none")]
    direction: Option<GraphDirection>,
}

/// One hop of a `path` witness chain.
#[derive(Debug, Clone, Serialize)]
struct PathStep {
    #[serde(flatten)]
    node: NodeRow,
    /// Source lines in the previous step's body where this step is
    /// called. Absent on the first step.
    #[serde(skip_serializing_if = "Option::is_none")]
    call_lines: Option<Vec<usize>>,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    query: GraphQueryKind,
    symbol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    /// Effective depth cap. Absent for an unbounded `path` search.
    #[serde(skip_serializing_if = "Option::is_none")]
    depth: Option<usize>,
    direction: GraphDirection,
    node_limit: usize,
    status: QueryStatus,
    /// Candidate matches for whichever endpoint failed to resolve
    /// uniquely (`status` says which), capped at `node_limit`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    candidates: Vec<NodeRow>,
    /// Total candidate count behind the (possibly capped) list above.
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<NodeRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<NodeRow>,
    /// Traversal rows in (depth, file, line) order, capped at
    /// `node_limit`. Empty for `path` queries.
    results: Vec<ResultRow>,
    /// Witness chain for `path` queries, seed to target inclusive.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    path: Vec<PathStep>,
    /// Distinct functions within the depth cap (seed excluded), before
    /// the node-count cap. Absent for `path` queries.
    #[serde(skip_serializing_if = "Option::is_none")]
    reachable_count: Option<usize>,
    truncated: bool,
}

impl Report {
    fn build(root: &Path, graph: &CallGraph, spec: &GraphQueryAnalyzer) -> Self {
        let mut report = Self {
            schema_version: SCHEMA_VERSION,
            root: root.display().to_string(),
            language: graph.language,
            query: spec.query,
            symbol: spec.symbol.clone(),
            to: spec.to.clone(),
            depth: spec.effective_depth(),
            direction: spec.effective_direction(),
            node_limit: spec.limit.unwrap_or(DEFAULT_GRAPH_QUERY_LIMIT),
            status: QueryStatus::Ok,
            candidates: Vec::new(),
            candidate_count: None,
            seed: None,
            target: None,
            results: Vec::new(),
            path: Vec::new(),
            reachable_count: None,
            truncated: false,
        };

        let Some(seed) = report.resolve_endpoint(
            graph,
            &spec.symbol,
            QueryStatus::SymbolNotFound,
            QueryStatus::SymbolAmbiguous,
        ) else {
            return report;
        };
        report.seed = Some(NodeRow::from_node(&graph.nodes[seed]));

        let adjacency = graph.resolved_adjacency();
        match spec.query {
            GraphQueryKind::Callers => {
                report.fill_traversal(graph, algo::reverse_bfs(&adjacency, &[seed]), None);
            }
            GraphQueryKind::Callees => {
                report.fill_traversal(graph, algo::bfs(&adjacency, &[seed]), None);
            }
            GraphQueryKind::Neighborhood => match report.direction {
                GraphDirection::In => {
                    report.fill_traversal(graph, algo::reverse_bfs(&adjacency, &[seed]), None);
                }
                GraphDirection::Out => {
                    report.fill_traversal(graph, algo::bfs(&adjacency, &[seed]), None);
                }
                GraphDirection::Both => report.fill_neighborhood(graph, &adjacency, seed),
            },
            GraphQueryKind::Path => {
                // `validate` guarantees `--to` is present for `path`.
                let to_symbol = spec.to.as_deref().unwrap_or_default();
                let Some(target) = report.resolve_endpoint(
                    graph,
                    to_symbol,
                    QueryStatus::ToNotFound,
                    QueryStatus::ToAmbiguous,
                ) else {
                    return report;
                };
                report.target = Some(NodeRow::from_node(&graph.nodes[target]));
                report.fill_path(graph, &adjacency, seed, target);
            }
        }
        report
    }

    /// Match one endpoint symbol against the graph. On a unique match
    /// returns its node index; otherwise records the failure status
    /// (and the capped candidate list when ambiguous) and returns
    /// `None`.
    fn resolve_endpoint(
        &mut self,
        graph: &CallGraph,
        symbol: &str,
        on_missing: QueryStatus,
        on_ambiguous: QueryStatus,
    ) -> Option<usize> {
        let matches = match_symbol(graph, symbol);
        match matches.as_slice() {
            [] => {
                self.status = on_missing;
                None
            }
            [unique] => Some(*unique),
            _ => {
                self.status = on_ambiguous;
                self.candidate_count = Some(matches.len());
                self.candidates = matches
                    .iter()
                    .take(self.node_limit)
                    .map(|&idx| NodeRow::from_node(&graph.nodes[idx]))
                    .collect();
                self.truncated = matches.len() > self.node_limit;
                None
            }
        }
    }

    /// Fold BFS visits into result rows: drop the seed (depth 0), keep
    /// visits within the depth cap, then cap by node count.
    fn fill_traversal(
        &mut self,
        graph: &CallGraph,
        visits: Vec<BfsVisit>,
        direction_of: Option<&BTreeMap<usize, GraphDirection>>,
    ) {
        let depth_cap = self.depth.unwrap_or(usize::MAX);
        let within: Vec<&BfsVisit> = visits
            .iter()
            .filter(|v| v.depth > 0 && v.depth <= depth_cap)
            .collect();
        self.reachable_count = Some(within.len());
        self.truncated = within.len() > self.node_limit;
        self.results = within
            .into_iter()
            .take(self.node_limit)
            .map(|visit| ResultRow {
                node: NodeRow::from_node(&graph.nodes[visit.node]),
                depth: visit.depth,
                direction: direction_of.map(|dirs| dirs[&visit.node]),
            })
            .collect();
    }

    /// Merge forward and reverse BFS into one ego graph: per node the
    /// minimum depth over both directions, tagged `in` / `out` /
    /// `both`.
    fn fill_neighborhood(&mut self, graph: &CallGraph, adjacency: &[Vec<usize>], seed: usize) {
        let depth_cap = self.depth.unwrap_or(usize::MAX);
        let mut merged: BTreeMap<usize, (usize, bool, bool)> = BTreeMap::new();
        let reached = [
            (algo::bfs(adjacency, &[seed]), false),
            (algo::reverse_bfs(adjacency, &[seed]), true),
        ];
        for (visits, inward) in reached {
            for visit in visits {
                if visit.depth == 0 || visit.depth > depth_cap {
                    continue;
                }
                let entry = merged
                    .entry(visit.node)
                    .or_insert((visit.depth, false, false));
                entry.0 = entry.0.min(visit.depth);
                if inward {
                    entry.1 = true;
                } else {
                    entry.2 = true;
                }
            }
        }
        let mut visits: Vec<BfsVisit> = merged
            .iter()
            .map(|(&node, &(depth, _, _))| BfsVisit { node, depth })
            .collect();
        visits.sort_by(|a, b| a.depth.cmp(&b.depth).then_with(|| a.node.cmp(&b.node)));
        let directions: BTreeMap<usize, GraphDirection> = merged
            .into_iter()
            .map(|(node, (_, inward, outward))| {
                let direction = match (inward, outward) {
                    (true, true) => GraphDirection::Both,
                    (true, false) => GraphDirection::In,
                    _ => GraphDirection::Out,
                };
                (node, direction)
            })
            .collect();
        self.fill_traversal(graph, visits, Some(&directions));
    }

    /// Shortest witness chain with per-hop call-line evidence. The
    /// chain is never truncated by `node_limit`: a partial chain is not
    /// a witness.
    fn fill_path(
        &mut self,
        graph: &CallGraph,
        adjacency: &[Vec<usize>],
        seed: usize,
        target: usize,
    ) {
        let Some(chain) = algo::shortest_path(adjacency, seed, target, self.depth) else {
            self.status = QueryStatus::NoPath;
            return;
        };
        self.path = chain
            .iter()
            .enumerate()
            .map(|(step, &node)| PathStep {
                node: NodeRow::from_node(&graph.nodes[node]),
                call_lines: (step > 0).then(|| {
                    resolved_call_lines(
                        graph,
                        &graph.nodes[chain[step - 1]].id,
                        &graph.nodes[node].id,
                    )
                }),
            })
            .collect();
    }
}

/// Call-site lines of every resolved edge `from_id -> to_id`, merged
/// across grouped edges (the same target can be reached under different
/// callee spellings).
fn resolved_call_lines(graph: &CallGraph, from_id: &str, to_id: &str) -> Vec<usize> {
    let mut lines: Vec<usize> = graph
        .edges
        .iter()
        .filter(|edge| {
            edge.resolution == Resolution::Resolved
                && edge.from.as_deref() == Some(from_id)
                && edge.to.as_deref() == Some(to_id)
        })
        .flat_map(|edge| edge.call_lines.iter().copied())
        .collect();
    lines.sort_unstable();
    lines.dedup();
    lines
}

fn format_markdown(report: &Report) -> String {
    let subject = report
        .seed
        .as_ref()
        .map_or(report.symbol.as_str(), |seed| seed.qualified_name.as_str());
    let mut out = match report.query {
        GraphQueryKind::Path => {
            let to = report.target.as_ref().map_or_else(
                || report.to.clone().unwrap_or_default(),
                |t| t.qualified_name.clone(),
            );
            format!("# Graph query: path from `{subject}` to `{to}`")
        }
        verb => format!("# Graph query: {} of `{subject}`", verb.as_str()),
    };
    out.push('\n');

    match report.status {
        QueryStatus::Ok => {}
        QueryStatus::SymbolNotFound | QueryStatus::ToNotFound => {
            let missing = if report.status == QueryStatus::SymbolNotFound {
                &report.symbol
            } else {
                report.to.as_deref().unwrap_or_default()
            };
            let _ = writeln!(
                out,
                "\n_No function matches `{missing}` (matching is by `::`-segment suffix \
                 on the qualified name, or an exact node id)._",
            );
            return out;
        }
        QueryStatus::SymbolAmbiguous | QueryStatus::ToAmbiguous => {
            render_candidates(&mut out, report);
            return out;
        }
        QueryStatus::NoPath => {
            let scope = report
                .depth
                .map_or_else(|| "any depth".to_owned(), |cap| format!("depth <= {cap}"));
            let _ = writeln!(
                out,
                "\n_No call chain over resolved edges ({scope}). Unresolved or ambiguous \
                 call sites may hide one; see the per-node counts on the endpoints._",
            );
            render_endpoint(&mut out, "Seed", report.seed.as_ref());
            render_endpoint(&mut out, "Target", report.target.as_ref());
            return out;
        }
    }

    if report.query == GraphQueryKind::Path {
        render_path(&mut out, report);
        return out;
    }
    render_traversal(&mut out, report);
    out
}

fn render_candidates(out: &mut String, report: &Report) {
    let symbol = if report.status == QueryStatus::SymbolAmbiguous {
        &report.symbol
    } else {
        report.to.as_deref().unwrap_or_default()
    };
    let total = report.candidate_count.unwrap_or(report.candidates.len());
    let shown = report.candidates.len();
    let cap_note = if report.truncated {
        format!(" (showing first {shown})")
    } else {
        String::new()
    };
    let _ = writeln!(
        out,
        "\n`{symbol}` matches {total} function(s){cap_note}. Not guessing — re-run \
         with a longer qualified-name suffix or an exact node id:\n",
    );
    for row in &report.candidates {
        let _ = writeln!(
            out,
            "- `{}` ({}:{}) id `{}`",
            row.qualified_name, row.file, row.line, row.id,
        );
    }
}

fn render_endpoint(out: &mut String, label: &str, row: Option<&NodeRow>) {
    let Some(row) = row else { return };
    let _ = writeln!(
        out,
        "\n{label}: `{}` ({}:{}-{}, module `{}`, unresolved out {}, ambiguous out {})",
        row.qualified_name,
        row.file,
        row.line,
        row.end_line,
        row.module,
        row.unresolved_outgoing_call_count,
        row.ambiguous_outgoing_call_count,
    );
}

fn render_traversal(out: &mut String, report: &Report) {
    let reachable = report.reachable_count.unwrap_or(0);
    let depth_note = report
        .depth
        .map_or_else(String::new, |d| format!("depth <= {d}, "));
    let direction_note = match report.query {
        GraphQueryKind::Neighborhood => format!("direction {}, ", report.direction.as_str()),
        _ => String::new(),
    };
    let cap_note = if report.truncated {
        format!(
            ", showing first {} (raise --limit for more)",
            report.results.len()
        )
    } else {
        String::new()
    };
    let _ = writeln!(
        out,
        "\n{direction_note}{depth_note}{reachable} function(s) reached over resolved \
         call edges{cap_note}. Results are lower bounds: a node's `unres`/`ambig` \
         counts say how many of its outgoing call sites the resolver could not follow.",
    );
    render_endpoint(out, "Seed", report.seed.as_ref());
    if report.results.is_empty() {
        out.push_str("\n_No results._\n");
        return;
    }
    out.push('\n');
    let detail = report.results.len() <= DETAIL_THRESHOLD;
    for row in &report.results {
        let tag = row
            .direction
            .map_or_else(String::new, |d| format!(" {}", d.as_str()));
        if detail {
            let _ = writeln!(
                out,
                "- `{}` ({}:{}-{}, module `{}`, depth {}{}{}, unres {}, ambig {})",
                row.node.qualified_name,
                row.node.file,
                row.node.line,
                row.node.end_line,
                row.node.module,
                row.depth,
                tag,
                if row.node.is_test { ", test" } else { "" },
                row.node.unresolved_outgoing_call_count,
                row.node.ambiguous_outgoing_call_count,
            );
        } else {
            let _ = writeln!(
                out,
                "- d{}{} `{}`{} unres={} ambig={}",
                row.depth,
                tag,
                row.node.id,
                if row.node.is_test { " test" } else { "" },
                row.node.unresolved_outgoing_call_count,
                row.node.ambiguous_outgoing_call_count,
            );
        }
    }
}

fn render_path(out: &mut String, report: &Report) {
    let hops = report.path.len().saturating_sub(1);
    let _ = writeln!(
        out,
        "\nShortest chain over resolved call edges: {hops} hop(s). One witness among \
         possibly many; unresolved call sites may hide shorter chains.\n",
    );
    for (step, hop) in report.path.iter().enumerate() {
        let evidence = hop.call_lines.as_ref().map_or_else(String::new, |lines| {
            format!(
                " — called from step {} at line(s) {}",
                step,
                lines
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
            )
        });
        let _ = writeln!(
            out,
            "{}. `{}` ({}:{}){}",
            step + 1,
            hop.node.qualified_name,
            hop.node.file,
            hop.node.line,
            evidence,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use rstest::rstest;
    use serde_json::Value;

    /// A small diamond: `top` calls two helpers, both funnel into
    /// `sink`.
    const DIAMOND: &str = "fn sink() {}\n\
                           fn helper_a() { sink(); }\n\
                           fn helper_b() { sink(); }\n\
                           fn top() { helper_a(); helper_b(); }\n";

    fn analyze_json(analyzer: &GraphQueryAnalyzer, path: &Path) -> Value {
        let json = analyzer.analyze(path, OutputFormat::Json).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn result_names(report: &Value) -> Vec<(String, u64)> {
        report["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["qualified_name"].as_str().unwrap().to_owned(),
                    row["depth"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    #[rstest]
    #[case::callers_depth_1(
        GraphQueryKind::Callers,
        "sink",
        None,
        vec![("crate::helper_a", 1), ("crate::helper_b", 1)]
    )]
    #[case::callers_depth_2(
        GraphQueryKind::Callers,
        "sink",
        Some(2),
        vec![("crate::helper_a", 1), ("crate::helper_b", 1), ("crate::top", 2)]
    )]
    #[case::callees_depth_1(
        GraphQueryKind::Callees,
        "top",
        None,
        vec![("crate::helper_a", 1), ("crate::helper_b", 1)]
    )]
    #[case::callees_depth_2(
        GraphQueryKind::Callees,
        "top",
        Some(2),
        vec![("crate::helper_a", 1), ("crate::helper_b", 1), ("crate::sink", 2)]
    )]
    fn traversal_verbs_walk_expected_nodes(
        #[case] query: GraphQueryKind,
        #[case] symbol: &str,
        #[case] depth: Option<usize>,
        #[case] expected: Vec<(&str, u64)>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let report = analyze_json(
            &GraphQueryAnalyzer::new(query, symbol).with_depth(depth),
            dir.path(),
        );
        assert_eq!(report["status"], "ok");
        let expected: Vec<(String, u64)> = expected
            .into_iter()
            .map(|(name, depth)| (name.to_owned(), depth))
            .collect();
        assert_eq!(result_names(&report), expected);
        assert_eq!(report["reachable_count"], expected.len());
        assert_eq!(report["truncated"], false);
    }

    #[test]
    fn report_carries_query_echo_and_seed_metadata() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, "sink"),
            dir.path(),
        );
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["language"], "rust");
        assert_eq!(report["query"], "callers");
        assert_eq!(report["symbol"], "sink");
        assert_eq!(report["depth"], 1);
        assert_eq!(report["direction"], "in");
        assert_eq!(report["node_limit"], DEFAULT_GRAPH_QUERY_LIMIT);
        let seed = &report["seed"];
        assert_eq!(seed["qualified_name"], "crate::sink");
        assert_eq!(seed["id"], "src/lib.rs:sink:1");
        assert_eq!(seed["file"], "src/lib.rs");
        assert_eq!(seed["line"], 1);
        assert_eq!(seed["module"], "crate");
    }

    #[test]
    fn neighborhood_merges_both_directions_with_tags() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Neighborhood, "helper_a"),
            dir.path(),
        );
        assert_eq!(report["status"], "ok");
        assert_eq!(report["direction"], "both");
        let rows: Vec<(&str, u64, &str)> = report["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["qualified_name"].as_str().unwrap(),
                    row["depth"].as_u64().unwrap(),
                    row["direction"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [("crate::sink", 1, "out"), ("crate::top", 1, "in")],
            "got {rows:?}"
        );
    }

    #[test]
    fn neighborhood_direction_in_matches_callers_and_drops_row_tags() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Neighborhood, "sink")
                .with_direction(Some(GraphDirection::In)),
            dir.path(),
        );
        assert_eq!(report["direction"], "in");
        assert_eq!(
            result_names(&report),
            [
                ("crate::helper_a".to_owned(), 1),
                ("crate::helper_b".to_owned(), 1)
            ],
        );
        assert_eq!(
            report["results"][0].get("direction"),
            None,
            "single-direction rows carry no redundant tag"
        );
    }

    #[test]
    fn neighborhood_node_reached_both_ways_is_tagged_both() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn ping() { pong(); }\nfn pong() { ping(); }\n",
        );

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Neighborhood, "ping"),
            dir.path(),
        );
        let rows = report["results"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["qualified_name"], "crate::pong");
        assert_eq!(rows[0]["direction"], "both");
    }

    #[test]
    fn path_reports_shortest_witness_chain_with_call_lines() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Path, "top").with_to(Some("sink".to_owned())),
            dir.path(),
        );
        assert_eq!(report["status"], "ok");
        assert_eq!(report["direction"], "out");
        assert_eq!(report.get("depth"), None, "unbounded search reports no cap");
        assert_eq!(report["target"]["qualified_name"], "crate::sink");
        let chain: Vec<(&str, Option<Vec<u64>>)> = report["path"]
            .as_array()
            .unwrap()
            .iter()
            .map(|step| {
                (
                    step["qualified_name"].as_str().unwrap(),
                    step.get("call_lines").map(|lines| {
                        lines
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|l| l.as_u64().unwrap())
                            .collect()
                    }),
                )
            })
            .collect();
        assert_eq!(
            chain,
            [
                ("crate::top", None),
                ("crate::helper_a", Some(vec![4])),
                ("crate::sink", Some(vec![2])),
            ],
        );
    }

    #[test]
    fn path_against_call_direction_reports_no_path() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Path, "sink").with_to(Some("top".to_owned())),
            dir.path(),
        );
        assert_eq!(report["status"], "no_path");
        assert_eq!(report["path"].as_array(), None);
        assert_eq!(report["seed"]["qualified_name"], "crate::sink");
        assert_eq!(report["target"]["qualified_name"], "crate::top");
    }

    #[test]
    fn path_depth_cap_bounds_the_search() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let capped = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Path, "top")
                .with_to(Some("sink".to_owned()))
                .with_depth(Some(1)),
            dir.path(),
        );
        assert_eq!(capped["status"], "no_path");
        assert_eq!(capped["depth"], 1);
    }

    #[test]
    fn ambiguous_symbol_lists_candidates_instead_of_guessing() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn dup() {} }\nmod b { pub fn dup() {} }\nfn caller() { a::dup(); }\n",
        );

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, "dup"),
            dir.path(),
        );
        assert_eq!(report["status"], "symbol_ambiguous");
        assert_eq!(report["candidate_count"], 2);
        let candidates: Vec<&str> = report["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["qualified_name"].as_str().unwrap())
            .collect();
        assert_eq!(candidates, ["crate::a::dup", "crate::b::dup"]);
        assert_eq!(report.get("seed"), None);
        assert_eq!(report["results"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn longer_suffix_and_exact_node_id_disambiguate() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn dup() {} }\nmod b { pub fn dup() {} }\nfn caller() { a::dup(); }\n",
        );

        let by_suffix = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, "a::dup"),
            dir.path(),
        );
        assert_eq!(by_suffix["status"], "ok");
        assert_eq!(by_suffix["seed"]["qualified_name"], "crate::a::dup");

        let by_id = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, "src/lib.rs:dup:2"),
            dir.path(),
        );
        assert_eq!(by_id["status"], "ok");
        assert_eq!(by_id["seed"]["qualified_name"], "crate::b::dup");
    }

    #[rstest]
    #[case::no_such_name("nope")]
    #[case::suffix_must_respect_segment_boundaries("ink")]
    fn unmatched_symbol_reports_not_found(#[case] symbol: &str) {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, symbol),
            dir.path(),
        );
        assert_eq!(report["status"], "symbol_not_found");
        assert_eq!(report.get("seed"), None);
    }

    #[test]
    fn unmatched_to_symbol_reports_to_not_found() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Path, "top").with_to(Some("nope".to_owned())),
            dir.path(),
        );
        assert_eq!(report["status"], "to_not_found");
        assert_eq!(report["seed"]["qualified_name"], "crate::top");
    }

    #[test]
    fn method_symbols_match_owner_qualified_suffix() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub struct S;\n\
             impl S { pub fn method(&self) {} }\n\
             fn caller() { S.method(); }\n",
        );

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, "S::method"),
            dir.path(),
        );
        assert_eq!(report["status"], "ok");
        assert_eq!(report["seed"]["qualified_name"], "crate::S::method");
    }

    #[test]
    fn neighborhood_excludes_seed_and_respects_depth_cap() {
        // A five-function chain: w -> x -> y -> z -> q. The seed (x)
        // must not appear in its own ego graph, and depth 2 must admit
        // z but not q.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn q() {}\nfn z() { q(); }\nfn y() { z(); }\nfn x() { y(); }\nfn w() { x(); }\n",
        );

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Neighborhood, "x").with_depth(Some(2)),
            dir.path(),
        );
        let rows: Vec<(&str, u64, &str)> = report["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["qualified_name"].as_str().unwrap(),
                    row["depth"].as_u64().unwrap(),
                    row["direction"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("crate::y", 1, "out"),
                ("crate::w", 1, "in"),
                ("crate::z", 2, "out"),
            ],
            "got {rows:?}"
        );
    }

    #[test]
    fn neighborhood_direction_tags_ignore_beyond_cap_reachability() {
        // Cycle x -> y -> a -> x. Within depth 1, y is only x's callee
        // and a only its caller; the depth-2 paths around the cycle
        // (y reaches x via a, a is reached via y) must not smear the
        // tags into `both`.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn x() { y(); }\nfn y() { a(); }\nfn a() { x(); }\n",
        );

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Neighborhood, "x"),
            dir.path(),
        );
        let rows: Vec<(&str, u64, &str)> = report["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["qualified_name"].as_str().unwrap(),
                    row["depth"].as_u64().unwrap(),
                    row["direction"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            [("crate::y", 1, "out"), ("crate::a", 1, "in")],
            "got {rows:?}"
        );
    }

    #[rstest]
    #[case::limit_below_matches(1, true, 1)]
    #[case::limit_equal_to_matches(2, false, 2)]
    fn ambiguous_candidate_list_truncates_only_beyond_limit(
        #[case] limit: usize,
        #[case] truncated: bool,
        #[case] listed: usize,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn dup() {} }\nmod b { pub fn dup() {} }\n",
        );

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, "dup").with_limit(Some(limit)),
            dir.path(),
        );
        assert_eq!(report["status"], "symbol_ambiguous");
        assert_eq!(report["candidate_count"], 2);
        assert_eq!(report["truncated"], truncated);
        assert_eq!(report["candidates"].as_array().unwrap().len(), listed);
    }

    #[test]
    fn limit_equal_to_reachable_count_is_not_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::from("fn sink() {}\n");
        for i in 0..5 {
            src.push_str(&format!("fn caller_{i}() {{ sink(); }}\n"));
        }
        write_file(dir.path(), "src/lib.rs", &src);

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, "sink").with_limit(Some(5)),
            dir.path(),
        );
        assert_eq!(report["reachable_count"], 5);
        assert_eq!(report["truncated"], false);
        assert_eq!(report["results"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn node_limit_caps_results_and_reports_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::from("fn sink() {}\n");
        for i in 0..5 {
            src.push_str(&format!("fn caller_{i}() {{ sink(); }}\n"));
        }
        write_file(dir.path(), "src/lib.rs", &src);

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, "sink").with_limit(Some(2)),
            dir.path(),
        );
        assert_eq!(report["node_limit"], 2);
        assert_eq!(report["reachable_count"], 5);
        assert_eq!(report["truncated"], true);
        assert_eq!(report["results"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn rows_carry_per_node_unresolved_and_ambiguous_counts() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod x { pub fn same() {} }\nmod y { pub fn same() {} }\n\
             fn sink() {}\n\
             fn caller() { sink(); external(); same(); }\n",
        );

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, "sink"),
            dir.path(),
        );
        let row = &report["results"][0];
        assert_eq!(row["qualified_name"], "crate::caller");
        assert_eq!(row["unresolved_outgoing_call_count"], 1);
        assert_eq!(row["ambiguous_outgoing_call_count"], 1);
    }

    #[test]
    fn test_callers_are_flagged_and_excludable() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn helper() {}\n\
             fn prod_caller() { helper(); }\n\
             #[cfg(test)]\nmod tests { fn t() { crate::helper(); } }\n",
        );

        let all = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, "helper"),
            dir.path(),
        );
        let flags: Vec<(&str, bool)> = all["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["qualified_name"].as_str().unwrap(),
                    row["is_test"].as_bool().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            flags,
            [("crate::prod_caller", false), ("crate::tests::t", true)],
        );

        let excluded = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, "helper").with_exclude_tests(true),
            dir.path(),
        );
        assert_eq!(
            result_names(&excluded),
            [("crate::prod_caller".to_owned(), 1)],
        );
    }

    #[rstest]
    #[case::path_without_to(GraphQueryKind::Path, None, None)]
    #[case::to_without_path(GraphQueryKind::Callers, Some("sink"), None)]
    #[case::direction_without_neighborhood(
        GraphQueryKind::Callees,
        None,
        Some(GraphDirection::Both)
    )]
    fn invalid_flag_combinations_are_rejected(
        #[case] query: GraphQueryKind,
        #[case] to: Option<&str>,
        #[case] direction: Option<GraphDirection>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let err = GraphQueryAnalyzer::new(query, "top")
            .with_to(to.map(str::to_owned))
            .with_direction(direction)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(
            matches!(err, AnalyzerError::InvalidQuery { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn analysis_is_deterministic_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let analyzer =
            GraphQueryAnalyzer::new(GraphQueryKind::Neighborhood, "helper_a").with_depth(Some(3));
        let a = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        let b = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn markdown_small_result_sets_render_span_and_module_detail() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let md = GraphQueryAnalyzer::new(GraphQueryKind::Callers, "sink")
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("# Graph query: callers of `crate::sink`"),
            "got: {md}"
        );
        assert!(md.contains("lower bounds"), "got: {md}");
        assert!(md.contains("Seed: `crate::sink`"), "got: {md}");
        assert!(
            md.contains("- `crate::helper_a` (src/lib.rs:2-2, module `crate`, depth 1"),
            "got: {md}",
        );
    }

    #[test]
    fn markdown_large_result_sets_fold_to_compact_id_rows() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::from("fn sink() {}\n");
        for i in 0..5 {
            src.push_str(&format!("fn caller_{i}() {{ sink(); }}\n"));
        }
        write_file(dir.path(), "src/lib.rs", &src);

        let md = GraphQueryAnalyzer::new(GraphQueryKind::Callers, "sink")
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("- d1 `src/lib.rs:caller_0:2` unres=0 ambig=0"),
            "got: {md}",
        );
        assert!(!md.contains("module `crate`, depth"), "got: {md}");
    }

    #[test]
    fn markdown_truncation_says_how_to_widen() {
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::from("fn sink() {}\n");
        for i in 0..5 {
            src.push_str(&format!("fn caller_{i}() {{ sink(); }}\n"));
        }
        write_file(dir.path(), "src/lib.rs", &src);

        let md = GraphQueryAnalyzer::new(GraphQueryKind::Callers, "sink")
            .with_limit(Some(2))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("showing first 2"), "got: {md}");
        assert!(md.contains("raise --limit"), "got: {md}");
    }

    #[test]
    fn markdown_ambiguity_lists_ids_for_reruns() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn dup() {} }\nmod b { pub fn dup() {} }\n",
        );

        let md = GraphQueryAnalyzer::new(GraphQueryKind::Callers, "dup")
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("`dup` matches 2 function(s)"), "got: {md}");
        assert!(md.contains("Not guessing"), "got: {md}");
        assert!(md.contains("id `src/lib.rs:dup:1`"), "got: {md}");
        assert!(md.contains("id `src/lib.rs:dup:2`"), "got: {md}");
    }

    #[test]
    fn markdown_neighborhood_names_direction_in_header_and_rows() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let md = GraphQueryAnalyzer::new(GraphQueryKind::Neighborhood, "helper_a")
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("direction both, "), "got: {md}");
        assert!(md.contains("depth 1 out,"), "got: {md}");
        assert!(md.contains("depth 1 in,"), "got: {md}");
    }

    #[test]
    fn markdown_path_renders_numbered_chain_with_evidence() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let md = GraphQueryAnalyzer::new(GraphQueryKind::Path, "top")
            .with_to(Some("sink".to_owned()))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("# Graph query: path from `crate::top` to `crate::sink`"),
            "got: {md}",
        );
        assert!(md.contains("2 hop(s)"), "got: {md}");
        assert!(md.contains("1. `crate::top` (src/lib.rs:4)"), "got: {md}");
        assert!(
            md.contains("2. `crate::helper_a` (src/lib.rs:2) — called from step 1 at line(s) 4"),
            "got: {md}",
        );
        assert!(
            md.contains("3. `crate::sink` (src/lib.rs:1) — called from step 2 at line(s) 2"),
            "got: {md}",
        );
    }

    #[test]
    fn markdown_not_found_explains_matching_rules() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let md = GraphQueryAnalyzer::new(GraphQueryKind::Callers, "nope")
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("No function matches `nope`"), "got: {md}");
        assert!(md.contains("segment suffix"), "got: {md}");
    }

    #[test]
    fn markdown_no_path_cites_endpoint_confidence() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", DIAMOND);

        let md = GraphQueryAnalyzer::new(GraphQueryKind::Path, "sink")
            .with_to(Some("top".to_owned()))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("_No call chain over resolved edges"),
            "got: {md}"
        );
        assert!(md.contains("Seed: `crate::sink`"), "got: {md}");
        assert!(md.contains("Target: `crate::top`"), "got: {md}");
    }

    #[test]
    fn typescript_graphs_are_queryable() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.ts",
            "import { helper } from './b';\nexport function entry() { helper(); }\n",
        );
        write_file(dir.path(), "b.ts", "export function helper() {}\n");

        let report = analyze_json(
            &GraphQueryAnalyzer::new(GraphQueryKind::Callers, "helper"),
            dir.path(),
        );
        assert_eq!(report["language"], "typescript");
        assert_eq!(report["status"], "ok");
        let rows = result_names(&report);
        assert_eq!(rows.len(), 1, "got {rows:?}");
        assert!(rows[0].0.ends_with("entry"), "got {rows:?}");
    }
}
