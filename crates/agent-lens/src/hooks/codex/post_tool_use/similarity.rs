//! `similarity` PostToolUse handler for Codex.
//!
//! The analysis itself lives in [`crate::hooks::core::similarity`]; this
//! module is the Codex adapter that converts the `apply_patch` envelope
//! into a list of [`crate::hooks::core::EditedSource`]s and wraps the
//! report string in `hookSpecificOutput.additionalContext`.

use agent_hooks::Hook;
use agent_hooks::codex::{PostToolUseHookSpecificOutput, PostToolUseInput, PostToolUseOutput};

use crate::hooks::codex::post_tool_use::{HOOK_EVENT_NAME, prepare_edited_sources};
use crate::hooks::core::HookError;
use crate::hooks::core::similarity::SimilarityCore;

pub use crate::hooks::core::HookError as SimilarityError;

/// Handler implementation for the `similarity` Codex `PostToolUse` hook.
#[derive(Debug, Clone)]
pub struct SimilarityHook {
    core: SimilarityCore,
}

impl SimilarityHook {
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
            hook_specific_output: Some(PostToolUseHookSpecificOutput {
                hook_event_name: HOOK_EVENT_NAME.to_owned(),
                additional_context: Some(report),
            }),
            ..PostToolUseOutput::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks::codex::HookContext;
    use serde_json::json;
    use std::io::Write;
    use std::path::{Path, PathBuf};

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
}
