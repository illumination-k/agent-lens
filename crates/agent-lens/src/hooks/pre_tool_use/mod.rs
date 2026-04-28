//! `PreToolUse` hook handlers for Claude Code.
//!
//! Each handler is exposed as a clap subcommand by the CLI so typos
//! surface at parse time rather than at runtime. The actual analysis
//! lives in [`crate::hooks::core`]; this module is the Claude Code
//! adapter (input shape → `EditedSource` list, output string →
//! `systemMessage`).
//!
//! Compared with [`super::post_tool_use`], the prepare step has to
//! tolerate a file that does not yet exist: `Write` of a brand-new file
//! is a normal pre-edit case and the hook should stay silent rather
//! than failing the tool call. `Edit` / `MultiEdit` always target
//! existing files, so a missing file there is still a hard error.

use agent_hooks::claude_code::{CommonHookOutput, PreToolUseInput, PreToolUseOutput};

use crate::hooks::core::{
    EditedSource, HookEnvelope, MissingFilePolicy, ReadEditedSourceError, read_edited_source,
};

/// Claude Code's PreToolUse adapter for the engine-agnostic hook
/// runner.
pub struct ClaudeCodePreToolUse;

impl HookEnvelope for ClaudeCodePreToolUse {
    type Input = PreToolUseInput;
    type Output = PreToolUseOutput;

    fn prepare_sources(input: &Self::Input) -> Result<Vec<EditedSource>, ReadEditedSourceError> {
        prepare_edited_sources(input)
    }

    fn wrap_report(report: String) -> Self::Output {
        PreToolUseOutput {
            common: CommonHookOutput {
                system_message: Some(report),
                ..CommonHookOutput::default()
            },
            ..PreToolUseOutput::default()
        }
    }
}

/// Claude Code `complexity` PreToolUse hook handler.
pub type ComplexityHook = crate::hooks::core::ComplexityHook<ClaudeCodePreToolUse>;
/// Claude Code `cohesion` PreToolUse hook handler.
pub type CohesionHook = crate::hooks::core::CohesionHook<ClaudeCodePreToolUse>;
/// Re-exported for symmetry with the PostToolUse handlers.
pub type ComplexityError = crate::hooks::core::HookError;
/// Re-exported for symmetry with the PostToolUse handlers.
pub type CohesionError = crate::hooks::core::HookError;

/// Tool names whose `tool_input.file_path` points at the file that is
/// about to be modified. Anything outside this set is ignored.
pub(crate) const EDITING_TOOL_NAMES: &[&str] = &["Write", "Edit", "MultiEdit"];

/// Tools where a missing file should be treated as "no current state to
/// read" rather than an error. `Write` can create a fresh file, in
/// which case there is nothing on disk yet and the hook simply has no
/// pre-edit context to inject. `Edit` and `MultiEdit` always target an
/// existing file, so a missing file there is still a hard error and
/// will fall through to the normal IO path.
const TOLERATE_MISSING_FILE_TOOLS: &[&str] = &["Write"];

/// Prepare the file the agent is about to edit for a PreToolUse hook.
///
/// Returns an empty `Vec` for "no opinion" cases — non-editing tools,
/// missing `file_path`, an extension the analysers can't handle, or a
/// `Write` call against a path that does not yet exist (a brand-new
/// file). The list is at most one element long; returning a `Vec` lets
/// the engine-agnostic core treat Claude Code and Codex inputs the same
/// way.
pub(crate) fn prepare_edited_sources(
    input: &PreToolUseInput,
) -> Result<Vec<EditedSource>, ReadEditedSourceError> {
    if !EDITING_TOOL_NAMES.contains(&input.tool_name.as_str()) {
        return Ok(Vec::new());
    }
    let Some(rel_path) = extract_file_path(&input.tool_input) else {
        return Ok(Vec::new());
    };
    let missing_policy = if TOLERATE_MISSING_FILE_TOOLS.contains(&input.tool_name.as_str()) {
        MissingFilePolicy::Skip
    } else {
        MissingFilePolicy::Error
    };
    Ok(
        read_edited_source(&input.context.cwd, rel_path, missing_policy)?
            .into_iter()
            .collect(),
    )
}

fn extract_file_path(tool_input: &serde_json::Value) -> Option<String> {
    tool_input
        .get("file_path")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use agent_hooks::Hook;
    use agent_hooks::claude_code::HookContext;
    use serde_json::json;
    use std::path::PathBuf;

    fn ctx(cwd: PathBuf) -> HookContext {
        HookContext {
            session_id: "sess".into(),
            transcript_path: PathBuf::from("/tmp/t.jsonl"),
            cwd,
            permission_mode: None,
        }
    }

    fn payload(cwd: PathBuf, tool_name: &str, tool_input: serde_json::Value) -> PreToolUseInput {
        PreToolUseInput {
            context: ctx(cwd),
            tool_name: tool_name.into(),
            tool_input,
        }
    }

    // -- complexity ---------------------------------------------------

    #[test]
    fn complexity_ignores_non_editing_tools() {
        let hook = ComplexityHook::new();
        let input = payload(PathBuf::from("/tmp"), "Bash", json!({"command": "ls"}));
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }

    #[test]
    fn complexity_ignores_unknown_extensions() {
        let hook = ComplexityHook::new();
        let input = payload(
            PathBuf::from("/tmp"),
            "Edit",
            json!({"file_path": "README.md"}),
        );
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }

    #[test]
    fn complexity_reports_via_system_message_for_nontrivial_function() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
fn nested(n: i32) -> i32 {
    if n > 0 {
        if n > 1 {
            if n > 2 {
                if n > 3 {
                    return n;
                }
            }
        }
    }
    0
}
"#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = ComplexityHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Edit",
            json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
        );
        let out = hook.handle(input).unwrap();
        let msg = out.common.system_message.expect("expected a report");
        assert!(msg.contains("nested"), "should mention function: {msg}");
        assert!(msg.contains("cog="), "should include cognitive: {msg}");
        assert!(out.decision.is_none());
        assert!(out.hook_specific_output.is_none());
    }

    #[test]
    fn complexity_silent_when_file_is_trivial() {
        let dir = tempfile::tempdir().unwrap();
        let source = "fn solo() -> i32 { 1 }\n";
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = ComplexityHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Edit",
            json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
        );
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }

    #[test]
    fn complexity_write_to_new_file_is_silent_no_op() {
        // A `Write` call against a brand-new file has no current state
        // on disk. The hook must stay silent instead of failing the
        // tool call with an IO error.
        let dir = tempfile::tempdir().unwrap();
        let hook = ComplexityHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Write",
            json!({"file_path": "fresh.rs"}),
        );
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }

    #[test]
    fn complexity_edit_against_missing_file_surfaces_io_error() {
        // `Edit` always targets an existing file. A missing file there
        // is a real problem and must surface, not be swallowed.
        let dir = tempfile::tempdir().unwrap();
        let hook = ComplexityHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Edit",
            json!({"file_path": "missing.rs"}),
        );
        let err = hook.handle(input).unwrap_err();
        assert!(matches!(err, ComplexityError::Io { .. }));
    }

    #[test]
    fn complexity_resolves_relative_path_against_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src");
        std::fs::create_dir_all(&nested).unwrap();
        let source = r#"
fn pyramid(n: i32) -> i32 {
    if n > 0 { if n > 1 { if n > 2 { if n > 3 { return n; } } } }
    0
}
"#;
        write_file(&nested, "lib.rs", source);

        let hook = ComplexityHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Edit",
            json!({"file_path": "src/lib.rs"}),
        );
        let out = hook.handle(input).unwrap();
        assert!(out.common.system_message.is_some());
    }

    // -- cohesion -----------------------------------------------------

    #[test]
    fn cohesion_reports_split_impl_via_system_message() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
struct Thing { a: i32, b: i32 }
impl Thing {
    fn ga(&self) -> i32 { self.a }
    fn gb(&self) -> i32 { self.b }
}
"#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = CohesionHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Edit",
            json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
        );
        let out = hook.handle(input).unwrap();
        let msg = out.common.system_message.expect("expected a report");
        assert!(msg.contains("impl Thing"), "should label impl: {msg}");
        assert!(msg.contains("LCOM4=2"), "should report lcom4: {msg}");
    }

    #[test]
    fn cohesion_silent_when_impl_is_cohesive() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
struct Counter { n: i32 }
impl Counter {
    fn inc(&mut self) { self.n += 1; }
    fn get(&self) -> i32 { self.n }
}
"#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = CohesionHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Edit",
            json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
        );
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }

    #[test]
    fn cohesion_write_to_new_file_is_silent_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let hook = CohesionHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Write",
            json!({"file_path": "fresh.rs"}),
        );
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }

    #[test]
    fn cohesion_ignores_missing_file_path() {
        let hook = CohesionHook::new();
        let input = payload(PathBuf::from("/tmp"), "Edit", json!({}));
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }
}
