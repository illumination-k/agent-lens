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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use lens_domain::{CallShape, FunctionComplexity, FunctionShape, InterfaceShape, WrapperFinding};
use lens_rust::CallIndexOptions;
use rayon::prelude::*;

use super::cargo_meta::CrateNameCache;
use super::index::{AnalysisIndex, SourceKey};
use super::{
    AnalyzePathFilter, AnalyzeRoots, AnalyzerError, DiffScope, LineRange, SourceFile, SourceLang,
    changed_line_ranges, collect_source_files, read_source, skip_parse_error_if_walked,
};
use model::{
    CallGraphEdge, CallGraphNode, DelegationFacts, EdgeWeights, GraphLanguage,
    ModuleResolutionSummary, NodeVisibility, NodeWeights, Resolution, ResolutionCallCounts,
    node_id, node_local_name,
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
    /// Interface method sets declared in the scanned files. Empty
    /// unless the graph was built with
    /// [`CallGraphBuilder::with_interface_facts`]; only the Go adapter
    /// extracts them today.
    pub(crate) interfaces: Vec<InterfaceShape>,
}

impl CallGraph {
    pub(crate) fn build(files: Vec<FileGraphInput>, argument_facts: bool) -> Self {
        let mut nodes = build_nodes(&files);
        let (mut edges, module_summary) = build_edges(&files, &nodes, argument_facts);
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
            interfaces: collect_interfaces(files),
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

    /// Reverse of [`Self::resolved_adjacency`], as distinct caller sets
    /// keyed by callee index: who *could be observed* calling each
    /// function. Self-recursion is excluded — a function is not its own
    /// caller for the purpose of "who needs this?" questions — and
    /// callees with no resolved caller are absent rather than present
    /// with an empty set, so `get` returning `None` and an empty set
    /// mean the same thing.
    ///
    /// Only resolved edges contribute, so a caller set is a lower bound:
    /// an ambiguous or unresolved call site is invisible here. Consult
    /// [`Self::module_summary`] for how much that hides.
    pub(crate) fn resolved_callers(&self) -> BTreeMap<usize, BTreeSet<usize>> {
        let index_by_id = self.node_index_by_id();
        let mut callers: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
        for edge in &self.edges {
            if edge.resolution != Resolution::Resolved {
                continue;
            }
            let (Some(&from), Some(&to)) = (
                edge.from.as_deref().and_then(|id| index_by_id.get(id)),
                edge.to.as_deref().and_then(|id| index_by_id.get(id)),
            ) else {
                continue;
            };
            if from != to {
                callers.entry(to).or_default().insert(from);
            }
        }
        callers
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
///
/// `Eq`/`Hash` cover the whole configuration because a builder clone
/// paired with its root set is the [`AnalysisIndex`] key under which an
/// assembled graph is shared between analyzers.
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub(crate) struct CallGraphBuilder {
    only_tests: bool,
    exclude_tests: bool,
    delegation_facts: bool,
    interface_facts: bool,
    argument_facts: bool,
    path_filter: AnalyzePathFilter,
}

impl CallGraphBuilder {
    pub(crate) fn new() -> Self {
        Self {
            only_tests: false,
            exclude_tests: false,
            delegation_facts: false,
            interface_facts: false,
            argument_facts: false,
            path_filter: AnalyzePathFilter::new(),
        }
    }

    /// Attach [`DelegationFacts`] to every node. Off by default: the
    /// `pass_through` fact runs each language's thin-wrapper detector,
    /// which is one more parse per file than the graph itself needs.
    pub(crate) fn with_delegation_facts(mut self, delegation_facts: bool) -> Self {
        self.delegation_facts = delegation_facts;
        self
    }

    /// Collect [`CallGraph::interfaces`] from the scanned files. Off by
    /// default for the same reason as delegation facts: the extraction
    /// is one more parse per (Go) file, and only the visibility
    /// analyzer reads the result.
    pub(crate) fn with_interface_facts(mut self, interface_facts: bool) -> Self {
        self.interface_facts = interface_facts;
        self
    }

    /// Attach per-call-site [`model::CallSiteFacts`] to every edge. Off
    /// by default: the adapters extract argument shapes during their one
    /// parse either way, but copying them onto edges holds every
    /// argument list of the workspace in the assembled graph, and only
    /// the parameters analyzer reads them.
    pub(crate) fn with_argument_facts(mut self, argument_facts: bool) -> Self {
        self.argument_facts = argument_facts;
        self
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
    ///
    /// Exposed to the crate so an analyzer that walks the corpus itself
    /// *and* consumes the graph — `search` — can scan exactly the file
    /// set the graph will see, instead of maintaining a second filter
    /// that can drift from this one.
    pub(crate) fn collection_filter(&self) -> AnalyzePathFilter {
        if self.only_tests {
            self.path_filter.clone().with_only_tests(false)
        } else {
            self.path_filter.clone()
        }
    }

    /// Changed line ranges (`git diff -U0`) under `scope` for every
    /// source file the graph would scan, keyed by the display path used
    /// in [`CallGraphNode::file`]. Files with no changes are absent.
    /// Uses the same collection filter as [`Self::build`] so diff-seeded
    /// analyzers see exactly the graph's file set.
    pub(crate) fn changed_line_ranges_by_display_path(
        &self,
        roots: &AnalyzeRoots,
        scope: &DiffScope,
    ) -> Result<BTreeMap<String, Vec<LineRange>>, AnalyzerError> {
        let filter = self.collection_filter().compile(roots.base())?;
        let mut out = BTreeMap::new();
        for source_file in collect_source_files(roots, &filter)? {
            let ranges = changed_line_ranges(&source_file.path, scope);
            if !ranges.is_empty() {
                out.insert(source_file.display_path, ranges);
            }
        }
        Ok(out)
    }

    /// Build the graph, sharing the result through the active
    /// [`AnalysisIndex`] when one is installed: two analyzers with the
    /// same configuration and roots then assemble the graph once. The
    /// per-file extractions inside [`Self::scan_file`] go through the
    /// index too, so even builds that differ only in a fact flag
    /// (delegation, interfaces) re-parse nothing.
    ///
    /// Returned behind an `Arc` so a shared graph is handed out without
    /// cloning its node and edge tables; analyzers only ever read it.
    pub(crate) fn build(&self, roots: &AnalyzeRoots) -> Result<Arc<CallGraph>, AnalyzerError> {
        match AnalysisIndex::active() {
            Some(index) => {
                index.call_graph((self.clone(), roots.clone()), || self.build_uncached(roots))
            }
            None => self.build_uncached(roots).map(Arc::new),
        }
    }

    fn build_uncached(&self, roots: &AnalyzeRoots) -> Result<CallGraph, AnalyzerError> {
        let filter = self.collection_filter().compile(roots.base())?;
        let sources: Vec<SourceFile> = collect_source_files(roots, &filter)?
            .into_iter()
            .filter(|source_file| graphed_language(&source_file.path))
            .collect();
        // Scanning parses every file, so it fans out across rayon
        // workers. The active analysis index lives in a thread local
        // the workers cannot see, so it is captured here and
        // re-installed around each task; results come back in input
        // order (indexed parallel collect) and errors are reduced in
        // that same order, so the graph — and the error a bad file
        // produces — match the sequential walk's exactly.
        let index = AnalysisIndex::active();
        let crate_cache = Mutex::new(CrateNameCache::new());
        let files = sources
            .par_iter()
            .map(|source_file| {
                super::index::with_installed(index.as_ref(), || {
                    let path_is_test = filter.is_test_path(&source_file.path);
                    // A walked file that fails to parse is dropped from
                    // the graph with a warning instead of failing the
                    // build, so one file of too-new syntax cannot take
                    // down every call-graph analyzer.
                    skip_parse_error_if_walked(
                        source_file,
                        self.scan_file(roots.base(), source_file, path_is_test, &crate_cache),
                    )
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect();
        Ok(CallGraph::build(files, self.argument_facts))
    }

    /// Hand every source file the graph would scan to `visit`, as its
    /// display path (the spelling [`CallGraphNode::file`] uses) and its
    /// text.
    ///
    /// The text is read a second time rather than kept from
    /// [`Self::build`]: only the reachability analyzer wants it, and
    /// holding every file of a workspace in memory to spare it one
    /// re-read would be paid for by every other analyzer. Returns the
    /// number of files visited.
    pub(crate) fn visit_source_texts(
        &self,
        roots: &AnalyzeRoots,
        mut visit: impl FnMut(&str, &str),
    ) -> Result<usize, AnalyzerError> {
        let filter = self.collection_filter().compile(roots.base())?;
        let mut visited = 0;
        for source_file in collect_source_files(roots, &filter)? {
            if !graphed_language(&source_file.path) {
                continue;
            }
            let (_, source) = read_source(&source_file.path)?;
            visit(&source_file.display_path, &source);
            visited += 1;
        }
        Ok(visited)
    }

    fn scan_file(
        &self,
        root: &Path,
        file: &SourceFile,
        path_is_test: bool,
        crate_cache: &Mutex<CrateNameCache>,
    ) -> Result<FileGraphInput, AnalyzerError> {
        let (lang, source) = read_source(&file.path)?;
        let crate_info = match lang {
            SourceLang::Rust => Some(
                crate_cache
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .lookup(&file.path),
            ),
            _ => None,
        };
        let module = module_path::module_path_for(root, file, lang, crate_info.as_ref(), &source);
        // Every fact below sits behind an `Arc` so a fact served from
        // the analysis index is shared, not deep-cloned. The test
        // filter therefore becomes the `included` mask instead of a
        // `retain` on an owned list.
        let functions = extract_function_shapes(lang, &source, &module)?;
        let included = functions
            .iter()
            .map(|f| self.includes_function(f, path_is_test))
            .collect();
        let calls = extract_call_shapes(lang, &source, &module, !self.exclude_tests)?;
        // Complexity (node weights) and wrapper findings (delegation
        // facts) are the same facts `analyze complexity` and `analyze
        // wrapper` report, so both go through the shared index helpers.
        let complexity = super::index::shared_complexity_units(lang, &source)?;
        let wrappers = self
            .delegation_facts
            .then(|| super::index::shared_wrapper_findings(lang, &source))
            .transpose()?;
        let interfaces = (self.interface_facts && matches!(lang, SourceLang::Go))
            .then(|| extract_interfaces(lang, &source, &module))
            .transpose()?
            .unwrap_or_default();

        Ok(FileGraphInput {
            file: file.display_path.clone(),
            language: lang.graph_language(),
            module,
            path_is_test,
            functions,
            included,
            calls,
            complexity,
            wrappers,
            interfaces,
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

/// Generate the three path/test filter builders every call-graph
/// analyzer exposes, forwarding to a [`CallGraphBuilder`]-typed field.
/// The per-analyzer copies differed only in their doc comments, which the
/// macro takes as arguments so an analyzer can still explain what the
/// flag means for its particular report:
///
/// ```ignore
/// delegate_call_graph_builders! {
///     builder,
///     /// Test functions are never audited, so this leaves nothing to report.
///     only_tests,
///     exclude_tests,
/// }
/// ```
///
/// Analyzers that also need a flag's value at report time (to phrase
/// the output differently, say) name the field to mirror it into:
/// `only_tests => only_tests`.
macro_rules! delegate_call_graph_builders {
    (
        $field:ident,
        $(#[$only_tests_doc:meta])* only_tests $(=> $only_tests_mirror:ident)?,
        $(#[$exclude_tests_doc:meta])* exclude_tests $(=> $exclude_tests_mirror:ident)?,
    ) => {
        $(#[$only_tests_doc])*
        pub fn with_only_tests(mut self, only_tests: bool) -> Self {
            $(self.$only_tests_mirror = only_tests;)?
            self.$field = self.$field.with_only_tests(only_tests);
            self
        }

        $(#[$exclude_tests_doc])*
        pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
            $(self.$exclude_tests_mirror = exclude_tests;)?
            self.$field = self.$field.with_exclude_tests(exclude_tests);
            self
        }

        pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
            self.$field = self.$field.with_exclude_patterns(exclude);
            self
        }
    };
}

pub(crate) use delegate_call_graph_builders;

/// Whether the file's extension maps to a language the graph is built
/// from. Other supported extensions exist (the per-file analyzers read
/// them), so this is the graph's own narrower gate.
fn graphed_language(path: &Path) -> bool {
    matches!(
        SourceLang::from_path(path),
        Some(SourceLang::Rust | SourceLang::TypeScript(_) | SourceLang::Python | SourceLang::Go)
    )
}

fn parse_err<E>(e: E) -> AnalyzerError
where
    E: std::error::Error + Send + Sync + 'static,
{
    AnalyzerError::Parse(Box::new(e))
}

// Each extraction below consults the active [`AnalysisIndex`] before
// parsing, keyed by the source *content*, so any two graph builds — and
// the per-file analyzers that extract the same facts — share one parse
// per file however their configurations differ. Without an active
// index they are plain extractions.

fn extract_function_shapes(
    lang: SourceLang,
    source: &str,
    module: &str,
) -> Result<Arc<Vec<FunctionShape>>, AnalyzerError> {
    match AnalysisIndex::active() {
        Some(index) => index.function_shapes(SourceKey::new(lang, source), module, || {
            extract_function_shapes_uncached(lang, source, module)
        }),
        None => extract_function_shapes_uncached(lang, source, module).map(Arc::new),
    }
}

fn extract_function_shapes_uncached(
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
) -> Result<Arc<Vec<CallShape>>, AnalyzerError> {
    match AnalysisIndex::active() {
        Some(index) => index.call_shapes(
            SourceKey::new(lang, source),
            module,
            include_cfg_test_blocks,
            || extract_call_shapes_uncached(lang, source, module, include_cfg_test_blocks),
        ),
        None => extract_call_shapes_uncached(lang, source, module, include_cfg_test_blocks)
            .map(Arc::new),
    }
}

fn extract_call_shapes_uncached(
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

/// Interface method sets for one (Go) file. Only collected when the
/// builder was asked for interface facts.
fn extract_interfaces(
    lang: SourceLang,
    source: &str,
    module: &str,
) -> Result<Arc<Vec<InterfaceShape>>, AnalyzerError> {
    match AnalysisIndex::active() {
        Some(index) => index.interface_shapes(SourceKey::new(lang, source), module, || {
            extract_interfaces_uncached(source, module)
        }),
        None => extract_interfaces_uncached(source, module).map(Arc::new),
    }
}

fn extract_interfaces_uncached(
    source: &str,
    module: &str,
) -> Result<Vec<InterfaceShape>, AnalyzerError> {
    lens_golang::extract_interface_shapes_with_module(source, module).map_err(parse_err)
}

/// Everything the graph needs from one scanned source file.
///
/// The fact lists sit behind `Arc`s because they may be shared with the
/// [`AnalysisIndex`] (and through it with other builds and analyzers);
/// the graph only reads them. What is build-specific — which functions
/// the test filter admits — lives in the parallel `included` mask.
pub(crate) struct FileGraphInput {
    pub(crate) file: String,
    pub(crate) language: GraphLanguage,
    /// Base module path of the file (functions may live in nested
    /// inline modules below it).
    pub(crate) module: String,
    pub(crate) path_is_test: bool,
    pub(crate) functions: Arc<Vec<FunctionShape>>,
    /// Parallel to `functions`: whether the builder's test filter
    /// admits each one as a graph node.
    pub(crate) included: Vec<bool>,
    pub(crate) calls: Arc<Vec<CallShape>>,
    pub(crate) complexity: Arc<Vec<FunctionComplexity>>,
    /// Thin-wrapper findings for this file, or `None` when delegation
    /// facts were not requested.
    pub(crate) wrappers: Option<Arc<Vec<WrapperFinding>>>,
    /// Interface declarations in this file. Empty unless interface
    /// facts were requested (and the language extracts them).
    pub(crate) interfaces: Arc<Vec<InterfaceShape>>,
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

impl CallGraphNode {
    /// The graph language of this node, from its file extension, or
    /// `None` for a path no language claims — which cannot happen for a
    /// node the graph built.
    pub(crate) fn graph_language(&self) -> Option<GraphLanguage> {
        SourceLang::from_path(Path::new(&self.file)).map(SourceLang::graph_language)
    }
}

/// Flatten the per-file interface declarations into one deterministic
/// list, sorted by qualified name (declarations shadowing each other
/// across files stay distinct entries — a method set is a method set).
fn collect_interfaces(files: Vec<FileGraphInput>) -> Vec<InterfaceShape> {
    let mut interfaces: Vec<InterfaceShape> = files
        .into_iter()
        .flat_map(|file| file.interfaces.iter().cloned().collect::<Vec<_>>())
        .collect();
    interfaces.sort_by(|a, b| {
        a.qualified_name
            .known_value()
            .cmp(&b.qualified_name.known_value())
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
    interfaces
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
        let wrappers = file
            .wrappers
            .as_ref()
            .map(|w| WrapperIndex::new(w.as_slice()));
        for (f, &included) in file.functions.iter().zip(&file.included) {
            if !included {
                continue;
            }
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
                owner_kind: f
                    .owner
                    .known_value()
                    .and_then(|owner| owner.as_ref())
                    .map(|owner| owner.kind),
                start_line: f.span.start_line,
                end_line: f.span.end_line,
                is_test: f.is_test || file.path_is_test,
                visibility: NodeVisibility::from_shape(&f.visibility),
                param_count: f.signature_shape().map(|s| s.parameter_count()),
                param_names: f.signature_shape().map(|s| {
                    s.params
                        .iter()
                        .map(|p| p.name.known_value().cloned().flatten())
                        .collect()
                }),
                has_receiver: f
                    .signature_shape()
                    .and_then(|s| {
                        s.receiver_shape()
                            .map(|shape| shape != lens_domain::ReceiverShape::None)
                    })
                    .or_else(|| {
                        // The Go adapter records the owner but leaves the
                        // receiver fact unknown: a method owner *is* the
                        // receiver there, and a known absence of an owner
                        // is a known absence of a receiver.
                        f.owner.known_value().map(|owner| {
                            matches!(owner, Some(o) if o.kind == lens_domain::OwnerKind::Receiver)
                        })
                    }),
                attributes: f.attributes.known_value().cloned(),
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
                delegation: wrappers
                    .as_ref()
                    .map(|wrappers| delegation_facts(f, wrappers)),
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

fn delegation_facts(f: &FunctionShape, wrappers: &WrapperIndex<'_>) -> DelegationFacts {
    DelegationFacts {
        statement_count: body_statement_count(f),
        pass_through: wrappers.contains(f),
        deprecated_doc: f.doc.as_deref().is_some_and(doc_says_deprecated),
    }
}

/// Root labels the adapters give a function body whose children are its
/// statements: Rust, Python, and Go emit `"Block"`, TypeScript emits
/// `"FunctionBody"`.
const BODY_BLOCK_LABELS: [&str; 2] = ["Block", "FunctionBody"];

/// Top-level statements in a body, when the adapter emitted one of the
/// statement blocks in [`BODY_BLOCK_LABELS`]. Anything else is `None` —
/// an unrecognised body shape is "cannot tell", and the delegation
/// analyzer treats that as a reason not to classify.
fn body_statement_count(f: &FunctionShape) -> Option<usize> {
    let body = f.body_tree();
    BODY_BLOCK_LABELS
        .contains(&body.label.as_str())
        .then_some(body.children.len())
}

/// Whether a doc comment announces the function as deprecated. Matching
/// the word anywhere in the doc is deliberately loose: the exemption is
/// there to keep the report off code already on its way out, and a
/// false exemption only costs a finding.
fn doc_says_deprecated(doc: &str) -> bool {
    doc.to_lowercase().contains("deprecated")
}

/// File-local lookup of thin-wrapper findings by the same
/// `Owner::name` spelling node ids use.
///
/// The join is by name plus overlapping lines rather than an exact
/// start line: a [`WrapperFinding`] spans signature-to-body while a
/// [`FunctionShape`] span can start earlier (attributes, doc comments),
/// and the two only have to agree on *which* function they describe.
struct WrapperIndex<'a> {
    by_name: HashMap<&'a str, Vec<&'a WrapperFinding>>,
}

impl<'a> WrapperIndex<'a> {
    fn new(findings: &'a [WrapperFinding]) -> Self {
        let mut by_name: HashMap<&'a str, Vec<&'a WrapperFinding>> = HashMap::new();
        for finding in findings {
            by_name
                .entry(finding.name.as_str())
                .or_default()
                .push(finding);
        }
        Self { by_name }
    }

    fn contains(&self, f: &FunctionShape) -> bool {
        let name = node_local_name(f);
        self.by_name.get(name.as_str()).is_some_and(|findings| {
            findings
                .iter()
                .any(|w| w.start_line <= f.span.end_line && f.span.start_line <= w.end_line)
        })
    }
}

fn build_edges(
    files: &[FileGraphInput],
    nodes: &[CallGraphNode],
    argument_facts: bool,
) -> (Vec<CallGraphEdge>, Vec<ModuleResolutionSummary>) {
    let resolver = Resolver::new(nodes);
    let caller_index = CallerIndex::new(nodes);
    let mut grouped: BTreeMap<EdgeKey, CallGraphEdge> = BTreeMap::new();
    let mut by_module: BTreeMap<String, ResolutionCallCounts> = BTreeMap::new();

    for file in files {
        for site in file.calls.iter() {
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
                call_sites: Vec::new(),
            });
            entry.call_count += 1;
            entry.weights.call_count += 1;
            entry.call_lines.push(site.line);
            if argument_facts && let Some(arguments) = site.arguments.known_value() {
                entry.call_sites.push(model::CallSiteFacts {
                    line: site.line,
                    has_receiver_expression: site.has_receiver_expression(),
                    arguments: arguments.clone(),
                });
            }
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
        let graph = CallGraphBuilder::new()
            .build(&AnalyzeRoots::from(dir.path()))
            .unwrap();

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
        let graph = CallGraphBuilder::new()
            .build(&AnalyzeRoots::from(dir.path()))
            .unwrap();

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

        let graph = CallGraphBuilder::new()
            .build(&AnalyzeRoots::from(dir.path()))
            .unwrap();
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

    /// Pointed at a file no graph language claims, the builder scans
    /// nothing rather than failing: the path filter admits a named file
    /// whatever its extension, so the language gate is what keeps a
    /// README out of the parser.
    #[test]
    fn a_file_outside_the_graph_languages_contributes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let readme = write_file(dir.path(), "README.md", "# not source\n");
        write_file(dir.path(), "src/lib.rs", "fn a() {}\n");

        let graph = CallGraphBuilder::new()
            .build(&AnalyzeRoots::from(&readme))
            .unwrap();
        assert!(graph.nodes.is_empty(), "got {:?}", graph.nodes);
        assert_eq!(graph.language, "unknown");

        let mut visited = Vec::new();
        let count = CallGraphBuilder::new()
            .visit_source_texts(&AnalyzeRoots::from(dir.path()), |file, _| {
                visited.push(file.to_owned())
            })
            .unwrap();
        assert_eq!(visited, ["src/lib.rs"], "the markdown file is not source");
        assert_eq!(count, 1);
    }

    /// One unparseable file found by the directory walk is dropped from
    /// the graph with a warning instead of failing the build, so a
    /// single file of too-new syntax cannot take down every call-graph
    /// analyzer (issue #494). An explicitly-named file keeps the hard
    /// error.
    #[test]
    fn walked_unparseable_file_is_dropped_from_the_graph() {
        let dir = tempfile::tempdir().unwrap();
        let broken = write_file(dir.path(), "src/broken.go", "package p\nfunc !!! {");
        write_file(
            dir.path(),
            "src/ok.go",
            "package p\n\nfunc A() { B() }\n\nfunc B() {}\n",
        );

        let graph = CallGraphBuilder::new()
            .build(&AnalyzeRoots::from(dir.path()))
            .unwrap();
        let names: Vec<&str> = graph.nodes.iter().map(|n| n.name.as_str()).collect();
        assert!(names.contains(&"A"), "got {names:?}");
        assert!(names.contains(&"B"), "got {names:?}");
        assert_eq!(graph.edges.len(), 1);

        let err = CallGraphBuilder::new()
            .build(&AnalyzeRoots::from(&broken))
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::Parse(_)), "{err:?}");
    }

    /// The three properties the caller sets carry beyond being the
    /// reverse of the adjacency: self-recursion is not a caller,
    /// duplicate call sites collapse to one caller, and a callee with no
    /// resolved caller is absent rather than empty.
    #[test]
    fn resolved_callers_reverses_the_adjacency_without_self_edges() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn a() { c(); c(); }\nfn b() { c(); }\nfn c() { c(); }\n",
        );

        let graph = CallGraphBuilder::new()
            .build(&AnalyzeRoots::from(dir.path()))
            .unwrap();
        let index = graph.node_index_by_id();
        let a = index["src/lib.rs:a:1"];
        let b = index["src/lib.rs:b:2"];
        let c = index["src/lib.rs:c:3"];

        let callers = graph.resolved_callers();
        assert_eq!(
            callers.get(&c).map(|s| s.iter().copied().collect()),
            Some(vec![a, b]),
            "two call sites from `a` count once; `c` calling itself is not a caller",
        );
        assert!(
            !callers.contains_key(&a) && !callers.contains_key(&b),
            "callees with no resolved caller stay absent, got {callers:?}",
        );
    }

    /// Unresolved and ambiguous call sites are not traversable, so they
    /// contribute no caller — the same lower-bound rule
    /// [`CallGraph::resolved_adjacency`] follows.
    #[test]
    fn resolved_callers_ignores_call_sites_that_did_not_resolve() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub fn target() {} }\n\
             mod b { pub fn target() {} }\n\
             fn ambiguous() { target(); }\n\
             fn unresolved() { nowhere(); }\n",
        );

        let graph = CallGraphBuilder::new()
            .build(&AnalyzeRoots::from(dir.path()))
            .unwrap();
        assert!(
            graph
                .edges
                .iter()
                .any(|e| e.resolution != Resolution::Resolved),
            "fixture must produce at least one non-resolved edge",
        );
        assert!(
            graph.resolved_callers().is_empty(),
            "only resolved edges contribute callers",
        );
    }

    /// The call graph's counterpart to the similarity case: a call edge
    /// from one root into another only exists if both roots were scanned
    /// into the same graph.
    #[test]
    fn several_roots_build_one_graph_with_edges_between_them() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "core/src/lib.rs", "pub fn helper() {}\n");
        write_file(dir.path(), "cli/src/main.rs", "fn main() { helper(); }\n");
        // Untouched by the roots below, so its call must stay out.
        write_file(dir.path(), "web/src/lib.rs", "fn other() { helper(); }\n");

        let roots = AnalyzeRoots::new([dir.path().join("core"), dir.path().join("cli")]);
        let graph = CallGraphBuilder::new().build(&roots).unwrap();

        let files: Vec<&str> = graph.nodes.iter().map(|n| n.file.as_str()).collect();
        assert_eq!(
            files,
            ["cli/src/main.rs", "core/src/lib.rs"],
            "got {files:?}"
        );
        let (resolution, target) = call_outcome(&graph, "main", "helper");
        assert_eq!(resolution, Resolution::Resolved);
        assert!(
            target.is_some_and(|t| t.ends_with("helper")),
            "got {target:?}",
        );
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

        let graph = CallGraphBuilder::new()
            .build(&AnalyzeRoots::from(dir.path()))
            .unwrap();
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
