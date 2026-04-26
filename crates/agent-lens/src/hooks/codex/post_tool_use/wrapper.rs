//! `wrapper` PostToolUse handler for Codex.
//!
//! The detection itself lives in [`crate::hooks::core::wrapper`]; this
//! module is the Codex adapter that converts the `apply_patch` envelope
//! into a list of [`crate::hooks::core::EditedSource`]s and wraps the
//! report string in `hookSpecificOutput.additionalContext`.

use agent_hooks::Hook;
use agent_hooks::codex::{PostToolUseHookSpecificOutput, PostToolUseInput, PostToolUseOutput};

use crate::hooks::codex::post_tool_use::{HOOK_EVENT_NAME, prepare_edited_sources};
use crate::hooks::core::HookError;
use crate::hooks::core::wrapper::WrapperCore;

pub use crate::hooks::core::HookError as WrapperError;

#[derive(Debug, Clone, Default)]
pub struct WrapperHook {
    core: WrapperCore,
}

impl WrapperHook {
    pub fn new() -> Self {
        Self {
            core: WrapperCore::new(),
        }
    }
}

impl Hook for WrapperHook {
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
        let payload = input(PathBuf::from("/tmp"), "Bash", "ls");
        let out = WrapperHook::new().handle(payload).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn ignores_unsupported_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "*** Update File: notes.md\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = WrapperHook::new().handle(payload).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn detects_wrappers_via_additional_context() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
fn render(x: &str) -> String { internal_render(x) }
fn meaningful(x: i32) -> i32 { let y = x + 1; y * 2 }
"#;
        write_file(dir.path(), "lib.rs", source);

        let patch = "*** Update File: lib.rs\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = WrapperHook::new().handle(payload).unwrap();

        let extra = out
            .hook_specific_output
            .expect("expected hook_specific_output");
        assert_eq!(extra.hook_event_name, "PostToolUse");
        let msg = extra
            .additional_context
            .expect("expected additionalContext");
        assert!(msg.contains("lib.rs"), "should mention file: {msg}");
        assert!(msg.contains("render"), "should mention render: {msg}");
        assert!(
            msg.contains("internal_render"),
            "should mention forwarding target: {msg}",
        );
        assert!(
            !msg.contains("meaningful"),
            "should not flag the function with real logic: {msg}",
        );
        assert!(out.decision.is_none());
    }

    #[test]
    fn report_includes_adapter_chain() {
        let dir = tempfile::tempdir().unwrap();
        let source = "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n";
        write_file(dir.path(), "lib.rs", source);

        let patch = "*** Update File: lib.rs\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = WrapperHook::new().handle(payload).unwrap();
        let msg = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("expected additionalContext");
        assert!(msg.contains("shim"), "should list the wrapper: {msg}");
        assert!(msg.contains("via"), "should annotate adapter chain: {msg}");
        assert!(msg.contains(".unwrap()"));
        assert!(msg.contains(".into()"));
    }

    #[test]
    fn no_report_when_no_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
fn alpha(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs {
        total += *x;
    }
    total
}
"#;
        write_file(dir.path(), "lib.rs", source);
        let patch = "*** Update File: lib.rs\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = WrapperHook::new().handle(payload).unwrap();
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
        let err = WrapperHook::new().handle(payload).unwrap_err();
        assert!(matches!(err, WrapperError::Io { .. }));
    }
}
