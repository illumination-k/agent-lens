//! The module-level dependency graph shared by `coupling` and
//! `context-span`.
//!
//! Both analyzers answer different questions off the same input: a list
//! of modules plus the [`CouplingEdge`]s between them. They used to keep
//! private, byte-identical copies of the [`ModuleFile`] / [`ModuleGraph`]
//! pair and of the four per-language builders, which meant a fix to (say)
//! Go root detection had to land twice.
//!
//! What legitimately differs between the two analyzers is *policy*, not
//! construction, so that part stays at the call site and is spelled out
//! by [`GraphPolicy`]: `coupling` has no Python module-graph backend and
//! tolerates an empty Go package list, `context-span` supports Python and
//! treats an empty package list as an unusable root.

use std::path::{Path, PathBuf};

use lens_domain::{CouplingEdge, ModulePath};

use super::{CrateAnalyzerError, SourceLang, resolve_crate_root};

/// One module in the graph, paired with the file it was read from.
#[derive(Debug)]
pub(crate) struct ModuleFile {
    pub(crate) path: ModulePath,
    pub(crate) file: PathBuf,
}

/// A language-agnostic module dependency graph.
#[derive(Debug)]
pub(crate) struct ModuleGraph {
    pub(crate) root: PathBuf,
    pub(crate) modules: Vec<ModuleFile>,
    pub(crate) edges: Vec<CouplingEdge>,
}

impl ModuleGraph {
    fn new<M: Into<ModuleFile>>(root: PathBuf, modules: Vec<M>, edges: Vec<CouplingEdge>) -> Self {
        Self {
            root,
            modules: modules.into_iter().map(Into::into).collect(),
            edges,
        }
    }
}

/// Each adapter's module type carries the same two fields under its own
/// name (`CrateModule`, `TsModule`, `PythonModule`, `GoPackage`); the
/// rest of its payload is adapter-private and already consumed by
/// `extract_edges` before we get here.
macro_rules! impl_into_module_file {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl From<$ty> for ModuleFile {
                fn from(m: $ty) -> Self {
                    Self { path: m.path, file: m.file }
                }
            }
        )+
    };
}

impl_into_module_file!(
    lens_rust::CrateModule,
    lens_ts::TsModule,
    lens_py::PythonModule,
    lens_golang::GoPackage,
);

/// The two axes on which the analyzers disagree about what counts as a
/// usable root.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GraphPolicy {
    /// Whether a Python file or package directory is a supported root.
    /// `coupling` has no Python module-graph backend and rejects one.
    python_roots: bool,
    /// Whether a directory walk that finds zero modules is an error
    /// rather than an empty graph. Applies to the Python and Go
    /// backends, the two that scan a directory instead of following a
    /// declared entry point.
    empty_walk_is_unsupported: bool,
}

impl GraphPolicy {
    /// `analyze coupling`: Rust, TS/JS, and Go roots only.
    pub(crate) const COUPLING: Self = Self {
        python_roots: false,
        empty_walk_is_unsupported: false,
    };

    /// `analyze context-span`: all four languages, and a root that walks
    /// to nothing is reported rather than rendered as an empty report.
    pub(crate) const CONTEXT_SPAN: Self = Self {
        python_roots: true,
        empty_walk_is_unsupported: true,
    };
}

/// Resolve `path` to a language backend and build its module graph.
///
/// A recognised source extension picks the backend directly. Otherwise a
/// directory is probed in order: `go.mod` first (the unambiguous Go
/// module marker — without this check a Go repo root would fall through
/// to the Rust crate-root resolver and fail with a confusing "no usable
/// Rust crate root"), then a Rust crate root, then Python when the
/// policy allows it.
pub(crate) fn build_graph(
    path: &Path,
    policy: GraphPolicy,
) -> Result<ModuleGraph, CrateAnalyzerError> {
    if let Some(lang) = SourceLang::from_path(path) {
        return match lang {
            SourceLang::Rust => build_rust_graph(path),
            SourceLang::TypeScript(_) => build_ts_graph(path),
            SourceLang::Go => build_go_graph(path, policy),
            SourceLang::Python if policy.python_roots => build_python_graph(path, policy),
            SourceLang::Python => Err(unsupported_root(path)),
        };
    }

    if path.is_dir() {
        if path.join("go.mod").is_file() {
            return build_go_graph(path, policy);
        }
        if policy.python_roots {
            return match resolve_crate_root(path) {
                Ok(root) => build_rust_graph(&root),
                Err(_) => build_python_graph(path, policy),
            };
        }
        return build_rust_graph(path);
    }

    if policy.python_roots {
        return Err(unsupported_root(path));
    }
    // Without a Python fallback there is still one root shape left to
    // try: a `.rs`-less path that the crate-root resolver may recognise.
    // It reports `UnsupportedRoot` itself when it cannot.
    build_rust_graph(path)
}

pub(crate) fn build_rust_graph(path: &Path) -> Result<ModuleGraph, CrateAnalyzerError> {
    let root = resolve_crate_root(path)?;
    let modules = lens_rust::build_module_tree(&root)?;
    let edges = lens_rust::extract_edges(&modules);
    Ok(ModuleGraph::new(root, modules, edges))
}

pub(crate) fn build_ts_graph(path: &Path) -> Result<ModuleGraph, CrateAnalyzerError> {
    let modules = lens_ts::build_module_tree(path)?;
    let edges = lens_ts::extract_edges(&modules);
    Ok(ModuleGraph::new(path.to_path_buf(), modules, edges))
}

fn build_python_graph(path: &Path, policy: GraphPolicy) -> Result<ModuleGraph, CrateAnalyzerError> {
    let modules = lens_py::build_module_tree(path)?;
    if policy.empty_walk_is_unsupported && modules.is_empty() {
        return Err(unsupported_root(path));
    }
    let edges = lens_py::extract_edges(&modules);
    Ok(ModuleGraph::new(path.to_path_buf(), modules, edges))
}

fn build_go_graph(path: &Path, policy: GraphPolicy) -> Result<ModuleGraph, CrateAnalyzerError> {
    let modules = lens_golang::build_module_tree(path)?;
    if policy.empty_walk_is_unsupported && modules.is_empty() {
        return Err(unsupported_root(path));
    }
    let edges = lens_golang::extract_edges(&modules);
    Ok(ModuleGraph::new(path.to_path_buf(), modules, edges))
}

fn unsupported_root(path: &Path) -> CrateAnalyzerError {
    CrateAnalyzerError::UnsupportedRoot {
        path: path.to_path_buf(),
    }
}

/// Clone out the module paths, the form [`lens_domain::compute_report`]
/// and the context-span closure walk both take.
pub(crate) fn module_paths(graph: &ModuleGraph) -> Vec<ModulePath> {
    graph.modules.iter().map(|m| m.path.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const COUPLING: GraphPolicy = GraphPolicy::COUPLING;
    const CONTEXT_SPAN: GraphPolicy = GraphPolicy::CONTEXT_SPAN;

    fn write(dir: &Path, rel: &str, body: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn python_file_root_is_unsupported_without_the_python_policy() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "m.py", "import os\n");
        let err = build_graph(&file, COUPLING).unwrap_err();
        assert!(matches!(err, CrateAnalyzerError::UnsupportedRoot { .. }));
    }

    #[test]
    fn python_file_root_builds_with_the_python_policy() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "m.py", "import os\n");
        let graph = build_graph(&file, CONTEXT_SPAN).unwrap();
        assert_eq!(graph.modules.len(), 1);
    }

    #[rstest]
    #[case(COUPLING)]
    #[case(CONTEXT_SPAN)]
    fn go_mod_marker_wins_over_the_rust_crate_resolver(#[case] policy: GraphPolicy) {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/p\n");
        write(dir.path(), "main.go", "package main\n\nfunc main() {}\n");
        // A `src/lib.rs` alongside `go.mod` would otherwise let the Rust
        // resolver claim this directory.
        write(dir.path(), "src/lib.rs", "pub fn f() {}\n");
        let graph = build_graph(dir.path(), policy).unwrap();
        assert_eq!(graph.root, dir.path());
        assert!(!graph.modules.is_empty());
    }

    #[test]
    fn empty_go_module_is_unsupported_only_under_the_strict_policy() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/p\n");
        assert!(
            build_graph(dir.path(), COUPLING)
                .unwrap()
                .modules
                .is_empty()
        );
        let err = build_graph(dir.path(), CONTEXT_SPAN).unwrap_err();
        assert!(matches!(err, CrateAnalyzerError::UnsupportedRoot { .. }));
    }

    #[test]
    fn rust_crate_directory_resolves_through_its_crate_root() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", "pub mod a;\n");
        write(dir.path(), "src/a.rs", "pub fn f() {}\n");
        for policy in [COUPLING, CONTEXT_SPAN] {
            let graph = build_graph(dir.path(), policy).unwrap();
            assert_eq!(graph.root, dir.path().join("src/lib.rs"));
            assert_eq!(graph.modules.len(), 2);
        }
    }

    #[test]
    fn python_directory_is_the_last_resort_under_the_python_policy() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pkg/m.py", "import os\n");
        let graph = build_graph(dir.path(), CONTEXT_SPAN).unwrap();
        assert!(!graph.modules.is_empty());
    }

    #[test]
    fn directory_with_no_recognisable_root_reports_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "README.md", "nothing to analyze\n");
        for policy in [COUPLING, CONTEXT_SPAN] {
            let err = build_graph(dir.path(), policy).unwrap_err();
            assert!(
                matches!(err, CrateAnalyzerError::UnsupportedRoot { .. }),
                "got {err:?}",
            );
        }
    }

    #[test]
    fn module_paths_mirrors_the_module_list() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", "pub mod a;\n");
        write(dir.path(), "src/a.rs", "pub fn f() {}\n");
        let graph = build_graph(dir.path(), COUPLING).unwrap();
        assert_eq!(module_paths(&graph).len(), graph.modules.len());
    }
}
