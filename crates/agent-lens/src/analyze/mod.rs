//! On-demand analyzers that emit LLM-friendly context.
//!
//! Each submodule is one analyzer (cohesion, complexity, coupling,
//! similarity, wrapper, hotspot, …) and is wired to a clap subcommand so
//! typos surface at parse time. Output is always written to stdout as JSON
//! by default; analyzers can opt in to a `--format md` mode for a more
//! compact human-readable summary.

pub mod cohesion;
pub mod complexity;
pub mod coupling;
pub mod hotspot;
pub mod similarity;
pub mod wrapper;

use std::path::{Path, PathBuf};

pub use cohesion::CohesionAnalyzer;
pub use complexity::ComplexityAnalyzer;
pub use coupling::{CouplingAnalyzer, CouplingAnalyzerError};
pub use hotspot::{HotspotAnalyzer, HotspotError};
pub use similarity::{DEFAULT_THRESHOLD as DEFAULT_SIMILARITY_THRESHOLD, SimilarityAnalyzer};
pub use wrapper::WrapperAnalyzer;

/// Output format shared across analyzers.
///
/// Lives at the module root so every analyzer's `--format` flag
/// resolves to the same enum, both in clap and in the public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Json,
    Md,
}

/// Source languages the analyzers know how to handle.
///
/// Centralising the extension → language mapping keeps every analyzer in
/// sync when a new language gets wired up; analyzers only need to add a
/// `match` arm for the new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLang {
    Rust,
}

impl SourceLang {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            _ => None,
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }
}

/// Errors common to single-file analyzers (cohesion, complexity).
///
/// Coupling carries extra variants (`UnsupportedRoot`, `MissingMod`) so it
/// keeps its own error type. The shared variants here keep the simple
/// analyzers from each repeating the same Io / extension / parse /
/// serialize boilerplate.
#[derive(Debug)]
pub enum AnalyzerError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    UnsupportedExtension {
        path: PathBuf,
    },
    Parse(Box<dyn std::error::Error + Send + Sync>),
    Serialize(serde_json::Error),
}

impl std::fmt::Display for AnalyzerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "failed to read {}: {source}", path.display()),
            Self::UnsupportedExtension { path } => {
                write!(f, "unsupported file extension: {}", path.display())
            }
            Self::Parse(e) => write!(f, "failed to parse source: {e}"),
            Self::Serialize(e) => write!(f, "failed to serialize report: {e}"),
        }
    }
}

impl std::error::Error for AnalyzerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse(e) => Some(e.as_ref()),
            Self::Serialize(e) => Some(e),
            Self::UnsupportedExtension { .. } => None,
        }
    }
}

/// Detect the source language from `path` and read it into memory.
///
/// Returns the matched [`SourceLang`] alongside the file contents so
/// callers can dispatch on the language without re-parsing the path.
pub fn read_source(path: &Path) -> Result<(SourceLang, String), AnalyzerError> {
    let lang = SourceLang::from_path(path).ok_or_else(|| AnalyzerError::UnsupportedExtension {
        path: path.to_path_buf(),
    })?;
    let source = std::fs::read_to_string(path).map_err(|source| AnalyzerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok((lang, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;
    use std::io;

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
        // Force a serde_json error by parsing invalid JSON.
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
