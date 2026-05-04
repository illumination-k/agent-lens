//! `analyze context-span` — for each module in a source graph, the
//! size of its transitive outgoing dependency closure.
//!
//! Rust walks a single crate from a `.rs` root and reuses the coupling
//! extractor. TypeScript / JavaScript starts at an entry file and
//! follows relative imports/re-exports. Python accepts either a `.py`
//! file or a directory and models discovered files as modules. For
//! every module, the report includes:
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
//! Rust limitations are inherited from the coupling extractor:
//! `#[path = ".."]` attributes are not honoured, cross-crate references
//! are dropped, macro-generated items are invisible, and non-standard
//! crate roots must be passed as the `.rs` file directly. TS / JS only
//! follows relative module specifiers reachable from the entry file.
//! Frameworks with many implicit entries (Next.js App Router,
//! file-routed Remix / Astro) can pass `--entry-glob` repeatedly to
//! merge several TS/JS entry trees into one report.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use lens_domain::{
    ContextSpanReport, CouplingEdge, ModuleContextSpan, ModulePath, compute_context_spans,
    compute_report,
};
use serde::Serialize;
use tracing::warn;

use super::{
    AnalyzePathFilter, OutputFormat, SourceLang, relative_display_path, resolve_crate_root,
};

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
        "unsupported context-span root {path:?}; pass a Rust crate root, a TS/JS entry file, a Python file/directory, or a Go file/module directory"
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
    #[error(transparent)]
    PathFilter(#[from] super::PathFilterError),
    #[error("--entry-glob requires {path:?} to be a directory")]
    EntryGlobBaseNotDir { path: PathBuf },
    #[error("invalid --entry-glob pattern {pattern:?}: {source}")]
    InvalidEntryGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },
    #[error("no files matched any --entry-glob pattern under {path:?}; patterns: {patterns:?}")]
    EntryGlobNoMatches {
        path: PathBuf,
        patterns: Vec<String>,
    },
}

impl From<lens_rust::CouplingError> for ContextSpanAnalyzerError {
    fn from(value: lens_rust::CouplingError) -> Self {
        match value {
            lens_rust::CouplingError::Io { path, source } => Self::Io { path, source },
            lens_rust::CouplingError::Parse { path, source } => Self::Parse {
                path,
                source: Box::new(source),
            },
            lens_rust::CouplingError::MissingMod { parent, name, near } => {
                Self::MissingMod { parent, name, near }
            }
        }
    }
}

impl From<super::CrateAnalyzerError> for ContextSpanAnalyzerError {
    fn from(value: super::CrateAnalyzerError) -> Self {
        match value {
            super::CrateAnalyzerError::Io { path, source } => Self::Io { path, source },
            super::CrateAnalyzerError::Parse { path, source } => Self::Parse { path, source },
            super::CrateAnalyzerError::UnsupportedRoot { path } => Self::UnsupportedRoot { path },
            super::CrateAnalyzerError::MissingMod { parent, name, near } => {
                Self::MissingMod { parent, name, near }
            }
            super::CrateAnalyzerError::Serialize(source) => Self::Serialize(source),
            super::CrateAnalyzerError::PathFilter(source) => Self::PathFilter(source),
        }
    }
}

impl From<lens_ts::CouplingError> for ContextSpanAnalyzerError {
    fn from(value: lens_ts::CouplingError) -> Self {
        match value {
            lens_ts::CouplingError::Io { path, source } => Self::Io { path, source },
            lens_ts::CouplingError::Parse { path, source } => Self::Parse {
                path,
                source: Box::new(source),
            },
        }
    }
}

impl From<lens_py::CouplingError> for ContextSpanAnalyzerError {
    fn from(value: lens_py::CouplingError) -> Self {
        match value {
            lens_py::CouplingError::Io { path, source } => Self::Io { path, source },
            lens_py::CouplingError::Parse { path, source } => Self::Parse {
                path,
                source: Box::new(source),
            },
            lens_py::CouplingError::UnsupportedRoot { path } => Self::UnsupportedRoot { path },
        }
    }
}

impl From<lens_golang::CouplingError> for ContextSpanAnalyzerError {
    fn from(value: lens_golang::CouplingError) -> Self {
        match value {
            lens_golang::CouplingError::Io { path, source } => Self::Io { path, source },
            lens_golang::CouplingError::Parse { path, source } => Self::Parse {
                path,
                source: Box::new(source),
            },
            lens_golang::CouplingError::UnsupportedRoot { path } => Self::UnsupportedRoot { path },
        }
    }
}

/// Stateless analyzer entry point. Kept as a struct so per-run
/// configuration (e.g. a `--top` cap) can be added later without
/// changing the call site.
#[derive(Debug, Default, Clone)]
pub struct ContextSpanAnalyzer {
    path_filter: AnalyzePathFilter,
    entry_globs: Vec<String>,
}

impl ContextSpanAnalyzer {
    pub fn new() -> Self {
        Self {
            path_filter: AnalyzePathFilter::new(),
            entry_globs: Vec::new(),
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

    /// Treat the analyzer's `path` argument as a project root and merge
    /// the TS/JS module trees rooted at every file matching one of
    /// `patterns`. `patterns` are gitignore-aware globs evaluated
    /// relative to `path`. Empty input keeps the default single-entry
    /// behavior.
    pub fn with_entry_globs(mut self, patterns: Vec<String>) -> Self {
        self.entry_globs = patterns;
        self
    }

    /// Resolve `path`, build the language-specific module graph, and
    /// produce a report in `format`.
    pub fn analyze(
        &self,
        path: &Path,
        format: OutputFormat,
    ) -> Result<String, ContextSpanAnalyzerError> {
        let mut graph = if self.entry_globs.is_empty() {
            build_graph(path)?
        } else {
            build_ts_graph_from_entry_globs(path, &self.entry_globs)?
        };
        let filter = self.path_filter.compile(&graph.root)?;
        graph.modules.retain(|m| filter.includes_path(&m.file));
        // `compute_report` deduplicates and drops self-loops; reuse its
        // cleaned edge list so the closure walk doesn't re-do that work.
        let module_paths: Vec<ModulePath> = graph.modules.iter().map(|m| m.path.clone()).collect();
        let report = compute_report(&module_paths, graph.edges);
        let spans = compute_context_spans(&module_paths, &report.edges);
        let view = ReportView::new(&graph.root, &spans, &graph.modules);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&view).map_err(ContextSpanAnalyzerError::Serialize)
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

fn build_graph(path: &Path) -> Result<ModuleGraph, ContextSpanAnalyzerError> {
    if let Some(lang) = SourceLang::from_path(path) {
        return match lang {
            SourceLang::Rust => build_rust_graph(path),
            SourceLang::TypeScript(_) => build_ts_graph(path),
            SourceLang::Python => build_python_graph(path),
            SourceLang::Go => build_go_graph(path),
        };
    }

    if path.is_dir() {
        // `go.mod` is the unambiguous signal of a Go module root, so
        // check it before the Rust crate-root resolver and the Python
        // fallback. Without this, a Go project would fall through to
        // Python's directory walk and report nonsense (or to the Rust
        // resolver and fail with a confusing error).
        if path.join("go.mod").is_file() {
            return build_go_graph(path);
        }
        if let Ok(root) = resolve_crate_root(path) {
            return build_rust_graph(&root);
        }
        return build_python_graph(path);
    }

    Err(ContextSpanAnalyzerError::UnsupportedRoot {
        path: path.to_path_buf(),
    })
}

fn build_rust_graph(path: &Path) -> Result<ModuleGraph, ContextSpanAnalyzerError> {
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

fn build_ts_graph(path: &Path) -> Result<ModuleGraph, ContextSpanAnalyzerError> {
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

fn build_python_graph(path: &Path) -> Result<ModuleGraph, ContextSpanAnalyzerError> {
    let modules = lens_py::build_module_tree(path)?;
    if modules.is_empty() {
        return Err(ContextSpanAnalyzerError::UnsupportedRoot {
            path: path.to_path_buf(),
        });
    }
    let edges = lens_py::extract_edges(&modules);
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

fn build_go_graph(path: &Path) -> Result<ModuleGraph, ContextSpanAnalyzerError> {
    let modules = lens_golang::build_module_tree(path)?;
    if modules.is_empty() {
        return Err(ContextSpanAnalyzerError::UnsupportedRoot {
            path: path.to_path_buf(),
        });
    }
    let edges = lens_golang::extract_edges(&modules);
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

/// Walk `root` (gitignore-aware) and pick every file matching one of
/// `patterns`, then merge the TS/JS module trees rooted at each match
/// into a single graph. Files are deduplicated by canonical path so
/// imports shared across entries collapse to one module.
fn build_ts_graph_from_entry_globs(
    root: &Path,
    patterns: &[String],
) -> Result<ModuleGraph, ContextSpanAnalyzerError> {
    if !root.is_dir() {
        return Err(ContextSpanAnalyzerError::EntryGlobBaseNotDir {
            path: root.to_path_buf(),
        });
    }
    let entries = collect_entry_glob_matches(root, patterns)?;
    if entries.is_empty() {
        return Err(ContextSpanAnalyzerError::EntryGlobNoMatches {
            path: root.to_path_buf(),
            patterns: patterns.to_vec(),
        });
    }

    let mut modules_by_file: BTreeMap<PathBuf, ModuleFile> = BTreeMap::new();
    let mut all_edges: Vec<CouplingEdge> = Vec::new();
    for entry in entries {
        match SourceLang::from_path(&entry) {
            Some(SourceLang::TypeScript(_)) => {}
            _ => {
                warn!(
                    target: "agent_lens::context_span",
                    file = %entry.display(),
                    "skipping non-TS/JS --entry-glob match",
                );
                continue;
            }
        }
        let modules = lens_ts::build_module_tree(&entry)?;
        let edges = lens_ts::extract_edges(&modules);
        for module in modules {
            modules_by_file
                .entry(module.file.clone())
                .or_insert_with(|| ModuleFile {
                    path: module.path,
                    file: module.file,
                });
        }
        all_edges.extend(edges);
    }

    if modules_by_file.is_empty() {
        return Err(ContextSpanAnalyzerError::EntryGlobNoMatches {
            path: root.to_path_buf(),
            patterns: patterns.to_vec(),
        });
    }

    Ok(ModuleGraph {
        root: root.to_path_buf(),
        modules: modules_by_file.into_values().collect(),
        edges: all_edges,
    })
}

fn collect_entry_glob_matches(
    root: &Path,
    patterns: &[String],
) -> Result<Vec<PathBuf>, ContextSpanAnalyzerError> {
    let globset = compile_entry_globs(patterns)?;
    let mut matches: BTreeSet<PathBuf> = BTreeSet::new();
    for entry in WalkBuilder::new(root).build() {
        let entry = entry.map_err(|e| ContextSpanAnalyzerError::Io {
            path: root.to_path_buf(),
            source: std::io::Error::other(e),
        })?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = relative_display_path(path, root);
        if globset.is_match(&rel) {
            matches.insert(path.to_path_buf());
        }
    }
    Ok(matches.into_iter().collect())
}

fn compile_entry_globs(patterns: &[String]) -> Result<GlobSet, ContextSpanAnalyzerError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob =
            Glob::new(pattern).map_err(|source| ContextSpanAnalyzerError::InvalidEntryGlob {
                pattern: pattern.clone(),
                source,
            })?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|source| ContextSpanAnalyzerError::InvalidEntryGlob {
            pattern: patterns.join(", "),
            source,
        })
}

#[derive(Debug, Serialize)]
struct ReportView<'a> {
    crate_root: String,
    module_count: usize,
    modules: Vec<ModuleSpanView<'a>>,
}

impl<'a> ReportView<'a> {
    fn new(root: &Path, spans: &'a ContextSpanReport, modules: &[ModuleFile]) -> Self {
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
    fn new(span: &'a ModuleContextSpan, modules: &[ModuleFile]) -> Self {
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
    modules: &[ModuleFile],
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
    use crate::test_support::write_file;
    use std::path::PathBuf;

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
    fn typescript_entry_file_reports_import_chain() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let main = write_file(
            &src,
            "main.ts",
            "import { f } from './a'; export const n = f();\n",
        );
        write_file(
            &src,
            "a.ts",
            "import { g } from './b'; export const f = () => g();\n",
        );
        write_file(&src, "b.ts", "export const g = () => 1;\n");

        let json = ContextSpanAnalyzer::new()
            .analyze(&main, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules = parsed["modules"].as_array().unwrap();
        let main = modules.iter().find(|m| m["path"] == "crate::main").unwrap();
        assert_eq!(main["direct"].as_u64().unwrap(), 1);
        assert_eq!(main["transitive"].as_u64().unwrap(), 2);
        assert_eq!(main["files"].as_u64().unwrap(), 2);
        let reachable: Vec<&str> = main["reachable"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(reachable.contains(&"crate::a"));
        assert!(reachable.contains(&"crate::b"));
    }

    #[test]
    fn go_module_directory_reports_import_chain() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "go.mod", "module github.com/x/proj\n");
        write_file(
            dir.path(),
            "a/a.go",
            "package a\n\nimport \"github.com/x/proj/b\"\n\nvar _ = b.X\n",
        );
        write_file(
            dir.path(),
            "b/b.go",
            "package b\n\nimport \"github.com/x/proj/c\"\n\nvar X = c.Y\n",
        );
        write_file(dir.path(), "c/c.go", "package c\n\nvar Y = 1\n");

        let json = ContextSpanAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules = parsed["modules"].as_array().unwrap();
        let a = modules.iter().find(|m| m["path"] == "crate::a").unwrap();
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
    }

    #[test]
    fn python_directory_reports_import_chain() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "a.py", "");
        write_file(dir.path(), "b.py", "import a\n");
        write_file(dir.path(), "c.py", "import b\n");

        let json = ContextSpanAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules = parsed["modules"].as_array().unwrap();
        let c = modules.iter().find(|m| m["path"] == "crate::c").unwrap();
        assert_eq!(c["direct"].as_u64().unwrap(), 1);
        assert_eq!(c["transitive"].as_u64().unwrap(), 2);
        assert_eq!(c["files"].as_u64().unwrap(), 2);
        let reachable: Vec<&str> = c["reachable"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(reachable.contains(&"crate::a"));
        assert!(reachable.contains(&"crate::b"));
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

        let only_tests = ContextSpanAnalyzer::new()
            .with_only_tests(true)
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&only_tests).unwrap();
        assert_eq!(parsed["module_count"], 1);
        assert_eq!(parsed["modules"][0]["path"], "crate::foo_test");

        let exclude_tests = ContextSpanAnalyzer::new()
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

        let exclude_generated = ContextSpanAnalyzer::new()
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

    /// Two TS entries that share a transitively-imported file. The
    /// shared file must collapse to one module so the report reflects
    /// the merged graph rather than a per-entry duplicate.
    #[test]
    fn entry_globs_merge_disjoint_trees_and_dedupe_shared_imports() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        let lib = dir.path().join("lib");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(&lib).unwrap();
        // Two App Router-style entries, both importing the same shared util.
        write_file(
            &app,
            "page.tsx",
            "import { sharedUtil } from '../lib/util';\nexport default function Page() { return sharedUtil(); }\n",
        );
        write_file(
            &app,
            "route.ts",
            "import { sharedUtil } from '../lib/util';\nexport function GET() { return sharedUtil(); }\n",
        );
        write_file(&lib, "util.ts", "export const sharedUtil = () => 'ok';\n");

        let json = ContextSpanAnalyzer::new()
            .with_entry_globs(vec![
                "app/**/page.tsx".to_owned(),
                "app/**/route.ts".to_owned(),
            ])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules: Vec<&str> = parsed["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        // Expect exactly: page, route, util — three discovered files.
        assert_eq!(parsed["module_count"], 3);
        assert!(modules.iter().any(|p| p.ends_with("::page")));
        assert!(modules.iter().any(|p| p.ends_with("::route")));
        assert!(modules.iter().any(|p| p.ends_with("::util")));
    }

    #[test]
    fn entry_globs_with_no_matches_error_clearly() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/main.ts", "export const x = 1;\n");
        let err = ContextSpanAnalyzer::new()
            .with_entry_globs(vec!["app/**/page.tsx".to_owned()])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(
            err,
            ContextSpanAnalyzerError::EntryGlobNoMatches { .. }
        ));
    }

    #[test]
    fn entry_globs_reject_file_path_as_base() {
        let dir = tempfile::tempdir().unwrap();
        let entry = write_file(dir.path(), "main.ts", "export const x = 1;\n");
        let err = ContextSpanAnalyzer::new()
            .with_entry_globs(vec!["**/*.ts".to_owned()])
            .analyze(&entry, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(
            err,
            ContextSpanAnalyzerError::EntryGlobBaseNotDir { .. }
        ));
    }

    #[test]
    fn entry_globs_reject_invalid_pattern() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "main.ts", "");
        let err = ContextSpanAnalyzer::new()
            .with_entry_globs(vec!["[".to_owned()])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(
            err,
            ContextSpanAnalyzerError::InvalidEntryGlob { .. }
        ));
    }

    /// `--entry-glob` should silently skip files whose extension is not
    /// TS/JS rather than blowing up the run, so a glob like
    /// `app/**/*` still produces a useful report when it incidentally
    /// catches a `.json` or `.css` sibling.
    #[test]
    fn entry_globs_skip_non_ts_matches() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("app");
        std::fs::create_dir_all(&app).unwrap();
        write_file(
            &app,
            "page.tsx",
            "import { x } from './data';\nexport default function P() { return x; }\n",
        );
        write_file(&app, "data.ts", "export const x = 1;\n");
        write_file(&app, "data.json", "{\"x\": 1}\n");

        let json = ContextSpanAnalyzer::new()
            .with_entry_globs(vec!["app/*".to_owned()])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let modules: Vec<&str> = parsed["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert!(modules.iter().any(|p| p.ends_with("::page")));
        assert!(modules.iter().any(|p| p.ends_with("::data")));
        assert!(!modules.iter().any(|p| p.ends_with("::data.json")));
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

    #[test]
    fn reachable_preview_at_exact_limit_has_no_more_suffix() {
        // Boundary: a list whose length equals TOP_REACHABLE_LIMIT must
        // not emit a "+0 more" tail. The strict `>` comparison in
        // `reachable_preview` is what guards this; weakening it to
        // `>=` would render a spurious "(+0 more)" suffix here.
        let xs: Vec<&str> = vec!["a", "b", "c", "d", "e"];
        assert_eq!(xs.len(), TOP_REACHABLE_LIMIT);
        let preview = reachable_preview(&xs);
        assert_eq!(preview, "a, b, c, d, e");
        assert!(!preview.contains("more"));
    }
}
