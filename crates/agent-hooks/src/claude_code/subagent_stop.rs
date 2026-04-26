use serde::{Deserialize, Serialize};

use super::context::{CommonHookOutput, HookContext};

/// Input payload for the `SubagentStop` hook (fires when a Task sub-agent ends).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentStopInput {
    #[serde(flatten)]
    pub context: HookContext,
    #[serde(default)]
    pub stop_hook_active: bool,
}

/// Output payload for the `SubagentStop` hook.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SubagentStopOutput {
    #[serde(flatten)]
    pub common: CommonHookOutput,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<SubagentStopDecision>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SubagentStopDecision {
    Block,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::ClaudeCodeHookInput;
    use serde_json::json;

    #[test]
    fn deserializes_subagent_stop_input() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "SubagentStop",
            "stop_hook_active": false
        });
        let input: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        let ClaudeCodeHookInput::SubagentStop(input) = input else {
            panic!("expected SubagentStop variant");
        };
        assert!(!input.stop_hook_active);
    }

    #[test]
    fn stop_hook_active_defaults_to_false_when_absent() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "SubagentStop",
        });
        let input: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        let ClaudeCodeHookInput::SubagentStop(input) = input else {
            panic!("expected SubagentStop variant");
        };
        assert!(!input.stop_hook_active);
    }

    #[test]
    fn missing_transcript_path_is_rejected() {
        let payload = json!({
            "session_id": "sess",
            "cwd": "/repo",
            "hook_event_name": "SubagentStop",
        });
        let err = serde_json::from_value::<ClaudeCodeHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains("transcript_path"), "{err}");
    }

    #[test]
    fn block_decision_round_trip() {
        let output = SubagentStopOutput {
            decision: Some(SubagentStopDecision::Block),
            reason: Some("keep going".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(v, json!({"decision": "block", "reason": "keep going"}));
    }
}
