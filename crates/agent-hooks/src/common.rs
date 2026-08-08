//! Hook output fields both supported agents spell the same way.
//!
//! Claude Code and Codex agree on the four "what should the agent do
//! after this hook" keys — `continue`, `stopReason`, `suppressOutput`,
//! `systemMessage` — so the struct lives here once and each engine
//! module re-exports it. Everything that genuinely differs between the
//! protocols (the input envelope, per-event decisions, hook-specific
//! output) stays in the engine module.
//!
//! Where the two engines differ is not the schema but what they *do*
//! with a field; those differences are called out per field below rather
//! than forked into a second struct.

use serde::{Deserialize, Serialize};

/// Output fields shared across every hook response.
///
/// Each hook-specific output flattens this struct to inherit the common
/// fields while keeping its own decision / hook-specific payload.
///
/// Codex honors only `system_message` on `PreToolUse` and
/// `PermissionRequest`, and does not implement `suppress_output` at all;
/// the fields are still parsed and serialized there so a handler can set
/// them without waiting on a schema change.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct CommonHookOutput {
    /// Whether the agent should continue after the hook runs.
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

    /// Message injected back into the conversation as a system message
    /// (surfaced as a warning in Codex's UI / event stream).
    #[serde(
        rename = "systemMessage",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub system_message: Option<String>,
}

impl CommonHookOutput {
    /// The common case for an advisory hook: say something, decide
    /// nothing.
    pub fn system_message(message: impl Into<String>) -> Self {
        Self {
            system_message: Some(message.into()),
            ..Self::default()
        }
    }
}

/// A hook response that flattens a [`CommonHookOutput`] block.
///
/// Implemented by every output type in both engine modules, so a handler
/// that only wants to say something can do it without naming the field
/// twice per event type.
pub trait CommonOutput: Default {
    fn common_mut(&mut self) -> &mut CommonHookOutput;

    /// An otherwise-default response carrying only a system message.
    fn with_system_message(message: impl Into<String>) -> Self {
        let mut output = Self::default();
        *output.common_mut() = CommonHookOutput::system_message(message);
        output
    }
}

/// Implement [`CommonOutput`] for each listed output type. Every hook
/// response names its shared block `common`, so the impl is mechanical;
/// listing the types per engine module keeps the set greppable.
macro_rules! impl_common_output {
    ($($ty:ty),+ $(,)?) => {
        $(
            impl $crate::common::CommonOutput for $ty {
                fn common_mut(&mut self) -> &mut $crate::common::CommonHookOutput {
                    &mut self.common
                }
            }
        )+
    };
}

pub(crate) use impl_common_output;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn system_message_sets_only_that_field() {
        let out = CommonHookOutput::system_message("note");
        assert_eq!(out.system_message.as_deref(), Some("note"));
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            json!({"systemMessage": "note"})
        );
    }

    #[test]
    fn default_serializes_to_empty_object() {
        let v = serde_json::to_value(CommonHookOutput::default()).unwrap();
        assert_eq!(v, json!({}));
    }

    #[test]
    fn uses_camel_case_keys() {
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
