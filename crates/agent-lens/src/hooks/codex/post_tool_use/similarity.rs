//! `similarity` PostToolUse handler for Codex.
//!
//! After `apply_patch` runs, parse every updated/added source file and
//! report any pairs of functions whose TSED score is at or above
//! [`DEFAULT_THRESHOLD`]. Findings are returned as
//! `hookSpecificOutput.additionalContext` so they land in Codex's
//! developer context without blocking the turn.
//!
//! Language is picked from each file's extension. Today only `.rs` is
//! supported; other extensions are treated as "no opinion" so the hook
//! stays silent instead of erroring.

use std::fmt::Write as _;
use std::path::PathBuf;

use agent_hooks::Hook;
use agent_hooks::codex::{PostToolUseHookSpecificOutput, PostToolUseInput, PostToolUseOutput};
use lens_domain::{FunctionDef, LanguageParser, SimilarPair, TSEDOptions, find_similar_functions};
use lens_rust::RustParser;

use crate::analyze::SourceLang;
use crate::hooks::codex::post_tool_use::{ReadEditedSourceError, prepare_edited_sources};

/// Default similarity threshold. Matches the Claude Code variant so the
/// two hooks behave the same on the same input.
pub const DEFAULT_THRESHOLD: f64 = 0.85;

const HOOK_EVENT_NAME: &str = "PostToolUse";

/// Handler implementation for the `similarity` Codex `PostToolUse` hook.
#[derive(Debug, Clone)]
pub struct SimilarityHook {
    threshold: f64,
    opts: TSEDOptions,
}

impl SimilarityHook {
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
        let sources = prepare_edited_sources(&input)?;
        if sources.is_empty() {
            return Ok(PostToolUseOutput::default());
        }

        let mut report = String::new();
        let mut total_pairs = 0usize;
        for src in &sources {
            let funcs = extract_functions(src.lang, &src.source)?;
            let pairs = find_similar_functions(&funcs, self.threshold, &self.opts);
            if pairs.is_empty() {
                continue;
            }
            total_pairs += pairs.len();
            append_file_report(&mut report, &src.rel_path, &pairs);
        }
        if total_pairs == 0 {
            return Ok(PostToolUseOutput::default());
        }

        let header =
            format!("agent-lens similarity: {total_pairs} similar function pair(s) detected\n");
        Ok(PostToolUseOutput {
            hook_specific_output: Some(PostToolUseHookSpecificOutput {
                hook_event_name: HOOK_EVENT_NAME.to_owned(),
                additional_context: Some(format!("{header}{report}")),
            }),
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

fn append_file_report(out: &mut String, file_path: &str, pairs: &[SimilarPair<'_>]) {
    // writeln! into a String cannot fail; the result is swallowed
    // deliberately rather than unwrapped to satisfy the workspace's
    // `unwrap_used` lint.
    let _ = writeln!(out, "{file_path}:");
    for pair in pairs {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks::codex::HookContext;
    use serde_json::json;
    use std::io::Write;
    use std::path::Path;

    fn ctx(cwd: PathBuf) -> HookContext {
        HookContext {
            session_id: "sess".into(),
            transcript_path: Some(PathBuf::from("/tmp/t.jsonl")),
            cwd,
            model: "gpt-5".into(),
        }
    }

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn input(cwd: PathBuf, tool_name: &str, command: &str) -> PostToolUseInput {
        PostToolUseInput {
            context: ctx(cwd),
            turn_id: "turn-1".into(),
            tool_name: tool_name.into(),
            tool_use_id: "call-1".into(),
            tool_input: json!({"command": command}),
            tool_response: json!({}),
        }
    }

    #[test]
    fn ignores_non_apply_patch_tools() {
        let hook = SimilarityHook::new();
        let mut payload = input(PathBuf::from("/tmp"), "Bash", "ls");
        payload.tool_input = json!({"command": "ls"});
        let out = hook.handle(payload).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn ignores_apply_patch_without_command() {
        let hook = SimilarityHook::new();
        let payload = PostToolUseInput {
            tool_input: json!({}),
            ..input(PathBuf::from("/tmp"), "apply_patch", "")
        };
        let out = hook.handle(payload).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn ignores_unsupported_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "*** Begin Patch\n*** Update File: notes.md\n*** End Patch\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = SimilarityHook::new().handle(payload).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn reports_pairs_via_additional_context() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
            fn alpha(x: i32) -> i32 { let y = x + 1; let z = y * 2; z }
            fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
        "#;
        write_file(dir.path(), "lib.rs", source);

        let patch = "*** Begin Patch\n*** Update File: lib.rs\n@@\n*** End Patch\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);

        let out = SimilarityHook::new()
            .with_threshold(0.5)
            .handle(payload)
            .unwrap();
        let extra = out
            .hook_specific_output
            .expect("expected hook_specific_output");
        assert_eq!(extra.hook_event_name, "PostToolUse");
        let msg = extra
            .additional_context
            .expect("expected additionalContext");
        assert!(msg.contains("lib.rs"), "should mention file: {msg}");
        assert!(msg.contains("alpha"), "should mention alpha: {msg}");
        assert!(msg.contains("beta"), "should mention beta: {msg}");
        assert!(out.decision.is_none());
    }

    #[test]
    fn aggregates_across_multiple_files() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
            fn alpha(x: i32) -> i32 { let y = x + 1; let z = y * 2; z }
            fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
        "#;
        write_file(dir.path(), "a.rs", source);
        write_file(dir.path(), "b.rs", source);

        let patch = "\
*** Begin Patch
*** Update File: a.rs
*** Add File: b.rs
*** End Patch
";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);

        let out = SimilarityHook::new()
            .with_threshold(0.5)
            .handle(payload)
            .unwrap();
        let msg = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("expected additionalContext");
        assert!(msg.contains("a.rs"));
        assert!(msg.contains("b.rs"));
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
        write_file(dir.path(), "lib.rs", source);

        let patch = "*** Update File: lib.rs\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = SimilarityHook::new().handle(payload).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn missing_file_surfaces_io_error() {
        let patch = "*** Update File: missing.rs\n";
        let payload = input(
            PathBuf::from("/definitely/does/not/exist"),
            "apply_patch",
            patch,
        );
        let err = SimilarityHook::new().handle(payload).unwrap_err();
        assert!(matches!(err, SimilarityError::Io { .. }));
    }

    #[test]
    fn with_threshold_actually_overrides_default() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
            fn alpha(x: i32) -> i32 { let y = x + 1; let z = y * 2; z }
            fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
        "#;
        write_file(dir.path(), "lib.rs", source);
        let patch = "*** Update File: lib.rs\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);

        let out = SimilarityHook::new()
            .with_threshold(1.5)
            .handle(payload)
            .unwrap();
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
}
