//! Engine-agnostic primitives shared by every PostToolUse hook.
//!
//! The Claude Code and Codex adapters each prepare their own
//! [`EditedSource`] list and then hand it off to a `…Core` here. The cores
//! own the actual analysis (parser dispatch, threshold handling, report
//! formatting) so each agent's hook module is just a thin trait
//! implementation that wires up the engine-specific input/output shapes.

pub mod similarity;
pub mod wrapper;

use std::path::PathBuf;

use crate::analyze::SourceLang;

/// One file the agent just edited, prepared for a hook to analyse.
///
/// The same struct is produced by both the Claude Code single-file path
/// (which yields zero or one entry) and the Codex `apply_patch` path
/// (which yields one entry per touched file).
#[derive(Debug)]
pub(crate) struct EditedSource {
    /// Path verbatim as it appeared in the agent's input — kept so reports
    /// quote it back without resolving it to an absolute path.
    pub rel_path: String,
    pub lang: SourceLang,
    pub source: String,
}

/// IO failure raised while preparing an [`EditedSource`].
///
/// Adapters convert this into [`HookError::Io`] via the `From` impl below
/// so callers only ever see one canonical error type.
#[derive(Debug)]
pub(crate) struct ReadEditedSourceError {
    pub path: PathBuf,
    pub source: std::io::Error,
}

/// Errors raised while running any PostToolUse hook.
///
/// Hooks used to each define their own copy of this enum (`SimilarityError`,
/// `WrapperError`, …); they are now thin aliases so the `From`, `Display`,
/// `source` plumbing only exists once.
#[derive(Debug)]
pub enum HookError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Boxed to keep the error type language-agnostic as more parsers
    /// are added.
    Parse(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for HookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::Parse(e) => write!(f, "failed to parse source: {e}"),
        }
    }
}

impl std::error::Error for HookError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse(e) => Some(e.as_ref()),
        }
    }
}

impl From<ReadEditedSourceError> for HookError {
    fn from(e: ReadEditedSourceError) -> Self {
        Self::Io {
            path: e.path,
            source: e.source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;

    #[test]
    fn hook_error_io_display_includes_path_and_source() {
        let err = HookError::Io {
            path: PathBuf::from("/tmp/missing.rs"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/missing.rs"), "got {msg}");
        assert!(msg.contains("missing"), "got {msg}");
        assert!(msg.starts_with("failed to read"), "got {msg}");
    }

    #[test]
    fn hook_error_parse_display_includes_inner() {
        let err = HookError::Parse(Box::<dyn std::error::Error + Send + Sync>::from(
            "syntax".to_owned(),
        ));
        let msg = err.to_string();
        assert!(msg.contains("syntax"), "got {msg}");
        assert!(msg.starts_with("failed to parse"), "got {msg}");
    }

    #[test]
    fn hook_error_io_source_is_present() {
        let err = HookError::Io {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::other("boom"),
        };
        assert!(err.source().is_some());
    }

    #[test]
    fn hook_error_parse_source_is_present() {
        let err = HookError::Parse(Box::<dyn std::error::Error + Send + Sync>::from(
            "boom".to_owned(),
        ));
        assert!(err.source().is_some());
    }

    #[test]
    fn read_edited_source_error_converts_to_hook_io_error() {
        let read_err = ReadEditedSourceError {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        let hook_err: HookError = read_err.into();
        assert!(matches!(hook_err, HookError::Io { .. }));
        let msg = hook_err.to_string();
        assert!(msg.contains("/tmp/x"), "got {msg}");
        assert!(msg.contains("denied"), "got {msg}");
    }
}
