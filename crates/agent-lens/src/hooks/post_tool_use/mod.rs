//! `PostToolUse` hook handlers for Claude Code.
//!
//! Each handler is exposed as a clap subcommand by the CLI so typos
//! surface at parse time rather than at runtime. The actual analysis
//! lives in [`crate::hooks::core`]; this module is the Claude Code
//! adapter (input shape → `EditedSource` list, output string →
//! `systemMessage`).

#[cfg(test)]
use std::path::Path;

use agent_hooks::claude_code::{CommonHookOutput, PostToolUseInput, PostToolUseOutput};

use crate::hooks::core::{
    EditedSource, HookEnvelope, MissingFilePolicy, ReadEditedSourceError, read_edited_source,
};

/// Claude Code's PostToolUse adapter for the engine-agnostic hook
/// runner.
pub struct ClaudeCodePostToolUse;

impl HookEnvelope for ClaudeCodePostToolUse {
    type Input = PostToolUseInput;
    type Output = PostToolUseOutput;

    fn prepare_sources(input: &Self::Input) -> Result<Vec<EditedSource>, ReadEditedSourceError> {
        prepare_edited_sources(input)
    }

    fn wrap_report(report: String) -> Self::Output {
        PostToolUseOutput {
            common: CommonHookOutput {
                system_message: Some(report),
                ..CommonHookOutput::default()
            },
            ..PostToolUseOutput::default()
        }
    }
}

/// Claude Code `similarity` PostToolUse hook handler.
pub type SimilarityHook = crate::hooks::core::SimilarityHook<ClaudeCodePostToolUse>;
/// Claude Code `wrapper` PostToolUse hook handler.
pub type WrapperHook = crate::hooks::core::WrapperHook<ClaudeCodePostToolUse>;
/// Re-exported for compatibility with earlier per-handler error aliases.
pub type SimilarityError = crate::hooks::core::HookError;
/// Re-exported for compatibility with earlier per-handler error aliases.
pub type WrapperError = crate::hooks::core::HookError;

/// Tool names whose `tool_input.file_path` points at the file that was
/// just modified. Anything outside this set is ignored.
pub(crate) const EDITING_TOOL_NAMES: &[&str] = &["Write", "Edit", "MultiEdit"];

/// Prepare the edited file for a PostToolUse hook to analyse.
///
/// Returns an empty `Vec` for "no opinion" cases — non-editing tools,
/// missing `file_path`, or an extension the analysers can't handle. The
/// list is at most one element long; returning a `Vec` lets the engine-
/// agnostic core treat Claude Code and Codex inputs the same way.
pub(crate) fn prepare_edited_sources(
    input: &PostToolUseInput,
) -> Result<Vec<EditedSource>, ReadEditedSourceError> {
    if !EDITING_TOOL_NAMES.contains(&input.tool_name.as_str()) {
        return Ok(Vec::new());
    }
    let Some(rel_path) = extract_file_path(&input.tool_input) else {
        return Ok(Vec::new());
    };
    Ok(
        read_edited_source(&input.context.cwd, rel_path, MissingFilePolicy::Error)?
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
    use agent_hooks::Hook;
    use agent_hooks::claude_code::HookContext;
    use serde_json::json;
    use std::io::Write;
    use std::path::PathBuf;

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

    /// Build a Claude Code PostToolUse payload with the given fields.
    fn payload(cwd: PathBuf, tool_name: &str, tool_input: serde_json::Value) -> PostToolUseInput {
        PostToolUseInput {
            context: ctx(cwd),
            tool_name: tool_name.into(),
            tool_input,
            tool_response: json!({}),
        }
    }

    // -- similarity ---------------------------------------------------

    fn assert_similarity_no_op(tool_name: &str, tool_input: serde_json::Value) {
        let hook = SimilarityHook::new();
        let input = payload(PathBuf::from("/tmp"), tool_name, tool_input.clone());
        let out = hook.handle(input).unwrap();
        assert_eq!(
            out,
            PostToolUseOutput::default(),
            "expected no-op for tool={tool_name} input={tool_input}",
        );
    }

    #[test]
    fn similarity_ignores_non_editing_tools() {
        assert_similarity_no_op("Bash", json!({"command": "ls"}));
    }

    #[test]
    fn similarity_ignores_unknown_extensions() {
        for ext in ["README.md", "notes.txt", "app.go"] {
            assert_similarity_no_op("Write", json!({ "file_path": ext }));
        }
    }

    #[test]
    fn similarity_ignores_extensionless_paths() {
        assert_similarity_no_op("Write", json!({"file_path": "Makefile"}));
    }

    #[test]
    fn similarity_ignores_missing_file_path() {
        assert_similarity_no_op("Edit", json!({}));
    }

    #[test]
    fn similarity_rust_extension_triggers_rust_parser() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
            fn alpha(x: i32) -> i32 { let y = x + 1; let z = y * 2; z }
            fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
        "#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = SimilarityHook::new().with_threshold(0.5);
        let input = payload(
            dir.path().to_path_buf(),
            "Write",
            json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
        );
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
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = SimilarityHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Write",
            json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
        );
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn similarity_tsx_extension_routes_through_typescript_parser() {
        // A `.tsx` file must reach the TypeScript parser with the JSX
        // dialect on. Without dialect threading, the wrapper component
        // would either be filtered out as unsupported or fail to parse
        // when oxc hits `<div />`.
        let dir = tempfile::tempdir().unwrap();
        let source = "\
function add(a: number, b: number): number { return a + b; }
function plus(a: number, b: number): number { return a + b; }
function Comp(): JSX.Element { return <div />; }
";
        let file = write_file(dir.path(), "App.tsx", source);

        let hook = SimilarityHook::new().with_threshold(0.5);
        let input = payload(
            dir.path().to_path_buf(),
            "Write",
            json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
        );
        let out = hook.handle(input).unwrap();
        let msg = out.common.system_message.expect("expected a report");
        assert!(msg.contains("add"), "should mention add: {msg}");
        assert!(msg.contains("plus"), "should mention plus: {msg}");
    }

    #[test]
    fn similarity_resolves_relative_path_against_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src");
        std::fs::create_dir_all(&nested).unwrap();
        let source = r#"
            fn alpha(x: i32) -> i32 { let y = x + 1; let z = y * 2; z }
            fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
        "#;
        write_file(&nested, "lib.rs", source);

        let hook = SimilarityHook::new().with_threshold(0.5);
        let input = payload(
            dir.path().to_path_buf(),
            "Edit",
            json!({"file_path": "src/lib.rs"}),
        );
        let out = hook.handle(input).unwrap();
        assert!(out.common.system_message.is_some());
    }

    #[test]
    fn similarity_missing_file_surfaces_io_error() {
        let hook = SimilarityHook::new();
        let input = payload(
            PathBuf::from("/definitely/does/not/exist"),
            "Write",
            json!({"file_path": "missing.rs"}),
        );
        let err = hook.handle(input).unwrap_err();
        assert!(matches!(err, SimilarityError::Io { .. }));
    }

    #[test]
    fn similarity_with_threshold_actually_overrides_default() {
        // Two near-identical bodies — at the default threshold they would
        // be reported. Setting an unreachable threshold has to suppress
        // them, or `with_threshold` is silently dropping the override.
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
            fn alpha(x: i32) -> i32 { let y = x + 1; let z = y * 2; z }
            fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
        "#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = SimilarityHook::new().with_threshold(1.5);
        let input = payload(
            dir.path().to_path_buf(),
            "Write",
            json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
        );
        let out = hook.handle(input).unwrap();
        assert_eq!(
            out,
            PostToolUseOutput::default(),
            "threshold=1.5 must suppress all pairs",
        );
    }

    // -- wrapper -----------------------------------------------------

    fn assert_wrapper_no_op(tool_name: &str, tool_input: serde_json::Value) {
        let hook = WrapperHook::new();
        let input = payload(PathBuf::from("/tmp"), tool_name, tool_input.clone());
        let out = hook.handle(input).unwrap();
        assert_eq!(
            out,
            PostToolUseOutput::default(),
            "expected no-op for tool={tool_name} input={tool_input}",
        );
    }

    #[test]
    fn wrapper_ignores_non_editing_tools() {
        assert_wrapper_no_op("Bash", json!({"command": "ls"}));
    }

    #[test]
    fn wrapper_ignores_unknown_extensions() {
        for ext in ["README.md", "notes.txt", "app.go"] {
            assert_wrapper_no_op("Write", json!({ "file_path": ext }));
        }
    }

    #[test]
    fn wrapper_ignores_extensionless_paths() {
        assert_wrapper_no_op("Write", json!({"file_path": "Makefile"}));
    }

    #[test]
    fn wrapper_ignores_missing_file_path() {
        assert_wrapper_no_op("Edit", json!({}));
    }

    #[test]
    fn wrapper_rust_extension_triggers_wrapper_detection() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
fn render(x: &str) -> String { internal_render(x) }
fn meaningful(x: i32) -> i32 { let y = x + 1; y * 2 }
"#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = WrapperHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Write",
            json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
        );
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
    fn wrapper_python_extension_triggers_wrapper_detection() {
        let dir = tempfile::tempdir().unwrap();
        let source = "
def render(x):
    return internal_render(x)

def meaningful(x):
    y = x + 1
    return y * 2
";
        let file = write_file(dir.path(), "lib.py", source);

        let hook = WrapperHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Write",
            json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
        );
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
    fn wrapper_report_includes_adapter_chain() {
        let dir = tempfile::tempdir().unwrap();
        let source = "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n";
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = WrapperHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Edit",
            json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
        );
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
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = WrapperHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Write",
            json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
        );
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn wrapper_resolves_relative_path_against_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src");
        std::fs::create_dir_all(&nested).unwrap();
        let source = "fn shim(x: i32) -> i32 { core(x) }\n";
        write_file(&nested, "lib.rs", source);

        let hook = WrapperHook::new();
        let input = payload(
            dir.path().to_path_buf(),
            "Edit",
            json!({"file_path": "src/lib.rs"}),
        );
        let out = hook.handle(input).unwrap();
        assert!(out.common.system_message.is_some());
    }

    #[test]
    fn wrapper_missing_file_surfaces_io_error() {
        let hook = WrapperHook::new();
        let input = payload(
            PathBuf::from("/definitely/does/not/exist"),
            "Write",
            json!({"file_path": "missing.rs"}),
        );
        let err = hook.handle(input).unwrap_err();
        assert!(matches!(err, WrapperError::Io { .. }));
    }
}
