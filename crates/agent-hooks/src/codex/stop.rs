use serde::{Deserialize, Serialize};

use super::context::{CommonHookOutput, HookContext};

/// Input payload for the `Stop` hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct StopInput {
    #[serde(flatten)]
    pub context: HookContext,
    pub turn_id: String,
    /// `true` when this turn was already continued by a previous `Stop`
    /// hook, used to break runaway continuation loops.
    #[serde(default)]
    pub stop_hook_active: bool,
    /// Latest assistant message text, when Codex has one available.
    #[serde(default)]
    pub last_assistant_message: Option<String>,
}

/// Output payload for the `Stop` hook.
///
/// `decision: "block"` does *not* reject the turn — it tells Codex to
/// continue and synthesises a new user prompt from `reason`. To actually
/// stop the loop, set `common.continue_ = Some(false)` instead.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct StopOutput {
    #[serde(flatten)]
    pub common: CommonHookOutput,

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
    use crate::codex::CodexHookInput;
    use serde_json::json;

    #[test]
    fn deserializes_stop_input() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "Stop",
            "turn_id": "turn-1",
            "stop_hook_active": true,
            "last_assistant_message": "all done"
        });
        let input: CodexHookInput = serde_json::from_value(payload).unwrap();
        let CodexHookInput::Stop(input) = input else {
            panic!("expected Stop variant");
        };
        assert!(input.stop_hook_active);
        assert_eq!(input.last_assistant_message.as_deref(), Some("all done"));
    }

    #[test]
    fn last_assistant_message_is_optional() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "Stop",
            "turn_id": "turn-1"
        });
        let input: CodexHookInput = serde_json::from_value(payload).unwrap();
        let CodexHookInput::Stop(input) = input else {
            panic!("expected Stop variant");
        };
        assert!(!input.stop_hook_active);
        assert!(input.last_assistant_message.is_none());
    }
}
