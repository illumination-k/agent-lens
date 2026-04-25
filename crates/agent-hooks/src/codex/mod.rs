//! Schema for Codex's hook protocol.
//!
//! Reference: <https://developers.openai.com/codex/hooks>.
//!
//! Every hook receives a JSON payload on stdin that shares a common envelope
//! (see [`HookContext`]) and carries hook-specific fields. The discriminator
//! `hook_event_name` is used by [`CodexHookInput`] to dispatch to the right
//! variant. Unknown fields are accepted silently to tolerate upstream
//! additions; missing required fields fail deserialization.
//!
//! Codex's protocol differs from Claude Code's in a few ways worth noting:
//!
//! * Every payload carries the active `model` slug.
//! * `transcript_path` is nullable.
//! * Turn-scoped events also carry a `turn_id`.
//! * `PostToolUse` can append developer context via `additionalContext`.
//! * `PermissionRequest` is a dedicated event that lets a hook approve or
//!   deny a tool invocation before the normal approval prompt is shown.
//! * There is a `SessionStart` event (no equivalent on Claude Code).
//! * There is no `SubagentStop` event.

mod context;
mod permission_request;
mod post_tool_use;
mod pre_tool_use;
mod session_start;
mod stop;
mod user_prompt_submit;

pub use context::{CommonHookOutput, HookContext};
pub use permission_request::{
    PermissionDecision, PermissionRequestHookSpecificOutput, PermissionRequestInput,
    PermissionRequestOutput,
};
pub use post_tool_use::{
    PostToolUseDecision, PostToolUseHookSpecificOutput, PostToolUseInput, PostToolUseOutput,
};
pub use pre_tool_use::{
    PreToolUseDecision, PreToolUseHookSpecificOutput, PreToolUseInput, PreToolUseOutput,
    PreToolUsePermissionDecision,
};
pub use session_start::{
    SessionStartHookSpecificOutput, SessionStartInput, SessionStartOutput, SessionStartSource,
};
pub use stop::{StopDecision, StopInput, StopOutput};
pub use user_prompt_submit::{
    UserPromptSubmitDecision, UserPromptSubmitHookSpecificOutput, UserPromptSubmitInput,
    UserPromptSubmitOutput,
};

use serde::{Deserialize, Serialize};

/// A tagged union over every Codex hook input.
///
/// Internally tagged by the `hook_event_name` field. Useful when a single
/// binary needs to accept any hook event and dispatch at runtime.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "hook_event_name")]
pub enum CodexHookInput {
    SessionStart(SessionStartInput),
    PreToolUse(PreToolUseInput),
    PermissionRequest(PermissionRequestInput),
    PostToolUse(PostToolUseInput),
    UserPromptSubmit(UserPromptSubmitInput),
    Stop(StopInput),
}
