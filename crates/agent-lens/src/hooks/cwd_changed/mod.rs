//! Claude Code `CwdChanged` hook handler.
//!
//! Fires whenever the session's working directory changes — a `cd`, or
//! entering a worktree — and records the checkpoint snapshot for the new
//! directory, so a stop there compares against the tree as the session
//! found it. Installed async: the snapshot prints nothing.

use std::path::Path;

use agent_hooks::claude_code::{CwdChangedInput, CwdChangedOutput};

use crate::hooks::core::CheckpointEnvelope;

/// Claude Code's CwdChanged adapter for the checkpoint snapshot.
pub struct ClaudeCodeCwdChanged;

impl CheckpointEnvelope for ClaudeCodeCwdChanged {
    type Input = CwdChangedInput;
    type Output = CwdChangedOutput;

    fn cwd(input: &Self::Input) -> &Path {
        &input.context.cwd
    }

    fn session_id(input: &Self::Input) -> &str {
        &input.context.session_id
    }
}

/// Claude Code CwdChanged handler that records the session checkpoint.
pub type SnapshotHook = crate::hooks::core::SnapshotHook<ClaudeCodeCwdChanged>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::core::runner::session_start_conformance::snapshot_is_recorded_silently;
    use crate::test_support::claude_hook_context;

    #[test]
    fn records_the_checkpoint_snapshot_silently() {
        snapshot_is_recorded_silently::<ClaudeCodeCwdChanged, _>(|cwd: &Path| CwdChangedInput {
            context: claude_hook_context(cwd),
            previous_cwd: Some(cwd.join("..")),
        });
    }
}
