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
mod session_start;
mod stop;
mod subagent_stop;
mod user_prompt_submit;

pub use context::{CommonHookOutput, HookContext, PermissionMode};
pub use post_tool_use::{PostToolUseInput, PostToolUseOutput};
pub use pre_tool_use::{
    PermissionDecision, PreToolUseDecision, PreToolUseHookSpecificOutput, PreToolUseInput,
    PreToolUseOutput,
};
pub use session_start::{
    SessionStartHookSpecificOutput, SessionStartInput, SessionStartOutput, SessionStartSource,
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
    SessionStart(SessionStartInput),
    PreToolUse(PreToolUseInput),
    PostToolUse(PostToolUseInput),
    UserPromptSubmit(UserPromptSubmitInput),
    Stop(StopInput),
    SubagentStop(SubagentStopInput),
}

#[cfg(test)]
mod dispatch_tests {
    use super::ClaudeCodeHookInput;
    use serde_json::json;

    fn ctx() -> serde_json::Value {
        json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
        })
    }

    #[test]
    fn dispatches_session_start_variant() {
        let mut payload = ctx();
        let obj = payload.as_object_mut().unwrap();
        obj.insert("hook_event_name".into(), json!("SessionStart"));
        obj.insert("source".into(), json!("startup"));
        let parsed: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, ClaudeCodeHookInput::SessionStart(_)));
    }

    #[test]
    fn dispatches_pre_tool_use_variant() {
        let mut payload = ctx();
        let obj = payload.as_object_mut().unwrap();
        obj.insert("hook_event_name".into(), json!("PreToolUse"));
        obj.insert("tool_name".into(), json!("Bash"));
        obj.insert("tool_input".into(), json!({}));
        let parsed: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, ClaudeCodeHookInput::PreToolUse(_)));
    }

    #[test]
    fn dispatches_post_tool_use_variant() {
        let mut payload = ctx();
        let obj = payload.as_object_mut().unwrap();
        obj.insert("hook_event_name".into(), json!("PostToolUse"));
        obj.insert("tool_name".into(), json!("Write"));
        obj.insert("tool_input".into(), json!({}));
        obj.insert("tool_response".into(), json!({}));
        let parsed: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, ClaudeCodeHookInput::PostToolUse(_)));
    }

    #[test]
    fn dispatches_user_prompt_submit_variant() {
        let mut payload = ctx();
        let obj = payload.as_object_mut().unwrap();
        obj.insert("hook_event_name".into(), json!("UserPromptSubmit"));
        obj.insert("prompt".into(), json!("hi"));
        let parsed: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, ClaudeCodeHookInput::UserPromptSubmit(_)));
    }

    #[test]
    fn dispatches_stop_variant() {
        let mut payload = ctx();
        let obj = payload.as_object_mut().unwrap();
        obj.insert("hook_event_name".into(), json!("Stop"));
        let parsed: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, ClaudeCodeHookInput::Stop(_)));
    }

    #[test]
    fn dispatches_subagent_stop_variant() {
        let mut payload = ctx();
        let obj = payload.as_object_mut().unwrap();
        obj.insert("hook_event_name".into(), json!("SubagentStop"));
        let parsed: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, ClaudeCodeHookInput::SubagentStop(_)));
    }

    #[test]
    fn missing_hook_event_name_is_rejected() {
        let payload = ctx();
        let err = serde_json::from_value::<ClaudeCodeHookInput>(payload).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("hook_event_name"),
            "expected discriminator complaint, got {msg}",
        );
    }

    #[test]
    fn unknown_hook_event_name_is_rejected() {
        let mut payload = ctx();
        payload
            .as_object_mut()
            .unwrap()
            .insert("hook_event_name".into(), json!("Telepathy"));
        let err = serde_json::from_value::<ClaudeCodeHookInput>(payload).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Telepathy") || msg.contains("variant"),
            "expected unknown-variant complaint, got {msg}",
        );
    }

    #[test]
    fn missing_required_session_id_is_rejected() {
        let payload = json!({
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "Stop",
        });
        let err = serde_json::from_value::<ClaudeCodeHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains("session_id"), "{err}");
    }
}
