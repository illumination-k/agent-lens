//! Codex `SessionStart` hook handler.
//!
//! Runs once per session and injects a one-shot context summary into
//! Codex via `additionalContext`: the highest churn × complexity files
//! (hotspot) and a thumbnail of the crate's coupling graph (top
//! Fan-In/Fan-Out modules, dependency cycles, most coupled pairs).
//!
//! The point is an "onboarding sketch" — what the agent should know
//! about this codebase before it starts touching files. Both halves are
//! best-effort: a session that starts outside a git working tree gets a
//! report without the hotspot section, and a session that isn't anchored
//! at a recognised module root gets one without the coupling section. If
//! neither half produces signal, the hook stays silent and falls through
//! to a default no-op response.
//!
//! The summary itself is rendered by
//! [`crate::hooks::core::session_summary::render_summary`] and driven by the
//! engine-agnostic [`crate::hooks::core::SummaryHook`]; this module is
//! just the Codex envelope around it.

use std::path::Path;

use agent_hooks::codex::{SessionStartHookSpecificOutput, SessionStartInput, SessionStartOutput};

use crate::hooks::core::SessionStartEnvelope;

const HOOK_EVENT_NAME: &str = "SessionStart";

/// Codex's SessionStart adapter for the engine-agnostic summary hook.
pub struct CodexSessionStart;

impl SessionStartEnvelope for CodexSessionStart {
    type Input = SessionStartInput;
    type Output = SessionStartOutput;

    fn cwd(input: &Self::Input) -> &Path {
        &input.context.cwd
    }

    fn wrap_summary(body: String) -> Self::Output {
        SessionStartOutput {
            hook_specific_output: Some(SessionStartHookSpecificOutput {
                hook_event_name: HOOK_EVENT_NAME.to_owned(),
                additional_context: Some(body),
            }),
            ..SessionStartOutput::default()
        }
    }
}

/// Codex SessionStart handler that emits a hotspot + coupling summary.
pub type SummaryHook = crate::hooks::core::SummaryHook<CodexSessionStart>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::init_repo_with_crate_for_session_summary;
    use agent_hooks::Hook;
    use agent_hooks::codex::{HookContext, SessionStartSource};
    use std::path::PathBuf;

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

    #[test]
    fn no_op_when_cwd_has_neither_repo_nor_crate() {
        let dir = tempfile::tempdir().unwrap();
        let out = SummaryHook::new()
            .handle(input(dir.path().to_path_buf()))
            .unwrap();
        assert_eq!(out, SessionStartOutput::default());
    }

    #[test]
    fn injects_summary_via_additional_context() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_crate_for_session_summary(dir.path());

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
        assert!(body.contains("## Coupling"), "want coupling: {body}");
    }
}
