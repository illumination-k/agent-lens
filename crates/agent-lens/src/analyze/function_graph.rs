//! `analyze function-graph` — emit a static function call graph.
//!
//! This report is intentionally machine-facing JSON for downstream
//! visualization tools. It is static and heuristic: no type inference,
//! macro expansion, cross-crate resolution, runtime timing, or git history
//! traversal is attempted here. Graph construction lives in
//! [`super::call_graph`]; this module is the serialization surface.
//!
//! # Schema history
//!
//! * `schema_version: 2`
//!   - nodes carry `visibility` (correct for Rust/Go, `unknown` for
//!     TypeScript/Python) and `outgoing_calls` (call-site counts by
//!     resolution — the per-node confidence signal).
//!   - ambiguous edges carry `candidates`, the sorted node-id set the
//!     resolver could not pick between (`to` stays `null`).
//!   - resolved and ambiguous edges carry `resolution_method`
//!     provenance (`lexical`, `self_method`, `last_segment`,
//!     `path_suffix`, `crate_narrowed`). Grouped call sites that
//!     reached the same target through different heuristics keep the
//!     most direct one. Ambiguous edges with different candidate sets
//!     no longer collapse into one edge.
//!   - `summary.modules` breaks the global resolution counts down per
//!     module as call-site counts (graph-confidence calibration).
//! * `schema_version: 1` — initial shape.

use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use super::call_graph::model::{CallGraphEdge, CallGraphNode, ModuleResolutionSummary, Resolution};
use super::call_graph::{CallGraph, CallGraphBuilder, delegate_call_graph_builders};
use super::runner::render_report;
use super::{AnalyzerError, OutputFormat};

const SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Default, Clone)]
pub struct FunctionGraphAnalyzer {
    builder: CallGraphBuilder,
}

impl FunctionGraphAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    delegate_call_graph_builders! {
        builder,
        only_tests,
        exclude_tests,
    }

    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let graph = self.builder.build(path)?;
        let report = Report::build(path, graph);
        render_report(&report, format, || format_markdown(&report))
    }
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    node_count: usize,
    edge_count: usize,
    nodes: Vec<CallGraphNode>,
    edges: Vec<CallGraphEdge>,
    summary: SummaryView,
}

impl Report {
    fn build(root: &Path, graph: CallGraph) -> Self {
        let summary = SummaryView::new(&graph.edges, graph.module_summary);
        Self {
            schema_version: SCHEMA_VERSION,
            root: root.display().to_string(),
            language: graph.language,
            node_count: graph.nodes.len(),
            edge_count: graph.edges.len(),
            nodes: graph.nodes,
            edges: graph.edges,
            summary,
        }
    }
}

#[derive(Debug, Serialize)]
struct SummaryView {
    resolved_edge_count: usize,
    unresolved_edge_count: usize,
    ambiguous_edge_count: usize,
    anonymous_edge_count: usize,
    total_static_call_count: usize,
    /// Per-module call-site resolution counts — the calibration layer:
    /// a module whose edges are mostly unresolved should have its
    /// graph-derived results read as lower bounds.
    modules: Vec<ModuleResolutionSummary>,
}

impl SummaryView {
    fn new(edges: &[CallGraphEdge], modules: Vec<ModuleResolutionSummary>) -> Self {
        Self {
            resolved_edge_count: edges
                .iter()
                .filter(|e| e.resolution == Resolution::Resolved)
                .count(),
            unresolved_edge_count: edges
                .iter()
                .filter(|e| e.resolution == Resolution::Unresolved)
                .count(),
            ambiguous_edge_count: edges
                .iter()
                .filter(|e| e.resolution == Resolution::Ambiguous)
                .count(),
            anonymous_edge_count: edges
                .iter()
                .filter(|e| e.resolution == Resolution::Anonymous)
                .count(),
            total_static_call_count: edges.iter().map(|e| e.call_count).sum(),
            modules,
        }
    }
}

fn format_markdown(report: &Report) -> String {
    let mut out = format!(
        "# Function graph: {} ({} node(s), {} edge(s))\n",
        report.root, report.node_count, report.edge_count
    );
    let _ = writeln!(
        out,
        "\n- resolved edges: {}\n- unresolved edges: {}\n- ambiguous edges: {}\n- anonymous edges: {}\n- static call sites: {}",
        report.summary.resolved_edge_count,
        report.summary.unresolved_edge_count,
        report.summary.ambiguous_edge_count,
        report.summary.anonymous_edge_count,
        report.summary.total_static_call_count
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use serde_json::Value;

    use rstest::rstest;

    fn analyze_json(path: &Path) -> Value {
        let json = FunctionGraphAnalyzer::new()
            .analyze(path, OutputFormat::Json)
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn target_qualified_name(report: &Value, edge: &Value) -> Option<String> {
        let target = edge["to"].as_str()?;
        report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == target)
            .and_then(|node| node["qualified_name"].as_str())
            .map(ToOwned::to_owned)
    }

    fn node_by_qualified_name<'a>(report: &'a Value, qualified_name: &str) -> &'a Value {
        report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["qualified_name"] == qualified_name)
            .unwrap()
    }

    fn edge_by_callee<'a>(report: &'a Value, callee: &str) -> &'a Value {
        report["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| edge["callee_name"] == callee)
            .unwrap()
    }

    #[test]
    fn emits_nodes_with_stable_ids_and_runtime_placeholders() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn helper() {}\nfn caller() { helper(); }\n",
        );

        let report = analyze_json(dir.path());
        let nodes = report["nodes"].as_array().unwrap();
        assert_eq!(report["schema_version"], 2);
        assert_eq!(report["language"], "rust");
        assert_eq!(nodes[0]["id"], "src/lib.rs:helper:1");
        assert_eq!(nodes[0]["name"], "helper");
        assert_eq!(nodes[0]["qualified_name"], "crate::helper");
        assert_eq!(nodes[0]["module"], "crate");
        assert_eq!(nodes[0]["impl_owner"], Value::Null);
        assert_eq!(nodes[0]["visibility"], "private");
        assert_eq!(nodes[0]["weights"]["loc"], 1);
        assert_eq!(nodes[0]["weights"]["fan_in"], 1);
        assert_eq!(nodes[0]["weights"]["fan_out"], 0);
        assert_eq!(nodes[0]["weights"]["cyclomatic_complexity"], 1);
        assert_eq!(nodes[0]["weights"]["cognitive_complexity"], 0);
        assert_eq!(nodes[0]["weights"]["max_nesting"], 0);
        assert!(nodes[0]["weights"].get("maintainability_index").is_some());
        assert!(nodes[0]["weights"].get("halstead_volume").is_some());
        assert_eq!(nodes[0]["weights"]["total_time_ms"], Value::Null);
        assert_eq!(nodes[0]["weights"]["self_time_ms"], Value::Null);
        assert_eq!(nodes[0]["weights"]["error_count"], Value::Null);
        assert_eq!(nodes[0]["outgoing_calls"]["resolved_call_count"], 0);
        assert_eq!(nodes[1]["outgoing_calls"]["resolved_call_count"], 1);
        assert_eq!(nodes[1]["outgoing_calls"]["unresolved_call_count"], 0);
    }

    #[test]
    fn includes_static_metrics_for_visualization_modes() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn branchy(n: i32) -> i32 {\n    if n > 0 {\n        return n;\n    }\n    0\n}\n",
        );

        let report = analyze_json(dir.path());
        let node = &report["nodes"].as_array().unwrap()[0];
        assert_eq!(node["weights"]["loc"], 6);
        assert_eq!(node["weights"]["cyclomatic_complexity"], 2);
        assert_eq!(node["weights"]["cognitive_complexity"], 1);
        assert_eq!(node["weights"]["max_nesting"], 1);
        assert!(node["weights"]["maintainability_index"].as_f64().is_some());
        assert!(node["weights"]["halstead_volume"].as_f64().is_some());
    }

    #[test]
    fn resolves_unique_edges_and_aggregates_repeated_calls() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn helper() {}\nfn caller() { helper(); helper(); }\n",
        );

        let report = analyze_json(dir.path());
        let edges = report["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["from"], "src/lib.rs:caller:2");
        assert_eq!(edges[0]["to"], "src/lib.rs:helper:1");
        assert_eq!(edges[0]["callee_name"], "helper");
        assert_eq!(edges[0]["resolution"], "resolved");
        assert_eq!(edges[0]["resolution_method"], "lexical");
        assert_eq!(edges[0].get("candidates"), None);
        assert_eq!(edges[0]["call_count"], 2);
        assert_eq!(edges[0]["weights"]["call_count"], 2);
        assert_eq!(edges[0]["weights"]["total_transition_time_ms"], Value::Null);

        let nodes = report["nodes"].as_array().unwrap();
        let caller = nodes.iter().find(|n| n["name"] == "caller").unwrap();
        let helper = nodes.iter().find(|n| n["name"] == "helper").unwrap();
        assert_eq!(caller["weights"]["outgoing_call_count"], 2);
        assert_eq!(caller["weights"]["fan_out"], 1);
        assert_eq!(helper["weights"]["incoming_call_count"], 2);
        assert_eq!(helper["weights"]["fan_in"], 1);
    }

    #[rstest]
    #[case::rust_public("pub_fn.rs", "pub fn exported() {}\n", "exported", "public")]
    #[case::rust_private("priv_fn.rs", "fn hidden() {}\n", "hidden", "private")]
    #[case::rust_restricted(
        "restricted_fn.rs",
        "pub(crate) fn scoped() {}\n",
        "scoped",
        "restricted"
    )]
    #[case::typescript_unknown("app.ts", "export function render() {}\n", "render", "unknown")]
    #[case::python_unknown("app.py", "def handler():\n    return 1\n", "handler", "unknown")]
    #[case::go_exported("app.go", "package app\n\nfunc Run() {}\n", "Run", "exported")]
    #[case::go_unexported("app.go", "package app\n\nfunc run() {}\n", "run", "unexported")]
    fn node_visibility_reflects_language_rules(
        #[case] file: &str,
        #[case] source: &str,
        #[case] name: &str,
        #[case] expected: &str,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), file, source);

        let report = analyze_json(dir.path());
        let node = report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == name)
            .unwrap();
        assert_eq!(node["visibility"], expected);
    }

    #[test]
    fn self_method_calls_resolve_to_owner_method() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "struct S;\nimpl S {\n    fn helper(&self) {}\n    fn caller(&self) { self.helper(); }\n}\n",
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "helper");
        assert_eq!(edge["from"], "src/lib.rs:S::caller:4");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(edge["resolution_method"], "self_method");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("crate::S::helper"),
        );
    }

    #[test]
    fn receiver_method_resolves_via_unique_workspace_match() {
        // `self.inner.helper()` is a method call on a non-self
        // receiver. We cannot infer `self.inner`'s type, but `helper`
        // is unique workspace-wide so the last-segment fallback
        // attributes the call to `Inner::helper`.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "struct Inner;\nimpl Inner { fn helper(&self) {} }\nstruct S { inner: Inner }\nimpl S { fn caller(&self) { self.inner.helper(); } }\n",
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "helper");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(edge["resolution_method"], "last_segment");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("crate::Inner::helper"),
        );
    }

    #[test]
    fn receiver_method_call_with_no_workspace_match_stays_unresolved() {
        // `s.len()` targets std's `String::len`, which is not in the
        // workspace. The receiver-method resolver must not invent a
        // match when the last segment has no workspace candidate.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn caller(s: String) { let _ = s.len(); }\n",
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "len");
        assert_eq!(edge["resolution"], "unresolved");
        assert_eq!(edge["to"], Value::Null);
        assert_eq!(edge.get("resolution_method"), None);
        assert_eq!(edge.get("candidates"), None);
    }

    #[test]
    fn receiver_method_call_on_ubiquitous_name_stays_unresolved() {
        // `v.clone()` targets std's `Clone`, but the workspace happens
        // to declare a unique `CoreHook::clone`. Uniqueness is not
        // evidence here: with `clone` on the ubiquitous-name table the
        // call must stay unresolved rather than becoming an in-edge on
        // the workspace method.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub struct CoreHook;\nimpl CoreHook { pub fn clone(&self) -> CoreHook { CoreHook } }\n\
             pub fn caller(v: &Vec<u8>) -> Vec<u8> { v.clone() }\n",
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "clone");
        assert_eq!(edge["resolution"], "unresolved");
        assert_eq!(edge["to"], Value::Null);
        assert_eq!(edge.get("resolution_method"), None);
        assert_eq!(edge.get("candidates"), None);
        let hook = node_by_qualified_name(&report, "crate::CoreHook::clone");
        assert_eq!(hook["weights"]["fan_in"], 0);
    }

    #[test]
    fn receiver_method_narrows_ambiguous_match_to_callers_crate() {
        // Two workspace crates each declare a method named
        // `parse_header`. A bare receiver call from `agent_a` cannot
        // pick either via last-segment alone, but crate narrowing keeps
        // the same-crate match. The name has to be one `std` does not
        // define, or the ubiquitous-name table would (correctly) refuse
        // the whole fallback — see `receive_method_call_on_ubiquitous_
        // name_stays_unresolved`.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        write_file(
            dir.path(),
            "crates/agent-a/Cargo.toml",
            "[package]\nname = \"agent-a\"\n",
        );
        write_file(
            dir.path(),
            "crates/agent-a/src/lib.rs",
            "pub struct Foo;\nimpl Foo { pub fn parse_header(&self) {} }\n\
             pub fn caller(f: Foo) { f.parse_header(); }\n",
        );
        write_file(
            dir.path(),
            "crates/agent-b/Cargo.toml",
            "[package]\nname = \"agent-b\"\n",
        );
        write_file(
            dir.path(),
            "crates/agent-b/src/lib.rs",
            "pub struct Bar;\nimpl Bar { pub fn parse_header(&self) {} }\n",
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "parse_header");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(edge["resolution_method"], "crate_narrowed");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("agent_a::Foo::parse_header"),
        );
    }

    #[test]
    fn receiver_method_with_multiple_candidates_in_callers_crate_is_ambiguous() {
        // Two methods named `parse_header` live in the caller's crate.
        // Crate narrowing cannot tiebreak, so the call goes ambiguous
        // and reports both candidates.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub struct Foo; impl Foo { pub fn parse_header(&self) {} } }\n\
             mod b { pub struct Bar; impl Bar { pub fn parse_header(&self) {} } }\n\
             fn caller(x: crate::a::Foo) { x.parse_header(); }\n",
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "parse_header");
        assert_eq!(edge["resolution"], "ambiguous");
        assert_eq!(edge["to"], Value::Null);
        assert_eq!(edge["resolution_method"], "crate_narrowed");
        let candidates: Vec<&str> = edge["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();
        assert_eq!(
            candidates,
            [
                "src/lib.rs:Bar::parse_header:2",
                "src/lib.rs:Foo::parse_header:1"
            ],
            "candidates must be sorted node ids",
        );
    }

    #[rstest]
    #[case::self_type("Self::helper();")]
    #[case::concrete_type("S::helper();")]
    fn resolves_syntactic_static_method_paths(#[case] call: &str) {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            &format!(
                "struct S;\nimpl S {{\n    fn helper() {{}}\n    fn caller() {{ {call} }}\n}}\n"
            ),
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "helper");
        assert_eq!(edge["from"], "src/lib.rs:S::caller:4");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("crate::S::helper"),
        );
    }

    #[test]
    fn default_mode_includes_cfg_test_call_sites() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn prod() {}\n#[cfg(test)]\nmod tests { fn helper() { prod(); } }\n",
        );

        let report = analyze_json(dir.path());
        let edges = report["edges"].as_array().unwrap();
        let edge = edges
            .iter()
            .find(|e| e["from"] == "src/lib.rs:helper:3")
            .expect("cfg(test) helper call should be included by default");
        assert_eq!(edge["to"], "src/lib.rs:prod:1");
        assert_eq!(edge["resolution"], "resolved");
    }

    #[test]
    fn typed_path_call_disambiguates_shared_method_name() {
        // Two `new` methods exist on different types. Bare `Type::new()`
        // calls would otherwise fall through to the last-segment fallback
        // and report ambiguous; the path suffix narrows them.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub struct Foo; impl Foo { pub fn new() -> Self { Self } } }\n\
             mod b { pub struct Bar; impl Bar { pub fn new() -> Self { Self } } }\n\
             mod c { fn caller() { let _ = crate::a::Foo::new(); let _ = crate::b::Bar::new(); } }\n",
        );

        let report = analyze_json(dir.path());
        let edges = report["edges"].as_array().unwrap();
        let foo_new = edges
            .iter()
            .find(|e| target_qualified_name(&report, e).as_deref() == Some("crate::a::Foo::new"))
            .expect("Foo::new should resolve");
        assert_eq!(foo_new["resolution"], "resolved");
        let bar_new = edges
            .iter()
            .find(|e| target_qualified_name(&report, e).as_deref() == Some("crate::b::Bar::new"))
            .expect("Bar::new should resolve");
        assert_eq!(bar_new["resolution"], "resolved");
        assert_eq!(report["summary"]["ambiguous_edge_count"], 0);
    }

    #[test]
    fn glob_imported_typed_path_resolves_via_suffix_narrowing() {
        // Without an explicit `use crate::a::Foo`, a `Foo::new()` call in
        // module `c` cannot match any direct lexical candidate, so it
        // hits the last-segment fallback. The suffix narrow then picks
        // `crate::a::Foo::new` out of the workspace's `new` candidates.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub struct Foo; impl Foo { pub fn new() -> Self { Self } } }\n\
             mod b { pub struct Bar; impl Bar { pub fn new() -> Self { Self } } }\n\
             mod c { use crate::a::*; fn caller() { let _ = Foo::new(); } }\n",
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "new");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(edge["resolution_method"], "path_suffix");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("crate::a::Foo::new"),
        );
    }

    #[test]
    fn external_typed_path_call_is_unresolved_not_ambiguous() {
        // `String::new()` is external, so it must not be silently bucketed
        // with unrelated workspace `new` methods. Path syntax disambiguates.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "struct Foo;\nimpl Foo { fn new() -> Self { Self } }\nfn caller() { let _ = String::new(); let _ = Foo::new(); }\n",
        );

        let report = analyze_json(dir.path());
        let edges = report["edges"].as_array().unwrap();
        let new_edges: Vec<_> = edges.iter().filter(|e| e["callee_name"] == "new").collect();
        assert_eq!(new_edges.len(), 2);
        let foo_new = new_edges
            .iter()
            .find(|e| e["resolution"] == "resolved")
            .expect("Foo::new should resolve");
        assert_eq!(
            target_qualified_name(&report, foo_new).as_deref(),
            Some("crate::Foo::new"),
        );
        let string_new = new_edges
            .iter()
            .find(|e| e["resolution"] == "unresolved")
            .expect("String::new should be unresolved, not ambiguous");
        assert_eq!(string_new["callee_name"], "new");
    }

    #[test]
    fn duplicate_callee_names_are_ambiguous_with_candidates() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn same() {} }\nmod b { pub fn same() {} }\nfn caller() { same(); }\n",
        );

        let report = analyze_json(dir.path());
        let edge = &report["edges"].as_array().unwrap()[0];
        assert_eq!(edge["to"], Value::Null);
        assert_eq!(edge["resolution"], "ambiguous");
        assert_eq!(edge["resolution_method"], "last_segment");
        let candidates: Vec<&str> = edge["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap())
            .collect();
        assert_eq!(candidates, ["src/lib.rs:same:1", "src/lib.rs:same:2"]);
        let node_ids: Vec<&str> = report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["id"].as_str().unwrap())
            .collect();
        for candidate in candidates {
            assert!(node_ids.contains(&candidate), "unknown candidate id");
        }
    }

    #[test]
    fn summary_breaks_resolution_counts_down_by_module() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn known() {} fn caller() { known(); external(); } }\n\
             mod b { fn caller() { crate::a::known(); } }\n",
        );

        let report = analyze_json(dir.path());
        let modules = report["summary"]["modules"].as_array().unwrap();
        assert_eq!(modules.len(), 2);
        assert_eq!(modules[0]["module"], "crate::a");
        assert_eq!(modules[0]["resolved_call_count"], 1);
        assert_eq!(modules[0]["unresolved_call_count"], 1);
        assert_eq!(modules[0]["ambiguous_call_count"], 0);
        assert_eq!(modules[0]["anonymous_call_count"], 0);
        assert_eq!(modules[0]["total_call_count"], 2);
        assert_eq!(modules[1]["module"], "crate::b");
        assert_eq!(modules[1]["resolved_call_count"], 1);
        assert_eq!(modules[1]["total_call_count"], 1);
    }

    #[test]
    fn cargo_manifest_qualifies_module_paths_with_real_crate_name() {
        // When a `Cargo.toml` is present, the module prefix should
        // come from `[package].name` (with hyphens normalised to
        // underscores) instead of the literal `crate`.
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "Cargo.toml", "[package]\nname = \"my-pkg\"\n");
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn helper() {}\nfn caller() { helper(); crate::helper(); }\n",
        );

        let report = analyze_json(dir.path());
        let nodes = report["nodes"].as_array().unwrap();
        let helper = nodes.iter().find(|n| n["name"] == "helper").unwrap();
        assert_eq!(helper["qualified_name"], "my_pkg::helper");
        assert_eq!(helper["module"], "my_pkg");

        // Both bare and `crate::`-prefixed calls land on the same
        // resolved node, so they aggregate into one edge. The bare
        // call resolves lexically, the prefixed one via path suffix;
        // the merged edge keeps the most direct method.
        let edges = report["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0]["resolution"], "resolved");
        assert_eq!(edges[0]["call_count"], 2);
        assert_eq!(edges[0]["resolution_method"], "lexical");
    }

    #[test]
    fn workspace_member_crates_disambiguate_same_named_items() {
        // Two workspace crates each declare `Foo::new`. Without crate
        // qualification both nodes collapse under `crate::Foo::new`
        // and the call goes ambiguous; with the manifest lookup they
        // become `agent_a::Foo::new` and `agent_b::Foo::new`.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        write_file(
            dir.path(),
            "crates/agent-a/Cargo.toml",
            "[package]\nname = \"agent-a\"\n",
        );
        write_file(
            dir.path(),
            "crates/agent-a/src/lib.rs",
            "pub struct Foo;\nimpl Foo { pub fn new() -> Self { Self } }\n\
             pub fn caller() { let _ = Foo::new(); }\n",
        );
        write_file(
            dir.path(),
            "crates/agent-b/Cargo.toml",
            "[package]\nname = \"agent-b\"\n",
        );
        write_file(
            dir.path(),
            "crates/agent-b/src/lib.rs",
            "pub struct Foo;\nimpl Foo { pub fn new() -> Self { Self } }\n",
        );

        let report = analyze_json(dir.path());
        let qualified: Vec<&str> = report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["qualified_name"].as_str())
            .collect();
        assert!(qualified.contains(&"agent_a::Foo::new"));
        assert!(qualified.contains(&"agent_b::Foo::new"));

        let edge = edge_by_callee(&report, "new");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("agent_a::Foo::new"),
        );
        assert_eq!(report["summary"]["ambiguous_edge_count"], 0);
    }

    #[test]
    fn same_module_bare_call_resolves_before_duplicate_name_fallback() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { fn parse() {} fn caller() { parse(); } }\nmod b { fn parse() {} }\n",
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "parse");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("crate::a::parse"),
        );
        assert_eq!(report["summary"]["ambiguous_edge_count"], 0);
    }

    #[rstest]
    #[case::absolute(
        "mod a { pub fn parse() {} }\nmod b { fn caller() { crate::a::parse(); } }\n",
        "crate::a::parse"
    )]
    #[case::self_relative(
        "mod a { fn parse() {} fn caller() { self::parse(); } }\nmod b { fn parse() {} }\n",
        "crate::a::parse"
    )]
    #[case::super_relative(
        "mod a { fn parse() {} mod inner { fn caller() { super::parse(); } } }\n",
        "crate::a::parse"
    )]
    fn resolves_lexical_module_paths(#[case] source: &str, #[case] expected: &str) {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", source);

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "parse");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(edge["resolution_method"], "lexical");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn imported_alias_resolves_bare_call() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn parse() {} }\nmod b { use crate::a::parse; fn caller() { parse(); } }\n",
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "parse");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("crate::a::parse"),
        );
    }

    #[test]
    fn external_calls_are_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn caller() { println!(); external(); }\n",
        );

        let report = analyze_json(dir.path());
        let edges = report["edges"].as_array().unwrap();
        assert!(edges.iter().any(|e| e["resolution"] == "unresolved"));
    }

    #[test]
    fn typescript_roots_emit_nodes_edges_and_language() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "helper.ts", "export function helper() {}\n");
        write_file(
            dir.path(),
            "index.ts",
            "import { helper } from './helper';\nfunction local() {}\nfunction caller() { helper(); local(); }\n",
        );

        let report = analyze_json(dir.path());

        assert_eq!(report["language"], "typescript");
        assert_eq!(report["node_count"], 3);
        let names: Vec<_> = report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["qualified_name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"helper::helper"));
        assert!(names.contains(&"index::caller"));
        assert!(names.contains(&"index::local"));

        let helper = edge_by_callee(&report, "helper");
        assert_eq!(helper["from"], "index.ts:caller:3");
        assert_eq!(helper["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, helper).as_deref(),
            Some("helper::helper"),
        );

        let local = edge_by_callee(&report, "local");
        assert_eq!(local["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, local).as_deref(),
            Some("index::local"),
        );
    }

    #[test]
    fn python_roots_emit_nodes_edges_and_language() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "helper.py", "def helper():\n    return 1\n");
        write_file(
            dir.path(),
            "main.py",
            "from helper import helper\n\ndef local():\n    return 2\n\ndef caller():\n    helper()\n    local()\n",
        );

        let report = analyze_json(dir.path());

        assert_eq!(report["language"], "python");
        assert_eq!(report["node_count"], 3);
        let names: Vec<_> = report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["qualified_name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"helper::helper"));
        assert!(names.contains(&"main::caller"));
        assert!(names.contains(&"main::local"));

        let helper = edge_by_callee(&report, "helper");
        assert_eq!(helper["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, helper).as_deref(),
            Some("helper::helper"),
        );

        let local = edge_by_callee(&report, "local");
        assert_eq!(local["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, local).as_deref(),
            Some("main::local"),
        );
    }

    #[test]
    fn python_self_method_calls_resolve_to_class_method() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "service.py",
            "class Service:\n    def helper(self):\n        return 1\n    def caller(self):\n        return self.helper()\n",
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "helper");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("service::Service::helper"),
        );
    }

    #[test]
    fn python_class_static_calls_resolve_to_owner_qualified_method() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "main.py",
            "class Helper:\n    @staticmethod\n    def run():\n        return 1\n\ndef caller():\n    Helper.run()\n",
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "run");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("main::Helper::run"),
        );
    }

    #[test]
    fn python_init_files_collapse_to_package_module_path() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "pkg/__init__.py", "def root():\n    return 1\n");

        let report = analyze_json(dir.path());
        let names: Vec<_> = report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["qualified_name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["pkg::root"]);
    }

    #[test]
    fn go_directory_yields_module_qualified_names_and_call_edges() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "go.mod", "module github.com/x/proj\n");
        write_file(
            dir.path(),
            "main.go",
            concat!(
                "package main\n\n",
                "import \"github.com/x/proj/pkg/util\"\n\n",
                "func caller() { util.Run() }\n",
            ),
        );
        write_file(
            dir.path(),
            "pkg/util/util.go",
            "package util\n\nfunc Run() {}\n",
        );

        let report = analyze_json(dir.path());
        let qualified: Vec<&str> = report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["qualified_name"].as_str().unwrap())
            .collect();
        assert!(
            qualified.contains(&"main::caller"),
            "expected main::caller node, got {qualified:?}",
        );
        assert!(
            qualified.contains(&"pkg::util::Run"),
            "expected pkg::util::Run node, got {qualified:?}",
        );

        let edge = edge_by_callee(&report, "Run");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("pkg::util::Run"),
        );
        assert_eq!(report["language"], "go");
    }

    /// Regression: a local closure named after a method in another
    /// package used to resolve to that method through the last-segment
    /// fallback, minting a cross-package edge the program cannot make —
    /// package `b` does not even import package `a`. One such edge is
    /// enough to turn a cleanly layered pair of modules into a reported
    /// module cycle.
    #[test]
    fn go_local_closure_does_not_resolve_to_a_same_named_method_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "go.mod", "module github.com/x/proj\n");
        write_file(
            dir.path(),
            "a/a.go",
            concat!(
                "package a\n\n",
                "type emitter struct{}\n\n",
                "func (e *emitter) emit(ev int) {}\n",
            ),
        );
        write_file(
            dir.path(),
            "b/b.go",
            concat!(
                "package b\n\n",
                "func pump(first int) {\n",
                "    emit := func(ev int) bool { return true }\n",
                "    emit(first)\n",
                "}\n",
            ),
        );

        let report = analyze_json(dir.path());
        let edges = report["edges"].as_array().unwrap();
        let fabricated: Vec<&Value> = edges
            .iter()
            .filter(|edge| edge["callee_name"] == "emit" && edge["resolution"] != "unresolved")
            .collect();
        assert!(
            fabricated.is_empty(),
            "local closure must not resolve to a::emitter::emit, got {fabricated:?}",
        );
    }

    /// The suppression is scoped to the shadowed name: a genuine
    /// cross-package call in the same function still resolves.
    #[test]
    fn go_shadowed_name_does_not_suppress_other_calls_in_the_same_function() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "go.mod", "module github.com/x/proj\n");
        write_file(
            dir.path(),
            "pkg/util/util.go",
            "package util\n\nfunc Run() {}\n",
        );
        write_file(
            dir.path(),
            "b/b.go",
            concat!(
                "package b\n\n",
                "import \"github.com/x/proj/pkg/util\"\n\n",
                "func pump(first int) {\n",
                "    emit := func(ev int) bool { return true }\n",
                "    emit(first)\n",
                "    util.Run()\n",
                "}\n",
            ),
        );

        let report = analyze_json(dir.path());
        let edge = edge_by_callee(&report, "Run");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("pkg::util::Run"),
        );
    }

    #[test]
    fn go_method_calls_carry_owner_qualified_caller() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "service/service.go",
            "package service\n\ntype Service struct{}\n\nfunc (s *Service) Helper() int { return 1 }\n\nfunc (s *Service) Caller() int { return Helper() }\n\nfunc Helper() int { return 0 }\n",
        );

        let report = analyze_json(dir.path());
        let qualified: Vec<&str> = report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["qualified_name"].as_str().unwrap())
            .collect();
        // module qualified to "service" (package directory).
        assert!(qualified.contains(&"service::Service::Caller"));
        assert!(qualified.contains(&"service::Service::Helper"));
        assert!(qualified.contains(&"service::Helper"));
    }

    #[test]
    fn tsx_files_are_parsed_for_function_graph() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "App.tsx",
            "function helper() {}\nexport function App() { return <button onClick={() => helper()}>Run</button>; }\n",
        );

        let report = analyze_json(dir.path());

        assert_eq!(report["language"], "typescript");
        // The `onClick` handler is a nested function, so it is its own
        // node alongside `helper` and `App`.
        assert_eq!(report["node_count"], 3);
        let qualified: Vec<&str> = report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["qualified_name"].as_str().unwrap())
            .collect();
        assert!(
            qualified.contains(&"App::App::closure#1"),
            "got {qualified:?}"
        );
        // The `helper()` call lives in the handler, so the resolved edge
        // is owned by the closure rather than re-attributed to `App`.
        let edge = edge_by_callee(&report, "helper");
        assert_eq!(edge["resolution"], "resolved");
        assert_eq!(
            target_qualified_name(&report, edge).as_deref(),
            Some("App::helper"),
        );
    }

    #[test]
    fn path_and_function_test_filters_are_respected() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn prod() {}\n#[cfg(test)]\nmod tests { fn helper() { prod(); } }\n",
        );
        write_file(dir.path(), "tests/integration.rs", "fn integration() {}\n");

        let exclude = FunctionGraphAnalyzer::new()
            .with_exclude_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let exclude: Value = serde_json::from_str(&exclude).unwrap();
        let names: Vec<_> = exclude["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["prod"]);

        let only = FunctionGraphAnalyzer::new()
            .with_only_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let only: Value = serde_json::from_str(&only).unwrap();
        let names: Vec<_> = only["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["helper", "integration"]);
        let integration = only["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == "integration")
            .unwrap();
        assert_eq!(integration["is_test"], true);
    }

    #[test]
    fn exclude_globs_are_respected() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/keep.rs", "fn keep() {}\n");
        write_file(dir.path(), "src/generated.rs", "fn generated() {}\n");

        let json = FunctionGraphAnalyzer::new()
            .with_exclude_patterns(vec!["generated.rs".to_owned()])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let report: Value = serde_json::from_str(&json).unwrap();
        let names: Vec<_> = report["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["keep"]);
    }

    #[test]
    fn markdown_reports_compact_summary() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn helper() {}\nfn caller() { helper(); }\n",
        );

        let md = FunctionGraphAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Function graph:"));
        assert!(md.contains("resolved edges: 1"));
    }

    #[test]
    fn single_file_input_uses_crate_module() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/nested/file.rs", "fn f() {}\n");
        let file = dir.path().join("src/nested/file.rs");

        let report = analyze_json(&file);
        let node = &report["nodes"].as_array().unwrap()[0];
        assert_eq!(node["module"], "crate");
    }
}
