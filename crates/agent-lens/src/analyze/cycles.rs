//! `analyze cycles` — function-level SCC tangles with cheapest-cut
//! suggestions.
//!
//! Runs the iterative Tarjan condensation from
//! [`super::call_graph::algo`] over the **resolved** edges of the
//! shared call graph and reports every strongly connected component
//! with two or more members: functions that call each other in a
//! cycle, directly or transitively, and therefore must be understood,
//! tested, and changed as one unit. Module-level cycles live in
//! `analyze coupling`; this view catches recursion knots and
//! cross-file entanglement the module rollup hides.
//!
//! Per tangle the report names the *cheapest* internal edges (by
//! static call-site count, greedy Eades–Lin–Smyth feedback-arc
//! heuristic) whose removal would break the cycle, with the call lines
//! as evidence. The suggestions are advisory: a cheap edge can still be
//! load-bearing in the design.
//!
//! Only resolved edges enter the cycle detection, so ambiguous call
//! sites can hide real members (or, once resolved, dissolve a tangle).
//! Each SCC therefore carries the count of ambiguous edges touching it
//! as a confidence warning, and same-file tangles — likely intentional
//! mutual recursion (parsers, tree walkers) — are ranked below
//! cross-file ones.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use super::call_graph::algo::{WeightedEdge, condense, greedy_feedback_arcs};
use super::call_graph::model::Resolution;
use super::call_graph::{CallGraph, CallGraphBuilder};
use super::{AnalyzerError, OutputFormat};

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default, Clone)]
pub struct CyclesAnalyzer {
    builder: CallGraphBuilder,
}

impl CyclesAnalyzer {
    pub fn new() -> Self {
        Self {
            builder: CallGraphBuilder::new(),
        }
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
        let graph = self.builder.build(path)?;
        let report = Report::build(path, &graph);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).map_err(AnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&report)),
        }
    }
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    sccs: Vec<SccView>,
    summary: Summary,
}

/// One strongly connected component of two or more functions.
#[derive(Debug, Serialize)]
struct SccView {
    size: usize,
    /// Number of distinct source files the members span.
    files: usize,
    /// All members live in one file — likely intentional mutual
    /// recursion (parsers, tree walkers); ranked below cross-file
    /// tangles.
    same_file: bool,
    /// Call sites on resolved edges between distinct members
    /// (self-recursion excluded).
    internal_call_sites: usize,
    /// Ambiguous edges whose caller or candidate targets touch this
    /// SCC. Each could add members (or internal edges) if resolved, so
    /// a high count means the tangle's true extent is uncertain.
    ambiguous_edge_count_nearby: usize,
    members: Vec<MemberView>,
    /// Advisory cheapest-cut edges, cheapest first: removing them all
    /// would break the cycle. Weighted by static call-site count via a
    /// greedy feedback-arc heuristic; a cheap edge can still be
    /// load-bearing.
    break_suggestions: Vec<BreakSuggestion>,
}

#[derive(Debug, Serialize)]
struct MemberView {
    id: String,
    file: String,
    line: usize,
}

#[derive(Debug, Serialize)]
struct BreakSuggestion {
    from: String,
    to: String,
    call_sites: usize,
    call_lines: Vec<usize>,
}

/// Aggregated evidence for one intra-SCC edge: call-site count plus
/// the source lines of those call sites.
type EdgeEvidence = (usize, Vec<usize>);

/// Resolved edges between distinct members of one SCC, keyed by
/// `(from, to)` node index.
type IntraEdges = BTreeMap<(usize, usize), EdgeEvidence>;

#[derive(Debug, Serialize)]
struct Summary {
    /// Number of SCCs with two or more members.
    scc_count: usize,
    /// Size of the largest reported SCC (0 when none).
    largest: usize,
    /// Distinct ambiguous edges touching any reported SCC.
    ambiguous_edge_count_nearby: usize,
}

impl Report {
    fn build(root: &Path, graph: &CallGraph) -> Self {
        let index_by_id = graph.node_index_by_id();
        let adjacency = graph.resolved_adjacency();
        let condensation = condense(&adjacency);

        // Component index -> aggregated intra-SCC resolved edges keyed
        // by (from, to) node index. Parallel edges (same endpoints
        // reached under different callee spellings) merge here.
        let mut intra: BTreeMap<usize, IntraEdges> = BTreeMap::new();
        let mut nearby: BTreeMap<usize, usize> = BTreeMap::new();
        let tangled = |component: usize| condensation.components[component].len() >= 2;
        for edge in &graph.edges {
            match edge.resolution {
                Resolution::Resolved => {
                    let (Some(from), Some(to)) = (edge.from.as_deref(), edge.to.as_deref()) else {
                        continue;
                    };
                    let (Some(&f), Some(&t)) = (index_by_id.get(from), index_by_id.get(to)) else {
                        continue;
                    };
                    let component = condensation.component_of[f];
                    if f == t || component != condensation.component_of[t] || !tangled(component) {
                        continue;
                    }
                    let (call_sites, call_lines) = intra
                        .entry(component)
                        .or_default()
                        .entry((f, t))
                        .or_insert_with(|| (0, Vec::new()));
                    *call_sites += edge.call_count;
                    call_lines.extend(&edge.call_lines);
                }
                Resolution::Ambiguous => {
                    // Count the edge once per SCC it touches — via its
                    // caller or any candidate target.
                    let mut touched: Vec<usize> = edge
                        .from
                        .as_deref()
                        .into_iter()
                        .chain(edge.candidates.iter().map(String::as_str))
                        .filter_map(|id| index_by_id.get(id))
                        .map(|&idx| condensation.component_of[idx])
                        .filter(|&component| tangled(component))
                        .collect();
                    touched.sort_unstable();
                    touched.dedup();
                    for component in touched {
                        *nearby.entry(component).or_default() += 1;
                    }
                }
                Resolution::Unresolved | Resolution::Anonymous => {}
            }
        }

        let mut sccs: Vec<SccView> = condensation
            .components
            .iter()
            .enumerate()
            .filter(|(_, members)| members.len() >= 2)
            .map(|(component, members)| {
                Self::scc_view(
                    graph,
                    members,
                    intra.remove(&component).unwrap_or_default(),
                    nearby.get(&component).copied().unwrap_or(0),
                )
            })
            .collect();
        // Same-file tangles are likely intentional mutual recursion, so
        // cross-file tangles rank first; then larger and chattier ones.
        sccs.sort_by(|a, b| {
            a.same_file
                .cmp(&b.same_file)
                .then_with(|| b.size.cmp(&a.size))
                .then_with(|| b.internal_call_sites.cmp(&a.internal_call_sites))
                .then_with(|| a.members[0].id.cmp(&b.members[0].id))
        });

        let summary = Summary {
            scc_count: sccs.len(),
            largest: sccs.iter().map(|s| s.size).max().unwrap_or(0),
            ambiguous_edge_count_nearby: nearby.values().sum(),
        };
        Self {
            schema_version: SCHEMA_VERSION,
            root: root.display().to_string(),
            language: graph.language,
            sccs,
            summary,
        }
    }

    fn scc_view(
        graph: &CallGraph,
        members: &[usize],
        intra: IntraEdges,
        ambiguous_edge_count_nearby: usize,
    ) -> SccView {
        let mut files: Vec<&str> = members
            .iter()
            .map(|&idx| graph.nodes[idx].file.as_str())
            .collect();
        files.sort_unstable();
        files.dedup();
        let file_count = files.len();

        let local_of: BTreeMap<usize, usize> = members
            .iter()
            .enumerate()
            .map(|(local, &idx)| (idx, local))
            .collect();
        let edges: Vec<(WeightedEdge, &Vec<usize>)> = intra
            .iter()
            .map(|(&(f, t), (call_sites, call_lines))| {
                (
                    WeightedEdge {
                        from: local_of[&f],
                        to: local_of[&t],
                        weight: *call_sites,
                    },
                    call_lines,
                )
            })
            .collect();
        let weighted: Vec<WeightedEdge> = edges.iter().map(|(edge, _)| *edge).collect();
        let mut break_suggestions: Vec<BreakSuggestion> =
            greedy_feedback_arcs(members.len(), &weighted)
                .into_iter()
                .map(|idx| {
                    let (edge, call_lines) = &edges[idx];
                    let mut call_lines = (*call_lines).clone();
                    call_lines.sort_unstable();
                    call_lines.dedup();
                    BreakSuggestion {
                        from: graph.nodes[members[edge.from]].id.clone(),
                        to: graph.nodes[members[edge.to]].id.clone(),
                        call_sites: edge.weight,
                        call_lines,
                    }
                })
                .collect();
        break_suggestions.sort_by(|a, b| {
            a.call_sites
                .cmp(&b.call_sites)
                .then_with(|| a.from.cmp(&b.from))
                .then_with(|| a.to.cmp(&b.to))
        });

        SccView {
            size: members.len(),
            files: file_count,
            same_file: file_count == 1,
            internal_call_sites: intra.values().map(|(call_sites, _)| call_sites).sum(),
            ambiguous_edge_count_nearby,
            // Members arrive sorted by node index, which the graph
            // orders by (file, start_line), so this listing is already
            // deterministic and readable.
            members: members
                .iter()
                .map(|&idx| {
                    let node = &graph.nodes[idx];
                    MemberView {
                        id: node.id.clone(),
                        file: node.file.clone(),
                        line: node.start_line,
                    }
                })
                .collect(),
            break_suggestions,
        }
    }
}

fn format_markdown(report: &Report) -> String {
    let mut out = format!(
        "# Function cycles: {} ({} tangle(s), largest {})\n",
        report.root, report.summary.scc_count, report.summary.largest
    );
    if report.sccs.is_empty() {
        out.push_str("\n_No function-level cycles (2+ members) over resolved call edges._\n");
        return out;
    }
    for scc in &report.sccs {
        let _ = writeln!(
            out,
            "\n## {} function(s) across {} file(s), {} internal call site(s){}",
            scc.size,
            scc.files,
            scc.internal_call_sites,
            if scc.same_file {
                " — same file (likely intentional mutual recursion)"
            } else {
                ""
            },
        );
        for member in &scc.members {
            let _ = writeln!(out, "- {}", member.id);
        }
        if scc.ambiguous_edge_count_nearby > 0 {
            let _ = writeln!(
                out,
                "- ambiguous edges nearby: {} (true extent uncertain)",
                scc.ambiguous_edge_count_nearby,
            );
        }
        if !scc.break_suggestions.is_empty() {
            let _ = writeln!(out, "\nBreak suggestions (advisory, cheapest first):");
            for s in &scc.break_suggestions {
                let _ = writeln!(
                    out,
                    "- {} → {} ({} call site(s) at line(s) {})",
                    s.from,
                    s.to,
                    s.call_sites,
                    s.call_lines
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use rstest::rstest;
    use serde_json::Value;

    fn analyze_json(path: &Path) -> Value {
        let json = CyclesAnalyzer::new()
            .analyze(path, OutputFormat::Json)
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn reports_mutual_recursion_with_members_and_break_suggestion() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn even(n: u32) -> bool { if n == 0 { true } else { odd(n - 1) } }\n\
             fn odd(n: u32) -> bool { if n == 0 { false } else { even(n - 1) } }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["language"], "rust");
        assert_eq!(report["summary"]["scc_count"], 1);
        assert_eq!(report["summary"]["largest"], 2);
        assert_eq!(report["summary"]["ambiguous_edge_count_nearby"], 0);

        let scc = &report["sccs"][0];
        assert_eq!(scc["size"], 2);
        assert_eq!(scc["files"], 1);
        assert_eq!(scc["same_file"], true);
        assert_eq!(scc["internal_call_sites"], 2);
        let members: Vec<(&str, &str, u64)> = scc["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                (
                    m["id"].as_str().unwrap(),
                    m["file"].as_str().unwrap(),
                    m["line"].as_u64().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            members,
            [
                ("src/lib.rs:even:1", "src/lib.rs", 1),
                ("src/lib.rs:odd:2", "src/lib.rs", 2),
            ],
        );

        // Cutting either edge breaks a 2-cycle; exactly one suggestion.
        let suggestions = scc["break_suggestions"].as_array().unwrap();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0]["call_sites"], 1);
        assert_eq!(suggestions[0]["from"], "src/lib.rs:odd:2");
        assert_eq!(suggestions[0]["to"], "src/lib.rs:even:1");
        assert_eq!(suggestions[0]["call_lines"], serde_json::json!([2]));
    }

    #[test]
    fn break_suggestions_prefer_the_cheapest_edge_with_line_evidence() {
        // a calls b three times; b calls a once. The cheapest cut is
        // the single b -> a call site.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn a() { b(); b(); b(); }\nfn b() { a(); }\n",
        );

        let report = analyze_json(dir.path());
        let suggestions = report["sccs"][0]["break_suggestions"].as_array().unwrap();
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0]["from"], "src/lib.rs:b:2");
        assert_eq!(suggestions[0]["to"], "src/lib.rs:a:1");
        assert_eq!(suggestions[0]["call_sites"], 1);
        assert_eq!(suggestions[0]["call_lines"], serde_json::json!([2]));
    }

    #[test]
    fn cross_file_tangles_rank_above_same_file_ones() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod parse;\nmod walk;\nfn ping() { pong(); }\nfn pong() { ping(); }\n",
        );
        write_file(
            dir.path(),
            "src/parse.rs",
            "pub fn parse() { crate::walk::walk(); }\n",
        );
        write_file(
            dir.path(),
            "src/walk.rs",
            "pub fn walk() { crate::parse::parse(); }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["summary"]["scc_count"], 2);
        let sccs = report["sccs"].as_array().unwrap();
        assert_eq!(sccs[0]["same_file"], false);
        assert_eq!(sccs[0]["files"], 2);
        assert_eq!(sccs[1]["same_file"], true);
        assert_eq!(sccs[1]["files"], 1);
    }

    #[test]
    fn ambiguous_edges_touching_a_tangle_are_counted_as_warning() {
        // `same` exists twice, so the call from inside the cycle is
        // ambiguous — it must not join the cycle, but it must be
        // counted as a nearby ambiguity.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod x { pub fn same() {} }\nmod y { pub fn same() {} }\n\
             fn a() { b(); same(); }\nfn b() { a(); }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["summary"]["scc_count"], 1);
        let scc = &report["sccs"][0];
        assert_eq!(scc["size"], 2);
        assert_eq!(scc["ambiguous_edge_count_nearby"], 1);
        assert_eq!(report["summary"]["ambiguous_edge_count_nearby"], 1);
    }

    #[rstest]
    #[case::acyclic("fn a() { b(); }\nfn b() {}\n")]
    #[case::self_recursion_only("fn a(n: u32) { if n > 0 { a(n - 1); } }\n")]
    fn cycle_free_code_reports_no_sccs(#[case] source: &str) {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", source);

        let report = analyze_json(dir.path());
        assert_eq!(report["sccs"], serde_json::json!([]));
        assert_eq!(report["summary"]["scc_count"], 0);
        assert_eq!(report["summary"]["largest"], 0);
    }

    #[test]
    fn resolved_edges_leaving_a_tangle_are_not_internal() {
        // `a` participates in the cycle and also calls `leaf`, which is
        // outside it. The outgoing edge must not inflate the internal
        // call-site count or the break-suggestion subgraph.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn a() { b(); leaf(); }\nfn b() { a(); }\nfn leaf() {}\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["summary"]["scc_count"], 1);
        let scc = &report["sccs"][0];
        assert_eq!(scc["size"], 2);
        assert_eq!(scc["internal_call_sites"], 2);
        let member_ids: Vec<&str> = scc["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        for suggestion in scc["break_suggestions"].as_array().unwrap() {
            for endpoint in ["from", "to"] {
                assert!(
                    member_ids.contains(&suggestion[endpoint].as_str().unwrap()),
                    "suggestion endpoint outside the tangle: {suggestion}",
                );
            }
        }
    }

    #[test]
    fn only_tests_restricts_cycles_to_test_functions() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn a() { b(); }\nfn b() { a(); }\n\
             #[cfg(test)]\nmod tests {\n    fn ta() { tb(); }\n    fn tb() { ta(); }\n}\n",
        );

        assert_eq!(analyze_json(dir.path())["summary"]["scc_count"], 2);

        let json = CyclesAnalyzer::new()
            .with_only_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let only: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(only["summary"]["scc_count"], 1);
        let member_ids: Vec<&str> = only["sccs"][0]["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(member_ids, ["src/lib.rs:ta:5", "src/lib.rs:tb:6"]);
    }

    #[test]
    fn exclude_patterns_drop_matching_files() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/keep.rs",
            "fn a() { b(); }\nfn b() { a(); }\n",
        );
        write_file(
            dir.path(),
            "src/generated.rs",
            "fn ga() { gb(); }\nfn gb() { ga(); }\n",
        );

        assert_eq!(analyze_json(dir.path())["summary"]["scc_count"], 2);

        let json = CyclesAnalyzer::new()
            .with_exclude_patterns(vec!["generated.rs".to_owned()])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let excluded: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(excluded["summary"]["scc_count"], 1);
        assert_eq!(excluded["sccs"][0]["members"][0]["file"], "src/keep.rs");
    }

    #[test]
    fn three_cycle_break_suggestions_break_the_whole_cycle() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn a() { b(); }\nfn b() { c(); }\nfn c() { a(); }\n",
        );

        let report = analyze_json(dir.path());
        let scc = &report["sccs"][0];
        assert_eq!(scc["size"], 3);
        assert_eq!(scc["internal_call_sites"], 3);
        // One edge suffices to break a simple 3-cycle.
        assert_eq!(scc["break_suggestions"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn self_recursion_inside_a_tangle_is_not_an_internal_call_site() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn a(n: u32) { a(n); b(); }\nfn b() { a(1); }\n",
        );

        let report = analyze_json(dir.path());
        let scc = &report["sccs"][0];
        assert_eq!(scc["size"], 2);
        assert_eq!(scc["internal_call_sites"], 2);
    }

    #[test]
    fn typescript_cycles_are_detected_across_files() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.ts",
            "import { b } from './b';\nexport function a() { b(); }\n",
        );
        write_file(
            dir.path(),
            "b.ts",
            "import { a } from './a';\nexport function b() { a(); }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["language"], "typescript");
        assert_eq!(report["summary"]["scc_count"], 1);
        assert_eq!(report["sccs"][0]["same_file"], false);
        assert_eq!(report["sccs"][0]["files"], 2);
    }

    #[test]
    fn markdown_reports_tangles_and_advisory_cuts() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn even(n: u32) -> bool { if n == 0 { true } else { odd(n - 1) } }\n\
             fn odd(n: u32) -> bool { if n == 0 { false } else { even(n - 1) } }\n",
        );

        let md = CyclesAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Function cycles:"), "got: {md}");
        assert!(md.contains("1 tangle(s), largest 2"), "got: {md}");
        assert!(md.contains("same file"), "got: {md}");
        assert!(md.contains("advisory, cheapest first"), "got: {md}");
        assert!(
            md.contains("src/lib.rs:odd:2 → src/lib.rs:even:1"),
            "got: {md}"
        );
        // No ambiguity in this fixture, so the warning line must be
        // absent rather than rendered with a zero count.
        assert!(!md.contains("ambiguous edges nearby"), "got: {md}");
    }

    #[test]
    fn markdown_reports_nearby_ambiguity_warning() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod x { pub fn same() {} }\nmod y { pub fn same() {} }\n\
             fn a() { b(); same(); }\nfn b() { a(); }\n",
        );

        let md = CyclesAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("ambiguous edges nearby: 1"), "got: {md}");
    }

    #[test]
    fn markdown_reports_cycle_free_codebases_quietly() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "fn a() {}\n");

        let md = CyclesAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("0 tangle(s), largest 0"), "got: {md}");
        assert!(md.contains("_No function-level cycles"), "got: {md}");
    }

    #[test]
    fn exclude_tests_drops_test_only_cycles() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn prod() {}\n#[cfg(test)]\nmod tests {\n    fn a() { b(); }\n    fn b() { a(); }\n}\n",
        );

        let all = analyze_json(dir.path());
        assert_eq!(all["summary"]["scc_count"], 1);

        let json = CyclesAnalyzer::new()
            .with_exclude_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let excluded: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(excluded["summary"]["scc_count"], 0);
    }
}
