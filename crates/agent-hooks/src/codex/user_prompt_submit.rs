use serde::{Deserialize, Serialize};

use super::context::{CommonHookOutput, HookContext};

/// Input payload for the `UserPromptSubmit` hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct UserPromptSubmitInput {
    #[serde(flatten)]
    pub context: HookContext,
    pub turn_id: String,
    pub prompt: String,
}

/// Output payload for the `UserPromptSubmit` hook.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct UserPromptSubmitOutput {
    #[serde(flatten)]
    pub common: CommonHookOutput,

    /// `"block"` prevents the prompt from reaching the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<UserPromptSubmitDecision>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    #[serde(
        rename = "hookSpecificOutput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub hook_specific_output: Option<UserPromptSubmitHookSpecificOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UserPromptSubmitDecision {
    Block,
}

/// Extra context appended to the prompt before it is sent to the model.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct UserPromptSubmitHookSpecificOutput {
    /// Must be the string `"UserPromptSubmit"`.
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
    fn deserializes_user_prompt_submit_input() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "UserPromptSubmit",
            "turn_id": "turn-1",
            "prompt": "Hello"
        });
        let input: CodexHookInput = serde_json::from_value(payload).unwrap();
        let CodexHookInput::UserPromptSubmit(input) = input else {
            panic!("expected UserPromptSubmit variant");
        };
        assert_eq!(input.prompt, "Hello");
        assert_eq!(input.turn_id, "turn-1");
    }

    #[test]
    fn serializes_additional_context_output() {
        let output = UserPromptSubmitOutput {
            hook_specific_output: Some(UserPromptSubmitHookSpecificOutput {
                hook_event_name: "UserPromptSubmit".to_owned(),
                additional_context: Some("extra".to_owned()),
            }),
            ..Default::default()
        };
        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(
            v,
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "UserPromptSubmit",
                    "additionalContext": "extra"
                }
            })
        );
    }

    #[test]
    fn missing_prompt_is_rejected() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "UserPromptSubmit",
            "turn_id": "turn-1",
        });
        let err = serde_json::from_value::<CodexHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains("prompt"), "{err}");
    }

    #[test]
    fn tolerates_unknown_fields() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "UserPromptSubmit",
            "turn_id": "turn-1",
            "prompt": "hi",
            "future_field": "ignored",
        });
        serde_json::from_value::<CodexHookInput>(payload).unwrap();
    }

    #[test]
    fn block_decision_round_trip() {
        let output = UserPromptSubmitOutput {
            decision: Some(UserPromptSubmitDecision::Block),
            reason: Some("blocked".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(v, json!({"decision": "block", "reason": "blocked"}));
    }
}
