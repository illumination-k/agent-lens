//! The module-level dependency graph shared by `coupling`,
//! `communities`, and the history analyzers' static file graph.
//!
//! Each consumer answers a different question off the same input: a list
//! of modules plus the [`CouplingEdge`]s between them. Building it here
//! keeps the four per-language builders in one place, so a fix to (say)
//! Go root detection lands once.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use tracing::warn;

use lens_domain::{CouplingEdge, ModulePath, TypeCensus};

use super::module_label::ModuleLabeler;
use super::{
    AnalyzePathFilter, CrateAnalyzerError, SourceLang, relative_display_path, resolve_crate_root,
};

/// One module in the graph, paired with the file it was read from.
#[derive(Debug, Clone)]
pub(crate) struct ModuleFile {
    pub(crate) path: ModulePath,
    pub(crate) file: PathBuf,
    /// Abstract and concrete types the module declares, for Martin's
    /// abstractness. `None` where the adapter takes no census
    /// (TypeScript / JavaScript and Python, which extract no abstraction
    /// declarations), as distinct from a census that found no types.
    pub(crate) types: Option<TypeCensus>,
}

/// A language-agnostic module dependency graph.
///
/// Module paths are stored in the canonical `crate::a::b` shape every
/// adapter emits; `labeler` carries the language's own spelling of that
/// shape and is applied when a report is rendered.
#[derive(Debug, Clone)]
pub(crate) struct ModuleGraph {
    pub(crate) root: PathBuf,
    pub(crate) labeler: ModuleLabeler,
    pub(crate) modules: Vec<ModuleFile>,
    pub(crate) edges: Vec<CouplingEdge>,
}

impl ModuleGraph {
    fn new<M: Into<ModuleFile>>(
        root: PathBuf,
        labeler: ModuleLabeler,
        modules: Vec<M>,
        edges: Vec<CouplingEdge>,
    ) -> Self {
        Self {
            root,
            labeler,
            modules: modules.into_iter().map(Into::into).collect(),
            edges,
        }
    }
}

/// Each adapter's module type carries `path` and `file` under its own
/// name (`CrateModule`, `TsModule`, `PythonModule`, `GoPackage`); the
/// rest of its payload is adapter-private and already consumed by
/// `extract_edges` before we get here, except the type census the Rust
/// and Go adapters can take.
impl From<lens_rust::CrateModule> for ModuleFile {
    fn from(m: lens_rust::CrateModule) -> Self {
        let types = Some(lens_rust::type_census(&m));
        Self {
            path: m.path,
            file: m.file,
            types,
        }
    }
}

impl From<lens_golang::GoPackage> for ModuleFile {
    fn from(m: lens_golang::GoPackage) -> Self {
        Self {
            path: m.path,
            file: m.file,
            types: Some(m.types),
        }
    }
}

macro_rules! impl_into_module_file_without_census {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl From<$ty> for ModuleFile {
                fn from(m: $ty) -> Self {
                    Self { path: m.path, file: m.file, types: None }
                }
            }
        )+
    };
}

impl_into_module_file_without_census!(lens_ts::TsModule, lens_py::PythonModule);

/// Resolve `path` to a language backend and build its module graph.
///
/// `filter` reaches the two adapters that discover modules by walking a
/// directory, so an excluded tree is never opened. Rust follows `mod`
/// declarations from the crate root and TypeScript follows imports from
/// an entry file, so neither can read a file the source did not name and
/// neither takes it. The caller applies the same filter to the finished
/// graph either way — that is what makes exclusion mean one thing across
/// all four languages; this parameter only decides who pays for it.
///
/// It is compiled against `path` rather than the resolved graph root, so
/// an anchored pattern means the same thing here as it does in the
/// caller's post-filter: for Go and Python the two are the same
/// directory.
///
/// A recognised source extension picks the backend directly. Otherwise a
/// directory is probed in order: `go.mod` or `go.work` first (the
/// unambiguous Go markers; `go.work` roots a multi-module workspace whose
/// root has no module of its own) — without this check a Go repo root would fall through
/// to the Rust crate-root resolver and fail with a confusing "no usable
/// Rust crate root"), then a Rust crate root, then Python as the last
/// resort. Python goes last because it has no root marker to test: any
/// directory can hold `.py` files, so the only honest probe is to walk
/// it and see whether anything came back.
pub(crate) fn build_graph(
    path: &Path,
    filter: &AnalyzePathFilter,
) -> Result<ModuleGraph, CrateAnalyzerError> {
    match super::index::AnalysisIndex::active() {
        // Keyed by the crate root and the path filter. The
        // filter is part of the identity because it shapes the walk, not
        // just the report: two analyzers in one profile that exclude
        // different trees do not see the same graph, and sharing one
        // between them would hand the second whichever exclusion the
        // first happened to run with.
        Some(index) => index
            .memoize_typed((path.to_path_buf(), filter.clone()), || {
                build_graph_uncached(path, filter)
            })
            .map(|graph| graph.as_ref().clone()),
        None => build_graph_uncached(path, filter),
    }
}

fn build_graph_uncached(
    path: &Path,
    filter: &AnalyzePathFilter,
) -> Result<ModuleGraph, CrateAnalyzerError> {
    if let Some(lang) = SourceLang::from_path(path) {
        return match lang {
            SourceLang::Rust => build_rust_graph(path),
            SourceLang::TypeScript(_) => build_ts_graph(path),
            SourceLang::Go => build_go_graph(path, filter),
            SourceLang::Python => build_python_graph(path, filter),
        };
    }

    if path.is_dir() {
        if path.join("go.mod").is_file() || path.join("go.work").is_file() {
            return build_go_graph(path, filter);
        }
        return match resolve_crate_root(path) {
            Ok(root) => build_rust_graph(&root),
            Err(_) => build_python_graph(path, filter),
        };
    }

    // Not a directory, and not a file any backend claims. Distinguish
    // "this file has no backend" from "this path is not there at all":
    // the second is a typo in the invocation, and saying so beats the
    // vaguer unsupported-root message.
    match std::fs::metadata(path) {
        Ok(_) => Err(unsupported_root(path)),
        Err(source) => Err(CrateAnalyzerError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(crate) fn build_rust_graph(path: &Path) -> Result<ModuleGraph, CrateAnalyzerError> {
    let root = resolve_crate_root(path)?;
    let modules = lens_rust::build_module_tree(&root)?;
    let edges = lens_rust::extract_edges(&modules);
    Ok(ModuleGraph::new(
        root,
        ModuleLabeler::rust(),
        modules,
        edges,
    ))
}

pub(crate) fn build_ts_graph(path: &Path) -> Result<ModuleGraph, CrateAnalyzerError> {
    let modules = lens_ts::build_module_tree(path)?;
    let edges = lens_ts::extract_edges(&modules);
    Ok(ModuleGraph::new(
        path.to_path_buf(),
        ModuleLabeler::typescript(),
        modules,
        edges,
    ))
}

/// Python is reached without a root marker — either an explicit `.py`
/// file or as the directory fallback — so an empty walk is the only
/// signal that the path was never a Python root, so it is unsupported.
/// An explicit Go module marker, by contrast, is evidence enough that
/// the caller meant that root: a module with no packages yet is an
/// empty graph, not a misuse.
fn build_python_graph(
    path: &Path,
    filter: &AnalyzePathFilter,
) -> Result<ModuleGraph, CrateAnalyzerError> {
    let modules = lens_py::build_module_tree(path, &filter.compile(path)?)?;
    if modules.is_empty() {
        return Err(unsupported_root(path));
    }
    let edges = lens_py::extract_edges(&modules);
    Ok(ModuleGraph::new(
        path.to_path_buf(),
        ModuleLabeler::python(),
        modules,
        edges,
    ))
}

fn build_go_graph(
    path: &Path,
    filter: &AnalyzePathFilter,
) -> Result<ModuleGraph, CrateAnalyzerError> {
    let modules = lens_golang::build_module_tree(path, &filter.compile(path)?)?;
    // The `go.mod` module directive is what turns a package's relative
    // position into the import path an agent would actually type, so it
    // is read for labelling even though edge resolution reads it too.
    let labeler = ModuleLabeler::go(lens_golang::module_prefix(&modules));
    let edges = lens_golang::extract_edges(&modules);
    Ok(ModuleGraph::new(
        path.to_path_buf(),
        labeler,
        modules,
        edges,
    ))
}

/// Walk `root` (gitignore-aware) and pick every file matching one of
/// `patterns`, then merge the TS/JS module trees rooted at each match
/// into a single graph. Files are deduplicated by path so imports shared
/// across entries collapse to one module. Frameworks with many implicit
/// entries (Next.js App Router, file-routed Remix / Astro) have no
/// single entry file to grow the graph from.
pub(crate) fn build_ts_graph_from_entry_globs(
    root: &Path,
    patterns: &[String],
) -> Result<ModuleGraph, CrateAnalyzerError> {
    if !root.is_dir() {
        return Err(CrateAnalyzerError::EntryGlobBaseNotDir {
            path: root.to_path_buf(),
        });
    }
    let no_matches = || CrateAnalyzerError::EntryGlobNoMatches {
        path: root.to_path_buf(),
        patterns: patterns.to_vec(),
    };
    let entries = collect_entry_glob_matches(root, patterns)?;
    let mut modules_by_file: BTreeMap<PathBuf, ModuleFile> = BTreeMap::new();
    let mut all_edges: Vec<CouplingEdge> = Vec::new();
    for entry in entries {
        if !matches!(
            SourceLang::from_path(&entry),
            Some(SourceLang::TypeScript(_))
        ) {
            warn!(
                target: "agent_lens::coupling",
                file = %entry.display(),
                "skipping non-TS/JS --entry-glob match",
            );
            continue;
        }
        let graph = build_ts_graph(&entry)?;
        all_edges.extend(graph.edges);
        for module in graph.modules {
            modules_by_file.entry(module.file.clone()).or_insert(module);
        }
    }
    if modules_by_file.is_empty() {
        return Err(no_matches());
    }
    Ok(ModuleGraph::new(
        root.to_path_buf(),
        ModuleLabeler::typescript(),
        modules_by_file.into_values().collect(),
        all_edges,
    ))
}

fn collect_entry_glob_matches(
    root: &Path,
    patterns: &[String],
) -> Result<Vec<PathBuf>, CrateAnalyzerError> {
    let globset = compile_entry_globs(patterns)?;
    let mut matches: BTreeSet<PathBuf> = BTreeSet::new();
    for entry in WalkBuilder::new(root).build() {
        let entry = entry.map_err(|e| CrateAnalyzerError::Io {
            path: root.to_path_buf(),
            source: std::io::Error::other(e),
        })?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if globset.is_match(relative_display_path(path, root)) {
            matches.insert(path.to_path_buf());
        }
    }
    Ok(matches.into_iter().collect())
}

fn compile_entry_globs(patterns: &[String]) -> Result<GlobSet, CrateAnalyzerError> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|source| CrateAnalyzerError::InvalidEntryGlob {
            pattern: pattern.clone(),
            source,
        })?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|source| CrateAnalyzerError::InvalidEntryGlob {
            pattern: patterns.join(", "),
            source,
        })
}

fn unsupported_root(path: &Path) -> CrateAnalyzerError {
    CrateAnalyzerError::UnsupportedRoot {
        path: path.to_path_buf(),
    }
}

/// Clone out the module paths, the form [`lens_domain::compute_report`]
/// and [`lens_domain::compute_context_spans`] both take.
pub(crate) fn module_paths(graph: &ModuleGraph) -> Vec<ModulePath> {
    graph.modules.iter().map(|m| m.path.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn python_file_root_builds() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "m.py", "import os\n");
        let graph = build_graph(&file, &AnalyzePathFilter::new()).unwrap();
        assert_eq!(graph.modules.len(), 1);
    }

    #[test]
    fn go_mod_marker_wins_over_the_rust_crate_resolver() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/p\n");
        write(dir.path(), "main.go", "package main\n\nfunc main() {}\n");
        // A `src/lib.rs` alongside `go.mod` would otherwise let the Rust
        // resolver claim this directory.
        write(dir.path(), "src/lib.rs", "pub fn f() {}\n");
        let graph = build_graph(dir.path(), &AnalyzePathFilter::new()).unwrap();
        assert_eq!(graph.root, dir.path());
        assert!(!graph.modules.is_empty());
    }

    /// A `go.work` root with no `go.mod` of its own is one Go graph over
    /// every module under it, so an import of a sibling module is an edge.
    #[test]
    fn go_work_root_spans_its_modules() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "go.work",
            "go 1.23\n\nuse (\n\t./app\n\t./lib\n)\n",
        );
        write(dir.path(), "app/go.mod", "module example.com/app\n");
        write(
            dir.path(),
            "app/main.go",
            "package main\n\nimport \"example.com/lib/util\"\n\nfunc main() { util.Run() }\n",
        );
        write(dir.path(), "lib/go.mod", "module example.com/lib\n");
        write(
            dir.path(),
            "lib/util/util.go",
            "package util\n\nfunc Run() {}\n",
        );
        // Without the `go.work` probe the Rust resolver would claim this.
        write(dir.path(), "src/lib.rs", "pub fn f() {}\n");

        let graph = build_graph(dir.path(), &AnalyzePathFilter::new()).unwrap();
        assert!(
            graph
                .edges
                .iter()
                .any(|e| e.from.as_str() == "crate::app" && e.to.as_str() == "crate::lib::util"),
            "{:?}",
            graph.edges,
        );
    }

    #[test]
    fn empty_go_module_is_an_empty_graph() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/p\n");
        assert!(
            build_graph(dir.path(), &AnalyzePathFilter::new())
                .unwrap()
                .modules
                .is_empty()
        );
    }

    #[test]
    fn rust_crate_directory_resolves_through_its_crate_root() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", "pub mod a;\n");
        write(dir.path(), "src/a.rs", "pub fn f() {}\n");
        let graph = build_graph(dir.path(), &AnalyzePathFilter::new()).unwrap();
        assert_eq!(graph.root, dir.path().join("src/lib.rs"));
        assert_eq!(graph.modules.len(), 2);
    }

    #[test]
    fn python_directory_is_the_last_resort() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pkg/m.py", "import os\n");
        let graph = build_graph(dir.path(), &AnalyzePathFilter::new()).unwrap();
        assert!(!graph.modules.is_empty());
    }

    #[test]
    fn rust_crate_root_wins_over_the_python_fallback() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", "pub fn f() {}\n");
        write(dir.path(), "setup.py", "from setuptools import setup\n");
        let graph = build_graph(dir.path(), &AnalyzePathFilter::new()).unwrap();
        assert_eq!(graph.root, dir.path().join("src/lib.rs"));
    }

    #[test]
    fn directory_with_no_recognisable_root_reports_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "README.md", "nothing to analyze\n");
        let err = build_graph(dir.path(), &AnalyzePathFilter::new()).unwrap_err();
        assert!(
            matches!(err, CrateAnalyzerError::UnsupportedRoot { .. }),
            "got {err:?}",
        );
    }

    #[test]
    fn extensionless_file_is_unsupported_but_a_missing_path_is_io() {
        let dir = tempfile::tempdir().unwrap();
        let notes = write(dir.path(), "NOTES", "nothing to analyze\n");
        let err = build_graph(&notes, &AnalyzePathFilter::new()).unwrap_err();
        assert!(
            matches!(err, CrateAnalyzerError::UnsupportedRoot { .. }),
            "got {err:?}",
        );

        let err = build_graph(&dir.path().join("ghost"), &AnalyzePathFilter::new()).unwrap_err();
        assert!(matches!(err, CrateAnalyzerError::Io { .. }), "got {err:?}");
    }

    #[test]
    fn module_paths_mirrors_the_module_list() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", "pub mod a;\n");
        write(dir.path(), "src/a.rs", "pub fn f() {}\n");
        let graph = build_graph(dir.path(), &AnalyzePathFilter::new()).unwrap();
        assert_eq!(module_paths(&graph).len(), graph.modules.len());
    }
}
