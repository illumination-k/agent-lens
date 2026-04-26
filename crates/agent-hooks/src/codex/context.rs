use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Fields shared by every Codex hook input payload.
///
/// `hook_event_name` is intentionally omitted: it is used as the discriminator
/// for [`super::CodexHookInput`] and is stripped during deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HookContext {
    pub session_id: String,
    /// Codex sends `null` when the session has no transcript on disk yet.
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    pub cwd: PathBuf,
    /// Active model slug for the session.
    pub model: String,
}

/// Output fields shared across most Codex hook responses.
///
/// Each hook-specific output flattens this struct to inherit the common
/// fields while keeping its own decision / hook-specific payload. Note that
/// `PreToolUse` and `PermissionRequest` only honor `system_message` today;
/// the other fields are parsed but ignored for those events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct CommonHookOutput {
    /// Whether Codex should continue after the hook runs.
    #[serde(rename = "continue", default, skip_serializing_if = "Option::is_none")]
    pub continue_: Option<bool>,

    /// Reason recorded when `continue_` is `Some(false)`.
    #[serde(
        rename = "stopReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_reason: Option<String>,

    /// Surfaced as a warning in the UI or event stream.
    #[serde(
        rename = "systemMessage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub system_message: Option<String>,

    /// Parsed but not yet implemented by Codex; preserved so handlers can
    /// set it without losing forward compatibility.
    #[serde(
        rename = "suppressOutput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub suppress_output: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hook_context_round_trip_with_path() {
        let v = json!({
            "session_id": "sess-1",
            "transcript_path": "/tmp/transcript.jsonl",
            "cwd": "/work",
            "model": "gpt-5-mini",
        });
        let ctx: HookContext = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(ctx.session_id, "sess-1");
        assert_eq!(ctx.model, "gpt-5-mini");
        assert!(ctx.transcript_path.is_some());
        assert_eq!(serde_json::to_value(&ctx).unwrap(), v);
    }

    #[test]
    fn hook_context_accepts_null_transcript_path() {
        let v = json!({
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/work",
            "model": "gpt-5",
        });
        let ctx: HookContext = serde_json::from_value(v).unwrap();
        assert!(ctx.transcript_path.is_none());
    }

    #[test]
    fn hook_context_accepts_missing_transcript_path() {
        // Codex's documented schema lets the field be absent entirely;
        // serde's `default` covers that.
        let v = json!({
            "session_id": "sess",
            "cwd": "/work",
            "model": "gpt-5",
        });
        let ctx: HookContext = serde_json::from_value(v).unwrap();
        assert!(ctx.transcript_path.is_none());
    }

    #[test]
    fn hook_context_missing_session_id_is_rejected() {
        let v = json!({
            "transcript_path": null,
            "cwd": "/work",
            "model": "gpt-5",
        });
        let err = serde_json::from_value::<HookContext>(v).unwrap_err();
        assert!(err.to_string().contains("session_id"), "{err}");
    }

    #[test]
    fn hook_context_missing_model_is_rejected() {
        let v = json!({
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/work",
        });
        let err = serde_json::from_value::<HookContext>(v).unwrap_err();
        assert!(err.to_string().contains("model"), "{err}");
    }

    #[test]
    fn common_hook_output_default_serializes_to_empty_object() {
        let v = serde_json::to_value(CommonHookOutput::default()).unwrap();
        assert_eq!(v, json!({}));
    }

    #[test]
    fn common_hook_output_uses_camel_case_keys() {
        let out = CommonHookOutput {
            continue_: Some(true),
            stop_reason: Some("done".into()),
            system_message: Some("note".into()),
            suppress_output: Some(false),
        };
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(
            v,
            json!({
                "continue": true,
                "stopReason": "done",
                "systemMessage": "note",
                "suppressOutput": false,
            })
        );
        let parsed: CommonHookOutput = serde_json::from_value(v).unwrap();
        assert_eq!(parsed, out);
    }
}
