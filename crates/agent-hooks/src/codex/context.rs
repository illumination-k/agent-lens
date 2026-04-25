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
