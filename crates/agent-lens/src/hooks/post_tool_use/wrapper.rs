//! `wrapper` PostToolUse handler for Claude Code.
//!
//! The detection itself lives in [`crate::hooks::core::wrapper`]; this
//! module is the Claude Code adapter that converts the hook input into a
//! list of [`crate::hooks::core::EditedSource`]s and wraps the report
//! string in a `systemMessage`.

use agent_hooks::Hook;
use agent_hooks::claude_code::{CommonHookOutput, PostToolUseInput, PostToolUseOutput};

use crate::hooks::core::HookError;
use crate::hooks::core::wrapper::WrapperCore;
use crate::hooks::post_tool_use::prepare_edited_sources;

pub use crate::hooks::core::HookError as WrapperError;

/// Handler implementation for the `wrapper` PostToolUse hook.
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

    fn assert_no_op(tool_name: &str, tool_input: serde_json::Value) {
        let hook = WrapperHook::new();
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
    fn rust_extension_triggers_wrapper_detection() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
fn render(x: &str) -> String { internal_render(x) }
fn meaningful(x: i32) -> i32 { let y = x + 1; y * 2 }
"#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = WrapperHook::new();
        let input = PostToolUseInput {
            context: ctx(dir.path().to_path_buf()),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
            tool_response: json!({"success": true}),
        };
        let out = hook.handle(input).unwrap();
        let msg = out.common.system_message.expect("expected a report");
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
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = WrapperHook::new();
        let input = PostToolUseInput {
            context: ctx(dir.path().to_path_buf()),
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
            tool_response: json!({}),
        };
        let out = hook.handle(input).unwrap();
        let msg = out.common.system_message.expect("expected a report");
        assert!(msg.contains("shim"), "should list the wrapper: {msg}");
        assert!(
            msg.contains("via"),
            "should annotate the adapter chain: {msg}"
        );
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
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = WrapperHook::new();
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
        let source = "fn shim(x: i32) -> i32 { core(x) }\n";
        write_file(&nested, "lib.rs", source);

        let hook = WrapperHook::new();
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
        let hook = WrapperHook::new();
        let input = PostToolUseInput {
            context: ctx(PathBuf::from("/definitely/does/not/exist")),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": "missing.rs"}),
            tool_response: json!({}),
        };
        let err = hook.handle(input).unwrap_err();
        assert!(matches!(err, WrapperError::Io { .. }));
    }
}
