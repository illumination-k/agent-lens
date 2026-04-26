use serde::{Deserialize, Serialize};

use super::context::{CommonHookOutput, HookContext};

/// Input payload for the `SessionStart` hook.
///
/// Unlike the turn-scoped events, `SessionStart` has no `turn_id`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionStartInput {
    #[serde(flatten)]
    pub context: HookContext,
    pub source: SessionStartSource,
}

/// How the session was started, used by Codex for the `matcher` field.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStartSource {
    Startup,
    Resume,
    Clear,
}

/// Output payload for the `SessionStart` hook.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionStartOutput {
    #[serde(flatten)]
    pub common: CommonHookOutput,

    #[serde(
        rename = "hookSpecificOutput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub hook_specific_output: Option<SessionStartHookSpecificOutput>,
}

/// Extra developer context attached to the new session.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionStartHookSpecificOutput {
    /// Must be the string `"SessionStart"`.
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
    fn deserializes_session_start_input() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "SessionStart",
            "source": "startup"
        });
        let input: CodexHookInput = serde_json::from_value(payload).unwrap();
        let CodexHookInput::SessionStart(input) = input else {
            panic!("expected SessionStart variant");
        };
        assert_eq!(input.source, SessionStartSource::Startup);
        assert!(input.context.transcript_path.is_none());
        assert_eq!(input.context.model, "gpt-5");
    }

    #[test]
    fn serializes_additional_context_output() {
        let output = SessionStartOutput {
            hook_specific_output: Some(SessionStartHookSpecificOutput {
                hook_event_name: "SessionStart".to_owned(),
                additional_context: Some("load conventions".to_owned()),
            }),
            ..Default::default()
        };
        let v = serde_json::to_value(&output).unwrap();
        assert_eq!(
            v,
            json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": "load conventions"
                }
            })
        );
    }

    #[test]
    fn deserializes_each_source_variant() {
        for (raw, expected) in [
            ("startup", SessionStartSource::Startup),
            ("resume", SessionStartSource::Resume),
            ("clear", SessionStartSource::Clear),
        ] {
            let parsed: SessionStartSource = serde_json::from_value(json!(raw)).unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[test]
    fn unknown_source_variant_is_rejected() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "SessionStart",
            "source": "fork"
        });
        let err = serde_json::from_value::<CodexHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains("variant"), "{err}");
    }

    #[test]
    fn missing_source_is_rejected() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "SessionStart",
        });
        let err = serde_json::from_value::<CodexHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains("source"), "{err}");
    }

    #[test]
    fn tolerates_unknown_fields() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "SessionStart",
            "source": "resume",
            "future_field": {"a": 1},
        });
        serde_json::from_value::<CodexHookInput>(payload).unwrap();
    }

    #[test]
    fn session_start_input_does_not_carry_turn_id() {
        // Sanity-check: if a turn_id sneaks in, deserialization must
        // still succeed because flatten + unknown fields tolerates it.
        let payload = json!({
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/repo",
            "model": "gpt-5",
            "hook_event_name": "SessionStart",
            "source": "startup",
            "turn_id": "turn-1",
        });
        let parsed: CodexHookInput = serde_json::from_value(payload).unwrap();
        assert!(matches!(parsed, CodexHookInput::SessionStart(_)));
    }
}
