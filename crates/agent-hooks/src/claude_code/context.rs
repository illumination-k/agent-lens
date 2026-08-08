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
///
/// `Other` absorbs modes Claude Code introduces after this enum is defined,
/// so an unrecognized value doesn't hard-fail hook input deserialization.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Default,
    AcceptEdits,
    BypassPermissions,
    Plan,
    Auto,
    #[serde(other)]
    Other,
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
            (PermissionMode::Auto, "auto"),
        ] {
            let v = serde_json::to_value(&mode).unwrap();
            assert_eq!(v, json!(expected));
            let parsed: PermissionMode = serde_json::from_value(json!(expected)).unwrap();
            assert_eq!(parsed, mode);
        }
    }

    #[test]
    fn unrecognized_permission_mode_falls_back_to_other() {
        let parsed: PermissionMode = serde_json::from_value(json!("yolo")).unwrap();
        assert_eq!(parsed, PermissionMode::Other);
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
}
