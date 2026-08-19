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
    use agent_hooks::codex::SessionStartSource;
    use rstest::rstest;

    use crate::hooks::core::runner::session_start_conformance as conformance;
    use crate::test_support::codex_hook_context;

    fn input(cwd: &Path) -> SessionStartInput {
        SessionStartInput {
            context: codex_hook_context(cwd),
            source: SessionStartSource::Startup,
        }
    }

    /// What every SessionStart envelope owes the runner. Bodies live in
    /// [`conformance`] so the Claude Code envelope is held to the same
    /// ones.
    #[rstest]
    #[case::silent_without_repo_or_crate(
        conformance::stays_silent_without_repo_or_crate::<CodexSessionStart, fn(&Path) -> SessionStartInput>
    )]
    #[case::injects_summary(
        conformance::injects_summary_via_additional_context::<CodexSessionStart, fn(&Path) -> SessionStartInput>
    )]
    fn envelope_contract(#[case] assertion: fn(fn(&Path) -> SessionStartInput)) {
        assertion(input);
    }
}
