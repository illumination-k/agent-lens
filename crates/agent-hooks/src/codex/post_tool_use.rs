use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::context::{CommonHookOutput, HookContext};

/// Input payload for the `PostToolUse` hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PostToolUseInput {
    #[serde(flatten)]
    pub context: HookContext,
    pub turn_id: String,
    pub tool_name: String,
    pub tool_use_id: String,
    /// Tool-specific input. For `Bash` and `apply_patch` this is
    /// `{ "command": "..." }`; MCP tools forward all of their args.
    pub tool_input: Value,
    /// Tool-specific output. For MCP tools this is the MCP call result.
    pub tool_response: Value,
}

/// Output payload for the `PostToolUse` hook.
///
/// `decision: "block"` does *not* undo the completed tool call. Codex
/// records the feedback, replaces the tool result with `reason`, and
/// continues from the hook-provided message.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PostToolUseOutput {
    #[serde(flatten)]
    pub common: CommonHookOutput,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<PostToolUseDecision>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    #[serde(
        rename = "hookSpecificOutput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub hook_specific_output: Option<PostToolUseHookSpecificOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PostToolUseDecision {
    Block,
}

/// Extra developer context appended to the conversation after the tool
/// call returns.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PostToolUseHookSpecificOutput {
    /// Must be the string `"PostToolUse"`.
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,

    #[serde(
        rename = "additionalContext",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub additional_context: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::CodexHookInput;
    use serde_json::json;

    #[test]
    fn deserializes_post_tool_use_input() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "PostToolUse",
            "turn_id": "turn-1",
            "tool_name": "apply_patch",
            "tool_use_id": "call-1",
            "tool_input": {"command": "*** Begin Patch\n*** End Patch"},
            "tool_response": {"success": true}
        });
        let input: CodexHookInput = serde_json::from_value(payload).unwrap();
        let CodexHookInput::PostToolUse(input) = input else {
            panic!("expected PostToolUse variant");
        };
        assert_eq!(input.tool_name, "apply_patch");
        assert_eq!(input.tool_response, json!({"success": true}));
    }

    #[test]
    fn serializes_additional_context_output() {
        let output = PostToolUseOutput {
            hook_specific_output: Some(PostToolUseHookSpecificOutput {
                hook_event_name: "PostToolUse".to_owned(),
                additional_context: Some("generated files updated".to_owned()),
            }),
            ..Default::default()
        };
        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(
            v,
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "additionalContext": "generated files updated"
                }
            })
        );
    }

    fn full_payload() -> serde_json::Value {
        json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "PostToolUse",
            "turn_id": "turn-1",
            "tool_name": "apply_patch",
            "tool_use_id": "call-1",
            "tool_input": {"command": "*** Begin Patch\n*** End Patch"},
            "tool_response": {"success": true}
        })
    }

    #[test]
    fn missing_turn_id_is_rejected() {
        let mut payload = full_payload();
        payload.as_object_mut().unwrap().remove("turn_id");
        let err = serde_json::from_value::<CodexHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains("turn_id"), "{err}");
    }

    #[test]
    fn missing_tool_use_id_is_rejected() {
        let mut payload = full_payload();
        payload.as_object_mut().unwrap().remove("tool_use_id");
        let err = serde_json::from_value::<CodexHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains("tool_use_id"), "{err}");
    }

    #[test]
    fn missing_tool_response_is_rejected() {
        let mut payload = full_payload();
        payload.as_object_mut().unwrap().remove("tool_response");
        let err = serde_json::from_value::<CodexHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains("tool_response"), "{err}");
    }

    #[test]
    fn tolerates_unknown_fields() {
        let mut payload = full_payload();
        payload
            .as_object_mut()
            .unwrap()
            .insert("future_field".into(), json!("ignored"));
        serde_json::from_value::<CodexHookInput>(payload).unwrap();
    }

    #[test]
    fn block_decision_round_trip() {
        let output = PostToolUseOutput {
            decision: Some(PostToolUseDecision::Block),
            reason: Some("override".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(v, json!({"decision": "block", "reason": "override"}));
    }
}
