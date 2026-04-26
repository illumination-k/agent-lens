use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Fields shared by every Claude Code hook input payload.
///
/// `hook_event_name` is intentionally omitted: it is used as the discriminator
/// for [`super::ClaudeCodeHookInput`] and is stripped during deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HookContext {
    pub session_id: String,
    pub transcript_path: PathBuf,
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<PermissionMode>,
}

/// The permission mode the Claude Code session is running under.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    Plan,
}

/// Output fields shared across every Claude Code hook response.
///
/// Each hook-specific output flattens this struct to inherit the common
/// fields while keeping its own decision / hook-specific payload.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct CommonHookOutput {
    /// Whether Claude Code should continue after the hook runs.
    #[serde(rename = "continue", default, skip_serializing_if = "Option::is_none")]
    pub continue_: Option<bool>,

    /// Reason surfaced to the user when `continue_` is `Some(false)`.
    #[serde(
        rename = "stopReason",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_reason: Option<String>,

    /// Suppress the hook's stdout from the transcript.
    #[serde(
        rename = "suppressOutput",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub suppress_output: Option<bool>,

    /// Message injected back into the conversation as a system message.
    #[serde(
        rename = "systemMessage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub system_message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn permission_mode_uses_camel_case() {
        for (mode, expected) in [
            (PermissionMode::Default, "default"),
            (PermissionMode::AcceptEdits, "acceptEdits"),
            (PermissionMode::BypassPermissions, "bypassPermissions"),
            (PermissionMode::Plan, "plan"),
        ] {
            let v = serde_json::to_value(&mode).unwrap();
            assert_eq!(v, json!(expected));
            let parsed: PermissionMode = serde_json::from_value(json!(expected)).unwrap();
            assert_eq!(parsed, mode);
        }
    }

    #[test]
    fn unknown_permission_mode_is_rejected() {
        let err = serde_json::from_value::<PermissionMode>(json!("yolo")).unwrap_err();
        assert!(err.to_string().contains("variant"), "{err}");
    }

    #[test]
    fn permission_mode_field_is_optional() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
        });
        let ctx: HookContext = serde_json::from_value(payload).unwrap();
        assert!(ctx.permission_mode.is_none());
    }

    #[test]
    fn common_hook_output_default_serializes_to_empty_object() {
        let v = serde_json::to_value(CommonHookOutput::default()).unwrap();
        assert_eq!(v, json!({}));
    }

    #[test]
    fn common_hook_output_uses_camel_case_keys() {
        let out = CommonHookOutput {
            continue_: Some(false),
            stop_reason: Some("done".into()),
            suppress_output: Some(true),
            system_message: Some("note".into()),
        };
        let v = serde_json::to_value(&out).unwrap();
        assert_eq!(
            v,
            json!({
                "continue": false,
                "stopReason": "done",
                "suppressOutput": true,
                "systemMessage": "note",
            })
        );
        let parsed: CommonHookOutput = serde_json::from_value(v).unwrap();
        assert_eq!(parsed, out);
    }
}
