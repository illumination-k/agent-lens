//! Claude Code `Stop` and `SubagentStop` hook handlers.
//!
//! Both run the session checkpoint
//! ([`crate::hooks::core::checkpoint`]): at the point the agent (or a
//! sub-agent) is about to hand control back, report what got worse
//! since the session started. Claude Code shows `systemMessage` to the
//! user only; the model reads a stop hook's output only as the `reason`
//! of a `decision: "block"`, which also keeps it working. So a new
//! regression blocks once, and a repeat — or a stop that is already the
//! forced continuation — is a message.

use std::path::Path;

use agent_hooks::claude_code::{
    CommonHookOutput, StopInput, StopOutput, SubagentStopInput, SubagentStopOutput,
};

use crate::hooks::core::StopEnvelope;

/// Claude Code's `Stop` adapter for the checkpoint.
pub struct ClaudeCodeStop;

impl StopEnvelope for ClaudeCodeStop {
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
                decision: Some(agent_hooks::claude_code::StopDecision::Block),
                reason: Some(report),
                ..StopOutput::default()
            }
        } else {
            StopOutput {
                common: message(report),
                ..StopOutput::default()
            }
        }
    }
}

/// Claude Code's `SubagentStop` adapter for the checkpoint.
pub struct ClaudeCodeSubagentStop;

impl StopEnvelope for ClaudeCodeSubagentStop {
    type Input = SubagentStopInput;
    type Output = SubagentStopOutput;

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
            SubagentStopOutput {
                decision: Some(agent_hooks::claude_code::SubagentStopDecision::Block),
                reason: Some(report),
                ..SubagentStopOutput::default()
            }
        } else {
            SubagentStopOutput {
                common: message(report),
                ..SubagentStopOutput::default()
            }
        }
    }
}

fn message(report: String) -> CommonHookOutput {
    CommonHookOutput {
        system_message: Some(report),
        ..CommonHookOutput::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    use crate::hooks::core::runner::stop_conformance as conformance;
    use crate::test_support::claude_hook_context;

    fn stop(cwd: &Path, session: &str, active: bool) -> StopInput {
        let mut context = claude_hook_context(cwd);
        context.session_id = session.to_owned();
        StopInput {
            context,
            stop_hook_active: active,
        }
    }

    fn subagent_stop(cwd: &Path, session: &str, active: bool) -> SubagentStopInput {
        let mut context = claude_hook_context(cwd);
        context.session_id = session.to_owned();
        SubagentStopInput {
            context,
            stop_hook_active: active,
        }
    }

    type Input<I> = fn(&Path, &str, bool) -> I;

    #[rstest]
    #[case::blocks_once(conformance::blocks_once_then_messages::<ClaudeCodeStop, Input<StopInput>>)]
    #[case::never_blocks_a_continuation(
        conformance::never_blocks_a_continuation_or_when_told_not_to::<ClaudeCodeStop, Input<StopInput>>
    )]
    fn stop_contract(#[case] assertion: fn(Input<StopInput>)) {
        assertion(stop);
    }

    #[rstest]
    #[case::blocks_once(
        conformance::blocks_once_then_messages::<ClaudeCodeSubagentStop, Input<SubagentStopInput>>
    )]
    #[case::never_blocks_a_continuation(
        conformance::never_blocks_a_continuation_or_when_told_not_to::<ClaudeCodeSubagentStop, Input<SubagentStopInput>>
    )]
    fn subagent_stop_contract(#[case] assertion: fn(Input<SubagentStopInput>)) {
        assertion(subagent_stop);
    }
}
