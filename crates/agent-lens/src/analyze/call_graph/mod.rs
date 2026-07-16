//! Shared static call-graph substrate.
//!
//! Builds a per-function call graph for Rust / TypeScript / Python /
//! Go from the lens adapters' `FunctionShape` / `CallShape` /
//! `FunctionComplexity` facts. `analyze function-graph` serializes the
//! graph verbatim; the planned analyzer family (hubs, cycles, impact,
//! …) consumes the same [`CallGraph`] plus the traversal algorithms in
//! [`algo`] instead of re-deriving the pipeline per analyzer.
//!
//! The graph is static and heuristic: no type inference, macro
//! expansion, cross-crate resolution, runtime timing, or git history
//! traversal is attempted here.

#[allow(dead_code)] // Exercised by tests; consumed by the upcoming analyzer family (#316).
pub(crate) mod algo;
pub(crate) mod model;
pub(crate) mod module_path;
pub(crate) mod resolve;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use lens_domain::{CallShape, FunctionComplexity, FunctionShape};
use lens_rust::CallIndexOptions;

use super::cargo_meta::CrateNameCache;
use super::{
    AnalyzePathFilter, AnalyzerError, SourceFile, SourceLang, collect_source_files, read_source,
};
use model::{
    CallGraphEdge, CallGraphNode, EdgeWeights, ModuleResolutionSummary, NodeVisibility,
    NodeWeights, Resolution, ResolutionCallCounts, node_id, node_local_name,
};
use resolve::{CallerIndex, Resolver};

/// The assembled call graph: deterministic node and edge orderings,
/// static degrees applied, per-module resolution confidence attached.
#[derive(Debug, Clone)]
pub(crate) struct CallGraph {
    /// Label of the (single) source language, or `"mixed"` /
    /// `"unknown"`.
    pub(crate) language: &'static str,
    pub(crate) nodes: Vec<CallGraphNode>,
    pub(crate) edges: Vec<CallGraphEdge>,
    pub(crate) module_summary: Vec<ModuleResolutionSummary>,
}

impl CallGraph {
    pub(crate) fn build(files: Vec<FileGraphInput>) -> Self {
        let mut nodes = build_nodes(&files);
        let (mut edges, module_summary) = build_edges(&files, &nodes);
        apply_static_degrees(&mut nodes, &edges);
        edges.sort_by(|a, b| {
            a.from
                .cmp(&b.from)
                .then_with(|| a.to.cmp(&b.to))
                .then_with(|| a.callee_name.cmp(&b.callee_name))
                .then_with(|| a.resolution.cmp(&b.resolution))
                .then_with(|| a.resolution_method.cmp(&b.resolution_method))
                .then_with(|| a.candidates.cmp(&b.candidates))
        });
        Self {
            language: graph_language_label(&files),
            nodes,
            edges,
            module_summary,
        }
    }

    /// Map node ids to their index in `nodes`.
    #[allow(dead_code)] // Exercised by tests; consumed by the upcoming analyzer family (#316).
    pub(crate) fn node_index_by_id(&self) -> HashMap<&str, usize> {
        self.nodes
            .iter()
            .enumerate()
            .map(|(idx, node)| (node.id.as_str(), idx))
            .collect()
    }

    /// Adjacency over resolved edges only, by node index, neighbor
    /// lists sorted and deduplicated. This is the traversal substrate
    /// for [`algo::condense`] / [`algo::bfs`]; unresolved, ambiguous,
    /// and anonymous edges are invisible to it — consult
    /// [`CallGraphNode::outgoing_calls`] and
    /// [`CallGraph::module_summary`] for how much of the graph that
    /// hides.
    #[allow(dead_code)] // Exercised by tests; consumed by the upcoming analyzer family (#316).
    pub(crate) fn resolved_adjacency(&self) -> Vec<Vec<usize>> {
        let index_by_id = self.node_index_by_id();
        let mut adjacency: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        for edge in &self.edges {
            if edge.resolution != Resolution::Resolved {
                continue;
            }
            if let (Some(from), Some(to)) = (edge.from.as_deref(), edge.to.as_deref())
                && let (Some(&f), Some(&t)) = (index_by_id.get(from), index_by_id.get(to))
            {
                adjacency[f].push(t);
            }
        }
        for neighbors in &mut adjacency {
            neighbors.sort_unstable();
            neighbors.dedup();
        }
        adjacency
    }
}

/// Scans source trees into [`FileGraphInput`]s and assembles the
/// [`CallGraph`]. Shared by `analyze function-graph` and the analyzer
/// family so test/path filtering semantics stay identical everywhere.
#[derive(Debug, Default, Clone)]
pub(crate) struct CallGraphBuilder {
    only_tests: bool,
    exclude_tests: bool,
    path_filter: AnalyzePathFilter,
}

impl CallGraphBuilder {
    pub(crate) fn new() -> Self {
        Self {
            only_tests: false,
            exclude_tests: false,
            path_filter: AnalyzePathFilter::new(),
        }
    }

    pub(crate) fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.only_tests = only_tests;
        self.path_filter = self.path_filter.with_only_tests(only_tests);
        self
    }

    pub(crate) fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.exclude_tests = exclude_tests;
        self.path_filter = self.path_filter.with_exclude_tests(exclude_tests);
        self
    }

    pub(crate) fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.path_filter = self.path_filter.with_exclude_patterns(exclude);
        self
    }

    pub(crate) fn build(&self, path: &Path) -> Result<CallGraph, AnalyzerError> {
        let collection_filter = if self.only_tests {
            self.path_filter.clone().with_only_tests(false)
        } else {
            self.path_filter.clone()
        };
        let filter = collection_filter.compile(path)?;
        let mut files = Vec::new();
        let mut crate_cache = CrateNameCache::new();
        for source_file in collect_source_files(path, &filter)? {
            if !matches!(
                SourceLang::from_path(&source_file.path),
                Some(
                    SourceLang::Rust
                        | SourceLang::TypeScript(_)
                        | SourceLang::Python
                        | SourceLang::Go
                )
            ) {
                continue;
            }
            let path_is_test = filter.is_test_path(&source_file.path);
            files.push(self.scan_file(path, &source_file, path_is_test, &mut crate_cache)?);
        }
        Ok(CallGraph::build(files))
    }

    fn scan_file(
        &self,
        root: &Path,
        file: &SourceFile,
        path_is_test: bool,
        crate_cache: &mut CrateNameCache,
    ) -> Result<FileGraphInput, AnalyzerError> {
        let (lang, source) = read_source(&file.path)?;
        let crate_info = match lang {
            SourceLang::Rust => Some(crate_cache.lookup(&file.path)),
            _ => None,
        };
        let module = module_path::module_path_for(root, file, lang, crate_info.as_ref(), &source);
        let mut functions = extract_function_shapes(lang, &source, &module)?;
        functions.retain(|f| self.includes_function(f, path_is_test));
        let calls = extract_call_shapes(lang, &source, &module, !self.exclude_tests)?;
        let complexity = extract_complexity(lang, &source)?;

        Ok(FileGraphInput {
            file: file.display_path.clone(),
            language: lang.graph_language(),
            module,
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

fn parse_err<E>(e: E) -> AnalyzerError
where
    E: std::error::Error + Send + Sync + 'static,
{
    AnalyzerError::Parse(Box::new(e))
}

fn extract_function_shapes(
    lang: SourceLang,
    source: &str,
    module: &str,
) -> Result<Vec<FunctionShape>, AnalyzerError> {
    match lang {
        SourceLang::Rust => {
            lens_rust::extract_function_shapes_with_modules(source, module).map_err(parse_err)
        }
        SourceLang::TypeScript(dialect) => {
            lens_ts::extract_function_shapes_with_module(source, dialect, module).map_err(parse_err)
        }
        SourceLang::Python => {
            lens_py::extract_function_shapes_with_module(source, module).map_err(parse_err)
        }
        SourceLang::Go => {
            lens_golang::extract_function_shapes_with_module(source, module).map_err(parse_err)
        }
    }
}

fn extract_call_shapes(
    lang: SourceLang,
    source: &str,
    module: &str,
    include_cfg_test_blocks: bool,
) -> Result<Vec<CallShape>, AnalyzerError> {
    match lang {
        SourceLang::Rust => lens_rust::extract_call_shapes_with_options_and_base_module(
            source,
            CallIndexOptions {
                include_cfg_test_blocks,
            },
            module,
        )
        .map_err(parse_err),
        SourceLang::TypeScript(dialect) => {
            lens_ts::extract_call_shapes_with_module(source, dialect, module).map_err(parse_err)
        }
        SourceLang::Python => {
            lens_py::extract_call_shapes_with_module(source, module).map_err(parse_err)
        }
        SourceLang::Go => {
            lens_golang::extract_call_shapes_with_module(source, module).map_err(parse_err)
        }
    }
}

fn extract_complexity(
    lang: SourceLang,
    source: &str,
) -> Result<Vec<FunctionComplexity>, AnalyzerError> {
    match lang {
        SourceLang::Rust => lens_rust::extract_complexity_units(source).map_err(parse_err),
        SourceLang::TypeScript(dialect) => {
            lens_ts::extract_complexity_units(source, dialect).map_err(parse_err)
        }
        SourceLang::Python => lens_py::extract_complexity_units(source).map_err(parse_err),
        SourceLang::Go => lens_golang::extract_complexity_units(source).map_err(parse_err),
    }
}

/// Everything the graph needs from one scanned source file.
pub(crate) struct FileGraphInput {
    pub(crate) file: String,
    pub(crate) language: GraphLanguage,
    /// Base module path of the file (functions may live in nested
    /// inline modules below it).
    pub(crate) module: String,
    pub(crate) path_is_test: bool,
    pub(crate) functions: Vec<FunctionShape>,
    pub(crate) calls: Vec<CallShape>,
    pub(crate) complexity: Vec<FunctionComplexity>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphLanguage {
    Rust,
    TypeScript,
    Python,
    Go,
}

impl GraphLanguage {
    fn label(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
            Self::Python => "python",
            Self::Go => "go",
        }
    }
}

impl SourceLang {
    pub(crate) fn graph_language(self) -> GraphLanguage {
        match self {
            Self::Rust => GraphLanguage::Rust,
            Self::TypeScript(_) => GraphLanguage::TypeScript,
            Self::Python => GraphLanguage::Python,
            Self::Go => GraphLanguage::Go,
        }
    }
}

fn graph_language_label(files: &[FileGraphInput]) -> &'static str {
    let mut langs = files.iter().map(|file| file.language);
    let Some(first) = langs.next() else {
        return "unknown";
    };
    if langs.all(|lang| lang == first) {
        first.label()
    } else {
        "mixed"
    }
}

fn build_nodes(files: &[FileGraphInput]) -> Vec<CallGraphNode> {
    let mut nodes = Vec::new();
    for file in files {
        let complexity = ComplexityIndex::new(&file.complexity);
        for f in &file.functions {
            let metrics = complexity.get(f);
            nodes.push(CallGraphNode {
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
                visibility: NodeVisibility::from_shape(&f.visibility),
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
                outgoing_calls: ResolutionCallCounts::default(),
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

fn build_edges(
    files: &[FileGraphInput],
    nodes: &[CallGraphNode],
) -> (Vec<CallGraphEdge>, Vec<ModuleResolutionSummary>) {
    let resolver = Resolver::new(nodes);
    let caller_index = CallerIndex::new(nodes);
    let mut grouped: BTreeMap<EdgeKey, CallGraphEdge> = BTreeMap::new();
    let mut by_module: BTreeMap<String, ResolutionCallCounts> = BTreeMap::new();

    for file in files {
        for site in &file.calls {
            let from = site
                .caller_qualified_name()
                .and_then(|caller| caller_index.resolve_in_file(&file.file, caller));
            if site.caller_qualified_name().is_some() && from.is_none() {
                continue;
            }
            let resolved = resolver.resolve(site);
            let module = site
                .caller_module()
                .unwrap_or(file.module.as_str())
                .to_owned();
            by_module
                .entry(module)
                .or_default()
                .record(resolved.resolution, 1);
            let key = EdgeKey {
                from: from.clone(),
                to: resolved.to.clone(),
                callee_name: site.callee_name().map(ToOwned::to_owned),
                resolution: resolved.resolution,
                candidates: resolved.candidates.clone(),
            };
            let entry = grouped.entry(key).or_insert_with(|| CallGraphEdge {
                from,
                to: resolved.to,
                callee_name: site.callee_name().map(ToOwned::to_owned),
                resolution: resolved.resolution,
                candidates: resolved.candidates,
                resolution_method: None,
                call_count: 0,
                call_lines: Vec::new(),
                weights: EdgeWeights::default(),
            });
            entry.call_count += 1;
            entry.weights.call_count += 1;
            entry.call_lines.push(site.line);
            // Call sites can reach the same aggregation key through
            // different heuristics (`helper()` lexically and
            // `crate::helper()` via path suffix). Keep the most direct
            // strategy — the [`ResolutionMethod`] Ord — so provenance
            // stays deterministic without splitting v1 edges.
            entry.resolution_method = match (entry.resolution_method, resolved.method) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
    }

    let edges = grouped
        .into_values()
        .map(|mut edge| {
            edge.call_lines.sort_unstable();
            edge.call_lines.dedup();
            edge
        })
        .collect();
    let module_summary = by_module
        .into_iter()
        .map(|(module, calls)| ModuleResolutionSummary {
            module,
            total_call_count: calls.total(),
            calls,
        })
        .collect();
    (edges, module_summary)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EdgeKey {
    from: Option<String>,
    to: Option<String>,
    callee_name: Option<String>,
    resolution: Resolution,
    candidates: Vec<String>,
}

fn apply_static_degrees(nodes: &mut [CallGraphNode], edges: &[CallGraphEdge]) {
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
            nodes[idx]
                .outgoing_calls
                .record(edge.resolution, edge.call_count);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;

    /// End-to-end substrate check: build the graph from source, take
    /// the resolved adjacency, and run the traversal algorithms the
    /// analyzer family will use.
    #[test]
    fn call_graph_substrate_supports_traversal_and_condensation() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn a() { b(); }\nfn b() { a(); c(); }\nfn c() {}\n",
        );

        let graph = CallGraphBuilder::new().build(dir.path()).unwrap();
        assert_eq!(graph.language, "rust");
        let index = graph.node_index_by_id();
        let a = index["src/lib.rs:a:1"];
        let b = index["src/lib.rs:b:2"];
        let c = index["src/lib.rs:c:3"];

        let adjacency = graph.resolved_adjacency();
        assert_eq!(adjacency[a], vec![b]);
        assert_eq!(adjacency[b], vec![a, c]);
        assert!(adjacency[c].is_empty());

        let condensation = algo::condense(&adjacency);
        assert_eq!(condensation.components, vec![vec![c], vec![a, b]]);

        let callers_of_c: Vec<usize> = algo::reverse_bfs(&adjacency, &[c])
            .into_iter()
            .map(|v| v.node)
            .collect();
        assert_eq!(callers_of_c, vec![c, b, a]);
    }

    #[test]
    fn per_module_summary_counts_call_sites_by_resolution() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn known() {} fn caller() { known(); external(); } }\n\
             mod b { fn caller() { crate::a::known(); } }\n",
        );

        let graph = CallGraphBuilder::new().build(dir.path()).unwrap();
        let summary: Vec<(&str, usize, usize, usize)> = graph
            .module_summary
            .iter()
            .map(|m| {
                (
                    m.module.as_str(),
                    m.calls.resolved_call_count,
                    m.calls.unresolved_call_count,
                    m.total_call_count,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [("crate::a", 1, 1, 2), ("crate::b", 1, 0, 1)],
            "got {summary:?}"
        );
    }
}
