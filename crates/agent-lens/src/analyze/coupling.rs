//! `analyze coupling` — module-level coupling metrics for a Rust crate.
//!
//! Walks a single crate from a `.rs` root, builds the module tree, then
//! reports five metrics derived from the cross-module reference graph:
//! Number of Couplings, Fan-In, Fan-Out, simplified Henry-Kafura
//! Information Flow Complexity, and per-pair Inter-module Coupling
//! (distinct shared symbols). JSON is the default machine-readable
//! output; `--format md` emits a compact summary tuned for LLM context
//! windows rather than for humans.
//!
//! Limitations carried over from the underlying extractor:
//!
//! * `#[path = "..."]` attributes on `mod` declarations are not honoured.
//! * Cross-crate references are silently dropped (this analyzer is
//!   single-crate by design).
//! * Macro-generated items are invisible to `syn` and therefore
//!   invisible here.
//! * Non-standard crate roots (e.g. `[lib].path` in `Cargo.toml`) are
//!   not detected. Pass the root file directly when the layout is
//!   unusual.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use lens_domain::{
    CouplingEdge, CouplingReport, ModuleMetrics, ModulePath, PairCoupling, compute_report,
};
use lens_rust::{CouplingError as RustCouplingError, build_module_tree, extract_edges};
use serde::Serialize;

use super::{OutputFormat, SourceLang};

/// Errors raised while running the coupling analyzer.
#[derive(Debug)]
pub enum CouplingAnalyzerError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The provided path exists but isn't a `.rs` file or a directory
    /// containing a recognisable crate root.
    UnsupportedRoot {
        path: PathBuf,
    },
    /// `mod foo;` was declared in a parent file but neither `foo.rs` nor
    /// `foo/mod.rs` could be found.
    MissingMod {
        parent: String,
        name: String,
        near: PathBuf,
    },
    Serialize(serde_json::Error),
}

impl std::fmt::Display for CouplingAnalyzerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "failed to read {}: {source}", path.display()),
            Self::Parse { path, source } => {
                write!(f, "failed to parse {}: {source}", path.display())
            }
            Self::UnsupportedRoot { path } => write!(
                f,
                "no usable Rust crate root found at {}; pass a .rs file or a directory containing src/lib.rs or src/main.rs",
                path.display()
            ),
            Self::MissingMod { parent, name, near } => write!(
                f,
                "module `{parent}::{name}` declared but neither {0}.rs nor {0}/mod.rs found in {1}",
                name,
                near.display()
            ),
            Self::Serialize(e) => write!(f, "failed to serialize report: {e}"),
        }
    }
}

impl std::error::Error for CouplingAnalyzerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source.as_ref()),
            Self::Serialize(e) => Some(e),
            Self::UnsupportedRoot { .. } | Self::MissingMod { .. } => None,
        }
    }
}

impl From<RustCouplingError> for CouplingAnalyzerError {
    fn from(value: RustCouplingError) -> Self {
        match value {
            RustCouplingError::Io { path, source } => Self::Io { path, source },
            RustCouplingError::Parse { path, source } => Self::Parse {
                path,
                source: Box::new(source),
            },
            RustCouplingError::MissingMod { parent, name, near } => {
                Self::MissingMod { parent, name, near }
            }
        }
    }
}

/// Stateless analyzer entry point. Kept as a struct so per-run
/// configuration (filters, thresholds) can be added later without
/// breaking the CLI surface.
#[derive(Debug, Default, Clone, Copy)]
pub struct CouplingAnalyzer;

impl CouplingAnalyzer {
    pub fn new() -> Self {
        Self
    }

    /// Resolve `path`, build the crate's module tree, and produce a
    /// report in `format`.
    pub fn analyze(
        &self,
        path: &Path,
        format: OutputFormat,
    ) -> Result<String, CouplingAnalyzerError> {
        let root = resolve_crate_root(path)?;
        let modules = build_module_tree(&root)?;
        let edges = extract_edges(&modules);
        let module_paths: Vec<ModulePath> = modules.into_iter().map(|m| m.path).collect();
        let report = compute_report(&module_paths, edges);
        let view = ReportView::new(&root, &report);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&view).map_err(CouplingAnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&view)),
        }
    }
}

/// Map a user-provided path to a single `.rs` crate root.
///
/// Supports three forms:
/// 1. Direct `.rs` file → use as-is.
/// 2. Directory containing `src/lib.rs` → use that.
/// 3. Directory containing `src/main.rs` → use that.
///
/// Anything else surfaces [`CouplingAnalyzerError::UnsupportedRoot`] or
/// an [`CouplingAnalyzerError::Io`] when the path doesn't exist at all.
fn resolve_crate_root(path: &Path) -> Result<PathBuf, CouplingAnalyzerError> {
    let meta = std::fs::metadata(path).map_err(|source| CouplingAnalyzerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.is_file() {
        if SourceLang::from_path(path) == Some(SourceLang::Rust) {
            return Ok(path.to_path_buf());
        }
        return Err(CouplingAnalyzerError::UnsupportedRoot {
            path: path.to_path_buf(),
        });
    }
    if meta.is_dir() {
        for candidate in ["src/lib.rs", "src/main.rs"] {
            let probe = path.join(candidate);
            if probe.is_file() {
                return Ok(probe);
            }
        }
    }
    Err(CouplingAnalyzerError::UnsupportedRoot {
        path: path.to_path_buf(),
    })
}

#[derive(Debug, Serialize)]
struct ReportView<'a> {
    crate_root: String,
    module_count: usize,
    edge_count: usize,
    modules: Vec<ModuleView<'a>>,
    edges: Vec<EdgeView<'a>>,
    pairs: Vec<PairView<'a>>,
}

impl<'a> ReportView<'a> {
    fn new(root: &Path, report: &'a CouplingReport) -> Self {
        Self {
            crate_root: root.display().to_string(),
            module_count: report.modules.len(),
            edge_count: report.number_of_couplings,
            modules: report.modules.iter().map(ModuleView::from).collect(),
            edges: report.edges.iter().map(EdgeView::from).collect(),
            pairs: report.pairs.iter().map(PairView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ModuleView<'a> {
    path: &'a str,
    fan_in: usize,
    fan_out: usize,
    ifc: u64,
}

impl<'a> From<&'a ModuleMetrics> for ModuleView<'a> {
    fn from(m: &'a ModuleMetrics) -> Self {
        Self {
            path: m.path.as_str(),
            fan_in: m.fan_in,
            fan_out: m.fan_out,
            ifc: m.ifc,
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
        "# Coupling report: {} ({} module(s), {} edge(s))\n",
        view.crate_root, view.module_count, view.edge_count,
    );
    if view.modules.is_empty() {
        out.push_str("\n_No modules discovered._\n");
        return out;
    }
    render_modules_table(&mut out, &view.modules);
    render_pairs(&mut out, &view.pairs);
    out
}

fn render_modules_table(out: &mut String, modules: &[ModuleView<'_>]) {
    // writeln! into a String cannot fail; the result is swallowed
    // deliberately rather than unwrapped to satisfy the workspace's
    // `unwrap_used` lint.
    let _ = writeln!(out, "\n## Modules (by IFC desc)\n");
    let _ = writeln!(out, "| module | fan_in | fan_out | ifc |");
    let _ = writeln!(out, "| --- | ---: | ---: | ---: |");
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
            "| {} | {} | {} | {} |",
            m.path, m.fan_in, m.fan_out, m.ifc
        );
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
    use std::io::Write;

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

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
}
