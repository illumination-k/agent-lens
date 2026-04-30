//! `analyze function-graph` — emit a Rust static function call graph.
//!
//! This report is intentionally machine-facing JSON for downstream
//! visualization tools. It is static and heuristic: no type inference,
//! macro expansion, cross-crate resolution, runtime timing, or git history
//! traversal is attempted here.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use lens_domain::{CallShape, FunctionComplexity, FunctionShape, SyntaxFact};
use lens_rust::CallIndexOptions;
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

        let module = module_path_for(root, file);
        let mut functions = lens_rust::extract_function_shapes_with_modules(&source, &module)
            .map_err(|e| AnalyzerError::Parse(Box::new(e)))?;
        functions.retain(|f| self.includes_function(f, path_is_test));

        let calls = lens_rust::extract_call_shapes_with_options_and_base_module(
            &source,
            CallIndexOptions {
                include_cfg_test_blocks: !self.exclude_tests,
            },
            &module,
        )
        .map_err(|e| AnalyzerError::Parse(Box::new(e)))?;
        let complexity = lens_rust::extract_complexity_units(&source)
            .map_err(|e| AnalyzerError::Parse(Box::new(e)))?;

        Ok(FileGraphInput {
            file: file.display_path.clone(),
            path_is_test,
            functions,
            calls,
            complexity,
        })
    }

    fn includes_function(&self, f: &FunctionShape, path_is_test: bool) -> bool {
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
    path_is_test: bool,
    functions: Vec<FunctionShape>,
    calls: Vec<CallShape>,
    complexity: Vec<FunctionComplexity>,
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
    qualified_name: String,
    file: String,
    module: String,
    impl_owner: Option<String>,
    start_line: usize,
    end_line: usize,
    is_test: bool,
    weights: NodeWeights,
}

#[derive(Debug, Clone, Default, Serialize)]
struct NodeWeights {
    incoming_call_count: usize,
    outgoing_call_count: usize,
    fan_in: usize,
    fan_out: usize,
    loc: usize,
    cyclomatic_complexity: Option<u32>,
    cognitive_complexity: Option<u32>,
    max_nesting: Option<u32>,
    maintainability_index: Option<f64>,
    halstead_volume: Option<f64>,
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
        let complexity = ComplexityIndex::new(&file.complexity);
        for f in &file.functions {
            let metrics = complexity.get(f);
            nodes.push(NodeView {
                id: node_id(&file.file, f),
                name: f.display_name.clone(),
                qualified_name: f
                    .qualified_name
                    .known_value()
                    .cloned()
                    .unwrap_or_else(|| f.display_name.clone()),
                file: file.file.clone(),
                module: f.module_path.known_value().cloned().unwrap_or_default(),
                impl_owner: f
                    .owner
                    .known_value()
                    .and_then(|owner| owner.as_ref())
                    .map(|owner| owner.display_name.clone()),
                start_line: f.span.start_line,
                end_line: f.span.end_line,
                is_test: f.is_test || file.path_is_test,
                weights: NodeWeights {
                    loc: f.line_count(),
                    cyclomatic_complexity: metrics.map(|m| m.cyclomatic),
                    cognitive_complexity: metrics.map(|m| m.cognitive),
                    max_nesting: metrics.map(|m| m.max_nesting),
                    maintainability_index: metrics
                        .and_then(FunctionComplexity::maintainability_index),
                    halstead_volume: metrics.and_then(|m| m.halstead.volume()),
                    ..NodeWeights::default()
                },
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

struct ComplexityIndex<'a> {
    by_exact: HashMap<(&'a str, usize, usize), &'a FunctionComplexity>,
}

impl<'a> ComplexityIndex<'a> {
    fn new(metrics: &'a [FunctionComplexity]) -> Self {
        let by_exact = metrics
            .iter()
            .map(|m| ((m.name.as_str(), m.start_line, m.end_line), m))
            .collect();
        Self { by_exact }
    }

    fn get(&self, f: &FunctionShape) -> Option<&'a FunctionComplexity> {
        self.by_exact
            .get(&(
                node_local_name(f).as_str(),
                f.span.start_line,
                f.span.end_line,
            ))
            .copied()
    }
}

fn build_edges(files: &[FileGraphInput], nodes: &[NodeView]) -> Vec<EdgeView> {
    let resolver = Resolver::new(nodes);
    let caller_index = CallerIndex::new(nodes);
    let mut grouped: BTreeMap<EdgeKey, EdgeView> = BTreeMap::new();

    for file in files {
        for site in &file.calls {
            let from = site
                .caller_qualified_name()
                .and_then(|caller| caller_index.resolve_in_file(&file.file, caller));
            if site.caller_qualified_name().is_some() && from.is_none() {
                continue;
            }
            let (to, resolution) = resolver.resolve(site);
            let key = EdgeKey {
                from: from.clone(),
                to: to.clone(),
                callee_name: site.callee_name().map(ToOwned::to_owned),
                resolution,
            };
            let entry = grouped.entry(key).or_insert_with(|| EdgeView {
                from,
                to,
                callee_name: site.callee_name().map(ToOwned::to_owned),
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
    by_file_and_qualified_name: HashMap<(String, String), Vec<String>>,
}

impl CallerIndex {
    fn new(nodes: &[NodeView]) -> Self {
        let mut by_file_and_qualified_name: HashMap<(String, String), Vec<String>> = HashMap::new();
        for node in nodes {
            by_file_and_qualified_name
                .entry((node.file.clone(), node.qualified_name.clone()))
                .or_default()
                .push(node.id.clone());
        }
        Self {
            by_file_and_qualified_name,
        }
    }

    fn resolve_in_file(&self, file: &str, qualified_name: &str) -> Option<String> {
        let ids = self
            .by_file_and_qualified_name
            .get(&(file.to_owned(), qualified_name.to_owned()))?;
        if ids.len() == 1 {
            return ids.first().cloned();
        }
        None
    }
}

struct Resolver {
    qualified: HashMap<String, Vec<String>>,
    last_segment: HashMap<String, Vec<String>>,
}

impl Resolver {
    fn new(nodes: &[NodeView]) -> Self {
        let mut qualified: HashMap<String, Vec<String>> = HashMap::new();
        let mut last_segment: HashMap<String, Vec<String>> = HashMap::new();
        for node in nodes {
            qualified
                .entry(node.qualified_name.clone())
                .or_default()
                .push(node.id.clone());
            last_segment
                .entry(name_last_segment(&node.qualified_name).to_owned())
                .or_default()
                .push(node.id.clone());
        }
        Self {
            qualified,
            last_segment,
        }
    }

    fn resolve(&self, site: &CallShape) -> (Option<String>, Resolution) {
        let Some(callee_name) = site.callee_name() else {
            return (None, Resolution::Anonymous);
        };
        if site.has_receiver_expression() {
            return (None, Resolution::Unresolved);
        }
        for candidate in lexical_candidates(site) {
            if let Some(ids) = self.qualified.get(&candidate) {
                return resolve_ids(ids);
            }
        }
        let Some(ids) = self.last_segment.get(callee_name) else {
            return (None, Resolution::Unresolved);
        };
        resolve_ids(ids)
    }
}

fn lexical_candidates(site: &CallShape) -> Vec<String> {
    let Some(callee_name) = site.callee_name() else {
        return Vec::new();
    };
    let Some(module) = site.caller_module() else {
        return Vec::new();
    };
    let Some(callee_path) = site.callee_path() else {
        return vec![qualify_module(module, callee_name)];
    };
    let segments: Vec<&str> = callee_path.split("::").collect();
    if segments.is_empty() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    match segments[0] {
        "crate" => candidates.push(callee_path.to_owned()),
        "self" => {
            if let Some(path) = prefix_with_tail(module_segments(module), &segments, 1) {
                candidates.push(path);
            }
        }
        "super" => {
            if let Some(path) = resolve_super_path(module, &segments) {
                candidates.push(path);
            }
        }
        "Self" => {
            if let Some(owner) = site.caller_owner()
                && let Some(tail) = join_tail(&segments, 1)
            {
                candidates.push(qualify_module(module, &format!("{owner}::{tail}")));
            }
        }
        _ => {
            if segments.len() == 1 {
                candidates.push(qualify_module(module, callee_name));
            } else {
                candidates.push(qualify_module(module, &callee_path));
            }
            if let Some(alias_target) = alias_target(site, segments[0])
                && let Some(path) = prefix_with_tail(
                    alias_target.split("::").map(ToOwned::to_owned).collect(),
                    &segments,
                    1,
                )
            {
                candidates.push(path);
            }
        }
    }
    if segments.len() == 1
        && let Some(alias_target) = alias_target(site, segments[0])
    {
        candidates.push(alias_target.to_owned());
    }
    dedupe_preserving_order(candidates)
}

fn alias_target<'a>(site: &'a CallShape, alias: &str) -> Option<&'a str> {
    site.visible_imports
        .iter()
        .rev()
        .find(|entry| {
            matches!(
                &entry.local_alias,
                SyntaxFact::Known(Some(local_alias)) if local_alias == alias
            )
        })
        .and_then(|entry| entry.imported_module.known_value())
        .map(String::as_str)
}

fn module_segments(module: &str) -> Vec<String> {
    module.split("::").map(ToOwned::to_owned).collect()
}

fn prefix_with_tail(
    mut prefix: Vec<String>,
    segments: &[&str],
    tail_start: usize,
) -> Option<String> {
    if tail_start > segments.len() {
        return None;
    }
    prefix.extend(segments.iter().skip(tail_start).map(|s| (*s).to_owned()));
    Some(prefix.join("::"))
}

fn resolve_super_path(module: &str, segments: &[&str]) -> Option<String> {
    let mut absolute = module_segments(module);
    for segment in segments {
        if *segment == "super" {
            if absolute.len() <= 1 {
                return None;
            }
            absolute.pop();
        } else {
            absolute.push((*segment).to_owned());
        }
    }
    Some(absolute.join("::"))
}

fn join_tail(segments: &[&str], start: usize) -> Option<String> {
    if start >= segments.len() {
        None
    } else {
        Some(segments[start..].join("::"))
    }
}

fn qualify_module(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{module}::{name}")
    }
}

fn dedupe_preserving_order(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
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
    let mut fan_in: HashMap<String, HashSet<String>> = HashMap::new();
    let mut fan_out: HashMap<String, HashSet<String>> = HashMap::new();
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
        if let (Some(from), Some(to), Resolution::Resolved) =
            (edge.from.as_deref(), edge.to.as_deref(), edge.resolution)
        {
            fan_out
                .entry(from.to_owned())
                .or_default()
                .insert(to.to_owned());
            fan_in
                .entry(to.to_owned())
                .or_default()
                .insert(from.to_owned());
        }
    }
    for node in nodes {
        node.weights.fan_in = fan_in.get(&node.id).map_or(0, HashSet::len);
        node.weights.fan_out = fan_out.get(&node.id).map_or(0, HashSet::len);
    }
}

fn node_id(file: &str, f: &FunctionShape) -> String {
    format!("{}:{}:{}", file, node_local_name(f), f.span.start_line)
}

fn node_local_name(f: &FunctionShape) -> String {
    f.owner
        .known_value()
        .and_then(|owner| owner.as_ref())
        .map_or_else(
            || f.display_name.clone(),
            |owner| format!("{}::{}", owner.display_name, f.display_name),
        )
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
    use lens_domain::{ImportShape, ReceiverExprKind};
    use rstest::rstest;
    use serde_json::Value;

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

    fn edge_by_callee<'a>(report: &'a Value, callee: &str) -> &'a Value {
        report["edges"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| edge["callee_name"] == callee)
            .unwrap()
    }

    fn site(path: &str) -> CallShape {
        CallShape {
            callee_display_name: SyntaxFact::Known(path.rsplit("::").next().map(ToOwned::to_owned)),
            callee_path_segments: SyntaxFact::Known(
                path.split("::").map(ToOwned::to_owned).collect(),
            ),
            caller_module: SyntaxFact::Known("crate::m".to_owned()),
            caller_qualified_name: SyntaxFact::Known(Some("crate::m::caller".to_owned())),
            caller_owner: SyntaxFact::Known(Some("S".to_owned())),
            receiver_expr_kind: SyntaxFact::Known(ReceiverExprKind::None),
            lexical_resolution: lens_domain::LexicalResolutionStatus::NotAttempted,
            visible_imports: vec![
                ImportShape {
                    local_alias: SyntaxFact::Known(Some("parse".to_owned())),
                    imported_module: SyntaxFact::Known("crate::a::parse".to_owned()),
                    exported_symbol: SyntaxFact::Unknown,
                },
                ImportShape {
                    local_alias: SyntaxFact::Known(Some("a".to_owned())),
                    imported_module: SyntaxFact::Known("crate::a".to_owned()),
                    exported_symbol: SyntaxFact::Unknown,
                },
            ],
            line: 1,
        }
    }

    #[rstest]
    #[case::absolute("crate::a::parse", &["crate::a::parse"])]
    #[case::self_relative("self::parse", &["crate::m::parse"])]
    #[case::super_relative("super::parse", &["crate::parse"])]
    #[case::self_type("Self::helper", &["crate::m::S::helper"])]
    #[case::local_type("S::helper", &["crate::m::S::helper"])]
    #[case::imported_module_alias("a::parse", &["crate::m::a::parse", "crate::a::parse"])]
    #[case::imported_function_alias("parse", &["crate::m::parse", "crate::a::parse"])]
    fn lexical_candidate_generation_is_ordered(#[case] path: &str, #[case] expected: &[&str]) {
        assert_eq!(lexical_candidates(&site(path)), expected);
    }

    #[test]
    fn lexical_path_helpers_handle_boundaries() {
        assert_eq!(
            prefix_with_tail(vec!["crate".to_owned(), "m".to_owned()], &["self"], 1).as_deref(),
            Some("crate::m"),
        );
        assert_eq!(resolve_super_path("crate", &["super", "parse"]), None);
        assert_eq!(
            resolve_super_path("crate::a::b", &["super", "super", "parse"]).as_deref(),
            Some("crate::parse"),
        );
        assert_eq!(join_tail(&["Self"], 1), None);
        assert_eq!(join_tail(&["Self", "parse"], 1).as_deref(), Some("parse"));
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
        assert_eq!(nodes[0]["name"], "helper");
        assert_eq!(nodes[0]["qualified_name"], "crate::helper");
        assert_eq!(nodes[0]["module"], "crate");
        assert_eq!(nodes[0]["impl_owner"], Value::Null);
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

    #[test]
    fn receiver_method_calls_remain_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "struct S;\nimpl S {\n    fn helper(&self) {}\n    fn caller(&self) { self.helper(); }\n}\n",
        );

        let report = analyze_json(dir.path());
        let edges = report["edges"].as_array().unwrap();
        let edge = edges
            .iter()
            .find(|e| e["callee_name"] == "helper")
            .expect("helper method call should be recorded");
        assert_eq!(edge["from"], "src/lib.rs:S::caller:4");
        assert_eq!(edge["to"], Value::Null);
        assert_eq!(edge["resolution"], "unresolved");
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
    fn module_paths_handle_crate_roots_nested_files_and_empty_relative_paths() {
        assert_eq!(module_path_from_relative_file("lib.rs"), "crate");
        assert_eq!(module_path_from_relative_file("main.rs"), "crate");
        assert_eq!(
            module_path_from_relative_file("src/analyze/function_graph.rs"),
            "crate::analyze::function_graph"
        );
        assert_eq!(
            module_path_from_relative_file("src/analyze/mod.rs"),
            "crate::analyze"
        );
        assert_eq!(module_path_from_relative_file(""), "crate");
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
