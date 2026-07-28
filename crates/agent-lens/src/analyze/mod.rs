//! On-demand analyzers that emit LLM-friendly context.
//!
//! Each submodule is one analyzer (cohesion, complexity, coupling,
//! similarity, wrapper, hotspot, …) and is wired to a clap subcommand so
//! typos surface at parse time. Output is always written to stdout as JSON
//! by default; analyzers can opt in to a `--format md` mode for a more
//! compact human-readable summary.

pub(crate) mod call_graph;
mod cargo_meta;
pub mod cohesion;
pub mod complexity;
pub mod context_span;
pub mod coupling;
mod crate_root;
pub mod cycles;
mod diff;
mod error_from;
mod format;
pub mod function_graph;
pub mod graph_query;
pub mod hotspot;
pub mod hubs;
pub mod impact;
pub mod layers;
mod module_graph;
mod path_filter;
mod runner;
pub mod similarity;
mod source_files;
pub mod untested;
pub mod wrapper;

use std::path::{Path, PathBuf};

use lens_domain::LanguageParser;

pub use cohesion::CohesionAnalyzer;
pub use complexity::ComplexityAnalyzer;
pub use context_span::{ContextSpanAnalyzer, ContextSpanAnalyzerError};
pub use coupling::CouplingAnalyzer;
pub use cycles::CyclesAnalyzer;
pub use function_graph::FunctionGraphAnalyzer;
pub use graph_query::{
    DEFAULT_GRAPH_QUERY_DEPTH, DEFAULT_GRAPH_QUERY_LIMIT, GraphDirection, GraphQueryAnalyzer,
    GraphQueryKind,
};
pub use hotspot::{HotspotAnalyzer, HotspotError};
pub use hubs::HubsAnalyzer;
pub use impact::{DEFAULT_IMPACT_DEPTH, ImpactAnalyzer};
pub use layers::LayersAnalyzer;

/// Backward-compatible alias for the unified [`CrateAnalyzerError`].
pub type CouplingAnalyzerError = CrateAnalyzerError;
pub use similarity::{
    DEFAULT_MIN_LINES as DEFAULT_SIMILARITY_MIN_LINES,
    DEFAULT_THRESHOLD as DEFAULT_SIMILARITY_THRESHOLD, FunctionSelection, SimilarityAnalyzer,
    SimilarityMethod,
};
pub use untested::UntestedAnalyzer;
pub use wrapper::WrapperAnalyzer;

pub use crate_root::resolve_crate_root;
pub(crate) use diff::overlaps_any;
pub use diff::{LineRange, changed_line_ranges};
pub(crate) use format::format_optional_f64;
pub use path_filter::{AnalyzePathFilter, CompiledPathFilter, PathFilterError};
pub use source_files::read_source;
pub(crate) use source_files::{SourceFile, collect_source_files};

/// Output format shared across analyzers.
///
/// Lives at the module root so every analyzer's `--format` flag
/// resolves to the same enum, both in clap and in the public API. The
/// `Deserialize` impl lets `agent-lens.toml` profiles name a format with
/// the same `json` / `md` spelling clap accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Json,
    Md,
}

/// Source languages the analyzers know how to handle.
///
/// Centralising the extension → language mapping keeps every analyzer in
/// sync when a new language gets wired up; analyzers only need to add a
/// `match` arm for the new variant. The TypeScript variant carries a
/// [`lens_ts::Dialect`] so the same dispatch covers `.ts` / `.tsx` /
/// `.jsx` / `.js` / `.mjs` / `.cjs` without re-deriving it at every call
/// site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLang {
    Rust,
    TypeScript(lens_ts::Dialect),
    Python,
    Go,
}

impl SourceLang {
    pub fn from_extension(ext: &str) -> Option<Self> {
        if ext == "rs" {
            return Some(Self::Rust);
        }
        if ext == "py" {
            return Some(Self::Python);
        }
        if ext == "go" {
            return Some(Self::Go);
        }
        lens_ts::Dialect::from_extension(ext).map(Self::TypeScript)
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }

    pub(crate) fn create_language_parser(&self) -> Box<dyn LanguageParser> {
        match self {
            Self::Rust => Box::new(lens_rust::RustParser::new()),
            Self::TypeScript(dialect) => {
                Box::new(lens_ts::TypeScriptParser::with_dialect(*dialect))
            }
            Self::Python => Box::new(lens_py::PythonParser::new()),
            Self::Go => Box::new(lens_golang::GoParser::new()),
        }
    }
}

/// Dispatch the per-language lens extractor `$extract` for `$lang` over
/// `$source`, boxing each adapter's error type into the shared
/// `Box<dyn Error + Send + Sync>` currency. Every extractor family
/// (cohesion units, complexity units, wrapper findings, ...) exposes the
/// same `lens_<lang>::$extract` shape — TypeScript additionally takes its
/// dialect — so the analyzers and hook cores share this one dispatch
/// instead of each repeating the four-arm match.
macro_rules! dispatch_lens {
    ($lang:expr, $source:expr, $extract:ident) => {{
        type BoxedError = ::std::boxed::Box<dyn ::std::error::Error + Send + Sync>;
        match $lang {
            $crate::analyze::SourceLang::Rust => {
                ::lens_rust::$extract($source).map_err(|e| ::std::boxed::Box::new(e) as BoxedError)
            }
            $crate::analyze::SourceLang::TypeScript(dialect) => {
                ::lens_ts::$extract($source, dialect)
                    .map_err(|e| ::std::boxed::Box::new(e) as BoxedError)
            }
            $crate::analyze::SourceLang::Python => {
                ::lens_py::$extract($source).map_err(|e| ::std::boxed::Box::new(e) as BoxedError)
            }
            $crate::analyze::SourceLang::Go => ::lens_golang::$extract($source)
                .map_err(|e| ::std::boxed::Box::new(e) as BoxedError),
        }
    }};
}
pub(crate) use dispatch_lens;

pub(crate) fn relative_display_path(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// Errors common to single-file analyzers (cohesion, complexity).
///
/// Coupling and context-span carry extra variants (`UnsupportedRoot`,
/// `MissingMod`) and use the dedicated [`CrateAnalyzerError`] below
/// instead.
#[derive(Debug, thiserror::Error)]
pub enum AnalyzerError {
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsupported file extension: {path:?}")]
    UnsupportedExtension { path: PathBuf },
    #[error("failed to parse source: {0}")]
    Parse(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("failed to serialize report: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error(
        "similarity scope is too broad: {candidate_pair_count} candidate pairs (eligible functions: {eligible_function_count}, theoretical max pairs: {theoretical_pair_count}, strategy: {strategy}, min_lines: {min_lines}) exceeds limit {max_candidate_pairs}; narrow the scope (path, --exclude, --diff-only, or raise --min-lines)"
    )]
    SimilarityScopeTooBroad {
        eligible_function_count: usize,
        theoretical_pair_count: u128,
        candidate_pair_count: usize,
        max_candidate_pairs: usize,
        min_lines: usize,
        strategy: &'static str,
    },
    #[error(transparent)]
    PathFilter(#[from] PathFilterError),
    #[error("invalid graph query: {message}")]
    InvalidQuery { message: String },
}

/// Errors raised by analyzers that walk a Rust crate from a `.rs` root
/// (coupling, context-span, …).
///
/// Both analyzers reach into the same `lens_rust` extractor and surface
/// the same handful of failure modes; they used to duplicate this enum.
#[derive(Debug, thiserror::Error)]
pub enum CrateAnalyzerError {
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
    /// The provided path exists but no language backend claims it: not a
    /// crate root, not an entry file, and a directory walk found nothing.
    #[error(
        "unsupported analysis root {path:?}; pass a Rust crate root (.rs file or a directory containing src/lib.rs or src/main.rs), a TS/JS entry file, a Python file/directory, or a Go file/module directory"
    )]
    UnsupportedRoot { path: PathBuf },
    /// `mod foo;` was declared in a parent file but neither `foo.rs` nor
    /// `foo/mod.rs` could be found.
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
    PathFilter(#[from] PathFilterError),
}

use error_from::impl_from_coupling_error;

impl_from_coupling_error!(lens_rust::CouplingError => CrateAnalyzerError, MissingMod);
impl_from_coupling_error!(lens_ts::CouplingError => CrateAnalyzerError);
impl_from_coupling_error!(lens_py::CouplingError => CrateAnalyzerError, UnsupportedRoot);
impl_from_coupling_error!(lens_golang::CouplingError => CrateAnalyzerError, UnsupportedRoot);

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;
    use std::io;

    #[test]
    fn source_lang_from_extension_covers_ts_family() {
        for (ext, expected) in [
            ("ts", lens_ts::Dialect::Ts),
            ("tsx", lens_ts::Dialect::Tsx),
            ("mts", lens_ts::Dialect::Mts),
            ("cts", lens_ts::Dialect::Cts),
            ("js", lens_ts::Dialect::Js),
            ("jsx", lens_ts::Dialect::Jsx),
            ("mjs", lens_ts::Dialect::Mjs),
            ("cjs", lens_ts::Dialect::Cjs),
        ] {
            assert_eq!(
                SourceLang::from_extension(ext),
                Some(SourceLang::TypeScript(expected)),
                "extension {ext} should map to dialect {expected:?}",
            );
        }
    }

    #[test]
    fn source_lang_from_extension_keeps_other_languages() {
        assert_eq!(SourceLang::from_extension("rs"), Some(SourceLang::Rust));
        assert_eq!(SourceLang::from_extension("py"), Some(SourceLang::Python));
        assert_eq!(SourceLang::from_extension("go"), Some(SourceLang::Go));
        assert_eq!(SourceLang::from_extension("md"), None);
    }

    #[test]
    fn analyzer_error_io_display_includes_path_and_source() {
        let err = AnalyzerError::Io {
            path: PathBuf::from("/tmp/nope.rs"),
            source: io::Error::new(io::ErrorKind::NotFound, "missing"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/nope.rs"), "got {msg}");
        assert!(msg.contains("missing"), "got {msg}");
        assert!(msg.starts_with("failed to read"), "got {msg}");
    }

    #[test]
    fn analyzer_error_unsupported_extension_display_includes_path() {
        let err = AnalyzerError::UnsupportedExtension {
            path: PathBuf::from("/tmp/file.txt"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/file.txt"), "got {msg}");
        assert!(msg.contains("unsupported file extension"), "got {msg}");
    }

    #[test]
    fn analyzer_error_parse_display_includes_inner() {
        let err = AnalyzerError::Parse(Box::<dyn std::error::Error + Send + Sync>::from(
            "boom".to_owned(),
        ));
        let msg = err.to_string();
        assert!(msg.contains("boom"), "got {msg}");
        assert!(msg.contains("parse"), "got {msg}");
    }

    #[test]
    fn analyzer_error_serialize_display_includes_inner() {
        let serde_err = serde_json::from_str::<serde_json::Value>("{invalid").unwrap_err();
        let err = AnalyzerError::Serialize(serde_err);
        let msg = err.to_string();
        assert!(msg.contains("serialize"), "got {msg}");
    }

    #[test]
    fn analyzer_error_io_source_is_present() {
        let err = AnalyzerError::Io {
            path: PathBuf::from("/tmp/x"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        };
        assert!(err.source().is_some());
    }

    #[test]
    fn analyzer_error_parse_source_is_present() {
        let err = AnalyzerError::Parse(Box::<dyn std::error::Error + Send + Sync>::from(
            "boom".to_owned(),
        ));
        assert!(err.source().is_some());
    }

    #[test]
    fn analyzer_error_serialize_source_is_present() {
        let serde_err = serde_json::from_str::<serde_json::Value>("{invalid").unwrap_err();
        let err = AnalyzerError::Serialize(serde_err);
        assert!(err.source().is_some());
    }

    #[test]
    fn analyzer_error_unsupported_extension_has_no_source() {
        let err = AnalyzerError::UnsupportedExtension {
            path: PathBuf::from("/tmp/x.txt"),
        };
        assert!(err.source().is_none());
    }
}
