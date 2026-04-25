use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::context::{CommonHookOutput, HookContext};

/// Input payload for the `PreToolUse` hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct PreToolUseInput {
    #[serde(flatten)]
    pub context: HookContext,
    /// Codex turn id this tool call belongs to.
    pub turn_id: String,
    /// Canonical tool name, such as `Bash`, `apply_patch`, or
    /// `mcp__server__tool`.
    pub tool_name: String,
    /// Tool-call id for this invocation.
    pub tool_use_id: String,
    /// Tool-specific input. For `Bash` and `apply_patch` this is
    /// `{ "command": "..." }`; MCP tools forward all of their args.
    pub tool_input: Value,
}

/// Output payload for the `PreToolUse` hook.
///
/// Both the legacy `decision` / `reason` pair and the newer
/// `hookSpecificOutput.permissionDecision` are supported; new code should
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

/// Legacy block decision. Codex parses `"approve"` too but only
/// `"block"` has runtime effect today, so this enum exposes just the
/// supported variant.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PreToolUseDecision {
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
    pub permission_decision: Option<PreToolUsePermissionDecision>,

    #[serde(
        rename = "permissionDecisionReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_decision_reason: Option<String>,
}

/// Permission decision for a Codex `PreToolUse` hook.
///
/// `Allow` and `Ask` are parsed today but Codex documents them as
/// "fail open" — only `Deny` is fully wired up. All three variants are
/// exposed here so handlers can be written against the documented
/// surface and start working as Codex catches up.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PreToolUsePermissionDecision {
    Allow,
    Deny,
    Ask,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::CodexHookInput;
    use serde_json::json;

    #[test]
    fn deserializes_pre_tool_use_input() {
        let payload = json!({
            "session_id": "sess-1",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "PreToolUse",
            "turn_id": "turn-1",
            "tool_name": "Bash",
            "tool_use_id": "call-1",
            "tool_input": {"command": "ls"}
        });
        let dispatched: CodexHookInput = serde_json::from_value(payload).unwrap();
        let CodexHookInput::PreToolUse(input) = dispatched else {
            panic!("expected PreToolUse variant");
        };
        assert_eq!(input.tool_name, "Bash");
        assert_eq!(input.turn_id, "turn-1");
        assert_eq!(input.tool_use_id, "call-1");
        assert_eq!(input.tool_input, json!({"command": "ls"}));
    }

    #[test]
    fn serializes_hook_specific_output_with_camel_case() {
        let output = PreToolUseOutput {
            hook_specific_output: Some(PreToolUseHookSpecificOutput {
                hook_event_name: "PreToolUse".to_owned(),
                permission_decision: Some(PreToolUsePermissionDecision::Deny),
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
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "PreToolUse",
            "turn_id": "turn-1",
            "tool_name": "Bash",
            "tool_use_id": "call-1",
            "tool_input": {},
            "future_field": 42
        });
        serde_json::from_value::<CodexHookInput>(payload).unwrap();
    }
}
