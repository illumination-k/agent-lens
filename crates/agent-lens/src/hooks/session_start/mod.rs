//! Claude Code `SessionStart` hook handler.
//!
//! Runs once per session and injects a one-shot context summary into
//! Claude Code via `additionalContext`: the highest churn × complexity
//! files (hotspot) and a thumbnail of the crate's coupling graph (top
//! Fan-In/Fan-Out modules, dependency cycles, most coupled pairs).
//!
//! The summary itself is rendered by
//! [`crate::hooks::core::session_summary::render_summary`] and driven by the
//! engine-agnostic [`crate::hooks::core::SummaryHook`]; this module is
//! just the Claude Code envelope around it.

use std::path::Path;

use agent_hooks::claude_code::{
    SessionStartHookSpecificOutput, SessionStartInput, SessionStartOutput,
};

use crate::hooks::core::SessionStartEnvelope;

const HOOK_EVENT_NAME: &str = "SessionStart";

/// Claude Code's SessionStart adapter for the engine-agnostic summary
/// hook.
pub struct ClaudeCodeSessionStart;

impl SessionStartEnvelope for ClaudeCodeSessionStart {
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

/// Claude Code SessionStart handler that emits a hotspot + coupling summary.
pub type SummaryHook = crate::hooks::core::SummaryHook<ClaudeCodeSessionStart>;

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks::claude_code::SessionStartSource;
    use rstest::rstest;

    use crate::hooks::core::runner::session_start_conformance as conformance;
    use crate::test_support::claude_hook_context;

    fn input(cwd: &Path) -> SessionStartInput {
        SessionStartInput {
            context: claude_hook_context(cwd),
            source: SessionStartSource::Startup,
        }
    }

    /// What every SessionStart envelope owes the runner. Bodies live in
    /// [`conformance`] so the Codex envelope is held to the same ones.
    #[rstest]
    #[case::silent_without_repo_or_crate(
        conformance::stays_silent_without_repo_or_crate::<ClaudeCodeSessionStart, fn(&Path) -> SessionStartInput>
    )]
    #[case::injects_summary(
        conformance::injects_summary_via_additional_context::<ClaudeCodeSessionStart, fn(&Path) -> SessionStartInput>
    )]
    fn envelope_contract(#[case] assertion: fn(fn(&Path) -> SessionStartInput)) {
        assertion(input);
    }
}
