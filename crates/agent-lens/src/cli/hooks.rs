//! The stdin-driven `hook` / `codex-hook` handlers. The commands that
//! wire them into an agent's config live in [`super::setup`].
//!
//! # Failure mode
//!
//! Hook handlers are advisory: whatever `agent-lens` has to say about the
//! file being edited is context, never a gate on the agent's tool call.
//! So a handler that fails still answers in the agent's own response
//! schema — the failure is reported through the same `systemMessage` /
//! `additionalContext` field a report would have used, prefixed with
//! `agent-lens ... hook failed:`, and the process exits 0 so the agent
//! parses it. The full error also goes to stderr through `tracing`.
//!
//! This covers everything from a malformed stdin payload to an analyzer
//! panicking on a pathological file. The alternative — exiting non-zero
//! with an empty stdout — leaves the agent with no structured response at
//! all, and (on Claude Code) a non-zero exit is a louder signal than an
//! advisory hook has any business sending.
//!
//! The `setup` commands are not hooks and keep the ordinary CLI
//! contract: an error propagates and the process exits non-zero.

use std::io::{self, Read};

use agent_hooks::Hook;
use agent_hooks::claude_code::ClaudeCodeHookInput;
use agent_hooks::codex::CodexHookInput;
use agent_lens::hooks::codex::post_tool_use::{
    CodexPostToolUse, FootprintHook as CodexFootprintHook, SimilarityHook as CodexSimilarityHook,
    WrapperHook as CodexWrapperHook,
};
use agent_lens::hooks::codex::pre_tool_use::{
    CodexPreToolUse, CohesionHook as CodexPreCohesionHook, ComplexityHook as CodexPreComplexityHook,
};
use agent_lens::hooks::codex::session_start::{
    CodexSessionStart, SnapshotHook as CodexSnapshotHook,
    SummaryHook as CodexSessionStartSummaryHook,
};
use agent_lens::hooks::codex::stop::CodexStop;
use agent_lens::hooks::core::{DeltaHook, HookEnvelope, SessionStartEnvelope, StopEnvelope};
use agent_lens::hooks::post_tool_use::{
    ClaudeCodePostToolUse, FootprintHook, SimilarityHook, WrapperHook,
};
use agent_lens::hooks::pre_tool_use::{ClaudeCodePreToolUse, CohesionHook, ComplexityHook};
use agent_lens::hooks::session_start::{
    ClaudeCodeSessionStart, SnapshotHook, SummaryHook as SessionStartSummaryHook,
};
use agent_lens::hooks::stop::{ClaudeCodeStop, ClaudeCodeSubagentStop};
use tracing::error;

use super::args::{
    CodexPostToolUseCommand, CodexPreToolUseCommand, CodexSessionStartCommand, PostToolUseCommand,
    PreToolUseCommand, SessionStartCommand, StopCommand,
};
use super::write_stdout_json;

pub(super) fn run_session_start(
    cmd: SessionStartCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    run_hook(
        "session-start",
        ClaudeCodeSessionStart::wrap_summary,
        || {
            let input = expect_event("SessionStart", |payload| match payload {
                ClaudeCodeHookInput::SessionStart(input) => Some(input),
                _ => None,
            })?;
            match cmd {
                SessionStartCommand::Summary => Ok(SessionStartSummaryHook::new().handle(input)?),
                SessionStartCommand::Snapshot => Ok(SnapshotHook::new().handle(input)?),
            }
        },
    )
}

pub(super) fn run_pre_tool_use(cmd: PreToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    run_hook("pre-tool-use", ClaudeCodePreToolUse::wrap_report, || {
        let input = expect_event("PreToolUse", |payload| match payload {
            ClaudeCodeHookInput::PreToolUse(input) => Some(input),
            _ => None,
        })?;
        Ok(match cmd {
            PreToolUseCommand::Complexity => ComplexityHook::new().handle(input)?,
            PreToolUseCommand::Cohesion => CohesionHook::new().handle(input)?,
        })
    })
}

pub(super) fn run_post_tool_use(cmd: PostToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    run_hook("post-tool-use", ClaudeCodePostToolUse::wrap_report, || {
        let input = expect_event("PostToolUse", |payload| match payload {
            ClaudeCodeHookInput::PostToolUse(input) => Some(input),
            _ => None,
        })?;
        Ok(match cmd {
            PostToolUseCommand::Similarity => SimilarityHook::new().handle(input)?,
            PostToolUseCommand::Wrapper => WrapperHook::new().handle(input)?,
            PostToolUseCommand::Footprint => FootprintHook::new().handle(input)?,
        })
    })
}

pub(super) fn run_stop(cmd: StopCommand) -> Result<(), Box<dyn std::error::Error>> {
    run_delta::<ClaudeCodeStop, _>("stop", "Stop", cmd, |payload| match payload {
        ClaudeCodeHookInput::Stop(input) => Some(input),
        _ => None,
    })
}

pub(super) fn run_subagent_stop(cmd: StopCommand) -> Result<(), Box<dyn std::error::Error>> {
    run_delta::<ClaudeCodeSubagentStop, _>("subagent-stop", "SubagentStop", cmd, |payload| {
        match payload {
            ClaudeCodeHookInput::SubagentStop(input) => Some(input),
            _ => None,
        }
    })
}

pub(super) fn run_codex_stop(cmd: StopCommand) -> Result<(), Box<dyn std::error::Error>> {
    run_delta::<CodexStop, _>("codex stop", "Codex Stop", cmd, |payload| match payload {
        CodexHookInput::Stop(input) => Some(input),
        _ => None,
    })
}

/// The one shape every stop handler shares: narrow the payload, run the
/// checkpoint, and report a failure as a non-blocking message.
fn run_delta<E: StopEnvelope, I: serde::de::DeserializeOwned>(
    label: &str,
    event: &str,
    cmd: StopCommand,
    narrow: impl FnOnce(I) -> Option<E::Input>,
) -> Result<(), Box<dyn std::error::Error>> {
    let StopCommand::Delta(args) = cmd;
    run_hook(
        label,
        |msg| E::wrap_delta(msg, false),
        || {
            let input = expect_event(event, narrow)?;
            Ok(DeltaHook::<E>::new()
                .with_block(!args.no_block)
                .handle(input)?)
        },
    )
}

pub(super) fn run_codex_session_start(
    cmd: CodexSessionStartCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    run_hook(
        "codex session-start",
        CodexSessionStart::wrap_summary,
        || {
            let input = expect_event("Codex SessionStart", |payload| match payload {
                CodexHookInput::SessionStart(input) => Some(input),
                _ => None,
            })?;
            match cmd {
                CodexSessionStartCommand::Summary => {
                    Ok(CodexSessionStartSummaryHook::new().handle(input)?)
                }
                CodexSessionStartCommand::Snapshot => Ok(CodexSnapshotHook::new().handle(input)?),
            }
        },
    )
}

pub(super) fn run_codex_pre_tool_use(
    cmd: CodexPreToolUseCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    run_hook("codex pre-tool-use", CodexPreToolUse::wrap_report, || {
        let input = expect_event("Codex PreToolUse", |payload| match payload {
            CodexHookInput::PreToolUse(input) => Some(input),
            _ => None,
        })?;
        Ok(match cmd {
            CodexPreToolUseCommand::Complexity => CodexPreComplexityHook::new().handle(input)?,
            CodexPreToolUseCommand::Cohesion => CodexPreCohesionHook::new().handle(input)?,
        })
    })
}

pub(super) fn run_codex_post_tool_use(
    cmd: CodexPostToolUseCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    run_hook("codex post-tool-use", CodexPostToolUse::wrap_report, || {
        let input = expect_event("Codex PostToolUse", |payload| match payload {
            CodexHookInput::PostToolUse(input) => Some(input),
            _ => None,
        })?;
        Ok(match cmd {
            CodexPostToolUseCommand::Similarity => CodexSimilarityHook::new().handle(input)?,
            CodexPostToolUseCommand::Wrapper => CodexWrapperHook::new().handle(input)?,
            CodexPostToolUseCommand::Footprint => CodexFootprintHook::new().handle(input)?,
        })
    })
}

/// Run one hook handler and write its response to stdout.
///
/// A failure anywhere in `run` — bad stdin, wrong event, a handler
/// blowing up — is logged to stderr and then handed to `report_failure`,
/// the envelope's own report constructor, so the agent still receives a
/// well-formed response. See the module docs for why the error travels
/// in-band.
fn run_hook<O: serde::Serialize>(
    event: &str,
    report_failure: impl FnOnce(String) -> O,
    run: impl FnOnce() -> Result<O, Box<dyn std::error::Error>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let output = run().unwrap_or_else(|err| report_failure(failure_report(event, &*err)));
    write_stdout_json(&output)
}

/// Log a hook failure to stderr and render the line the agent sees in
/// place of a report.
fn failure_report(event: &str, err: &dyn std::error::Error) -> String {
    error!(event, error = %err, "hook failed; reporting it in the hook response");
    format!("agent-lens {event} hook failed: {err}")
}

/// Read a hook payload from stdin and narrow it to the event this
/// subcommand handles.
fn expect_event<I: serde::de::DeserializeOwned, T>(
    event: &str,
    narrow: impl FnOnce(I) -> Option<T>,
) -> Result<T, Box<dyn std::error::Error>> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    narrow(serde_json::from_str(&buf)?)
        .ok_or_else(|| format!("expected a {event} hook payload on stdin").into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks::claude_code::PostToolUseOutput;

    /// The end-to-end path is covered by the CLI smoke tests; these pin
    /// the two halves `run_hook` composes — the message, and the envelope
    /// field each engine carries it in.
    #[test]
    fn a_failure_is_reported_in_the_claude_envelope() {
        let out = ClaudeCodePostToolUse::wrap_report(failure_report(
            "post-tool-use",
            &io::Error::other("stdin was not JSON"),
        ));
        assert_ne!(
            out,
            PostToolUseOutput::default(),
            "an empty response would leave the agent with no signal",
        );
        let msg = out
            .common
            .system_message
            .expect("a failure must still produce a report");
        assert!(msg.contains("post-tool-use hook failed"), "got {msg}");
        assert!(msg.contains("stdin was not JSON"), "got {msg}");
    }

    #[test]
    fn a_failure_is_reported_in_the_codex_envelope() {
        // Codex injects context through `additionalContext`, not
        // `systemMessage`; the failure has to follow the same route.
        let out = CodexPostToolUse::wrap_report(failure_report(
            "codex post-tool-use",
            &io::Error::other("boom"),
        ));
        let msg = out
            .hook_specific_output
            .and_then(|extra| extra.additional_context)
            .expect("a failure must still produce a report");
        assert!(msg.contains("codex post-tool-use hook failed"), "got {msg}");
        assert!(msg.contains("boom"), "got {msg}");
    }

    #[test]
    fn a_failure_is_reported_in_the_session_start_envelope() {
        let out = ClaudeCodeSessionStart::wrap_summary(failure_report(
            "session-start",
            &io::Error::other("no git tree"),
        ));
        let msg = out
            .hook_specific_output
            .and_then(|extra| extra.additional_context)
            .expect("a failure must still produce a summary");
        assert!(msg.contains("session-start hook failed"), "got {msg}");
        assert!(msg.contains("no git tree"), "got {msg}");
    }
}
