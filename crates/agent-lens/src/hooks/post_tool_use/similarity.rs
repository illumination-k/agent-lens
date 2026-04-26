//! `similarity` PostToolUse handler for Claude Code.
//!
//! The analysis itself lives in [`crate::hooks::core::similarity`]; this
//! module is the Claude Code adapter that converts the hook input into a
//! list of [`crate::hooks::core::EditedSource`]s and wraps the report
//! string in a `systemMessage`.

use agent_hooks::Hook;
use agent_hooks::claude_code::{CommonHookOutput, PostToolUseInput, PostToolUseOutput};

use crate::hooks::core::HookError;
use crate::hooks::core::similarity::SimilarityCore;
use crate::hooks::post_tool_use::prepare_edited_sources;

pub use crate::hooks::core::HookError as SimilarityError;

/// Handler implementation for the `similarity` PostToolUse hook.
#[derive(Debug, Clone)]
pub struct SimilarityHook {
    core: SimilarityCore,
}

impl SimilarityHook {
    /// Construct a handler with the default similarity threshold and TSED
    /// options.
    pub fn new() -> Self {
        Self {
            core: SimilarityCore::new(),
        }
    }

    /// Override the similarity threshold. Useful for tests; the binary
    /// currently always uses the default.
    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.core = self.core.with_threshold(threshold);
        self
    }
}

impl Default for SimilarityHook {
    fn default() -> Self {
        Self::new()
    }
}

impl Hook for SimilarityHook {
    type Input = PostToolUseInput;
    type Output = PostToolUseOutput;
    type Error = HookError;

    fn handle(&self, input: PostToolUseInput) -> Result<PostToolUseOutput, Self::Error> {
        let sources = prepare_edited_sources(&input)?;
        let Some(report) = self.core.run(&sources)? else {
            return Ok(PostToolUseOutput::default());
        };

        Ok(PostToolUseOutput {
            common: CommonHookOutput {
                system_message: Some(report),
                ..CommonHookOutput::default()
            },
            ..PostToolUseOutput::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks::claude_code::HookContext;
    use serde_json::json;
    use std::io::Write;
    use std::path::{Path, PathBuf};

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
    /// tool_input)` as out of scope and returns the empty default output.
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
        for ext in ["README.md", "notes.txt", "script.py", "app.go"] {
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
}
