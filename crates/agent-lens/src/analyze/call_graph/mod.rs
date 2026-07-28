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
    AnalyzePathFilter, AnalyzerError, LineRange, SourceFile, SourceLang, changed_line_ranges,
    collect_source_files, read_source,
};
use model::{
    CallGraphEdge, CallGraphNode, EdgeWeights, GraphLanguage, ModuleResolutionSummary,
    NodeVisibility, NodeWeights, Resolution, ResolutionCallCounts, node_id, node_local_name,
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

/// Match a symbol against the graph: an exact node id wins outright
/// (ids are unique — the escape hatch out of any ambiguity); otherwise
/// every node whose `qualified_name` equals the symbol or ends with
/// `::<symbol>` matches. Matching on segment boundaries keeps `foo`
/// from matching `crate::buffoo`. Shared by every analyzer that takes a
/// `--symbol` / `--function` flag so the matching rules never diverge.
pub(crate) fn match_symbol(graph: &CallGraph, symbol: &str) -> Vec<usize> {
    if let Some(idx) = graph.nodes.iter().position(|node| node.id == symbol) {
        return vec![idx];
    }
    let suffix = format!("::{symbol}");
    graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| node.qualified_name == symbol || node.qualified_name.ends_with(&suffix))
        .map(|(idx, _)| idx)
        .collect()
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

    /// File-collection variant of the path filter: with `--only-tests`
    /// the path-level test filter is disabled here so non-test files
    /// containing test functions are still scanned, and the test
    /// restriction is applied per function instead.
    fn collection_filter(&self) -> AnalyzePathFilter {
        if self.only_tests {
            self.path_filter.clone().with_only_tests(false)
        } else {
            self.path_filter.clone()
        }
    }

    /// Unstaged changed line ranges (`git diff -U0`) for every source
    /// file the graph would scan, keyed by the display path used in
    /// [`CallGraphNode::file`]. Files with no unstaged changes are
    /// absent. Uses the same collection filter as [`Self::build`] so
    /// diff-seeded analyzers see exactly the graph's file set.
    pub(crate) fn changed_line_ranges_by_display_path(
        &self,
        path: &Path,
    ) -> Result<BTreeMap<String, Vec<LineRange>>, AnalyzerError> {
        let filter = self.collection_filter().compile(path)?;
        let mut out = BTreeMap::new();
        for source_file in collect_source_files(path, &filter)? {
            let ranges = changed_line_ranges(&source_file.path);
            if !ranges.is_empty() {
                out.insert(source_file.display_path, ranges);
            }
        }
        Ok(out)
    }

    pub(crate) fn build(&self, path: &Path) -> Result<CallGraph, AnalyzerError> {
        let filter = self.collection_filter().compile(path)?;
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
            let resolved = resolver.resolve(site, file.language);
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
    use model::CallGraphEdge;
    use rstest::rstest;

    /// One language's worth of the receiver-resolution contract: a
    /// receiver call on a name the standard library owns, a receiver
    /// call on a name only the workspace owns, and a typed path call on
    /// the standard-library name. All three name the same workspace
    /// methods, so only the *form* of the call site differs.
    #[derive(Debug, Clone, Copy)]
    struct ReceiverFixture {
        file: &'static str,
        source: &'static str,
        /// Caller of `recv.<ubiquitous>()`, which must not resolve.
        ubiquitous_caller: &'static str,
        ubiquitous_callee: &'static str,
        /// Caller of `recv.<specific>()`, which must resolve.
        specific_caller: &'static str,
        specific_callee: &'static str,
        /// Caller of `W.<ubiquitous>(recv)`, which must resolve: the
        /// path carries the owner the receiver form lacks.
        path_caller: &'static str,
    }

    const RUST_FIXTURE: ReceiverFixture = ReceiverFixture {
        file: "src/lib.rs",
        source: "pub struct W;\n\
                 impl W {\n\
                 pub fn clone(&self) -> W { W }\n\
                 pub fn with_children(&self) -> usize { 0 }\n\
                 }\n\
                 pub fn ubiquitous_receiver(v: &Vec<u8>) -> Vec<u8> { v.clone() }\n\
                 pub fn specific_receiver(w: &W) -> usize { w.with_children() }\n\
                 pub fn path_call(w: &W) -> W { W::clone(w) }\n",
        ubiquitous_caller: "ubiquitous_receiver",
        ubiquitous_callee: "clone",
        specific_caller: "specific_receiver",
        specific_callee: "with_children",
        path_caller: "path_call",
    };

    const TYPESCRIPT_FIXTURE: ReceiverFixture = ReceiverFixture {
        file: "src/lib.ts",
        source: "export class W {\n\
                 static map(w: W): number { return 0; }\n\
                 withChildren(): number { return 1; }\n\
                 }\n\
                 export function ubiquitousReceiver(xs: number[]): number[] { return xs.map(id); }\n\
                 export function specificReceiver(w: W): number { return w.withChildren(); }\n\
                 export function pathCall(w: W): number { return W.map(w); }\n",
        ubiquitous_caller: "ubiquitousReceiver",
        ubiquitous_callee: "map",
        specific_caller: "specificReceiver",
        specific_callee: "withChildren",
        path_caller: "pathCall",
    };

    const PYTHON_FIXTURE: ReceiverFixture = ReceiverFixture {
        file: "src/lib.py",
        source: "class W:\n\
                 \x20   def get(self):\n\
                 \x20       return 0\n\
                 \x20   def with_children(self):\n\
                 \x20       return 1\n\
                 \n\
                 def ubiquitous_receiver(values):\n\
                 \x20   return values.get(\"k\")\n\
                 \n\
                 def specific_receiver(w):\n\
                 \x20   return w.with_children()\n\
                 \n\
                 def path_call(w):\n\
                 \x20   return W.get(w)\n",
        ubiquitous_caller: "ubiquitous_receiver",
        ubiquitous_callee: "get",
        specific_caller: "specific_receiver",
        specific_callee: "with_children",
        path_caller: "path_call",
    };

    const GO_FIXTURE: ReceiverFixture = ReceiverFixture {
        file: "src/lib.go",
        source: "package lib\n\
                 \n\
                 type W struct{}\n\
                 \n\
                 type builder struct{}\n\
                 \n\
                 func (w W) String() string { return \"\" }\n\
                 \n\
                 func (w W) WithChildren() int { return 0 }\n\
                 \n\
                 func UbiquitousReceiver(b builder) string { return b.String() }\n\
                 \n\
                 func SpecificReceiver(w W) int { return w.WithChildren() }\n\
                 \n\
                 func PathCall(w W) string { return W.String(w) }\n",
        ubiquitous_caller: "UbiquitousReceiver",
        ubiquitous_callee: "String",
        specific_caller: "SpecificReceiver",
        specific_callee: "WithChildren",
        path_caller: "PathCall",
    };

    /// The one edge leaving the function named `caller` for a call
    /// named `callee`, as `(resolution, target qualified name)`.
    fn call_outcome<'a>(
        graph: &'a CallGraph,
        caller: &str,
        callee: &str,
    ) -> (Resolution, Option<&'a str>) {
        let by_id: HashMap<&str, &CallGraphNode> =
            graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
        let matched: Vec<&CallGraphEdge> = graph
            .edges
            .iter()
            .filter(|edge| {
                edge.callee_name.as_deref() == Some(callee)
                    && edge
                        .from
                        .as_deref()
                        .and_then(|id| by_id.get(id))
                        .is_some_and(|node| node.name == caller)
            })
            .collect();
        assert_eq!(
            matched.len(),
            1,
            "expected exactly one {caller} -> {callee} edge, got {matched:?}"
        );
        let edge = matched[0];
        (
            edge.resolution,
            edge.to
                .as_deref()
                .and_then(|id| by_id.get(id))
                .map(|node| node.qualified_name.as_str()),
        )
    }

    /// Regression for the over-resolution that made every `.clone()` an
    /// edge into a workspace `W::clone`: a receiver call on a
    /// standard-library method name carries no evidence, so it must
    /// stay unresolved — while the same workspace, reached by a
    /// workspace-specific name or by a typed path, still resolves.
    #[rstest]
    #[case::rust(RUST_FIXTURE)]
    #[case::typescript(TYPESCRIPT_FIXTURE)]
    #[case::python(PYTHON_FIXTURE)]
    #[case::go(GO_FIXTURE)]
    fn receiver_calls_on_ubiquitous_names_do_not_become_workspace_edges(
        #[case] fixture: ReceiverFixture,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), fixture.file, fixture.source);
        let graph = CallGraphBuilder::new().build(dir.path()).unwrap();

        assert_eq!(
            call_outcome(&graph, fixture.ubiquitous_caller, fixture.ubiquitous_callee),
            (Resolution::Unresolved, None),
            "receiver call on the ubiquitous name {}",
            fixture.ubiquitous_callee,
        );

        assert_resolves_to_method(
            call_outcome(&graph, fixture.specific_caller, fixture.specific_callee),
            fixture.specific_callee,
            "receiver call on a workspace-specific name",
        );
        assert_resolves_to_method(
            call_outcome(&graph, fixture.path_caller, fixture.ubiquitous_callee),
            fixture.ubiquitous_callee,
            "typed path call on a ubiquitous name",
        );
    }

    /// One language's take on "a workspace symbol shares a builtin's
    /// name": the builtin is called bare, so it never reaches the
    /// receiver table that guards `.clone()`.
    #[derive(Debug, Clone, Copy)]
    struct BuiltinFixture {
        file: &'static str,
        source: &'static str,
        /// Caller of the bare builtin, which must stay unresolved.
        builtin_caller: &'static str,
        builtin_callee: &'static str,
        /// Caller of a bare workspace call, which must still resolve.
        workspace_caller: &'static str,
        workspace_callee: &'static str,
    }

    const GO_BUILTIN_FIXTURE: BuiltinFixture = BuiltinFixture {
        file: "src/lib.go",
        source: "package lib\n\
                 \n\
                 type buffer struct{ items []int }\n\
                 \n\
                 func (b *buffer) append(v int) { b.items = []int{v} }\n\
                 \n\
                 func Collect(xs []int) []int { return append([]int{}, xs...) }\n\
                 \n\
                 func Specific() int { return helper() }\n\
                 \n\
                 func helper() int { return 0 }\n",
        builtin_caller: "Collect",
        builtin_callee: "append",
        workspace_caller: "Specific",
        workspace_callee: "helper",
    };

    const PYTHON_BUILTIN_FIXTURE: BuiltinFixture = BuiltinFixture {
        file: "src/lib.py",
        source: "class W:\n\
                 \x20   def len(self):\n\
                 \x20       return 0\n\
                 \n\
                 def measure(xs):\n\
                 \x20   return len(xs)\n\
                 \n\
                 def specific():\n\
                 \x20   return helper()\n\
                 \n\
                 def helper():\n\
                 \x20   return 0\n",
        builtin_caller: "measure",
        builtin_callee: "len",
        workspace_caller: "specific",
        workspace_callee: "helper",
    };

    const TYPESCRIPT_BUILTIN_FIXTURE: BuiltinFixture = BuiltinFixture {
        file: "src/lib.ts",
        source: "export class W {\n\
                 static parseInt(s: string): number { return 0; }\n\
                 }\n\
                 export function toNumber(s: string): number { return parseInt(s, 10); }\n\
                 export function specific(): number { return helper(); }\n\
                 export function helper(): number { return 0; }\n",
        builtin_caller: "toNumber",
        builtin_callee: "parseInt",
        workspace_caller: "specific",
        workspace_callee: "helper",
    };

    const RUST_BUILTIN_FIXTURE: BuiltinFixture = BuiltinFixture {
        file: "src/lib.rs",
        source: "pub struct W;\n\
                 impl W {\n\
                 pub fn drop(&self) {}\n\
                 }\n\
                 pub fn release(v: Vec<u8>) { drop(v); }\n\
                 pub fn specific() -> usize { helper() }\n\
                 pub fn helper() -> usize { 0 }\n",
        builtin_caller: "release",
        builtin_callee: "drop",
        workspace_caller: "specific",
        workspace_callee: "helper",
    };

    /// Regression for the plain-call counterpart of the receiver-call
    /// over-resolution: a bare `append(...)` is the language's builtin,
    /// so it must not become an edge into the one workspace symbol that
    /// happens to share the name — while ordinary bare calls into the
    /// workspace keep resolving.
    #[rstest]
    #[case::go(GO_BUILTIN_FIXTURE)]
    #[case::python(PYTHON_BUILTIN_FIXTURE)]
    #[case::typescript(TYPESCRIPT_BUILTIN_FIXTURE)]
    #[case::rust(RUST_BUILTIN_FIXTURE)]
    fn plain_calls_to_builtins_do_not_become_workspace_edges(#[case] fixture: BuiltinFixture) {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), fixture.file, fixture.source);
        let graph = CallGraphBuilder::new().build(dir.path()).unwrap();

        assert_eq!(
            call_outcome(&graph, fixture.builtin_caller, fixture.builtin_callee),
            (Resolution::Unresolved, None),
            "plain call to the builtin {}",
            fixture.builtin_callee,
        );

        let (resolution, target) =
            call_outcome(&graph, fixture.workspace_caller, fixture.workspace_callee);
        assert_eq!(
            resolution,
            Resolution::Resolved,
            "plain call to the workspace function {}",
            fixture.workspace_callee,
        );
        assert!(
            target.is_some_and(|t| t.ends_with(fixture.workspace_callee)),
            "expected a target ending in {}, got {target:?}",
            fixture.workspace_callee,
        );
    }

    /// Asserts a [`call_outcome`] landed on `W::<method>` — the target
    /// is checked by suffix because the module prefix differs per
    /// language.
    fn assert_resolves_to_method(outcome: (Resolution, Option<&str>), method: &str, context: &str) {
        let (resolution, target) = outcome;
        assert_eq!(resolution, Resolution::Resolved, "{context}: {method}");
        let expected_suffix = format!("W::{method}");
        assert!(
            target.is_some_and(|t| t.ends_with(&expected_suffix)),
            "{context}: expected a target ending in {expected_suffix}, got {target:?}"
        );
    }

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
