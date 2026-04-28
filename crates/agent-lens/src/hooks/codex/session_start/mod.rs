//! Codex `SessionStart` hook handler.
//!
//! Runs once per session and injects a one-shot context summary into
//! Codex via `additionalContext`: the highest churn × complexity files
//! (hotspot) and a thumbnail of the crate's coupling graph (top
//! Fan-In/Fan-Out modules, dependency cycles, most coupled pairs).
//!
//! The point is an "onboarding sketch" — what the agent should know
//! about this codebase before it starts touching files. Both halves
//! are best-effort: a session that starts outside a git working tree
//! gets a report without the hotspot section, and a session that isn't
//! anchored at a Rust crate gets one without the coupling section. If
//! neither half produces signal, the hook stays silent and falls
//! through to a default no-op response.
//!
//! The body itself is rendered by [`crate::hooks::core::render_summary`]
//! and shared with the parallel Claude Code SessionStart handler; this
//! module is just the Codex-shaped wrapper around it.

use agent_hooks::Hook;
use agent_hooks::codex::{SessionStartHookSpecificOutput, SessionStartInput, SessionStartOutput};

use crate::hooks::core::{SessionSummaryError, render_summary};

const HOOK_EVENT_NAME: &str = "SessionStart";

/// Codex SessionStart handler that emits a hotspot + coupling summary.
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
    use agent_hooks::codex::{HookContext, SessionStartSource};
    use std::path::{Path, PathBuf};

    fn ctx(cwd: PathBuf) -> HookContext {
        HookContext {
            session_id: "sess".into(),
            transcript_path: None,
            cwd,
            model: "gpt-5".into(),
        }
    }

    fn input(cwd: PathBuf) -> SessionStartInput {
        SessionStartInput {
            context: ctx(cwd),
            source: SessionStartSource::Startup,
        }
    }

    /// Produce a minimal repo with a `src/` crate and two commits so
    /// hotspot has churn to rank by.
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
        // Touch b.rs again so its churn dominates a.rs.
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
        assert!(
            body.contains("## Hotspots"),
            "should include hotspot: {body}"
        );
        assert!(
            body.contains("src/b.rs"),
            "should mention churn target: {body}"
        );
        assert!(
            body.contains("## Coupling"),
            "should include coupling: {body}"
        );
        assert!(body.contains("crate::a"), "should mention modules: {body}");
        assert!(body.contains("crate::b"), "should mention modules: {body}");
    }

    #[test]
    fn coupling_only_when_no_git_repo() {
        // A bare crate that isn't checked into git: hotspot section
        // is skipped, coupling stays.
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
        // A git repo with .rs files but no recognisable crate root
        // (no src/lib.rs or src/main.rs at the top level).
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
