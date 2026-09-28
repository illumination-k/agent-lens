use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::context::HookContext;
use crate::common::CommonHookOutput;

/// Input payload for the `CwdChanged` hook. It fires whenever Claude's
/// working directory changes — a `cd`, or entering a worktree — and has
/// no matcher. The common `cwd` is the new directory.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CwdChangedInput {
    #[serde(flatten)]
    pub context: HookContext,

    /// The directory Claude left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_cwd: Option<PathBuf>,
}

/// Output payload for the `CwdChanged` hook: the common fields only.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct CwdChangedOutput {
    #[serde(flatten)]
    pub common: CommonHookOutput,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::ClaudeCodeHookInput;
    use serde_json::json;

    #[test]
    fn deserializes_cwd_changed_input() {
        let payload = json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo/.claude/worktrees/wt",
            "hook_event_name": "CwdChanged",
            "previous_cwd": "/repo"
        });
        let input: ClaudeCodeHookInput = serde_json::from_value(payload).unwrap();
        let ClaudeCodeHookInput::CwdChanged(input) = input else {
            panic!("expected CwdChanged variant");
        };
        assert_eq!(
            input.context.cwd,
            PathBuf::from("/repo/.claude/worktrees/wt")
        );
        assert_eq!(input.previous_cwd, Some(PathBuf::from("/repo")));
    }

    #[test]
    fn previous_cwd_is_optional() {
        let input: CwdChangedInput = serde_json::from_value(json!({
            "session_id": "sess",
            "transcript_path": "/tmp/t.jsonl",
            "cwd": "/repo",
        }))
        .unwrap();
        assert_eq!(input.previous_cwd, None);
    }

    #[test]
    fn default_output_is_an_empty_object() {
        assert_eq!(
            serde_json::to_value(CwdChangedOutput::default()).unwrap(),
            json!({})
        );
    }
}
