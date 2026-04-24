//! Schema for Claude Code's hook protocol.
//!
//! Reference: <https://docs.claude.com/en/docs/claude-code/hooks>.
//!
//! Every hook receives a JSON payload on stdin that shares a common envelope
//! (see [`HookContext`]) and carries hook-specific fields. The discriminator
//! `hook_event_name` is used by [`ClaudeCodeHookInput`] to dispatch to the
//! right variant. Unknown fields are accepted silently to tolerate upstream
//! additions; missing required fields fail deserialization.

mod context;
mod post_tool_use;
mod pre_tool_use;
mod stop;
mod subagent_stop;
mod user_prompt_submit;

pub use context::{CommonHookOutput, HookContext, PermissionMode};
pub use post_tool_use::{PostToolUseInput, PostToolUseOutput};
pub use pre_tool_use::{
    PermissionDecision, PreToolUseDecision, PreToolUseHookSpecificOutput, PreToolUseInput,
    PreToolUseOutput,
};
pub use stop::{StopInput, StopOutput};
pub use subagent_stop::{SubagentStopInput, SubagentStopOutput};
pub use user_prompt_submit::{
    UserPromptSubmitHookSpecificOutput, UserPromptSubmitInput, UserPromptSubmitOutput,
};

use serde::{Deserialize, Serialize};

/// A tagged union over every Claude Code hook input.
///
/// Internally tagged by the `hook_event_name` field. Useful when a single
/// binary needs to accept any hook event and dispatch at runtime.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "hook_event_name")]
pub enum ClaudeCodeHookInput {
    PreToolUse(PreToolUseInput),
    PostToolUse(PostToolUseInput),
    UserPromptSubmit(UserPromptSubmitInput),
    Stop(StopInput),
    SubagentStop(SubagentStopInput),
}
