//! `analyze context-span` — for each module in a Rust crate, the size
//! of its transitive outgoing dependency closure.
//!
//! Walks a single crate from a `.rs` root, builds the module tree
//! (reusing the coupling extractor), then for every module reports:
//!
//! * `direct` — modules it directly depends on (= `fan_out`).
//! * `transitive` — modules reachable through one or more outgoing
//!   edges, excluding the module itself.
//! * `files` — distinct source files those reachable modules live in,
//!   excluding the module's own file. This is the headline number an
//!   agent uses to estimate "how many files do I need to open to
//!   understand this".
//!
//! Cycles are handled — a module never counts itself in its own
//! transitive set, even when the graph loops back.
//!
//! Limitations are inherited from the coupling extractor: `#[path = ".."]`
//! attributes are not honoured, cross-crate references are dropped,
//! macro-generated items are invisible, and non-standard crate roots
//! must be passed as the `.rs` file directly.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use lens_domain::{
    ContextSpanReport, ModuleContextSpan, ModulePath, compute_context_spans, compute_report,
};
use lens_rust::{
    CouplingError as RustCouplingError, CrateModule, build_module_tree, extract_edges,
};
use serde::Serialize;

use super::{OutputFormat, SourceLang};

/// Errors raised while running the context-span analyzer.
///
/// Mirrors [`super::coupling::CouplingAnalyzerError`] because both
/// analyzers walk a Rust crate root in the same way; the variants are
/// duplicated rather than shared so each analyzer keeps a self-contained
/// error surface that doesn't leak across CLI subcommands.
#[derive(Debug, thiserror::Error)]
pub enum ContextSpanAnalyzerError {
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path:?}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error(
        "no usable Rust crate root found at {path:?}; pass a .rs file or a directory containing src/lib.rs or src/main.rs"
    )]
    UnsupportedRoot { path: PathBuf },
    #[error(
        "module `{parent}::{name}` declared but neither {name}.rs nor {name}/mod.rs found in {near:?}"
    )]
    MissingMod {
        parent: String,
        name: String,
        near: PathBuf,
    },
    #[error("failed to serialize report: {0}")]
    Serialize(#[from] serde_json::Error),
}

impl From<RustCouplingError> for ContextSpanAnalyzerError {
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
/// configuration (e.g. a `--top` cap) can be added later without
/// changing the call site.
#[derive(Debug, Default, Clone, Copy)]
pub struct ContextSpanAnalyzer;

impl ContextSpanAnalyzer {
    pub fn new() -> Self {
        Self
    }

    /// Resolve `path`, build the crate's module tree, and produce a
    /// report in `format`.
    pub fn analyze(
        &self,
        path: &Path,
        format: OutputFormat,
    ) -> Result<String, ContextSpanAnalyzerError> {
        let root = resolve_crate_root(path)?;
        let modules = build_module_tree(&root)?;
        let edges = extract_edges(&modules);
        // `compute_report` deduplicates and drops self-loops; reuse its
        // cleaned edge list so the closure walk doesn't re-do that work.
        let module_paths: Vec<ModulePath> = modules.iter().map(|m| m.path.clone()).collect();
        let report = compute_report(&module_paths, edges);
        let spans = compute_context_spans(&module_paths, &report.edges);
        let view = ReportView::new(&root, &spans, &modules);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&view).map_err(ContextSpanAnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&view)),
        }
    }
}

fn resolve_crate_root(path: &Path) -> Result<PathBuf, ContextSpanAnalyzerError> {
    let meta = std::fs::metadata(path).map_err(|source| ContextSpanAnalyzerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.is_file() {
        if SourceLang::from_path(path) == Some(SourceLang::Rust) {
            return Ok(path.to_path_buf());
        }
        return Err(ContextSpanAnalyzerError::UnsupportedRoot {
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
    Err(ContextSpanAnalyzerError::UnsupportedRoot {
        path: path.to_path_buf(),
    })
}

#[derive(Debug, Serialize)]
struct ReportView<'a> {
    crate_root: String,
    module_count: usize,
    modules: Vec<ModuleSpanView<'a>>,
}

impl<'a> ReportView<'a> {
    fn new(root: &Path, spans: &'a ContextSpanReport, modules: &[CrateModule]) -> Self {
        let module_views = spans
            .modules
            .iter()
            .map(|s| ModuleSpanView::new(s, modules))
            .collect();
        Self {
            crate_root: root.display().to_string(),
            module_count: spans.modules.len(),
            modules: module_views,
        }
    }
}

#[derive(Debug, Serialize)]
struct ModuleSpanView<'a> {
    path: &'a str,
    direct: usize,
    transitive: usize,
    /// Distinct source files the transitive closure spans, excluding
    /// this module's own file. Multiple inline modules in the same
    /// parent file collapse to one count here.
    files: usize,
    reachable: Vec<&'a str>,
}

impl<'a> ModuleSpanView<'a> {
    fn new(span: &'a ModuleContextSpan, modules: &[CrateModule]) -> Self {
        let files = transitive_file_count(&span.path, &span.reachable, modules);
        Self {
            path: span.path.as_str(),
            direct: span.direct,
            transitive: span.transitive,
            files,
            reachable: span.reachable.iter().map(ModulePath::as_str).collect(),
        }
    }
}

/// Map each reachable module to its source file and count how many
/// distinct files appear, excluding the home module's own file.
///
/// Uses a `BTreeSet<&Path>` so two modules that share a parent file
/// (e.g. inline `mod` blocks) collapse to one entry. Modules whose
/// path doesn't appear in `modules` are skipped — that should only
/// happen when a caller passes mismatched inputs.
fn transitive_file_count(
    home: &ModulePath,
    reachable: &[ModulePath],
    modules: &[CrateModule],
) -> usize {
    let home_file = modules
        .iter()
        .find(|m| &m.path == home)
        .map(|m| m.file.as_path());
    let mut files: BTreeSet<&Path> = BTreeSet::new();
    for path in reachable {
        let Some(file) = modules.iter().find(|m| &m.path == path) else {
            continue;
        };
        if Some(file.file.as_path()) == home_file {
            continue;
        }
        files.insert(file.file.as_path());
    }
    files.len()
}

const TOP_REACHABLE_LIMIT: usize = 5;

fn format_markdown(view: &ReportView<'_>) -> String {
    let mut out = format!(
        "# Context span report: {} ({} module(s))\n",
        view.crate_root, view.module_count,
    );
    if view.modules.is_empty() {
        out.push_str("\n_No modules discovered._\n");
        return out;
    }
    render_modules_table(&mut out, &view.modules);
    out
}

fn render_modules_table(out: &mut String, modules: &[ModuleSpanView<'_>]) {
    // writeln! into a String cannot fail; the result is swallowed
    // deliberately to satisfy the workspace's `unwrap_used` lint.
    let _ = writeln!(out, "\n## Modules (by transitive desc)\n");
    let _ = writeln!(out, "| module | direct | transitive | files | reachable |");
    let _ = writeln!(out, "| --- | ---: | ---: | ---: | --- |");
    let mut sorted: Vec<&ModuleSpanView<'_>> = modules.iter().collect();
    sorted.sort_by(|a, b| {
        b.transitive
            .cmp(&a.transitive)
            .then_with(|| b.files.cmp(&a.files))
            .then_with(|| b.direct.cmp(&a.direct))
            .then_with(|| a.path.cmp(b.path))
    });
    for m in sorted {
        let preview = reachable_preview(&m.reachable);
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} |",
            m.path, m.direct, m.transitive, m.files, preview,
        );
    }
}

fn reachable_preview(reachable: &[&str]) -> String {
    if reachable.is_empty() {
        return "—".to_owned();
    }
    let head: Vec<&&str> = reachable.iter().take(TOP_REACHABLE_LIMIT).collect();
    let mut s = head.iter().map(|x| **x).collect::<Vec<_>>().join(", ");
    if reachable.len() > TOP_REACHABLE_LIMIT {
        let _ = write!(s, ", … (+{} more)", reachable.len() - TOP_REACHABLE_LIMIT);
    }
    s
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

    /// Build a small crate with a clear chain a → b → c so the
    /// closure has something interesting to walk:
    ///
    /// * `crate::a` uses `crate::b::Bar` and calls into `crate::c`
    ///   transitively via `crate::b`.
    /// * `crate::b` depends on `crate::c::Quux`.
    /// * `crate::c` is a leaf.
    fn chain_crate(dir: &Path) -> PathBuf {
        let lib = write_file(dir, "lib.rs", "pub mod a;\npub mod b;\npub mod c;\n");
        write_file(dir, "a.rs", "use crate::b::Bar;\npub fn _x(_b: Bar) {}\n");
        write_file(
            dir,
            "b.rs",
            "use crate::c::Quux;\npub struct Bar;\npub fn _y(_q: Quux) {}\n",
        );
        write_file(dir, "c.rs", "pub struct Quux;\n");
        lib
    }

    #[test]
    fn json_report_includes_top_level_counts() {
        let dir = tempfile::tempdir().unwrap();
        let lib = chain_crate(dir.path());
        let json = ContextSpanAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // 4 modules: crate, crate::a, crate::b, crate::c.
        assert_eq!(parsed["module_count"], 4);
        let modules = parsed["modules"].as_array().unwrap();
        assert_eq!(modules.len(), 4);
    }

    #[test]
    fn json_report_records_direct_transitive_and_files_for_chain() {
        let dir = tempfile::tempdir().unwrap();
        let lib = chain_crate(dir.path());
        let json = ContextSpanAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules = parsed["modules"].as_array().unwrap();
        let a = modules.iter().find(|m| m["path"] == "crate::a").unwrap();
        // a directly depends on b only; transitively it reaches b and c.
        assert_eq!(a["direct"].as_u64().unwrap(), 1);
        assert_eq!(a["transitive"].as_u64().unwrap(), 2);
        assert_eq!(a["files"].as_u64().unwrap(), 2);
        let reachable: Vec<&str> = a["reachable"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(reachable.contains(&"crate::b"));
        assert!(reachable.contains(&"crate::c"));

        let c = modules.iter().find(|m| m["path"] == "crate::c").unwrap();
        // c is a leaf.
        assert_eq!(c["direct"].as_u64().unwrap(), 0);
        assert_eq!(c["transitive"].as_u64().unwrap(), 0);
        assert_eq!(c["files"].as_u64().unwrap(), 0);
    }

    #[test]
    fn markdown_report_contains_table_and_module_paths() {
        let dir = tempfile::tempdir().unwrap();
        let lib = chain_crate(dir.path());
        let md = ContextSpanAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Context span report:"));
        assert!(md.contains("## Modules"));
        assert!(md.contains("| module |"));
        assert!(md.contains("crate::a"));
        assert!(md.contains("crate::b"));
        assert!(md.contains("crate::c"));
    }

    #[test]
    fn cycle_does_not_count_start_in_its_own_span() {
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
        let json = ContextSpanAnalyzer::new()
            .analyze(&dir.path().join("lib.rs"), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules = parsed["modules"].as_array().unwrap();
        let a = modules.iter().find(|m| m["path"] == "crate::a").unwrap();
        let reachable: Vec<&str> = a["reachable"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // The closure reaches b but not a itself.
        assert!(!reachable.contains(&"crate::a"));
        assert!(reachable.contains(&"crate::b"));
        assert_eq!(a["transitive"].as_u64().unwrap(), 1);
    }

    #[test]
    fn inline_modules_share_a_file_so_files_dedupe() {
        // Two inline modules in lib.rs both depend on a separate file.
        // crate::host (in lib.rs) reaches crate::leaf (in leaf.rs).
        // The inline-mod sibling (crate::sibling) doesn't change the
        // file count for crate::host because they share lib.rs.
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(
            dir.path(),
            "lib.rs",
            r#"
            pub mod leaf;
            pub mod host {
                use crate::leaf::Leaf;
                pub fn _x(_l: Leaf) {}
                pub fn _y() { crate::sibling::ping(); }
            }
            pub mod sibling {
                pub fn ping() {}
            }
            "#,
        );
        write_file(dir.path(), "leaf.rs", "pub struct Leaf;\n");
        let json = ContextSpanAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules = parsed["modules"].as_array().unwrap();
        let host = modules.iter().find(|m| m["path"] == "crate::host").unwrap();
        // host reaches {leaf, sibling} = 2 modules.
        assert_eq!(host["transitive"].as_u64().unwrap(), 2);
        // But sibling lives in lib.rs (same file as host); only leaf.rs
        // is a new file. files = 1.
        assert_eq!(host["files"].as_u64().unwrap(), 1);
    }

    #[test]
    fn directory_root_detects_src_lib_rs() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "pub fn solo() {}\n");
        let json = ContextSpanAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["module_count"], 1);
        let only = &parsed["modules"][0];
        assert_eq!(only["transitive"].as_u64().unwrap(), 0);
        assert_eq!(only["files"].as_u64().unwrap(), 0);
    }

    #[test]
    fn unsupported_extension_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_file(dir.path(), "notes.txt", "hello");
        let err = ContextSpanAnalyzer::new()
            .analyze(&f, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(
            err,
            ContextSpanAnalyzerError::UnsupportedRoot { .. }
        ));
    }

    #[test]
    fn missing_path_surfaces_io_error() {
        let err = ContextSpanAnalyzer::new()
            .analyze(
                Path::new("/definitely/does/not/exist.rs"),
                OutputFormat::Json,
            )
            .unwrap_err();
        assert!(matches!(err, ContextSpanAnalyzerError::Io { .. }));
    }

    #[test]
    fn missing_mod_file_surfaces_missing_mod_error() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "mod ghost;\n");
        let err = ContextSpanAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, ContextSpanAnalyzerError::MissingMod { .. }));
    }

    #[test]
    fn invalid_rust_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "fn ??? {");
        let err = ContextSpanAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, ContextSpanAnalyzerError::Parse { .. }));
    }

    #[test]
    fn reachable_preview_truncates_long_lists() {
        let xs: Vec<&str> = vec!["a", "b", "c", "d", "e", "f", "g"];
        let preview = reachable_preview(&xs);
        assert!(preview.starts_with("a, b, c, d, e"));
        assert!(preview.contains("(+2 more)"));
    }

    #[test]
    fn reachable_preview_renders_em_dash_when_empty() {
        let xs: Vec<&str> = Vec::new();
        assert_eq!(reachable_preview(&xs), "—");
    }
}
