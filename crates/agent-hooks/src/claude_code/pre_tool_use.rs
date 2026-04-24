use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::context::{CommonHookOutput, HookContext};

/// Input payload for the `PreToolUse` hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PreToolUseInput {
    #[serde(flatten)]
    pub context: HookContext,
    pub tool_name: String,
    pub tool_input: Value,
}

/// Output payload for the `PreToolUse` hook.
///
/// Both the legacy `decision` / `reason` pair and the newer
/// `hookSpecificOutput.permissionDecision` are supported; newer clients should
/// prefer [`PreToolUseHookSpecificOutput`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PreToolUseOutput {
    #[serde(flatten)]
    pub common: CommonHookOutput,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<PreToolUseDecision>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    #[serde(
        rename = "hookSpecificOutput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub hook_specific_output: Option<PreToolUseHookSpecificOutput>,
}

/// Legacy approve/block decision surfaced by a `PreToolUse` hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PreToolUseDecision {
    Approve,
    Block,
}

/// Structured permission decision returned under `hookSpecificOutput`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PreToolUseHookSpecificOutput {
    /// Must be the string `"PreToolUse"`.
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,

    #[serde(
        rename = "permissionDecision",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_decision: Option<PermissionDecision>,

    #[serde(
        rename = "permissionDecisionReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_decision_reason: Option<String>,
}

/// Newer-style permission decision returned by a `PreToolUse` hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionDecision {
    Allow,
    Deny,
    Ask,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::ClaudeCodeHookInput;
    use serde_json::json;

    #[test]
    fn deserializes_pre_tool_use_input() {
        let payload = json!({
            "session_id": "sess-1",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "ls"}
        });

        let dispatched: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        let ClaudeCodeHookInput::PreToolUse(input) = dispatched else {
            panic!("expected PreToolUse variant");
        };
        assert_eq!(input.tool_name, "Bash");
        assert_eq!(input.tool_input, json!({"command": "ls"}));
        assert_eq!(input.context.session_id, "sess-1");
    }

    #[test]
    fn serializes_hook_specific_output_with_camel_case() {
        let output = PreToolUseOutput {
            hook_specific_output: Some(PreToolUseHookSpecificOutput {
                hook_event_name: "PreToolUse".to_owned(),
                permission_decision: Some(PermissionDecision::Deny),
                permission_decision_reason: Some("blocked by policy".to_owned()),
            }),
            ..Default::default()
        };

        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(
            v,
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": "deny",
                    "permissionDecisionReason": "blocked by policy"
                }
            })
        );
    }

    #[test]
    fn tolerates_unknown_fields() {
        let payload = json!({
            "session_id": "sess-1",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {},
            "future_field": 42
        });
        serde_json::from_value::<ClaudeCodeHookInput>(payload).unwrap();
    }
}
