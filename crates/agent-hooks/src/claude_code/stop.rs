use serde::{Deserialize, Serialize};

use super::context::{CommonHookOutput, HookContext};

/// Input payload for the `Stop` hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StopInput {
    #[serde(flatten)]
    pub context: HookContext,

    /// `true` when the hook has already fired once for this stop, used to
    /// avoid infinite loops when the hook itself tries to keep the agent going.
    #[serde(default)]
    pub stop_hook_active: bool,
}

/// Output payload for the `Stop` hook.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct StopOutput {
    #[serde(flatten)]
    pub common: CommonHookOutput,

    /// `"block"` forces the agent to keep working; `reason` must explain why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<StopDecision>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum StopDecision {
    Block,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::ClaudeCodeHookInput;
    use serde_json::json;

    #[test]
    fn deserializes_stop_input() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "Stop",
            "stop_hook_active": true
        });
        let input: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        let ClaudeCodeHookInput::Stop(input) = input else {
            panic!("expected Stop variant");
        };
        assert!(input.stop_hook_active);
    }

    #[test]
    fn stop_hook_active_defaults_to_false_when_absent() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "Stop",
        });
        let input: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        let ClaudeCodeHookInput::Stop(input) = input else {
            panic!("expected Stop variant");
        };
        assert!(!input.stop_hook_active);
    }

    #[test]
    fn missing_cwd_is_rejected() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "hook_event_name": "Stop",
        });
        let err = serde_json::from_value::<ClaudeCodeHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains("cwd"), "{err}");
    }

    #[test]
    fn block_decision_round_trip() {
        let output = StopOutput {
            decision: Some(StopDecision::Block),
            reason: Some("keep going".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(v, json!({"decision": "block", "reason": "keep going"}));
    }
}
