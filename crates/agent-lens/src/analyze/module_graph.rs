//! The module-level dependency graph shared by `coupling` and
//! `context-span`.
//!
//! Both analyzers answer different questions off the same input: a list
//! of modules plus the [`CouplingEdge`]s between them. They used to keep
//! private, byte-identical copies of the [`ModuleFile`] / [`ModuleGraph`]
//! pair and of the four per-language builders, which meant a fix to (say)
//! Go root detection had to land twice.
//!
//! Both analyzers accept the same four languages. What legitimately
//! differs is *policy*, not construction, so that part stays at the call
//! site and is spelled out by [`GraphPolicy`]: `coupling` reports an
//! empty Go module as an empty graph, `context-span` treats it as an
//! unusable root.

use std::path::{Path, PathBuf};

use lens_domain::{CouplingEdge, ModulePath};

use super::module_label::ModuleLabeler;
use super::{AnalyzePathFilter, CrateAnalyzerError, SourceLang, resolve_crate_root};

/// One module in the graph, paired with the file it was read from.
#[derive(Debug, Clone)]
pub(crate) struct ModuleFile {
    pub(crate) path: ModulePath,
    pub(crate) file: PathBuf,
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

/// The one axis on which the analyzers disagree about what counts as a
/// usable root.
///
/// `Eq`/`Hash` because the policy is half of the [`AnalysisIndex`] key
/// an assembled graph is shared under (the root path is the other).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct GraphPolicy {
    /// Whether a `go.mod` directory that walks to zero packages is an
    /// error rather than an empty graph. An explicit Go module marker is
    /// evidence enough for `coupling` that the caller meant this root; a
    /// module with no buildable packages yet is an empty report, not a
    /// misuse.
    empty_go_module_is_unsupported: bool,
}

impl GraphPolicy {
    /// `analyze coupling`: an empty Go module renders as an empty report.
    pub(crate) const COUPLING: Self = Self {
        empty_go_module_is_unsupported: false,
    };

    /// `analyze context-span`: a Go root that walks to nothing is
    /// reported rather than rendered as an empty report.
    pub(crate) const CONTEXT_SPAN: Self = Self {
        empty_go_module_is_unsupported: true,
    };
}

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
/// directory is probed in order: `go.mod` first (the unambiguous Go
/// module marker — without this check a Go repo root would fall through
/// to the Rust crate-root resolver and fail with a confusing "no usable
/// Rust crate root"), then a Rust crate root, then Python as the last
/// resort. Python goes last because it has no root marker to test: any
/// directory can hold `.py` files, so the only honest probe is to walk
/// it and see whether anything came back.
pub(crate) fn build_graph(
    path: &Path,
    policy: GraphPolicy,
    filter: &AnalyzePathFilter,
) -> Result<ModuleGraph, CrateAnalyzerError> {
    match super::index::AnalysisIndex::active() {
        Some(index) => index
            .module_graph((path.to_path_buf(), policy, filter.clone()), || {
                build_graph_uncached(path, policy, filter)
            })
            .map(|graph| graph.as_ref().clone()),
        None => build_graph_uncached(path, policy, filter),
    }
}

fn build_graph_uncached(
    path: &Path,
    policy: GraphPolicy,
    filter: &AnalyzePathFilter,
) -> Result<ModuleGraph, CrateAnalyzerError> {
    if let Some(lang) = SourceLang::from_path(path) {
        return match lang {
            SourceLang::Rust => build_rust_graph(path),
            SourceLang::TypeScript(_) => build_ts_graph(path),
            SourceLang::Go => build_go_graph(path, policy, filter),
            SourceLang::Python => build_python_graph(path, filter),
        };
    }

    if path.is_dir() {
        if path.join("go.mod").is_file() {
            return build_go_graph(path, policy, filter);
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
/// signal that the path was never a Python root, and it is unsupported
/// under either policy.
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
    policy: GraphPolicy,
    filter: &AnalyzePathFilter,
) -> Result<ModuleGraph, CrateAnalyzerError> {
    let modules = lens_golang::build_module_tree(path, &filter.compile(path)?)?;
    if policy.empty_go_module_is_unsupported && modules.is_empty() {
        return Err(unsupported_root(path));
    }
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

    #[rstest]
    #[case(COUPLING)]
    #[case(CONTEXT_SPAN)]
    fn python_file_root_builds_under_either_policy(#[case] policy: GraphPolicy) {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "m.py", "import os\n");
        let graph = build_graph(&file, policy, &AnalyzePathFilter::new()).unwrap();
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
        let graph = build_graph(dir.path(), policy, &AnalyzePathFilter::new()).unwrap();
        assert_eq!(graph.root, dir.path());
        assert!(!graph.modules.is_empty());
    }

    #[test]
    fn empty_go_module_is_unsupported_only_under_the_strict_policy() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "go.mod", "module example.com/p\n");
        assert!(
            build_graph(dir.path(), COUPLING, &AnalyzePathFilter::new())
                .unwrap()
                .modules
                .is_empty()
        );
        let err = build_graph(dir.path(), CONTEXT_SPAN, &AnalyzePathFilter::new()).unwrap_err();
        assert!(matches!(err, CrateAnalyzerError::UnsupportedRoot { .. }));
    }

    #[test]
    fn rust_crate_directory_resolves_through_its_crate_root() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", "pub mod a;\n");
        write(dir.path(), "src/a.rs", "pub fn f() {}\n");
        for policy in [COUPLING, CONTEXT_SPAN] {
            let graph = build_graph(dir.path(), policy, &AnalyzePathFilter::new()).unwrap();
            assert_eq!(graph.root, dir.path().join("src/lib.rs"));
            assert_eq!(graph.modules.len(), 2);
        }
    }

    #[rstest]
    #[case(COUPLING)]
    #[case(CONTEXT_SPAN)]
    fn python_directory_is_the_last_resort(#[case] policy: GraphPolicy) {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pkg/m.py", "import os\n");
        let graph = build_graph(dir.path(), policy, &AnalyzePathFilter::new()).unwrap();
        assert!(!graph.modules.is_empty());
    }

    #[rstest]
    #[case(COUPLING)]
    #[case(CONTEXT_SPAN)]
    fn rust_crate_root_wins_over_the_python_fallback(#[case] policy: GraphPolicy) {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", "pub fn f() {}\n");
        write(dir.path(), "setup.py", "from setuptools import setup\n");
        let graph = build_graph(dir.path(), policy, &AnalyzePathFilter::new()).unwrap();
        assert_eq!(graph.root, dir.path().join("src/lib.rs"));
    }

    #[rstest]
    #[case(COUPLING)]
    #[case(CONTEXT_SPAN)]
    fn directory_with_no_recognisable_root_reports_unsupported(#[case] policy: GraphPolicy) {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "README.md", "nothing to analyze\n");
        let err = build_graph(dir.path(), policy, &AnalyzePathFilter::new()).unwrap_err();
        assert!(
            matches!(err, CrateAnalyzerError::UnsupportedRoot { .. }),
            "got {err:?}",
        );
    }

    #[rstest]
    #[case(COUPLING)]
    #[case(CONTEXT_SPAN)]
    fn extensionless_file_is_unsupported_but_a_missing_path_is_io(#[case] policy: GraphPolicy) {
        let dir = tempfile::tempdir().unwrap();
        let notes = write(dir.path(), "NOTES", "nothing to analyze\n");
        let err = build_graph(&notes, policy, &AnalyzePathFilter::new()).unwrap_err();
        assert!(
            matches!(err, CrateAnalyzerError::UnsupportedRoot { .. }),
            "got {err:?}",
        );

        let err =
            build_graph(&dir.path().join("ghost"), policy, &AnalyzePathFilter::new()).unwrap_err();
        assert!(matches!(err, CrateAnalyzerError::Io { .. }), "got {err:?}");
    }

    #[test]
    fn module_paths_mirrors_the_module_list() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/lib.rs", "pub mod a;\n");
        write(dir.path(), "src/a.rs", "pub fn f() {}\n");
        let graph = build_graph(dir.path(), COUPLING, &AnalyzePathFilter::new()).unwrap();
        assert_eq!(module_paths(&graph).len(), graph.modules.len());
    }
}
