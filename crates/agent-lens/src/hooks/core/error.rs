//! Canonical error type for every PostToolUse hook handler.
//!
//! Each handler used to define its own `SimilarityError` / `WrapperError`
//! copy; collapsing them into one `HookError` keeps the `From` /
//! `Display` / `source()` plumbing in a single place. Splitting the
//! type out of `hooks::core` itself (where the engine-agnostic runners
//! and IO-prep types live) lets callers depend on just the error
//! surface without pulling in everything else from `core`.

use std::path::PathBuf;

/// IO failure raised while preparing an [`super::EditedSource`].
///
/// The adapter layer converts this into [`HookError::Io`] via the
/// `From` impl below so callers only ever observe a single error type.
#[derive(Debug)]
pub struct ReadEditedSourceError {
    pub path: PathBuf,
    pub source: std::io::Error,
}

/// Errors raised while running any PostToolUse hook.
///
/// Hooks used to each define their own copy of this enum
/// (`SimilarityError`, `WrapperError`, …); they are now thin aliases so
/// the `From`, `Display`, and `source()` plumbing only exists once.
#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Boxed to keep the error type language-agnostic as more parsers
    /// are added.
    #[error("failed to parse source: {0}")]
    Parse(#[source] Box<dyn std::error::Error + Send + Sync>),
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
