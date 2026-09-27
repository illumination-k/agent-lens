//! Codex `PostToolUse` hook handlers.
//!
//! Codex's only source-modifying tool today is `apply_patch`, which carries
//! the entire patch as a single string in `tool_input.command`. This
//! module parses that envelope, walks the `*** Update File:` and
//! `*** Add File:` markers, and reads each touched file off disk so the
//! engine-agnostic [`crate::hooks::core`] runners can analyse them.

use agent_hooks::codex::{PostToolUseHookSpecificOutput, PostToolUseInput, PostToolUseOutput};

use crate::hooks::core::{EditedSource, HookEnvelope, MissingFilePolicy, ReadEditedSourceError};

pub(crate) const HOOK_EVENT_NAME: &str = "PostToolUse";

/// `apply_patch` markers whose files exist once the patch lands.
/// `*** Delete File:` entries are skipped because the file is gone by the
/// time the hook runs; a listed file that is missing is an error.
const PATCHED_PATH_MARKERS: &[&str] = &["*** Update File: ", "*** Add File: "];

/// Codex's PostToolUse adapter for the engine-agnostic hook runner.
pub struct CodexPostToolUse;

impl HookEnvelope for CodexPostToolUse {
    type Input = PostToolUseInput;
    type Output = PostToolUseOutput;

    fn prepare_sources(input: &Self::Input) -> Result<Vec<EditedSource>, ReadEditedSourceError> {
        super::prepare_patched_sources(
            &input.tool_name,
            &input.tool_input,
            &input.context.cwd,
            PATCHED_PATH_MARKERS,
            MissingFilePolicy::Error,
        )
    }

    fn cwd(input: &Self::Input) -> &std::path::Path {
        &input.context.cwd
    }

    fn wrap_report(report: String) -> Self::Output {
        PostToolUseOutput {
            hook_specific_output: Some(PostToolUseHookSpecificOutput {
                hook_event_name: HOOK_EVENT_NAME.to_owned(),
                additional_context: Some(report),
            }),
            ..PostToolUseOutput::default()
        }
    }
}

/// Codex `similarity` PostToolUse hook handler.
pub type SimilarityHook = crate::hooks::core::SimilarityHook<CodexPostToolUse>;
/// Codex `wrapper` PostToolUse hook handler.
pub type WrapperHook = crate::hooks::core::WrapperHook<CodexPostToolUse>;
/// Codex `footprint` PostToolUse hook handler.
pub type FootprintHook = crate::hooks::core::FootprintHook<CodexPostToolUse>;
/// Re-exported for compatibility with earlier per-handler error aliases.
pub type SimilarityError = crate::hooks::core::HookError;
/// Re-exported for compatibility with earlier per-handler error aliases.
pub type WrapperError = crate::hooks::core::HookError;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{codex_hook_context, write_file};
    use agent_hooks::Hook;
    use serde_json::json;
    use std::path::PathBuf;

    fn input(cwd: PathBuf, tool_name: &str, command: &str) -> PostToolUseInput {
        PostToolUseInput {
            context: codex_hook_context(&cwd),
            turn_id: "turn-1".into(),
            tool_name: tool_name.into(),
            tool_use_id: "call-1".into(),
            tool_input: json!({"command": command}),
            tool_response: json!({}),
        }
    }

    // -- similarity --------------------------------------------------

    #[test]
    fn similarity_ignores_non_apply_patch_tools() {
        let hook = SimilarityHook::new();
        let mut payload = input(PathBuf::from("/tmp"), "Bash", "ls");
        payload.tool_input = json!({"command": "ls"});
        let out = hook.handle(payload).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn similarity_ignores_apply_patch_without_command() {
        let hook = SimilarityHook::new();
        let payload = PostToolUseInput {
            tool_input: json!({}),
            ..input(PathBuf::from("/tmp"), "apply_patch", "")
        };
        let out = hook.handle(payload).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn similarity_ignores_unsupported_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "*** Begin Patch\n*** Update File: notes.md\n*** End Patch\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = SimilarityHook::new().handle(payload).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn similarity_reports_clusters_via_additional_context() {
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
    fn similarity_aggregates_across_multiple_files() {
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
    fn similarity_no_report_when_below_threshold() {
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
    fn similarity_missing_file_surfaces_io_error() {
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
    fn similarity_with_threshold_actually_overrides_default() {
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

    // -- wrapper -----------------------------------------------------

    #[test]
    fn wrapper_ignores_non_apply_patch_tools() {
        let payload = input(PathBuf::from("/tmp"), "Bash", "ls");
        let out = WrapperHook::new().handle(payload).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn wrapper_ignores_unsupported_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "*** Update File: notes.md\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = WrapperHook::new().handle(payload).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn wrapper_detects_wrappers_via_additional_context() {
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
    fn wrapper_report_includes_adapter_chain() {
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
    fn wrapper_no_report_when_no_wrappers() {
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
    fn wrapper_missing_file_surfaces_io_error() {
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
