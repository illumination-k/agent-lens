use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::context::{CommonHookOutput, HookContext};

/// Input payload for the `PermissionRequest` hook.
///
/// Codex fires this when it is about to surface an approval prompt for a
/// shell escalation, managed-network call, or similar guarded action.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PermissionRequestInput {
    #[serde(flatten)]
    pub context: HookContext,
    pub turn_id: String,
    pub tool_name: String,
    /// Tool-specific input. The documented sub-field
    /// `tool_input.description` carries Codex's approval reason when one
    /// is available; callers can read it via `tool_input.get("description")`.
    pub tool_input: Value,
}

/// Output payload for the `PermissionRequest` hook.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct PermissionRequestOutput {
    #[serde(flatten)]
    pub common: CommonHookOutput,

    #[serde(
        rename = "hookSpecificOutput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub hook_specific_output: Option<PermissionRequestHookSpecificOutput>,
}

/// Structured permission decision returned under `hookSpecificOutput`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PermissionRequestHookSpecificOutput {
    /// Must be the string `"PermissionRequest"`.
    #[serde(rename = "hookEventName")]
    pub hook_event_name: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<PermissionDecision>,
}

/// `Allow` lets the request proceed silently. `Deny` blocks it and
/// surfaces `message` to the user. If multiple matching hooks return a
/// decision, any `Deny` wins.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "behavior", rename_all = "lowercase")]
pub enum PermissionDecision {
    Allow,
    Deny { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::CodexHookInput;
    use serde_json::json;

    #[test]
    fn deserializes_permission_request_input() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "PermissionRequest",
            "turn_id": "turn-1",
            "tool_name": "Bash",
            "tool_input": {"command": "rm -rf /", "description": "destructive"}
        });
        let input: CodexHookInput = serde_json::from_value(payload).unwrap();
        let CodexHookInput::PermissionRequest(input) = input else {
            panic!("expected PermissionRequest variant");
        };
        assert_eq!(input.tool_name, "Bash");
        assert_eq!(
            input.tool_input.get("description").and_then(|v| v.as_str()),
            Some("destructive")
        );
    }

    #[test]
    fn serializes_allow_decision() {
        let output = PermissionRequestOutput {
            hook_specific_output: Some(PermissionRequestHookSpecificOutput {
                hook_event_name: "PermissionRequest".to_owned(),
                decision: Some(PermissionDecision::Allow),
            }),
            ..Default::default()
        };
        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(
            v,
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {"behavior": "allow"}
                }
            })
        );
    }

    #[test]
    fn serializes_deny_decision_with_message() {
        let output = PermissionRequestOutput {
            hook_specific_output: Some(PermissionRequestHookSpecificOutput {
                hook_event_name: "PermissionRequest".to_owned(),
                decision: Some(PermissionDecision::Deny {
                    message: "blocked by repository policy".to_owned(),
                }),
            }),
            ..Default::default()
        };
        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(
            v,
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "PermissionRequest",
                    "decision": {
                        "behavior": "deny",
                        "message": "blocked by repository policy"
                    }
                }
            })
        );
    }
}
