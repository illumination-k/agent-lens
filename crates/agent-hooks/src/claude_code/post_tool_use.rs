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
    use crate::claude_code::ClaudeCodeHookInput;
    use serde_json::json;

    #[test]
    fn deserializes_post_tool_use_input() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": "a.rs"},
            "tool_response": {"success": true}
        });
        let input: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        let ClaudeCodeHookInput::PostToolUse(input) = input else {
            panic!("expected PostToolUse variant");
        };
        assert_eq!(input.tool_name, "Write");
        assert_eq!(input.tool_response, json!({"success": true}));
    }
}
