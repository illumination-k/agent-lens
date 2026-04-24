//! `rust-similarity` PostToolUse handler.
//!
//! After an agent edits a Rust file, parse it and report any pairs of
//! functions whose TSED score is at or above [`DEFAULT_THRESHOLD`]. The
//! findings come back as a `systemMessage` so they land in the agent's
//! context without blocking the tool call.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent_hooks::Hook;
use agent_hooks::claude_code::{CommonHookOutput, PostToolUseInput, PostToolUseOutput};
use lens_domain::{LanguageParser, SimilarPair, TSEDOptions, find_similar_functions};
use lens_rust::{RustParseError, RustParser};

/// Tool names whose `tool_input.file_path` points at the file that was
/// just modified. Anything outside this set is ignored.
const EDITING_TOOL_NAMES: &[&str] = &["Write", "Edit", "MultiEdit"];

/// Default similarity threshold. Picked to match the cutoff used in the
/// existing similarity tests and to avoid flooding the transcript with
/// near-misses.
pub const DEFAULT_THRESHOLD: f64 = 0.85;

/// Handler implementation for the `rust-similarity` PostToolUse hook.
#[derive(Debug, Clone)]
pub struct RustSimilarityHook {
    threshold: f64,
    opts: TSEDOptions,
}

impl RustSimilarityHook {
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

impl Default for RustSimilarityHook {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors raised while running [`RustSimilarityHook`].
#[derive(Debug)]
pub enum RustSimilarityError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse(RustParseError),
}

impl std::fmt::Display for RustSimilarityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::Parse(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RustSimilarityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse(e) => Some(e),
        }
    }
}

impl Hook for RustSimilarityHook {
    type Input = PostToolUseInput;
    type Output = PostToolUseOutput;
    type Error = RustSimilarityError;

    fn handle(&self, input: PostToolUseInput) -> Result<PostToolUseOutput, Self::Error> {
        // Ignore non-edit tool calls and anything that isn't a Rust file.
        // Returning an empty output is the "no opinion" signal — Claude
        // Code keeps going without injecting anything into the transcript.
        if !EDITING_TOOL_NAMES.contains(&input.tool_name.as_str()) {
            return Ok(PostToolUseOutput::default());
        }
        let Some(file_path) = extract_file_path(&input.tool_input) else {
            return Ok(PostToolUseOutput::default());
        };
        let rel_path = Path::new(&file_path);
        if rel_path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            return Ok(PostToolUseOutput::default());
        }

        let abs_path = if rel_path.is_absolute() {
            rel_path.to_path_buf()
        } else {
            input.context.cwd.join(rel_path)
        };

        let source =
            std::fs::read_to_string(&abs_path).map_err(|source| RustSimilarityError::Io {
                path: abs_path.clone(),
                source,
            })?;

        let mut parser = RustParser::new();
        let funcs = parser
            .extract_functions(&source)
            .map_err(RustSimilarityError::Parse)?;

        let pairs = find_similar_functions(&funcs, self.threshold, &self.opts);
        if pairs.is_empty() {
            return Ok(PostToolUseOutput::default());
        }

        Ok(PostToolUseOutput {
            common: CommonHookOutput {
                system_message: Some(format_report(&file_path, &pairs)),
                ..CommonHookOutput::default()
            },
            ..PostToolUseOutput::default()
        })
    }
}

fn extract_file_path(tool_input: &serde_json::Value) -> Option<String> {
    tool_input
        .get("file_path")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn format_report(file_path: &str, pairs: &[SimilarPair<'_>]) -> String {
    let mut out = format!(
        "agent-lens rust-similarity: {} similar function pair(s) in {}\n",
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

    #[test]
    fn ignores_non_editing_tools() {
        let hook = RustSimilarityHook::new();
        let input = PostToolUseInput {
            context: ctx(PathBuf::from("/tmp")),
            tool_name: "Bash".into(),
            tool_input: json!({"command": "ls"}),
            tool_response: json!({}),
        };
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn ignores_non_rust_files() {
        let hook = RustSimilarityHook::new();
        let input = PostToolUseInput {
            context: ctx(PathBuf::from("/tmp")),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": "README.md"}),
            tool_response: json!({}),
        };
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn ignores_missing_file_path() {
        let hook = RustSimilarityHook::new();
        let input = PostToolUseInput {
            context: ctx(PathBuf::from("/tmp")),
            tool_name: "Edit".into(),
            tool_input: json!({}),
            tool_response: json!({}),
        };
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn reports_similar_pair_via_system_message() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
            fn alpha(x: i32) -> i32 { let y = x + 1; let z = y * 2; z }
            fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
        "#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = RustSimilarityHook::new().with_threshold(0.5);
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
        // Two structurally unrelated functions so the score stays low.
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

        let hook = RustSimilarityHook::new();
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

        let hook = RustSimilarityHook::new().with_threshold(0.5);
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
        let hook = RustSimilarityHook::new();
        let input = PostToolUseInput {
            context: ctx(PathBuf::from("/definitely/does/not/exist")),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": "missing.rs"}),
            tool_response: json!({}),
        };
        let err = hook.handle(input).unwrap_err();
        assert!(matches!(err, RustSimilarityError::Io { .. }));
    }
}
