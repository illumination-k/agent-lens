//! `analyze coupling` — module-level coupling metrics for a Rust crate,
//! a TypeScript / JavaScript module graph, a Go module, or a Python
//! package tree.
//!
//! Builds a language-specific module tree, then reports the metrics
//! derived from the cross-module reference graph: Number of Couplings,
//! Fan-In, Fan-Out, simplified Henry-Kafura Information Flow Complexity,
//! per-pair Inter-module Coupling (distinct shared symbols),
//! Robert C. Martin's Instability `I = Ce / (Ca + Ce)`, and the strongly
//! connected components of the dependency graph (cycles). JSON is the
//! default machine-readable output; `--format md` emits a compact
//! summary tuned for LLM context windows rather than for humans.
//!
//! For Rust the entry point is a `.rs` crate root (or a directory
//! containing `src/lib.rs` / `src/main.rs`); each `mod` declaration
//! becomes a node and `use` / qualified-path references become `Use`
//! edges. For TypeScript / JavaScript the entry point is a single
//! source file (`.ts`, `.tsx`, `.mts`, `.cts`, `.js`, `.jsx`, `.mjs`,
//! `.cjs`) and the graph is grown by following relative `import` /
//! `export … from` specifiers; one source file is one module. For Go the
//! entry point is a `.go` file or a directory containing `go.mod`, and
//! one package is one module. For Python the entry point is a `.py` file
//! or a package directory, one source file is one module, and `import` /
//! `from … import` statements become the edges.
//!
//! Module paths are reported in the analyzed language's own spelling —
//! Go packages by `go.mod` import path, TS/JS and Python modules by
//! their path relative to the module tree's source root — while the
//! graph itself keeps one canonical shape. See
//! [`super::module_label`].
//!
//! Limitations carried over from the underlying extractors:
//!
//! * Rust: `#[path = "..."]` attributes on `mod` declarations are not
//!   honoured; cross-crate references are silently dropped (this
//!   analyzer is single-crate by design); macro-generated items are
//!   invisible to `syn` and therefore invisible here; non-standard
//!   crate roots (e.g. `[lib].path` in `Cargo.toml`) are not detected
//!   — pass the root file directly when the layout is unusual.
//! * TypeScript / JavaScript: only relative module specifiers
//!   (`./` and `../`) are followed. Bare specifiers and TypeScript
//!   path aliases are not resolved.
//! * Python: only imports that resolve to a `.py` file under the root
//!   become edges — standard-library and third-party imports are
//!   dropped, matching the single-tree scope of the other backends.
//!   Dynamic imports (`importlib`, `__import__`) are invisible, and
//!   `sys.path` manipulation is not modelled.

use std::fmt::Write as _;
use std::path::Path;

use lens_domain::{
    CouplingEdge, CouplingReport, DependencyCycle, ModuleMetrics, ModulePath, PairCoupling,
    compute_report,
};
use serde::Serialize;

use super::module_graph::{GraphPolicy, build_graph, module_paths};
use super::module_label::ModuleLabeler;
use super::options::analyzer_options;
use super::{AnalyzePathFilter, CouplingAnalyzerError, OutputFormat, format_optional_f64};

analyzer_options! {
    /// `analyze coupling` flags, and the `[profile.<name>.coupling]` table.
    pub struct CouplingOptions {
        @shared(ranking);
    }
}

/// Modules listed in markdown when `--top` is not given. JSON always
/// carries every module; this only bounds the rendered table, which has
/// one row per module and is therefore the longest report the analyzer
/// family produces on a large package.
const DEFAULT_TOP: usize = 20;

/// Coupled pairs listed in markdown. The pair list is a supporting
/// exhibit rather than the report's ranking, so it keeps its own tighter
/// cap and `--top` can only narrow it further.
const TOP_PAIRS_LIMIT: usize = 10;

/// Analyzer entry point.
#[derive(Debug, Default, Clone)]
pub struct CouplingAnalyzer {
    path_filter: AnalyzePathFilter,
    top: Option<usize>,
}

impl CouplingAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cap the markdown module table to the top-N rows by IFC, and the
    /// coupled-pair list to at most that many. JSON output always
    /// carries every module. `None` uses the markdown default of 20.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    /// Apply a whole [`CouplingOptions`] group. The CLI flags and the
    /// `[profile.<name>.coupling]` table are the same type, so this is
    /// the only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: CouplingOptions) -> Self {
        self.with_top(opts.top)
    }

    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_only_tests(only_tests);
        self
    }

    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_exclude_tests(exclude_tests);
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.path_filter = self.path_filter.with_exclude_patterns(exclude);
        self
    }

    /// Resolve `path`, build the language-specific module tree, and
    /// produce a report in `format`. Rust resolves the crate root from
    /// a `.rs` file or a directory; TypeScript / JavaScript starts at
    /// the entry source file and follows relative imports; Go and Python
    /// walk a directory for packages and `.py` files respectively.
    pub fn analyze(
        &self,
        path: impl AsRef<Path>,
        format: OutputFormat,
    ) -> Result<String, CouplingAnalyzerError> {
        let mut graph = build_graph(path.as_ref(), GraphPolicy::COUPLING)?;
        let filter = self.path_filter.compile(&graph.root)?;
        graph.modules.retain(|m| filter.includes_path(&m.file));
        let kept: std::collections::HashSet<&ModulePath> =
            graph.modules.iter().map(|m| &m.path).collect();
        graph
            .edges
            .retain(|e| kept.contains(&e.from) && kept.contains(&e.to));
        let report = compute_report(&module_paths(&graph), graph.edges);
        let view = ReportView::new(&graph.root, &graph.labeler, &report);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&view).map_err(CouplingAnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&view, self.top)),
        }
    }
}

/// A [`CouplingReport`] paired with the spelling its module paths should
/// be rendered in. The report itself stays in the canonical `crate::a::b`
/// shape; the labeler knows how the analyzed language writes that.
pub(crate) struct LabeledReport {
    pub(crate) report: CouplingReport,
    pub(crate) labeler: ModuleLabeler,
}

/// Dispatch on language and compute the raw [`CouplingReport`] for `path`,
/// or return `None` when `path` is not anchored at a supported root
/// (e.g. a directory holding no Rust crate, Go module, or Python files).
///
/// This is the language-agnostic entry point the SessionStart summary
/// hook needs: it wants the metrics, not the analyzer's JSON/markdown
/// rendering, and it must not be pinned to a single language the way a
/// direct `lens_rust::build_module_tree` call would be. `UnsupportedRoot`
/// is folded into `None` because "this directory isn't the kind of root
/// we analyze" is not an error worth surfacing at session start.
pub(crate) fn report_for_path(path: &Path) -> Result<Option<LabeledReport>, CouplingAnalyzerError> {
    let graph = match build_graph(path, GraphPolicy::COUPLING) {
        Ok(graph) => graph,
        Err(CouplingAnalyzerError::UnsupportedRoot { .. }) => return Ok(None),
        Err(e) => return Err(e),
    };
    let labeler = graph.labeler.clone();
    Ok(Some(LabeledReport {
        report: compute_report(&module_paths(&graph), graph.edges),
        labeler,
    }))
}

#[derive(Debug, Serialize)]
struct ReportView<'a> {
    crate_root: String,
    module_count: usize,
    edge_count: usize,
    cycle_count: usize,
    modules: Vec<ModuleView>,
    edges: Vec<EdgeView<'a>>,
    pairs: Vec<PairView>,
    cycles: Vec<CycleView>,
}

impl<'a> ReportView<'a> {
    fn new(root: &Path, labeler: &ModuleLabeler, report: &'a CouplingReport) -> Self {
        Self {
            crate_root: root.display().to_string(),
            module_count: report.modules.len(),
            edge_count: report.number_of_couplings,
            cycle_count: report.cycles.len(),
            modules: report
                .modules
                .iter()
                .map(|m| ModuleView::new(m, labeler))
                .collect(),
            edges: report
                .edges
                .iter()
                .map(|e| EdgeView::new(e, labeler))
                .collect(),
            pairs: report
                .pairs
                .iter()
                .map(|p| PairView::new(p, labeler))
                .collect(),
            cycles: report
                .cycles
                .iter()
                .map(|c| CycleView::new(c, labeler))
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ModuleView {
    path: String,
    fan_in: usize,
    fan_out: usize,
    ifc: u64,
    /// Robert C. Martin's instability `I = Ce / (Ca + Ce)`. Omitted from
    /// JSON when the module has no edges (so the ratio is undefined).
    #[serde(skip_serializing_if = "Option::is_none")]
    instability: Option<f64>,
}

impl ModuleView {
    fn new(m: &ModuleMetrics, labeler: &ModuleLabeler) -> Self {
        Self {
            path: labeler.label(&m.path),
            fan_in: m.fan_in,
            fan_out: m.fan_out,
            ifc: m.ifc,
            instability: m.instability,
        }
    }
}

#[derive(Debug, Serialize)]
struct CycleView {
    size: usize,
    members: Vec<String>,
}

impl CycleView {
    fn new(c: &DependencyCycle, labeler: &ModuleLabeler) -> Self {
        Self {
            size: c.members.len(),
            members: c.members.iter().map(|m| labeler.label(m)).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct EdgeView<'a> {
    from: String,
    to: String,
    symbol: &'a str,
    kind: &'static str,
}

impl<'a> EdgeView<'a> {
    fn new(e: &'a CouplingEdge, labeler: &ModuleLabeler) -> Self {
        Self {
            from: labeler.label(&e.from),
            to: labeler.label(&e.to),
            symbol: e.symbol.as_str(),
            kind: e.kind.as_str(),
        }
    }
}

#[derive(Debug, Serialize)]
struct PairView {
    a: String,
    b: String,
    shared_symbols: usize,
}

impl PairView {
    fn new(p: &PairCoupling, labeler: &ModuleLabeler) -> Self {
        Self {
            a: labeler.label(&p.a),
            b: labeler.label(&p.b),
            shared_symbols: p.shared_symbols,
        }
    }
}

fn format_markdown(view: &ReportView<'_>, top: Option<usize>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    let mut out = format!(
        "# Coupling report: {} ({} module(s), {} edge(s), {} cycle(s))\n",
        view.crate_root, view.module_count, view.edge_count, view.cycle_count,
    );
    if view.modules.is_empty() {
        out.push_str("\n_No modules discovered._\n");
        return out;
    }
    render_modules_table(&mut out, &view.modules, limit);
    // Cycles are deliberately uncapped: a truncated cycle list reads as
    // "these are the cycles" while hiding the rest, and the list is
    // short whenever the news is good.
    render_cycles(&mut out, &view.cycles);
    render_pairs(&mut out, &view.pairs, limit.min(TOP_PAIRS_LIMIT));
    out
}

fn render_modules_table(out: &mut String, modules: &[ModuleView], limit: usize) {
    // writeln! into a String cannot fail; the result is swallowed
    // deliberately rather than unwrapped to satisfy the workspace's
    // `unwrap_used` lint.
    let _ = writeln!(out, "\n## Modules (by IFC desc, top {limit})\n");
    let _ = writeln!(out, "| module | fan_in | fan_out | ifc | instability |");
    let _ = writeln!(out, "| --- | ---: | ---: | ---: | ---: |");
    let mut sorted: Vec<&ModuleView> = modules.iter().collect();
    sorted.sort_by(|a, b| {
        b.ifc
            .cmp(&a.ifc)
            .then_with(|| b.fan_in.cmp(&a.fan_in))
            .then_with(|| b.fan_out.cmp(&a.fan_out))
            .then_with(|| a.path.cmp(&b.path))
    });
    for m in sorted.iter().take(limit) {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            m.path,
            m.fan_in,
            m.fan_out,
            m.ifc,
            format_optional_f64(m.instability, 2),
        );
    }
    let omitted = sorted.len().saturating_sub(limit);
    if omitted > 0 {
        let _ = writeln!(
            out,
            "\n_{omitted} lower-IFC module(s) not shown; raise --top or use --format json._",
        );
    }
}

fn render_cycles(out: &mut String, cycles: &[CycleView]) {
    if cycles.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## Dependency cycles\n");
    for c in cycles {
        let _ = writeln!(out, "- {} module(s): {}", c.size, c.members.join(" → "));
    }
}

fn render_pairs(out: &mut String, pairs: &[PairView], limit: usize) {
    if pairs.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## Top coupled pairs\n");
    for p in pairs.iter().take(limit) {
        let _ = writeln!(out, "- {} ↔ {} ({} symbol(s))", p.a, p.b, p.shared_symbols);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use std::path::PathBuf;

    fn small_crate(dir: &Path) -> PathBuf {
        // Layout:
        //   lib.rs declares mod a; mod b;
        //   a.rs declares pub fn helper() and pub struct Foo
        //   b.rs uses crate::a::Foo and calls crate::a::helper()
        let lib = write_file(dir, "lib.rs", "pub mod a;\npub mod b;\n");
        write_file(dir, "a.rs", "pub fn helper() {}\npub struct Foo;\n");
        write_file(
            dir,
            "b.rs",
            r#"
            use crate::a::Foo;
            fn _x(_f: Foo) { crate::a::helper(); }
            "#,
        );
        lib
    }

    #[test]
    fn json_report_includes_top_level_counts() {
        let dir = tempfile::tempdir().unwrap();
        let lib = small_crate(dir.path());
        let json = CouplingAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // 3 modules: crate, crate::a, crate::b.
        assert_eq!(parsed["module_count"], 3);
        assert!(parsed["edge_count"].as_u64().unwrap() >= 2);
    }

    #[test]
    fn json_report_records_fan_in_fan_out_and_ifc() {
        let dir = tempfile::tempdir().unwrap();
        let lib = small_crate(dir.path());
        let json = CouplingAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules = parsed["modules"].as_array().unwrap();
        let a = modules
            .iter()
            .find(|m| m["path"] == "crate::a")
            .expect("crate::a present");
        let b = modules
            .iter()
            .find(|m| m["path"] == "crate::b")
            .expect("crate::b present");
        // a is depended on by b → fan_in >= 1, fan_out = 0.
        assert!(a["fan_in"].as_u64().unwrap() >= 1);
        assert_eq!(a["fan_out"], 0);
        assert_eq!(a["ifc"], 0);
        // b depends on a → fan_out >= 1, fan_in = 0.
        assert!(b["fan_out"].as_u64().unwrap() >= 1);
        assert_eq!(b["fan_in"], 0);
        assert_eq!(b["ifc"], 0);
    }

    #[test]
    fn json_report_lists_pair_coupling() {
        let dir = tempfile::tempdir().unwrap();
        let lib = small_crate(dir.path());
        let json = CouplingAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let pairs = parsed["pairs"].as_array().unwrap();
        let a_b = pairs
            .iter()
            .find(|p| {
                (p["a"] == "crate::a" && p["b"] == "crate::b")
                    || (p["a"] == "crate::b" && p["b"] == "crate::a")
            })
            .expect("a-b pair present");
        // {Foo (use), Foo (type), helper (call)} — at least 2 distinct
        // symbols cross the boundary (Foo and helper).
        assert!(a_b["shared_symbols"].as_u64().unwrap() >= 2);
    }

    #[test]
    fn markdown_report_contains_module_table_and_pair_section() {
        let dir = tempfile::tempdir().unwrap();
        let lib = small_crate(dir.path());
        let md = CouplingAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Coupling report:"));
        assert!(md.contains("## Modules"));
        assert!(md.contains("| module |"));
        assert!(md.contains("crate::a"));
        assert!(md.contains("crate::b"));
        assert!(md.contains("Top coupled pairs"));
    }

    /// The module table is the report's longest section — one row per
    /// module — so `--top` has to bound it, and the rows it dropped have
    /// to be counted rather than silently missing.
    #[test]
    fn markdown_module_table_is_capped_by_top_and_reports_what_it_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let lib = small_crate(dir.path());
        let md = CouplingAnalyzer::new()
            .with_top(Some(1))
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("(by IFC desc, top 1)"), "got: {md}");
        // Three modules exist (crate, crate::a, crate::b); one row of
        // the table survives and the other two are accounted for.
        assert_eq!(md.matches("| crate").count(), 1, "got: {md}");
        assert!(md.contains("_2 lower-IFC module(s) not shown"), "got: {md}",);

        // Above the module count the note disappears entirely.
        let md = CouplingAnalyzer::new()
            .with_top(Some(50))
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(!md.contains("not shown"), "got: {md}");
        assert_eq!(md.matches("| crate").count(), 3, "got: {md}");
    }

    /// JSON is the machine-readable half and must stay complete: `--top`
    /// is a markdown-rendering cap, not a filter on the analysis.
    #[test]
    fn top_does_not_touch_the_json_report() {
        let dir = tempfile::tempdir().unwrap();
        let lib = small_crate(dir.path());
        let json = CouplingAnalyzer::new()
            .with_top(Some(1))
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["module_count"], 3);
        assert_eq!(parsed["modules"].as_array().map(Vec::len), Some(3));
    }

    #[test]
    fn path_filters_apply_to_module_tree() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(
            dir.path(),
            "lib.rs",
            "pub mod prod;\npub mod foo_test;\npub mod generated;\n",
        );
        write_file(dir.path(), "prod.rs", "pub fn prod() {}\n");
        write_file(dir.path(), "foo_test.rs", "pub fn test_case() {}\n");
        write_file(dir.path(), "generated.rs", "pub fn generated() {}\n");

        let only_tests = CouplingAnalyzer::new()
            .with_only_tests(true)
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&only_tests).unwrap();
        assert_eq!(parsed["module_count"], 1);
        assert_eq!(parsed["modules"][0]["path"], "crate::foo_test");

        let exclude_tests = CouplingAnalyzer::new()
            .with_exclude_tests(true)
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exclude_tests).unwrap();
        let modules: Vec<&str> = parsed["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert!(modules.contains(&"crate"));
        assert!(modules.contains(&"crate::prod"));
        assert!(!modules.contains(&"crate::foo_test"));

        let exclude_generated = CouplingAnalyzer::new()
            .with_exclude_patterns(vec!["generated.rs".to_owned()])
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exclude_generated).unwrap();
        let modules: Vec<&str> = parsed["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert!(!modules.contains(&"crate::generated"));
    }

    #[test]
    fn directory_root_detects_src_lib_rs() {
        let dir = tempfile::tempdir().unwrap();
        // Layout matches `cargo new --lib`.
        write_file(dir.path(), "src/lib.rs", "pub fn solo() {}\n");
        let json = CouplingAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["module_count"], 1);
    }

    #[test]
    fn directory_root_falls_back_to_src_main_rs() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/main.rs", "fn main() {}\n");
        let json = CouplingAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["module_count"], 1);
    }

    #[test]
    fn unsupported_extension_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_file(dir.path(), "notes.txt", "hello");
        let err = CouplingAnalyzer::new()
            .analyze(&f, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, CouplingAnalyzerError::UnsupportedRoot { .. }));
    }

    #[test]
    fn missing_path_surfaces_io_error() {
        let err = CouplingAnalyzer::new()
            .analyze(
                Path::new("/definitely/does/not/exist.rs"),
                OutputFormat::Json,
            )
            .unwrap_err();
        assert!(matches!(err, CouplingAnalyzerError::Io { .. }));
    }

    #[test]
    fn missing_mod_file_surfaces_missing_mod_error() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "mod ghost;\n");
        let err = CouplingAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, CouplingAnalyzerError::MissingMod { .. }));
    }

    #[test]
    fn invalid_rust_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "fn ??? {");
        let err = CouplingAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, CouplingAnalyzerError::Parse { .. }));
    }

    /// One unparseable file inside the walked Go module must not poison
    /// the module-wide profile: its package keeps its parseable files'
    /// edges and the report still covers the whole module (issue #494).
    #[test]
    fn go_module_with_one_unparseable_file_still_reports() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "go.mod", "module example.com/p\n");
        write_file(
            dir.path(),
            "a/a.go",
            "package a\n\nimport \"example.com/p/b\"\n\nfunc A() { b.B() }\n",
        );
        write_file(dir.path(), "b/b.go", "package b\n\nfunc B() {}\n");
        write_file(dir.path(), "b/broken.go", "package b\nfunc !!! {");
        let json = CouplingAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let paths: Vec<&str> = parsed["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"example.com/p/a"), "got {paths:?}");
        assert!(paths.contains(&"example.com/p/b"), "got {paths:?}");
        let a = parsed["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["path"] == "example.com/p/a")
            .unwrap();
        assert_eq!(a["fan_out"], 1, "the a -> b edge survives the bad file");
    }

    #[test]
    fn json_report_records_instability_for_directional_modules() {
        let dir = tempfile::tempdir().unwrap();
        let lib = small_crate(dir.path());
        let json = CouplingAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules = parsed["modules"].as_array().unwrap();
        let a = modules.iter().find(|m| m["path"] == "crate::a").unwrap();
        let b = modules.iter().find(|m| m["path"] == "crate::b").unwrap();
        // a is only depended on (Ce=0, Ca>0), so I = 0.
        assert_eq!(a["instability"].as_f64().unwrap(), 0.0);
        // b only depends on others (Ca=0, Ce>0), so I = 1.
        assert_eq!(b["instability"].as_f64().unwrap(), 1.0);
    }

    #[test]
    fn json_report_lists_cycles_when_modules_form_an_scc() {
        let dir = tempfile::tempdir().unwrap();
        // a → b via Foo, b → a via Bar — a two-node cycle.
        write_file(dir.path(), "lib.rs", "pub mod a;\npub mod b;\n");
        write_file(
            dir.path(),
            "a.rs",
            "use crate::b::Bar;\npub struct Foo;\nfn _x(_b: Bar) {}\n",
        );
        write_file(
            dir.path(),
            "b.rs",
            "use crate::a::Foo;\npub struct Bar;\nfn _y(_f: Foo) {}\n",
        );
        let json = CouplingAnalyzer::new()
            .analyze(dir.path().join("lib.rs"), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cycle_count"], 1);
        let cycles = parsed["cycles"].as_array().unwrap();
        assert_eq!(cycles.len(), 1);
        let members = cycles[0]["members"].as_array().unwrap();
        let names: Vec<&str> = members.iter().map(|m| m.as_str().unwrap()).collect();
        assert!(names.contains(&"crate::a"));
        assert!(names.contains(&"crate::b"));
    }

    #[test]
    fn coupling_error_io_display_includes_path_and_source() {
        let err = CouplingAnalyzerError::Io {
            path: PathBuf::from("/tmp/missing.rs"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/missing.rs"), "got {msg}");
        assert!(msg.contains("missing"), "got {msg}");
        assert!(msg.starts_with("failed to read"), "got {msg}");
    }

    #[test]
    fn coupling_error_parse_display_includes_path_and_source() {
        let err = CouplingAnalyzerError::Parse {
            path: PathBuf::from("/tmp/bad.rs"),
            source: Box::<dyn std::error::Error + Send + Sync>::from("syntax".to_owned()),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/bad.rs"), "got {msg}");
        assert!(msg.contains("syntax"), "got {msg}");
        assert!(msg.starts_with("failed to parse"), "got {msg}");
    }

    #[test]
    fn coupling_error_unsupported_root_display_includes_path() {
        let err = CouplingAnalyzerError::UnsupportedRoot {
            path: PathBuf::from("/tmp/odd"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/odd"), "got {msg}");
        assert!(msg.contains("unsupported analysis root"), "got {msg}");
        // Every backend the dispatch can reach is named, so the message
        // does not read as a Rust-only failure.
        for hint in ["Rust", "TS/JS", "Python", "Go"] {
            assert!(msg.contains(hint), "{hint} missing from {msg}");
        }
    }

    #[test]
    fn coupling_error_missing_mod_display_includes_parent_name_and_path() {
        let err = CouplingAnalyzerError::MissingMod {
            parent: "crate".to_owned(),
            name: "ghost".to_owned(),
            near: PathBuf::from("/tmp/proj"),
        };
        let msg = err.to_string();
        assert!(msg.contains("crate"), "got {msg}");
        assert!(msg.contains("ghost"), "got {msg}");
        assert!(msg.contains("/tmp/proj"), "got {msg}");
    }

    #[test]
    fn coupling_error_serialize_display_includes_inner() {
        let serde_err = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        let err = CouplingAnalyzerError::Serialize(serde_err);
        let msg = err.to_string();
        assert!(msg.contains("serialize"), "got {msg}");
    }

    #[test]
    fn coupling_error_io_source_is_present() {
        use std::error::Error as _;
        let err = CouplingAnalyzerError::Io {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::other("denied"),
        };
        assert!(err.source().is_some());
    }

    #[test]
    fn coupling_error_parse_source_is_present() {
        use std::error::Error as _;
        let err = CouplingAnalyzerError::Parse {
            path: PathBuf::from("/tmp/x"),
            source: Box::<dyn std::error::Error + Send + Sync>::from("boom".to_owned()),
        };
        assert!(err.source().is_some());
    }

    #[test]
    fn coupling_error_serialize_source_is_present() {
        use std::error::Error as _;
        let serde_err = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        let err = CouplingAnalyzerError::Serialize(serde_err);
        assert!(err.source().is_some());
    }

    #[test]
    fn coupling_error_variants_without_source_return_none() {
        use std::error::Error as _;
        let err = CouplingAnalyzerError::UnsupportedRoot {
            path: PathBuf::from("/tmp/x"),
        };
        assert!(err.source().is_none());
        let err = CouplingAnalyzerError::MissingMod {
            parent: "crate".into(),
            name: "ghost".into(),
            near: PathBuf::from("/tmp"),
        };
        assert!(err.source().is_none());
    }

    #[test]
    fn typescript_entry_file_reports_fan_in_fan_out_and_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let main = write_file(
            &src,
            "main.ts",
            "import { add } from './util'; export const r = add(1, 2);\n",
        );
        write_file(
            &src,
            "util.ts",
            "export function add(a: number, b: number) { return a + b; }\n",
        );

        let json = CouplingAnalyzer::new()
            .analyze(&main, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["module_count"], 2);
        assert!(parsed["edge_count"].as_u64().unwrap() >= 1);
        let modules = parsed["modules"].as_array().unwrap();
        // TS modules are files, so they are labelled with their path
        // relative to the module tree's source root — not `crate::…`.
        let main_m = modules.iter().find(|m| m["path"] == "main").expect("main");
        let util_m = modules.iter().find(|m| m["path"] == "util").expect("util");
        assert!(main_m["fan_out"].as_u64().unwrap() >= 1);
        assert_eq!(main_m["fan_in"], 0);
        // util is depended on, so I = 0 (fully stable).
        assert_eq!(util_m["instability"].as_f64().unwrap(), 0.0);
        // main only depends on others, so I = 1 (fully unstable).
        assert_eq!(main_m["instability"].as_f64().unwrap(), 1.0);

        let pairs = parsed["pairs"].as_array().unwrap();
        assert!(pairs.iter().any(|p| {
            (p["a"] == "main" && p["b"] == "util") || (p["a"] == "util" && p["b"] == "main")
        }));
    }

    #[test]
    fn typescript_circular_imports_become_a_cycle() {
        // a → b via Bar, b → a via Foo: a two-node SCC across files.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let entry = write_file(
            &src,
            "a.ts",
            "import { Bar } from './b'; export class Foo { b?: Bar }\n",
        );
        write_file(
            &src,
            "b.ts",
            "import { Foo } from './a'; export class Bar { a?: Foo }\n",
        );

        let json = CouplingAnalyzer::new()
            .analyze(&entry, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["cycle_count"], 1);
        let cycles = parsed["cycles"].as_array().unwrap();
        let members: Vec<&str> = cycles[0]["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m.as_str().unwrap())
            .collect();
        assert!(members.contains(&"a"));
        assert!(members.contains(&"b"));
    }

    #[test]
    fn typescript_markdown_report_contains_module_table() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let main = write_file(
            &src,
            "main.ts",
            "import { add } from './util'; export const r = add(1, 2);\n",
        );
        write_file(
            &src,
            "util.ts",
            "export function add(a: number, b: number) { return a + b; }\n",
        );

        let md = CouplingAnalyzer::new()
            .analyze(&main, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Coupling report:"));
        assert!(md.contains("## Modules"));
        assert!(md.contains("| main |"));
        assert!(md.contains("| util |"));
        assert!(
            !md.contains("crate::"),
            "TS rows must not be Rust-spelled: {md}"
        );
        assert!(md.contains("Top coupled pairs"));
    }

    #[test]
    fn typescript_path_exclude_drops_modules_and_their_edges() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let main = write_file(
            &src,
            "main.ts",
            "import { add } from './generated'; export const r = add(1, 2);\n",
        );
        write_file(
            &src,
            "generated.ts",
            "export function add(a: number, b: number) { return a + b; }\n",
        );

        let json = CouplingAnalyzer::new()
            .with_exclude_patterns(vec!["generated.ts".to_owned()])
            .analyze(&main, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules: Vec<&str> = parsed["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert!(modules.contains(&"main"));
        assert!(!modules.contains(&"generated"));
        // No edges should reference the dropped module.
        for e in parsed["edges"].as_array().unwrap() {
            assert_ne!(e["from"], "generated");
            assert_ne!(e["to"], "generated");
        }
    }

    #[test]
    fn go_module_directory_reports_local_import_edges() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "go.mod", "module github.com/x/proj\n");
        write_file(
            dir.path(),
            "main.go",
            concat!(
                "package main\n\n",
                "import \"github.com/x/proj/pkg/util\"\n\n",
                "func main() { util.Run() }\n",
            ),
        );
        write_file(
            dir.path(),
            "pkg/util/util.go",
            "package util\n\nfunc Run() {}\n",
        );

        let json = CouplingAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules: Vec<&str> = parsed["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        // Go packages are named by import path, taken from `go.mod`.
        assert!(modules.contains(&"github.com/x/proj"), "got {modules:?}");
        assert!(
            modules.contains(&"github.com/x/proj/pkg/util"),
            "got {modules:?}",
        );
        assert!(parsed["edge_count"].as_u64().unwrap() >= 1);
        let util = parsed["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["path"] == "github.com/x/proj/pkg/util")
            .expect("util module");
        // util is depended on by main, so I = 0 (fully stable).
        assert_eq!(util["instability"].as_f64().unwrap(), 0.0);
    }

    #[test]
    fn go_external_imports_do_not_create_edges() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "go.mod", "module github.com/x/proj\n");
        write_file(
            dir.path(),
            "main.go",
            concat!(
                "package main\n\n",
                "import (\n    \"fmt\"\n    \"github.com/foo/bar\"\n)\n\n",
                "func main() { fmt.Println(bar.Stuff) }\n",
            ),
        );

        let json = CouplingAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["edge_count"], 0);
    }

    #[test]
    fn python_package_directory_reports_in_tree_import_edges() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "__init__.py", "");
        write_file(
            dir.path(),
            "app.py",
            concat!(
                "import os\n",
                "from util.text import slugify\n\n",
                "def main():\n    return slugify(os.getcwd())\n",
            ),
        );
        write_file(dir.path(), "util/__init__.py", "");
        write_file(
            dir.path(),
            "util/text.py",
            "def slugify(value):\n    return value.lower()\n",
        );

        let json = CouplingAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules: Vec<&str> = parsed["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        // Python modules are spelled with dots, relative to the root.
        assert!(modules.contains(&"app"), "got {modules:?}");
        assert!(modules.contains(&"util.text"), "got {modules:?}");
        // `import os` is stdlib and resolves to nothing in-tree, so the
        // only edge is app -> util.text.
        assert_eq!(parsed["edge_count"], 1);
        assert_eq!(parsed["edges"][0]["from"], "app");
        assert_eq!(parsed["edges"][0]["to"], "util.text");
    }

    #[test]
    fn python_file_root_is_a_single_module() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "solo.py", "def f():\n    return 1\n");
        let json = CouplingAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["module_count"], 1);
        assert_eq!(parsed["edge_count"], 0);
    }

    #[test]
    fn markdown_report_renders_cycles_when_present() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "lib.rs", "pub mod a;\npub mod b;\n");
        write_file(
            dir.path(),
            "a.rs",
            "use crate::b::Bar;\npub struct Foo;\nfn _x(_b: Bar) {}\n",
        );
        write_file(
            dir.path(),
            "b.rs",
            "use crate::a::Foo;\npub struct Bar;\nfn _y(_f: Foo) {}\n",
        );
        let md = CouplingAnalyzer::new()
            .analyze(dir.path().join("lib.rs"), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Dependency cycles"));
        assert!(md.contains("instability"));
    }
}
