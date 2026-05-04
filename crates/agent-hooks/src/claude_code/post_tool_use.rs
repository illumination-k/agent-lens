use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::context::{CommonHookOutput, HookContext};

/// Input payload for the `PostToolUse` hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PostToolUseInput {
    #[serde(flatten)]
    pub context: HookContext,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_response: Value,
}

/// Output payload for the `PostToolUse` hook.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PostToolUseOutput {
    #[serde(flatten)]
    pub common: CommonHookOutput,

    /// `"block"` feeds the `reason` back to the model; other values are ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<PostToolUseDecision>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PostToolUseDecision {
    Block,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::ClaudeCodeHookInput;
    use rstest::rstest;
    use serde_json::json;

    fn full_payload() -> serde_json::Value {
        json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "a.rs"},
            "tool_response": {"success": true}
        })
    }

    #[test]
    fn deserializes_post_tool_use_input() {
        let input: ClaudeCodeHookInput = serde_json::from_value(full_payload()).unwrap();
        let ClaudeCodeHookInput::PostToolUse(input) = input else {
            panic!("expected PostToolUse variant");
        };
        assert_eq!(input.tool_name, "Write");
        assert_eq!(input.tool_response, json!({"success": true}));
    }

    #[rstest]
    #[case("tool_name")]
    #[case("tool_response")]
    fn rejects_missing_required_field(#[case] field: &str) {
        let mut payload = full_payload();
        payload.as_object_mut().unwrap().remove(field);
        let err = serde_json::from_value::<ClaudeCodeHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains(field), "{err}");
    }

    #[test]
    fn block_decision_round_trip() {
        let output = PostToolUseOutput {
            decision: Some(PostToolUseDecision::Block),
            reason: Some("nope".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(v, json!({"decision": "block", "reason": "nope"}));
        let parsed: PostToolUseOutput = serde_json::from_value(v).unwrap();
        assert_eq!(parsed, output);
    }

    #[test]
    fn default_output_omits_optional_fields() {
        let v = serde_json::to_value(PostToolUseOutput::default()).unwrap();
        assert_eq!(v, json!({}));
    }
}
