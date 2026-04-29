//! `analyze function-graph` — emit a Rust static function call graph.
//!
//! This report is intentionally machine-facing JSON for downstream
//! visualization tools. It is static and heuristic: no type inference,
//! macro expansion, cross-crate resolution, runtime timing, or git history
//! traversal is attempted here.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;

use lens_domain::{FunctionDef, LanguageParser};
use lens_rust::{CallIndexOptions, CallSite, RustParser};
use serde::Serialize;

use super::{
    AnalyzePathFilter, AnalyzerError, OutputFormat, SourceFile, SourceLang, collect_source_files,
    read_source,
};

const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default, Clone)]
pub struct FunctionGraphAnalyzer {
    only_tests: bool,
    exclude_tests: bool,
    path_filter: AnalyzePathFilter,
}

impl FunctionGraphAnalyzer {
    pub fn new() -> Self {
        Self {
            only_tests: false,
            exclude_tests: false,
            path_filter: AnalyzePathFilter::new(),
        }
    }

    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.only_tests = only_tests;
        self.path_filter = self.path_filter.with_only_tests(only_tests);
        self
    }

    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.exclude_tests = exclude_tests;
        self.path_filter = self.path_filter.with_exclude_tests(exclude_tests);
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.path_filter = self.path_filter.with_exclude_patterns(exclude);
        self
    }

    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let collection_filter = if self.only_tests {
            self.path_filter.clone().with_only_tests(false)
        } else {
            self.path_filter.clone()
        };
        let filter = collection_filter.compile(path)?;
        let mut files = Vec::new();
        for source_file in collect_source_files(path, &filter)? {
            if !matches!(
                SourceLang::from_path(&source_file.path),
                Some(SourceLang::Rust)
            ) {
                continue;
            }
            let path_is_test = filter.is_test_path(&source_file.path);
            files.push(self.scan_file(path, &source_file, path_is_test)?);
        }
        let report = Report::build(path, files);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).map_err(AnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&report)),
        }
    }

    fn scan_file(
        &self,
        root: &Path,
        file: &SourceFile,
        path_is_test: bool,
    ) -> Result<FileGraphInput, AnalyzerError> {
        let (lang, source) = read_source(&file.path)?;
        if !matches!(lang, SourceLang::Rust) {
            return Err(AnalyzerError::UnsupportedExtension {
                path: file.path.clone(),
            });
        }

        let mut parser = RustParser::new();
        let mut functions = parser
            .extract_functions(&source)
            .map_err(|e| AnalyzerError::Parse(Box::new(e)))?;
        functions.retain(|f| self.includes_function(f, path_is_test));

        let calls = lens_rust::extract_call_sites_with_options(
            &source,
            CallIndexOptions {
                include_cfg_test_blocks: !self.exclude_tests,
            },
        )
        .map_err(|e| AnalyzerError::Parse(Box::new(e)))?;

        Ok(FileGraphInput {
            file: file.display_path.clone(),
            module: module_path_for(root, file),
            path_is_test,
            functions,
            calls,
        })
    }

    fn includes_function(&self, f: &FunctionDef, path_is_test: bool) -> bool {
        let is_test = f.is_test || path_is_test;
        if self.only_tests {
            return is_test;
        }
        if self.exclude_tests {
            return !is_test;
        }
        true
    }
}

struct FileGraphInput {
    file: String,
    module: String,
    path_is_test: bool,
    functions: Vec<FunctionDef>,
    calls: Vec<CallSite>,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    node_count: usize,
    edge_count: usize,
    nodes: Vec<NodeView>,
    edges: Vec<EdgeView>,
    summary: SummaryView,
}

impl Report {
    fn build(root: &Path, files: Vec<FileGraphInput>) -> Self {
        let mut nodes = build_nodes(&files);
        let mut edges = build_edges(&files, &nodes);
        apply_static_degrees(&mut nodes, &edges);
        edges.sort_by(|a, b| {
            a.from
                .cmp(&b.from)
                .then_with(|| a.to.cmp(&b.to))
                .then_with(|| a.callee_name.cmp(&b.callee_name))
                .then_with(|| a.resolution.cmp(&b.resolution))
        });
        let summary = SummaryView::new(&edges);
        Self {
            schema_version: SCHEMA_VERSION,
            root: root.display().to_string(),
            language: "rust",
            node_count: nodes.len(),
            edge_count: edges.len(),
            nodes,
            edges,
            summary,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct NodeView {
    id: String,
    name: String,
    file: String,
    module: String,
    start_line: usize,
    end_line: usize,
    is_test: bool,
    weights: NodeWeights,
}

#[derive(Debug, Clone, Default, Serialize)]
struct NodeWeights {
    incoming_call_count: usize,
    outgoing_call_count: usize,
    total_time_ms: Option<f64>,
    self_time_ms: Option<f64>,
    error_count: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
struct EdgeView {
    from: Option<String>,
    to: Option<String>,
    callee_name: Option<String>,
    resolution: Resolution,
    call_count: usize,
    call_lines: Vec<usize>,
    weights: EdgeWeights,
}

#[derive(Debug, Clone, Default, Serialize)]
struct EdgeWeights {
    call_count: usize,
    total_transition_time_ms: Option<f64>,
    error_count: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Resolution {
    Resolved,
    Unresolved,
    Ambiguous,
    Anonymous,
}

#[derive(Debug, Serialize)]
struct SummaryView {
    resolved_edge_count: usize,
    unresolved_edge_count: usize,
    ambiguous_edge_count: usize,
    anonymous_edge_count: usize,
    total_static_call_count: usize,
}

impl SummaryView {
    fn new(edges: &[EdgeView]) -> Self {
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
        }
    }
}

fn build_nodes(files: &[FileGraphInput]) -> Vec<NodeView> {
    let mut nodes = Vec::new();
    for file in files {
        for f in &file.functions {
            nodes.push(NodeView {
                id: node_id(&file.file, f),
                name: f.name.clone(),
                file: file.file.clone(),
                module: file.module.clone(),
                start_line: f.start_line,
                end_line: f.end_line,
                is_test: f.is_test || file.path_is_test,
                weights: NodeWeights::default(),
            });
        }
    }
    nodes.sort_by(|a, b| {
        a.file
            .cmp(&b.file)
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.name.cmp(&b.name))
    });
    nodes
}

fn build_edges(files: &[FileGraphInput], nodes: &[NodeView]) -> Vec<EdgeView> {
    let resolver = Resolver::new(nodes);
    let caller_index = CallerIndex::new(nodes);
    let mut grouped: BTreeMap<EdgeKey, EdgeView> = BTreeMap::new();

    for file in files {
        for site in &file.calls {
            let from = site
                .caller_name
                .as_deref()
                .and_then(|caller| caller_index.resolve_in_file(&file.file, caller));
            if site.caller_name.is_some() && from.is_none() {
                continue;
            }
            let (to, resolution) = resolver.resolve(site);
            let key = EdgeKey {
                from: from.clone(),
                to: to.clone(),
                callee_name: site.callee_name.clone(),
                resolution,
            };
            let entry = grouped.entry(key).or_insert_with(|| EdgeView {
                from,
                to,
                callee_name: site.callee_name.clone(),
                resolution,
                call_count: 0,
                call_lines: Vec::new(),
                weights: EdgeWeights::default(),
            });
            entry.call_count += 1;
            entry.weights.call_count += 1;
            entry.call_lines.push(site.line);
        }
    }

    grouped
        .into_values()
        .map(|mut edge| {
            edge.call_lines.sort_unstable();
            edge.call_lines.dedup();
            edge
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EdgeKey {
    from: Option<String>,
    to: Option<String>,
    callee_name: Option<String>,
    resolution: Resolution,
}

struct CallerIndex {
    by_file_and_name: HashMap<(String, String), Vec<String>>,
}

impl CallerIndex {
    fn new(nodes: &[NodeView]) -> Self {
        let mut by_file_and_name: HashMap<(String, String), Vec<String>> = HashMap::new();
        for node in nodes {
            by_file_and_name
                .entry((node.file.clone(), node.name.clone()))
                .or_default()
                .push(node.id.clone());
        }
        Self { by_file_and_name }
    }

    fn resolve_in_file(&self, file: &str, caller: &str) -> Option<String> {
        let ids = self
            .by_file_and_name
            .get(&(file.to_owned(), caller.to_owned()))?;
        if ids.len() == 1 {
            return ids.first().cloned();
        }
        None
    }
}

struct Resolver {
    exact: HashMap<String, Vec<String>>,
    last_segment: HashMap<String, Vec<String>>,
}

impl Resolver {
    fn new(nodes: &[NodeView]) -> Self {
        let mut exact: HashMap<String, Vec<String>> = HashMap::new();
        let mut last_segment: HashMap<String, Vec<String>> = HashMap::new();
        for node in nodes {
            exact
                .entry(node.name.clone())
                .or_default()
                .push(node.id.clone());
            last_segment
                .entry(name_last_segment(&node.name).to_owned())
                .or_default()
                .push(node.id.clone());
        }
        Self {
            exact,
            last_segment,
        }
    }

    fn resolve(&self, site: &CallSite) -> (Option<String>, Resolution) {
        let Some(callee_name) = site.callee_name.as_deref() else {
            return (None, Resolution::Anonymous);
        };
        if let Some(callee_path) = site.callee_path.as_deref()
            && let Some(ids) = self.exact.get(callee_path)
        {
            return resolve_ids(ids);
        }
        let Some(ids) = self.last_segment.get(callee_name) else {
            return (None, Resolution::Unresolved);
        };
        resolve_ids(ids)
    }
}

fn resolve_ids(ids: &[String]) -> (Option<String>, Resolution) {
    if ids.len() == 1 {
        (ids.first().cloned(), Resolution::Resolved)
    } else {
        (None, Resolution::Ambiguous)
    }
}

fn apply_static_degrees(nodes: &mut [NodeView], edges: &[EdgeView]) {
    let mut by_id: HashMap<String, usize> = HashMap::new();
    for (idx, node) in nodes.iter().enumerate() {
        by_id.insert(node.id.clone(), idx);
    }
    for edge in edges {
        if let Some(from) = edge.from.as_deref()
            && let Some(idx) = by_id.get(from).copied()
        {
            nodes[idx].weights.outgoing_call_count += edge.call_count;
        }
        if let Some(to) = edge.to.as_deref()
            && let Some(idx) = by_id.get(to).copied()
        {
            nodes[idx].weights.incoming_call_count += edge.call_count;
        }
    }
}

fn node_id(file: &str, f: &FunctionDef) -> String {
    format!("{}:{}:{}", file, f.name, f.start_line)
}

fn name_last_segment(name: &str) -> &str {
    name.rsplit_once("::").map_or(name, |(_, last)| last)
}

fn module_path_for(root: &Path, file: &SourceFile) -> String {
    if !root.is_dir() {
        return "crate".to_owned();
    }
    module_path_from_relative_file(&file.display_path)
}

fn module_path_from_relative_file(file: &str) -> String {
    let mut rel = file.replace('\\', "/");
    if let Some(stripped) = rel.strip_prefix("src/") {
        rel = stripped.to_owned();
    }
    if rel == "lib.rs" || rel == "main.rs" {
        return "crate".to_owned();
    }
    if let Some(stripped) = rel.strip_suffix("/mod.rs") {
        rel = stripped.to_owned();
    } else if let Some(stripped) = rel.strip_suffix(".rs") {
        rel = stripped.to_owned();
    }
    let module = rel
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("::");
    if module.is_empty() {
        "crate".to_owned()
    } else {
        format!("crate::{module}")
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

    fn analyze_json(path: &Path) -> Value {
        let json = FunctionGraphAnalyzer::new()
            .analyze(path, OutputFormat::Json)
            .unwrap();
        serde_json::from_str(&json).unwrap()
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
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["language"], "rust");
        assert_eq!(nodes[0]["id"], "src/lib.rs:helper:1");
        assert_eq!(nodes[0]["module"], "crate");
        assert_eq!(nodes[0]["weights"]["total_time_ms"], Value::Null);
        assert_eq!(nodes[0]["weights"]["self_time_ms"], Value::Null);
        assert_eq!(nodes[0]["weights"]["error_count"], Value::Null);
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
        assert_eq!(edges[0]["call_count"], 2);
        assert_eq!(edges[0]["weights"]["call_count"], 2);
        assert_eq!(edges[0]["weights"]["total_transition_time_ms"], Value::Null);
    }

    #[test]
    fn duplicate_callee_names_are_ambiguous() {
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
}
