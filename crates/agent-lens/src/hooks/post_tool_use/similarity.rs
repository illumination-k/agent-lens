//! `similarity` PostToolUse handler.
//!
//! After an agent edits a source file, parse it and report any pairs of
//! functions whose TSED score is at or above [`DEFAULT_THRESHOLD`]. The
//! findings come back as a `systemMessage` so they land in the agent's
//! context without blocking the tool call.
//!
//! Language is picked from the file extension. Today only `.rs` is
//! supported; other extensions are treated as "no opinion" so the hook
//! stays silent instead of erroring.

use std::fmt::Write as _;
use std::path::PathBuf;

use agent_hooks::Hook;
use agent_hooks::claude_code::{CommonHookOutput, PostToolUseInput, PostToolUseOutput};
use lens_domain::{FunctionDef, LanguageParser, SimilarPair, TSEDOptions, find_similar_functions};
use lens_rust::RustParser;

use crate::analyze::SourceLang;
use crate::hooks::post_tool_use::{EditedSource, ReadEditedSourceError, prepare_edited_source};

/// Default similarity threshold. Picked to match the cutoff used in the
/// existing similarity tests and to avoid flooding the transcript with
/// near-misses.
pub const DEFAULT_THRESHOLD: f64 = 0.85;

/// Handler implementation for the `similarity` PostToolUse hook.
#[derive(Debug, Clone)]
pub struct SimilarityHook {
    threshold: f64,
    opts: TSEDOptions,
}

impl SimilarityHook {
    /// Construct a handler with [`DEFAULT_THRESHOLD`] and the default
    /// TSED options.
    pub fn new() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            opts: TSEDOptions::default(),
        }
    }

    /// Override the similarity threshold. Useful for tests; the binary
    /// currently always uses the default.
    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.threshold = threshold;
        self
    }
}

impl Default for SimilarityHook {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors raised while running [`SimilarityHook`].
#[derive(Debug)]
pub enum SimilarityError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Boxed to keep the error type language-agnostic as more parsers
    /// are added.
    Parse(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for SimilarityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::Parse(e) => write!(f, "failed to parse source: {e}"),
        }
    }
}

impl std::error::Error for SimilarityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse(e) => Some(e.as_ref()),
        }
    }
}

impl From<ReadEditedSourceError> for SimilarityError {
    fn from(e: ReadEditedSourceError) -> Self {
        Self::Io {
            path: e.path,
            source: e.source,
        }
    }
}

impl Hook for SimilarityHook {
    type Input = PostToolUseInput;
    type Output = PostToolUseOutput;
    type Error = SimilarityError;

    fn handle(&self, input: PostToolUseInput) -> Result<PostToolUseOutput, Self::Error> {
        let Some(EditedSource {
            rel_path,
            lang,
            source,
            ..
        }) = prepare_edited_source(&input)?
        else {
            return Ok(PostToolUseOutput::default());
        };

        let funcs = extract_functions(lang, &source)?;
        let pairs = find_similar_functions(&funcs, self.threshold, &self.opts);
        if pairs.is_empty() {
            return Ok(PostToolUseOutput::default());
        }

        Ok(PostToolUseOutput {
            common: CommonHookOutput {
                system_message: Some(format_report(&rel_path, &pairs)),
                ..CommonHookOutput::default()
            },
            ..PostToolUseOutput::default()
        })
    }
}

fn extract_functions(lang: SourceLang, source: &str) -> Result<Vec<FunctionDef>, SimilarityError> {
    match lang {
        SourceLang::Rust => {
            let mut parser = RustParser::new();
            parser
                .extract_functions(source)
                .map_err(|e| SimilarityError::Parse(Box::new(e)))
        }
    }
}

fn format_report(file_path: &str, pairs: &[SimilarPair<'_>]) -> String {
    let mut out = format!(
        "agent-lens similarity: {} similar function pair(s) in {}\n",
        pairs.len(),
        file_path,
    );
    for pair in pairs {
        // writeln! into a String cannot fail; the result is swallowed
        // deliberately rather than unwrapped to satisfy the workspace's
        // `unwrap_used` lint.
        let _ = writeln!(
            out,
            "- {} (L{}-{}) <-> {} (L{}-{}): {:.0}% similar",
            pair.a.name,
            pair.a.start_line,
            pair.a.end_line,
            pair.b.name,
            pair.b.start_line,
            pair.b.end_line,
            pair.similarity * 100.0,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks::claude_code::HookContext;
    use serde_json::json;
    use std::io::Write;
    use std::path::Path;

    fn ctx(cwd: PathBuf) -> HookContext {
        HookContext {
            session_id: "sess".into(),
            transcript_path: PathBuf::from("/tmp/t.jsonl"),
            cwd,
            permission_mode: None,
        }
    }

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    /// Assert that the default-configured hook treats `(tool_name,
    /// tool_input)` as out of scope and returns the empty default
    /// output. Folds together every "should ignore this" path so each
    /// case is a single line at the call site.
    fn assert_no_op(tool_name: &str, tool_input: serde_json::Value) {
        let hook = SimilarityHook::new();
        let input = PostToolUseInput {
            context: ctx(PathBuf::from("/tmp")),
            tool_name: tool_name.into(),
            tool_input: tool_input.clone(),
            tool_response: json!({}),
        };
        let out = hook.handle(input).unwrap();
        assert_eq!(
            out,
            PostToolUseOutput::default(),
            "expected no-op for tool={tool_name} input={tool_input}",
        );
    }

    #[test]
    fn ignores_non_editing_tools() {
        assert_no_op("Bash", json!({"command": "ls"}));
    }

    #[test]
    fn ignores_unknown_extensions() {
        for ext in ["README.md", "notes.txt", "script.py", "app.ts"] {
            assert_no_op("Write", json!({ "file_path": ext }));
        }
    }

    #[test]
    fn ignores_extensionless_paths() {
        assert_no_op("Write", json!({"file_path": "Makefile"}));
    }

    #[test]
    fn ignores_missing_file_path() {
        assert_no_op("Edit", json!({}));
    }

    #[test]
    fn rust_extension_triggers_rust_parser() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
            fn alpha(x: i32) -> i32 { let y = x + 1; let z = y * 2; z }
            fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
        "#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = SimilarityHook::new().with_threshold(0.5);
        let input = PostToolUseInput {
            context: ctx(dir.path().to_path_buf()),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
            tool_response: json!({"success": true}),
        };
        let out = hook.handle(input).unwrap();
        let msg = out.common.system_message.expect("expected a report");
        assert!(msg.contains("alpha"), "message should mention alpha: {msg}");
        assert!(msg.contains("beta"), "message should mention beta: {msg}");
        assert!(
            msg.contains("similar"),
            "message should describe similarity: {msg}"
        );
        assert!(out.decision.is_none());
    }

    #[test]
    fn no_report_when_below_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
            fn alpha() -> i32 { 42 }
            fn beta(xs: &[i32]) -> i32 {
                let mut total = 0;
                for x in xs {
                    if *x > 0 {
                        total += x;
                    }
                }
                total
            }
        "#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = SimilarityHook::new();
        let input = PostToolUseInput {
            context: ctx(dir.path().to_path_buf()),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
            tool_response: json!({}),
        };
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn resolves_relative_path_against_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src");
        std::fs::create_dir_all(&nested).unwrap();
        let source = r#"
            fn alpha(x: i32) -> i32 { let y = x + 1; let z = y * 2; z }
            fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
        "#;
        write_file(&nested, "lib.rs", source);

        let hook = SimilarityHook::new().with_threshold(0.5);
        let input = PostToolUseInput {
            context: ctx(dir.path().to_path_buf()),
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": "src/lib.rs"}),
            tool_response: json!({}),
        };
        let out = hook.handle(input).unwrap();
        assert!(out.common.system_message.is_some());
    }

    #[test]
    fn missing_file_surfaces_io_error() {
        let hook = SimilarityHook::new();
        let input = PostToolUseInput {
            context: ctx(PathBuf::from("/definitely/does/not/exist")),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": "missing.rs"}),
            tool_response: json!({}),
        };
        let err = hook.handle(input).unwrap_err();
        assert!(matches!(err, SimilarityError::Io { .. }));
    }

    #[test]
    fn with_threshold_actually_overrides_default() {
        // Two near-identical bodies — at the default threshold they would be
        // reported. Setting an unreachable threshold has to suppress them, or
        // `with_threshold` is silently dropping the override.
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
            fn alpha(x: i32) -> i32 { let y = x + 1; let z = y * 2; z }
            fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
        "#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = SimilarityHook::new().with_threshold(1.5);
        let input = PostToolUseInput {
            context: ctx(dir.path().to_path_buf()),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
            tool_response: json!({}),
        };
        let out = hook.handle(input).unwrap();
        assert_eq!(
            out,
            PostToolUseOutput::default(),
            "threshold=1.5 must suppress all pairs",
        );
    }

    #[test]
    fn similarity_error_io_display_includes_path_and_inner_error() {
        let err = SimilarityError::Io {
            path: PathBuf::from("/some/missing.rs"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "boom"),
        };
        let msg = format!("{err}");
        assert!(msg.contains("/some/missing.rs"), "got {msg}");
        assert!(msg.contains("boom"), "got {msg}");
    }

    #[test]
    fn similarity_error_parse_display_includes_inner_error() {
        let inner: Box<dyn std::error::Error + Send + Sync> = "broken".into();
        let err = SimilarityError::Parse(inner);
        let msg = format!("{err}");
        assert!(msg.contains("parse"), "got {msg}");
        assert!(msg.contains("broken"), "got {msg}");
    }

    #[test]
    fn similarity_error_io_exposes_underlying_io_error_via_source() {
        let err = SimilarityError::Io {
            path: PathBuf::from("/x"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "boom"),
        };
        let source = std::error::Error::source(&err).expect("source should be Some");
        assert!(format!("{source}").contains("boom"));
    }

    #[test]
    fn similarity_error_parse_exposes_inner_error_via_source() {
        let inner: Box<dyn std::error::Error + Send + Sync> = "broken".into();
        let err = SimilarityError::Parse(inner);
        let source = std::error::Error::source(&err).expect("source should be Some");
        assert_eq!(format!("{source}"), "broken");
    }
}
