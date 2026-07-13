//! `analyze error-chain` — rank wrap-at-every-layer propagation chains.
//!
//! Joins two existing models: the per-function
//! [`wrap_only_error_path`](lens_domain::FunctionErrorShape::wrap_only_error_path)
//! marker from the error-shape extractors, and the resolved
//! caller→callee edges from the function-graph builder. A chain is a
//! maximal caller→callee path where *every* function only (possibly
//! wraps and) propagates the error — meaning the error crosses that
//! many layers before anything actually handles it.
//!
//! Chains are shape, not verdicts: in Go and Rust, annotating an error
//! with fresh context at each hop is idiomatic. What the ranking
//! surfaces is *where* the long corridors are, so an agent can check
//! whether the middle layers add context or just repeat it. Edge
//! resolution is heuristic (see the function-graph docs); only edges
//! the resolver marked `resolved` participate, so missing chains are
//! more likely than fabricated ones.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use lens_domain::{FunctionErrorShape, WrapChain, compute_wrap_chains};
use serde::Serialize;

use super::function_graph::{GraphModel, NodeView, Resolution};
use super::runner::{FilterConfig, render_report};
use super::{AnalyzerError, FunctionGraphAnalyzer, OutputFormat, SourceFile, read_source};

/// Chains shorter than this are dropped by default: depth 1 is an
/// isolated wrap-only function and depth 2 is a single hop, neither of
/// which says anything about layering.
pub const DEFAULT_ERROR_CHAIN_MIN_DEPTH: usize = 3;

/// Analyzer entry point.
#[derive(Debug, Clone)]
pub struct ErrorChainAnalyzer {
    graph: FunctionGraphAnalyzer,
    filter: FilterConfig,
    min_depth: usize,
    top: Option<usize>,
}

impl Default for ErrorChainAnalyzer {
    fn default() -> Self {
        Self {
            graph: FunctionGraphAnalyzer::new(),
            filter: FilterConfig::default(),
            min_depth: DEFAULT_ERROR_CHAIN_MIN_DEPTH,
            top: None,
        }
    }
}

impl ErrorChainAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Minimum chain depth (function count) included in the report.
    pub fn with_min_depth(mut self, min_depth: Option<usize>) -> Self {
        self.min_depth = min_depth.unwrap_or(DEFAULT_ERROR_CHAIN_MIN_DEPTH);
        self
    }

    /// Cap the markdown report's chain ranking to the top-N entries.
    /// JSON output always carries the full list.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    // The path-filter builders forward to both underlying walks: the
    // call-graph collection and the error-shape file walk must see the
    // same file set or the join silently drops functions.
    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.graph = self.graph.with_only_tests(only_tests);
        self.filter = self.filter.with_only_tests(only_tests);
        self
    }

    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.graph = self.graph.with_exclude_tests(exclude_tests);
        self.filter = self.filter.with_exclude_tests(exclude_tests);
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.graph = self.graph.with_exclude_patterns(exclude.clone());
        self.filter = self.filter.with_exclude_patterns(exclude);
        self
    }

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let model = self.graph.build_model(path)?;
        let wrap_index = self.collect_wrap_index(path)?;
        let report = build_report(path, &model, &wrap_index, self.min_depth);
        render_report(&report, format, || format_markdown(&report, self.top))
    }

    /// Per-file error shapes, keyed for the node join: `(file,
    /// local_name)` → candidate `(start_line, wrap_only)` pairs.
    fn collect_wrap_index(&self, path: &Path) -> Result<WrapIndex, AnalyzerError> {
        let files = self.filter.collect_per_file(path, |sf| {
            let shapes = extract_shapes(sf)?;
            Ok((!shapes.is_empty()).then_some((sf.display_path.clone(), shapes)))
        })?;
        let mut index: WrapIndex = HashMap::new();
        for (file, shapes) in files {
            for shape in shapes {
                index
                    .entry((file.clone(), shape.name.clone()))
                    .or_default()
                    .push((shape.start_line, shape.wrap_only_error_path));
            }
        }
        Ok(index)
    }
}

type WrapIndex = HashMap<(String, String), Vec<(usize, bool)>>;

fn extract_shapes(file: &SourceFile) -> Result<Vec<FunctionErrorShape>, AnalyzerError> {
    let (lang, source) = read_source(&file.path)?;
    super::dispatch_lens!(lang, source.as_str(), extract_error_shapes).map_err(AnalyzerError::Parse)
}

/// The error-shape extractors qualify methods as `Owner::name`; the
/// graph node carries the pieces separately.
fn node_local_name(node: &NodeView) -> String {
    match &node.impl_owner {
        Some(owner) => format!("{owner}::{}", node.name),
        None => node.name.clone(),
    }
}

/// True when the node's function was marked wrap-only. Joined by
/// `(file, local_name)`, disambiguated by start line; a lone same-name
/// candidate is accepted even when the two extractors disagree on the
/// exact signature line.
fn node_is_wrap_only(node: &NodeView, index: &WrapIndex) -> bool {
    let Some(candidates) = index.get(&(node.file.clone(), node_local_name(node))) else {
        return false;
    };
    if let Some(&(_, wrap_only)) = candidates
        .iter()
        .find(|(start, _)| *start == node.start_line)
    {
        return wrap_only;
    }
    match candidates.as_slice() {
        [(_, wrap_only)] => *wrap_only,
        _ => false,
    }
}

#[derive(Debug, Serialize)]
struct Report {
    root: String,
    language: &'static str,
    /// Functions marked wrap-only across the corpus.
    wrap_only_function_count: usize,
    /// Resolved caller→callee edges where both ends are wrap-only.
    wrap_edge_count: usize,
    chain_count: usize,
    summary: Summary,
    chains: Vec<ChainView>,
}

#[derive(Debug, Serialize)]
struct Summary {
    max_depth: usize,
    cyclic_chain_count: usize,
}

#[derive(Debug, Serialize)]
struct ChainView {
    /// Number of functions on the chain.
    depth: usize,
    /// True when some link is a mutual-recursion group.
    has_cycle: bool,
    /// Caller→callee order; `links[0]` is the entry point no other
    /// wrap-only function calls.
    links: Vec<LinkView>,
}

#[derive(Debug, Serialize)]
struct LinkView {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    cycle: bool,
    functions: Vec<FunctionRef>,
}

#[derive(Debug, Serialize)]
struct FunctionRef {
    file: String,
    name: String,
    start_line: usize,
}

fn build_report(path: &Path, model: &GraphModel, index: &WrapIndex, min_depth: usize) -> Report {
    let wrap_nodes: Vec<&NodeView> = model
        .nodes
        .iter()
        .filter(|node| node_is_wrap_only(node, index))
        .collect();
    let id_to_idx: HashMap<&str, usize> = wrap_nodes
        .iter()
        .enumerate()
        .map(|(idx, node)| (node.id.as_str(), idx))
        .collect();

    let mut edges = Vec::new();
    for edge in &model.edges {
        if edge.resolution != Resolution::Resolved {
            continue;
        }
        let (Some(from), Some(to)) = (edge.from.as_deref(), edge.to.as_deref()) else {
            continue;
        };
        if let (Some(&f), Some(&t)) = (id_to_idx.get(from), id_to_idx.get(to))
            && f != t
        {
            edges.push((f, t));
        }
    }

    let chains: Vec<ChainView> = compute_wrap_chains(wrap_nodes.len(), &edges)
        .into_iter()
        .filter(|chain| chain.depth() >= min_depth)
        .map(|chain| chain_view(&chain, &wrap_nodes))
        .collect();

    Report {
        root: path.display().to_string(),
        language: model.language,
        wrap_only_function_count: wrap_nodes.len(),
        wrap_edge_count: edges.len(),
        chain_count: chains.len(),
        summary: Summary {
            max_depth: chains.iter().map(|c| c.depth).max().unwrap_or(0),
            cyclic_chain_count: chains.iter().filter(|c| c.has_cycle).count(),
        },
        chains,
    }
}

fn chain_view(chain: &WrapChain, wrap_nodes: &[&NodeView]) -> ChainView {
    ChainView {
        depth: chain.depth(),
        has_cycle: chain.has_cycle(),
        links: chain
            .links
            .iter()
            .map(|link| LinkView {
                cycle: link.is_cycle(),
                functions: link
                    .members
                    .iter()
                    .map(|&idx| {
                        let node = wrap_nodes[idx];
                        FunctionRef {
                            file: node.file.clone(),
                            name: node_local_name(node),
                            start_line: node.start_line,
                        }
                    })
                    .collect(),
            })
            .collect(),
    }
}

const DEFAULT_TOP: usize = 10;

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let mut out = format!(
        "# Error-chain report: {} ({} wrap-only function(s), {} wrap edge(s), {} chain(s))\n",
        report.root, report.wrap_only_function_count, report.wrap_edge_count, report.chain_count,
    );
    if report.chains.is_empty() {
        out.push_str("\n_No wrap chains at or above the depth threshold._\n");
        return out;
    }
    let _ = writeln!(
        out,
        "\n## Summary\n- max_depth: {}\n- cyclic_chains: {}",
        report.summary.max_depth, report.summary.cyclic_chain_count,
    );
    let limit = top.unwrap_or(DEFAULT_TOP);
    let _ = writeln!(out, "\n## Top {limit} chains by depth");
    for chain in report.chains.iter().take(limit) {
        let rendered: Vec<String> = chain.links.iter().map(render_link).collect();
        let _ = writeln!(out, "- depth {}: {}", chain.depth, rendered.join(" -> "),);
    }
    out
}

fn render_link(link: &LinkView) -> String {
    let names: Vec<String> = link
        .functions
        .iter()
        .map(|f| format!("{}:`{}` (L{})", f.file, f.name, f.start_line))
        .collect();
    if link.cycle {
        format!("({})", names.join(" <-> "))
    } else {
        names.join(" -> ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;

    const RUST_CHAIN: &str = r#"
fn leaf(s: &str) -> Result<i32, String> {
    s.parse::<i32>().map_err(|e| e.to_string())
}
fn mid(s: &str) -> Result<i32, String> {
    Ok(leaf(s)? + 1)
}
fn top(s: &str) -> Result<i32, String> {
    Ok(mid(s)? + 1)
}
fn boundary(s: &str) -> i32 {
    match top(s) {
        Ok(v) => v,
        Err(_) => 0,
    }
}
"#;

    fn analyze_json(path: &Path, analyzer: ErrorChainAnalyzer) -> serde_json::Value {
        let json = analyzer.analyze(path, OutputFormat::Json).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    #[test]
    fn rust_wrap_chain_is_reported_in_caller_to_callee_order() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", RUST_CHAIN);
        let report = analyze_json(dir.path(), ErrorChainAnalyzer::new());

        // `boundary` recovers, so only leaf/mid/top are wrap-only.
        assert_eq!(report["wrap_only_function_count"], 3);
        assert_eq!(report["wrap_edge_count"], 2);
        assert_eq!(report["chain_count"], 1);
        assert_eq!(report["summary"]["max_depth"], 3);
        assert_eq!(report["summary"]["cyclic_chain_count"], 0);

        let chain = &report["chains"][0];
        assert_eq!(chain["depth"], 3);
        assert_eq!(chain["has_cycle"], false);
        let names: Vec<&str> = chain["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|link| link["functions"][0]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["top", "mid", "leaf"]);
        assert_eq!(chain["links"][0]["functions"][0]["file"], "src/lib.rs");
    }

    #[test]
    fn min_depth_filters_short_chains() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            r#"
fn leaf(s: &str) -> Result<i32, String> {
    s.parse::<i32>().map_err(|e| e.to_string())
}
fn caller(s: &str) -> Result<i32, String> {
    Ok(leaf(s)? + 1)
}
"#,
        );
        // Default min depth is 3: a two-hop chain is dropped.
        let report = analyze_json(dir.path(), ErrorChainAnalyzer::new());
        assert_eq!(report["chain_count"], 0);
        assert_eq!(report["summary"]["max_depth"], 0);

        let report = analyze_json(
            dir.path(),
            ErrorChainAnalyzer::new().with_min_depth(Some(2)),
        );
        assert_eq!(report["chain_count"], 1);
        assert_eq!(report["chains"][0]["depth"], 2);
    }

    #[test]
    fn mutual_recursion_is_reported_as_a_cyclic_link() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            r#"
fn ping(n: u32) -> Result<(), String> {
    if n == 0 {
        return Ok(());
    }
    pong(n - 1)?;
    Ok(())
}
fn pong(n: u32) -> Result<(), String> {
    if n == 0 {
        return Ok(());
    }
    ping(n - 1)?;
    Ok(())
}
"#,
        );
        let report = analyze_json(
            dir.path(),
            ErrorChainAnalyzer::new().with_min_depth(Some(2)),
        );
        assert_eq!(report["chain_count"], 1);
        let chain = &report["chains"][0];
        assert_eq!(chain["depth"], 2);
        assert_eq!(chain["has_cycle"], true);
        assert_eq!(chain["links"][0]["cycle"], true);
        let members = chain["links"][0]["functions"].as_array().unwrap();
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn go_wrap_chain_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "main.go",
            r#"
package p

func leaf() error {
    if err := step(); err != nil {
        return fmt.Errorf("leaf: %w", err)
    }
    return nil
}

func mid() error {
    if err := leaf(); err != nil {
        return fmt.Errorf("mid: %w", err)
    }
    return nil
}

func top() error {
    if err := mid(); err != nil {
        return fmt.Errorf("top: %w", err)
    }
    return nil
}
"#,
        );
        let report = analyze_json(dir.path(), ErrorChainAnalyzer::new());
        assert_eq!(report["language"], "go");
        assert_eq!(report["wrap_only_function_count"], 3);
        assert_eq!(report["chain_count"], 1);
        let names: Vec<&str> = report["chains"][0]["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|link| link["functions"][0]["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["top", "mid", "leaf"]);
    }

    #[test]
    fn markdown_renders_chains_as_arrows() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", RUST_CHAIN);
        let md = ErrorChainAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Error-chain report"));
        assert!(md.contains("Top 10 chains by depth"));
        assert!(md.contains(
            "depth 3: src/lib.rs:`top` (L8) -> src/lib.rs:`mid` (L5) -> src/lib.rs:`leaf` (L2)"
        ));
    }

    #[test]
    fn markdown_reports_absence_of_chains() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "fn quiet() { let _ = 1; }\n");
        let md = ErrorChainAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("No wrap chains"));
    }

    #[test]
    fn recovery_in_the_middle_splits_the_chain() {
        // top -> recover -> leaf: `recover` handles the error, so no
        // three-function corridor exists even though calls line up.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            r#"
fn leaf(s: &str) -> Result<i32, String> {
    s.parse::<i32>().map_err(|e| e.to_string())
}
fn recover(s: &str) -> Result<i32, String> {
    match leaf(s) {
        Ok(v) => Ok(v),
        Err(_) => Ok(0),
    }
}
fn top(s: &str) -> Result<i32, String> {
    Ok(recover(s)? + 1)
}
"#,
        );
        let report = analyze_json(dir.path(), ErrorChainAnalyzer::new());
        assert_eq!(report["chain_count"], 0);
        // top and leaf are wrap-only, but `recover` between them isn't.
        assert_eq!(report["wrap_only_function_count"], 2);
        assert_eq!(report["wrap_edge_count"], 0);
    }
}
