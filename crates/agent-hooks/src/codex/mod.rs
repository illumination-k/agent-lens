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

#[cfg(test)]
mod dispatch_tests {
    use super::CodexHookInput;
    use rstest::rstest;
    use serde_json::{Value, json};

    fn ctx() -> serde_json::Value {
        json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "model": "gpt-5",
        })
    }

    fn turn_ctx() -> serde_json::Value {
        let mut v = ctx();
        v.as_object_mut()
            .unwrap()
            .insert("turn_id".into(), json!("turn-1"));
        v
    }

    #[test]
    fn dispatches_session_start_variant() {
        let mut payload = ctx();
        let obj = payload.as_object_mut().unwrap();
        obj.insert("hook_event_name".into(), json!("SessionStart"));
        obj.insert("source".into(), json!("startup"));
        let parsed: CodexHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, CodexHookInput::SessionStart(_)));
    }

    #[test]
    fn dispatches_pre_tool_use_variant() {
        let mut payload = turn_ctx();
        let obj = payload.as_object_mut().unwrap();
        obj.insert("hook_event_name".into(), json!("PreToolUse"));
        obj.insert("tool_name".into(), json!("Bash"));
        obj.insert("tool_use_id".into(), json!("call-1"));
        obj.insert("tool_input".into(), json!({}));
        let parsed: CodexHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, CodexHookInput::PreToolUse(_)));
    }

    #[test]
    fn dispatches_permission_request_variant() {
        let mut payload = turn_ctx();
        let obj = payload.as_object_mut().unwrap();
        obj.insert("hook_event_name".into(), json!("PermissionRequest"));
        obj.insert("tool_name".into(), json!("Bash"));
        obj.insert("tool_input".into(), json!({}));
        let parsed: CodexHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, CodexHookInput::PermissionRequest(_)));
    }

    #[test]
    fn dispatches_post_tool_use_variant() {
        let mut payload = turn_ctx();
        let obj = payload.as_object_mut().unwrap();
        obj.insert("hook_event_name".into(), json!("PostToolUse"));
        obj.insert("tool_name".into(), json!("apply_patch"));
        obj.insert("tool_use_id".into(), json!("call-1"));
        obj.insert("tool_input".into(), json!({}));
        obj.insert("tool_response".into(), json!({}));
        let parsed: CodexHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, CodexHookInput::PostToolUse(_)));
    }

    #[test]
    fn dispatches_user_prompt_submit_variant() {
        let mut payload = turn_ctx();
        let obj = payload.as_object_mut().unwrap();
        obj.insert("hook_event_name".into(), json!("UserPromptSubmit"));
        obj.insert("prompt".into(), json!("hi"));
        let parsed: CodexHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, CodexHookInput::UserPromptSubmit(_)));
    }

    #[test]
    fn dispatches_stop_variant() {
        let mut payload = turn_ctx();
        payload
            .as_object_mut()
            .unwrap()
            .insert("hook_event_name".into(), json!("Stop"));
        let parsed: CodexHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, CodexHookInput::Stop(_)));
    }

    #[test]
    fn missing_model_is_rejected() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "Stop",
            "turn_id": "turn-1",
        });
        let err = serde_json::from_value::<CodexHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains("model"), "{err}");
    }

    #[test]
    fn unknown_hook_event_name_is_rejected() {
        let mut payload = ctx();
        payload
            .as_object_mut()
            .unwrap()
            .insert("hook_event_name".into(), json!("Telepathy"));
        let err = serde_json::from_value::<CodexHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains("variant"), "{err}");
    }

    #[rstest]
    #[case::permission_request(json!({
        "session_id": "sess",
        "transcript_path": null,
        "cwd": "/repo",
        "model": "gpt-5",
        "hook_event_name": "PermissionRequest",
        "turn_id": "turn-1",
        "tool_name": "Bash",
        "tool_input": {},
        "future_field": [],
    }))]
    #[case::pre_tool_use(json!({
        "session_id": "sess",
        "transcript_path": null,
        "cwd": "/repo",
        "model": "gpt-5",
        "hook_event_name": "PreToolUse",
        "turn_id": "turn-1",
        "tool_name": "Bash",
        "tool_use_id": "call-1",
        "tool_input": {},
        "future_field": 42,
    }))]
    #[case::post_tool_use(json!({
        "session_id": "sess",
        "transcript_path": "/tmp/t.jsonl",
        "cwd": "/repo",
        "model": "gpt-5",
        "hook_event_name": "PostToolUse",
        "turn_id": "turn-1",
        "tool_name": "apply_patch",
        "tool_use_id": "call-1",
        "tool_input": {"command": "*** Begin Patch\n*** End Patch"},
        "tool_response": {"success": true},
        "future_field": "ignored",
    }))]
    #[case::session_start(json!({
        "session_id": "sess",
        "transcript_path": null,
        "cwd": "/repo",
        "model": "gpt-5",
        "hook_event_name": "SessionStart",
        "source": "resume",
        "future_field": {"a": 1},
    }))]
    #[case::stop(json!({
        "session_id": "sess",
        "transcript_path": null,
        "cwd": "/repo",
        "model": "gpt-5",
        "hook_event_name": "Stop",
        "turn_id": "turn-1",
        "future_field": [1, 2],
    }))]
    #[case::user_prompt_submit(json!({
        "session_id": "sess",
        "transcript_path": null,
        "cwd": "/repo",
        "model": "gpt-5",
        "hook_event_name": "UserPromptSubmit",
        "turn_id": "turn-1",
        "prompt": "hi",
        "future_field": "ignored",
    }))]
    fn tolerates_unknown_fields(#[case] payload: Value) {
        serde_json::from_value::<CodexHookInput>(payload).unwrap();
    }

    #[test]
    fn null_transcript_path_is_accepted() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "Stop",
            "turn_id": "turn-1",
        });
        let parsed: CodexHookInput = serde_json::from_value(payload).unwrap();
        let CodexHookInput::Stop(stop) = parsed else {
            panic!("expected Stop variant");
        };
        assert!(stop.context.transcript_path.is_none());
    }
}
