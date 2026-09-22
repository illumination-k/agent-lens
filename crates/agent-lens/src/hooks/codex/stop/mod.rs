//! Codex `Stop` hook handler.
//!
//! Runs the session checkpoint ([`crate::hooks::core::checkpoint`]).
//! Codex reads `decision: "block"` as "continue, with `reason` as the
//! next prompt", so a new regression is handed back that way once; a
//! repeat, or a stop that is already a forced continuation, becomes a
//! `systemMessage` warning instead.

use std::path::Path;

use agent_hooks::codex::{CommonHookOutput, StopDecision, StopInput, StopOutput};

use crate::hooks::core::StopEnvelope;

/// Codex's `Stop` adapter for the checkpoint.
pub struct CodexStop;

impl StopEnvelope for CodexStop {
    type Input = StopInput;
    type Output = StopOutput;

    fn cwd(input: &Self::Input) -> &Path {
        &input.context.cwd
    }

    fn session_id(input: &Self::Input) -> &str {
        &input.context.session_id
    }

    fn stop_hook_active(input: &Self::Input) -> bool {
        input.stop_hook_active
    }

    fn wrap_delta(report: String, block: bool) -> Self::Output {
        if block {
            StopOutput {
                decision: Some(StopDecision::Block),
                reason: Some(report),
                ..StopOutput::default()
            }
        } else {
            StopOutput {
                common: CommonHookOutput {
                    system_message: Some(report),
                    ..CommonHookOutput::default()
                },
                ..StopOutput::default()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    use crate::hooks::core::runner::stop_conformance as conformance;
    use crate::test_support::codex_hook_context;

    fn stop(cwd: &Path, session: &str, active: bool) -> StopInput {
        let mut context = codex_hook_context(cwd);
        context.session_id = session.to_owned();
        StopInput {
            context,
            turn_id: "turn-1".to_owned(),
            stop_hook_active: active,
            last_assistant_message: None,
        }
    }

    type Input = fn(&Path, &str, bool) -> StopInput;

    #[rstest]
    #[case::blocks_once(conformance::blocks_once_then_messages::<CodexStop, Input>)]
    #[case::never_blocks_a_continuation(
        conformance::never_blocks_a_continuation_or_when_told_not_to::<CodexStop, Input>
    )]
    fn stop_contract(#[case] assertion: fn(Input)) {
        assertion(stop);
    }
}
