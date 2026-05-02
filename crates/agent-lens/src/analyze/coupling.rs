//! `analyze coupling` — module-level coupling metrics for a Rust crate
//! or a TypeScript / JavaScript module graph.
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
//! `export … from` specifiers; one source file is one module.
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

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use lens_domain::{
    CouplingEdge, CouplingReport, DependencyCycle, ModuleMetrics, ModulePath, PairCoupling,
    compute_report,
};
use serde::Serialize;

use super::{
    AnalyzePathFilter, CouplingAnalyzerError, OutputFormat, SourceLang, format_optional_f64,
    resolve_crate_root,
};

/// Stateless analyzer entry point. Kept as a struct so per-run
/// configuration (filters, thresholds) can be added later without
/// breaking the CLI surface.
#[derive(Debug, Default, Clone)]
pub struct CouplingAnalyzer {
    path_filter: AnalyzePathFilter,
}

impl CouplingAnalyzer {
    pub fn new() -> Self {
        Self {
            path_filter: AnalyzePathFilter::new(),
        }
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
    /// the entry source file and follows relative imports.
    pub fn analyze(
        &self,
        path: &Path,
        format: OutputFormat,
    ) -> Result<String, CouplingAnalyzerError> {
        let mut graph = build_graph(path)?;
        let filter = self.path_filter.compile(&graph.root)?;
        graph.modules.retain(|m| filter.includes_path(&m.file));
        let kept: std::collections::HashSet<&ModulePath> =
            graph.modules.iter().map(|m| &m.path).collect();
        graph
            .edges
            .retain(|e| kept.contains(&e.from) && kept.contains(&e.to));
        let module_paths: Vec<ModulePath> = graph.modules.iter().map(|m| m.path.clone()).collect();
        let report = compute_report(&module_paths, graph.edges);
        let view = ReportView::new(&graph.root, &report);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&view).map_err(CouplingAnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&view)),
        }
    }
}

#[derive(Debug)]
struct ModuleFile {
    path: ModulePath,
    file: PathBuf,
}

#[derive(Debug)]
struct ModuleGraph {
    root: PathBuf,
    modules: Vec<ModuleFile>,
    edges: Vec<CouplingEdge>,
}

fn build_graph(path: &Path) -> Result<ModuleGraph, CouplingAnalyzerError> {
    if let Some(SourceLang::TypeScript(_)) = SourceLang::from_path(path) {
        return build_ts_graph(path);
    }
    build_rust_graph(path)
}

fn build_rust_graph(path: &Path) -> Result<ModuleGraph, CouplingAnalyzerError> {
    let root = resolve_crate_root(path)?;
    let modules = lens_rust::build_module_tree(&root)?;
    let edges = lens_rust::extract_edges(&modules);
    Ok(ModuleGraph {
        root,
        modules: modules
            .into_iter()
            .map(|m| ModuleFile {
                path: m.path,
                file: m.file,
            })
            .collect(),
        edges,
    })
}

fn build_ts_graph(path: &Path) -> Result<ModuleGraph, CouplingAnalyzerError> {
    let modules = lens_ts::build_module_tree(path)?;
    let edges = lens_ts::extract_edges(&modules);
    Ok(ModuleGraph {
        root: path.to_path_buf(),
        modules: modules
            .into_iter()
            .map(|m| ModuleFile {
                path: m.path,
                file: m.file,
            })
            .collect(),
        edges,
    })
}

#[derive(Debug, Serialize)]
struct ReportView<'a> {
    crate_root: String,
    module_count: usize,
    edge_count: usize,
    cycle_count: usize,
    modules: Vec<ModuleView<'a>>,
    edges: Vec<EdgeView<'a>>,
    pairs: Vec<PairView<'a>>,
    cycles: Vec<CycleView<'a>>,
}

impl<'a> ReportView<'a> {
    fn new(root: &Path, report: &'a CouplingReport) -> Self {
        Self {
            crate_root: root.display().to_string(),
            module_count: report.modules.len(),
            edge_count: report.number_of_couplings,
            cycle_count: report.cycles.len(),
            modules: report.modules.iter().map(ModuleView::from).collect(),
            edges: report.edges.iter().map(EdgeView::from).collect(),
            pairs: report.pairs.iter().map(PairView::from).collect(),
            cycles: report.cycles.iter().map(CycleView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ModuleView<'a> {
    path: &'a str,
    fan_in: usize,
    fan_out: usize,
    ifc: u64,
    /// Robert C. Martin's instability `I = Ce / (Ca + Ce)`. Omitted from
    /// JSON when the module has no edges (so the ratio is undefined).
    #[serde(skip_serializing_if = "Option::is_none")]
    instability: Option<f64>,
}

impl<'a> From<&'a ModuleMetrics> for ModuleView<'a> {
    fn from(m: &'a ModuleMetrics) -> Self {
        Self {
            path: m.path.as_str(),
            fan_in: m.fan_in,
            fan_out: m.fan_out,
            ifc: m.ifc,
            instability: m.instability,
        }
    }
}

#[derive(Debug, Serialize)]
struct CycleView<'a> {
    size: usize,
    members: Vec<&'a str>,
}

impl<'a> From<&'a DependencyCycle> for CycleView<'a> {
    fn from(c: &'a DependencyCycle) -> Self {
        Self {
            size: c.members.len(),
            members: c.members.iter().map(ModulePath::as_str).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct EdgeView<'a> {
    from: &'a str,
    to: &'a str,
    symbol: &'a str,
    kind: &'static str,
}

impl<'a> From<&'a CouplingEdge> for EdgeView<'a> {
    fn from(e: &'a CouplingEdge) -> Self {
        Self {
            from: e.from.as_str(),
            to: e.to.as_str(),
            symbol: e.symbol.as_str(),
            kind: e.kind.as_str(),
        }
    }
}

#[derive(Debug, Serialize)]
struct PairView<'a> {
    a: &'a str,
    b: &'a str,
    shared_symbols: usize,
}

impl<'a> From<&'a PairCoupling> for PairView<'a> {
    fn from(p: &'a PairCoupling) -> Self {
        Self {
            a: p.a.as_str(),
            b: p.b.as_str(),
            shared_symbols: p.shared_symbols,
        }
    }
}

const TOP_PAIRS_LIMIT: usize = 10;

fn format_markdown(view: &ReportView<'_>) -> String {
    let mut out = format!(
        "# Coupling report: {} ({} module(s), {} edge(s), {} cycle(s))\n",
        view.crate_root, view.module_count, view.edge_count, view.cycle_count,
    );
    if view.modules.is_empty() {
        out.push_str("\n_No modules discovered._\n");
        return out;
    }
    render_modules_table(&mut out, &view.modules);
    render_cycles(&mut out, &view.cycles);
    render_pairs(&mut out, &view.pairs);
    out
}

fn render_modules_table(out: &mut String, modules: &[ModuleView<'_>]) {
    // writeln! into a String cannot fail; the result is swallowed
    // deliberately rather than unwrapped to satisfy the workspace's
    // `unwrap_used` lint.
    let _ = writeln!(out, "\n## Modules (by IFC desc)\n");
    let _ = writeln!(out, "| module | fan_in | fan_out | ifc | instability |");
    let _ = writeln!(out, "| --- | ---: | ---: | ---: | ---: |");
    let mut sorted: Vec<&ModuleView<'_>> = modules.iter().collect();
    sorted.sort_by(|a, b| {
        b.ifc
            .cmp(&a.ifc)
            .then_with(|| b.fan_in.cmp(&a.fan_in))
            .then_with(|| b.fan_out.cmp(&a.fan_out))
            .then_with(|| a.path.cmp(b.path))
    });
    for m in sorted {
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
}

fn render_cycles(out: &mut String, cycles: &[CycleView<'_>]) {
    if cycles.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## Dependency cycles\n");
    for c in cycles {
        let _ = writeln!(out, "- {} module(s): {}", c.size, c.members.join(" → "));
    }
}

fn render_pairs(out: &mut String, pairs: &[PairView<'_>]) {
    if pairs.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## Top coupled pairs\n");
    for p in pairs.iter().take(TOP_PAIRS_LIMIT) {
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
            .analyze(&dir.path().join("lib.rs"), OutputFormat::Json)
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
        assert!(msg.contains("no usable Rust crate root"), "got {msg}");
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
        let main_m = modules
            .iter()
            .find(|m| m["path"] == "crate::main")
            .expect("crate::main");
        let util_m = modules
            .iter()
            .find(|m| m["path"] == "crate::util")
            .expect("crate::util");
        assert!(main_m["fan_out"].as_u64().unwrap() >= 1);
        assert_eq!(main_m["fan_in"], 0);
        // util is depended on, so I = 0 (fully stable).
        assert_eq!(util_m["instability"].as_f64().unwrap(), 0.0);
        // main only depends on others, so I = 1 (fully unstable).
        assert_eq!(main_m["instability"].as_f64().unwrap(), 1.0);

        let pairs = parsed["pairs"].as_array().unwrap();
        assert!(pairs.iter().any(|p| {
            (p["a"] == "crate::main" && p["b"] == "crate::util")
                || (p["a"] == "crate::util" && p["b"] == "crate::main")
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
        assert!(members.contains(&"crate::a"));
        assert!(members.contains(&"crate::b"));
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
        assert!(md.contains("crate::main"));
        assert!(md.contains("crate::util"));
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
        assert!(modules.contains(&"crate::main"));
        assert!(!modules.contains(&"crate::generated"));
        // No edges should reference the dropped module.
        for e in parsed["edges"].as_array().unwrap() {
            assert_ne!(e["from"], "crate::generated");
            assert_ne!(e["to"], "crate::generated");
        }
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
            .analyze(&dir.path().join("lib.rs"), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Dependency cycles"));
        assert!(md.contains("instability"));
    }
}
