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

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::{Value, json};

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

    #[rstest]
    #[case::session_id(
        json!({
            "transcript_path": null,
            "cwd": "/work",
            "model": "gpt-5",
        }),
        "session_id",
    )]
    #[case::model(
        json!({
            "session_id": "sess",
            "transcript_path": null,
            "cwd": "/work",
        }),
        "model",
    )]
    fn hook_context_rejects_missing_required_field(#[case] v: Value, #[case] expected: &str) {
        let err = serde_json::from_value::<HookContext>(v).unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
    }
}
