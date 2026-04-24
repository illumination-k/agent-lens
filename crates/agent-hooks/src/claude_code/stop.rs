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
}
