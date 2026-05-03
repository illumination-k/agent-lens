use serde::{Deserialize, Serialize};

use super::context::{CommonHookOutput, HookContext};

/// Input payload for the `SessionStart` hook.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct SessionStartInput {
    #[serde(flatten)]
    pub context: HookContext,
    pub source: SessionStartSource,
}

/// How the session was started, used as the matcher Claude Code dispatches on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStartSource {
    Startup,
    Resume,
    Clear,
    Compact,
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

/// Extra context to inject into the new session.
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
    use crate::claude_code::ClaudeCodeHookInput;
    use rstest::rstest;
    use serde_json::{Value, json};

    #[test]
    fn deserializes_session_start_input() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "SessionStart",
            "source": "startup"
        });
        let input: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        let ClaudeCodeHookInput::SessionStart(input) = input else {
            panic!("expected SessionStart variant");
        };
        assert_eq!(input.source, SessionStartSource::Startup);
        assert_eq!(input.context.session_id, "sess");
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
            ("compact", SessionStartSource::Compact),
        ] {
            let parsed: SessionStartSource = serde_json::from_value(json!(raw)).unwrap();
            assert_eq!(parsed, expected);
        }
    }

    #[rstest]
    #[case::unknown_variant(
        json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "SessionStart",
            "source": "fork",
        }),
        "variant",
    )]
    #[case::missing_source(
        json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
            "hook_event_name": "SessionStart",
        }),
        "source",
    )]
    fn rejects_invalid_source(#[case] payload: Value, #[case] expected: &str) {
        let err = serde_json::from_value::<ClaudeCodeHookInput>(payload).unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
    }

    #[test]
    fn empty_default_output_serializes_to_empty_object() {
        let v = serde_json::to_value(SessionStartOutput::default()).unwrap();
        assert_eq!(v, json!({}));
    }
}
