//! `wrapper` PostToolUse handler for Codex.
//!
//! After `apply_patch` runs, parse every updated/added Rust source file
//! and report any function whose body, after stripping a short chain of
//! trivial adapters, is just a forwarding call to another function.
//! Findings are returned as `hookSpecificOutput.additionalContext` so
//! they land in Codex's developer context without blocking the turn.

use std::fmt::Write as _;
use std::path::PathBuf;

use agent_hooks::Hook;
use agent_hooks::codex::{PostToolUseHookSpecificOutput, PostToolUseInput, PostToolUseOutput};
use lens_rust::{WrapperFinding, find_wrappers};

use crate::analyze::SourceLang;
use crate::hooks::codex::post_tool_use::{ReadEditedSourceError, prepare_edited_sources};

const HOOK_EVENT_NAME: &str = "PostToolUse";

#[derive(Debug, Clone, Default)]
pub struct WrapperHook;

impl WrapperHook {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug)]
pub enum WrapperError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for WrapperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::Parse(e) => write!(f, "failed to parse source: {e}"),
        }
    }
}

impl std::error::Error for WrapperError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse(e) => Some(e.as_ref()),
        }
    }
}

impl From<ReadEditedSourceError> for WrapperError {
    fn from(e: ReadEditedSourceError) -> Self {
        Self::Io {
            path: e.path,
            source: e.source,
        }
    }
}

impl Hook for WrapperHook {
    type Input = PostToolUseInput;
    type Output = PostToolUseOutput;
    type Error = WrapperError;

    fn handle(&self, input: PostToolUseInput) -> Result<PostToolUseOutput, Self::Error> {
        let sources = prepare_edited_sources(&input)?;
        if sources.is_empty() {
            return Ok(PostToolUseOutput::default());
        }

        let mut report = String::new();
        let mut total = 0usize;
        for src in &sources {
            let findings = run_wrappers(src.lang, &src.source)?;
            if findings.is_empty() {
                continue;
            }
            total += findings.len();
            append_file_report(&mut report, &src.rel_path, &findings);
        }
        if total == 0 {
            return Ok(PostToolUseOutput::default());
        }

        let header = format!("agent-lens wrapper: {total} thin wrapper(s) detected\n");
        Ok(PostToolUseOutput {
            hook_specific_output: Some(PostToolUseHookSpecificOutput {
                hook_event_name: HOOK_EVENT_NAME.to_owned(),
                additional_context: Some(format!("{header}{report}")),
            }),
            ..PostToolUseOutput::default()
        })
    }
}

fn run_wrappers(lang: SourceLang, source: &str) -> Result<Vec<WrapperFinding>, WrapperError> {
    match lang {
        SourceLang::Rust => find_wrappers(source).map_err(|e| WrapperError::Parse(Box::new(e))),
    }
}

fn append_file_report(out: &mut String, file_path: &str, findings: &[WrapperFinding]) {
    let _ = writeln!(out, "{file_path}:");
    for finding in findings {
        if finding.adapters.is_empty() {
            let _ = writeln!(
                out,
                "- {} (L{}-{}) -> {}",
                finding.name, finding.start_line, finding.end_line, finding.callee,
            );
        } else {
            let _ = writeln!(
                out,
                "- {} (L{}-{}) -> {} [via {}]",
                finding.name,
                finding.start_line,
                finding.end_line,
                finding.callee,
                finding.adapters.join(""),
            );
        }
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
