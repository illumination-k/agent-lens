//! Claude Code `SessionStart` hook handler.
//!
//! Runs once per session and injects a one-shot context summary into
//! Claude Code via `additionalContext`: the highest churn × complexity
//! files (hotspot) and a thumbnail of the crate's coupling graph (top
//! Fan-In/Fan-Out modules, dependency cycles, most coupled pairs).
//!
//! Mirrors the Codex SessionStart handler — both share the same body
//! renderer ([`crate::hooks::core::render_summary`]) and only differ in
//! how the resulting string is wrapped into a hook response.

use agent_hooks::Hook;
use agent_hooks::claude_code::{
    SessionStartHookSpecificOutput, SessionStartInput, SessionStartOutput,
};

use crate::hooks::core::{SessionSummaryError, render_summary};

const HOOK_EVENT_NAME: &str = "SessionStart";

/// Claude Code SessionStart handler that emits a hotspot + coupling summary.
#[derive(Debug, Default, Clone, Copy)]
pub struct SummaryHook;

impl SummaryHook {
    pub fn new() -> Self {
        Self
    }
}

impl Hook for SummaryHook {
    type Input = SessionStartInput;
    type Output = SessionStartOutput;
    type Error = SessionSummaryError;

    fn handle(&self, input: Self::Input) -> Result<Self::Output, Self::Error> {
        let Some(body) = render_summary(&input.context.cwd)? else {
            return Ok(SessionStartOutput::default());
        };
        Ok(SessionStartOutput {
            hook_specific_output: Some(SessionStartHookSpecificOutput {
                hook_event_name: HOOK_EVENT_NAME.to_owned(),
                additional_context: Some(body),
            }),
            ..SessionStartOutput::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};
    use agent_hooks::claude_code::{HookContext, SessionStartSource};
    use std::path::{Path, PathBuf};

    fn ctx(cwd: PathBuf) -> HookContext {
        HookContext {
            session_id: "sess".into(),
            transcript_path: PathBuf::from("/tmp/t.jsonl"),
            cwd,
            permission_mode: None,
        }
    }

    fn input(cwd: PathBuf) -> SessionStartInput {
        SessionStartInput {
            context: ctx(cwd),
            source: SessionStartSource::Startup,
        }
    }

    fn init_repo_with_crate(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        write_file(dir, "src/lib.rs", "pub mod a;\npub mod b;\n");
        write_file(
            dir,
            "src/a.rs",
            "use crate::b::Bar;\npub struct Foo;\nfn _x(_b: Bar) {}\n",
        );
        write_file(
            dir,
            "src/b.rs",
            r#"
pub struct Bar;
pub fn nest(n: i32) -> i32 {
    if n > 0 { if n > 1 { if n > 2 { if n > 3 { return n; } } } }
    0
}
"#,
        );
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", "initial"]);
        write_file(
            dir,
            "src/b.rs",
            r#"
pub struct Bar;
pub fn nest(n: i32) -> i32 {
    if n > 0 { if n > 1 { if n > 2 { if n > 3 { return n; } if n > 4 { return n + 1; } } } }
    0
}
"#,
        );
        run_git(dir, &["add", "src/b.rs"]);
        run_git(dir, &["commit", "-q", "-m", "tweak b"]);
    }

    #[test]
    fn no_op_when_cwd_has_neither_repo_nor_crate() {
        let dir = tempfile::tempdir().unwrap();
        let out = SummaryHook::new()
            .handle(input(dir.path().to_path_buf()))
            .unwrap();
        assert_eq!(out, SessionStartOutput::default());
    }

    #[test]
    fn injects_hotspot_and_coupling_sections() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_crate(dir.path());

        let out = SummaryHook::new()
            .handle(input(dir.path().to_path_buf()))
            .unwrap();
        let extra = out
            .hook_specific_output
            .expect("expected hook_specific_output");
        assert_eq!(extra.hook_event_name, "SessionStart");
        let body = extra
            .additional_context
            .expect("expected additionalContext");

        assert!(body.starts_with("# agent-lens session-start"), "got {body}");
        assert!(body.contains("## Hotspots"), "want hotspot: {body}");
        assert!(body.contains("src/b.rs"), "want churn target: {body}");
        assert!(body.contains("## Coupling"), "want coupling: {body}");
        assert!(body.contains("crate::a"), "want modules: {body}");
        assert!(body.contains("crate::b"), "want modules: {body}");
    }

    #[test]
    fn coupling_only_when_no_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "pub mod a;\n");
        write_file(dir.path(), "src/a.rs", "pub fn solo() {}\n");

        let out = SummaryHook::new()
            .handle(input(dir.path().to_path_buf()))
            .unwrap();
        let body = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("expected additionalContext");
        assert!(body.contains("## Coupling"));
        assert!(!body.contains("## Hotspots"), "should skip hotspot: {body}");
    }

    #[test]
    fn hotspot_only_when_no_crate_root() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(
            dir.path(),
            "loose.rs",
            r#"
pub fn nest(n: i32) -> i32 {
    if n > 0 { if n > 1 { if n > 2 { if n > 3 { return n; } } } }
    0
}
"#,
        );
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let out = SummaryHook::new()
            .handle(input(dir.path().to_path_buf()))
            .unwrap();
        let body = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("expected additionalContext");
        assert!(body.contains("## Hotspots"));
        assert!(
            !body.contains("## Coupling"),
            "should skip coupling: {body}"
        );
    }
}
