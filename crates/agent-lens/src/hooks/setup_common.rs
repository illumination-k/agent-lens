//! Bits of setup logic shared between the Claude Code and Codex setup
//! commands.
//!
//! The two setup files diverge on file format (JSON vs TOML), error
//! types, and plan/summary shapes, but the path-resolution and
//! command-prefix matching logic is identical and is collected here.

use std::path::PathBuf;

pub(crate) const SESSION_START_EVENT: &str = "SessionStart";
pub(crate) const PRE_TOOL_USE_EVENT: &str = "PreToolUse";
pub(crate) const POST_TOOL_USE_EVENT: &str = "PostToolUse";

pub(crate) const CLAUDE_EDITING_TOOL_MATCHER: &str = "Edit|Write|MultiEdit";
pub(crate) const CLAUDE_SESSION_START_MATCHER: &str = "startup|resume";
pub(crate) const CLAUDE_SESSION_START_COMMANDS: &[&str] =
    &["agent-lens hook session-start summary"];
pub(crate) const CLAUDE_PRE_TOOL_USE_COMMANDS: &[&str] = &[
    "agent-lens hook pre-tool-use complexity",
    "agent-lens hook pre-tool-use cohesion",
];
pub(crate) const CLAUDE_POST_TOOL_USE_COMMANDS: &[&str] = &[
    "agent-lens hook post-tool-use similarity",
    "agent-lens hook post-tool-use wrapper",
];

pub(crate) const CODEX_APPLY_PATCH_MATCHER: &str = "^apply_patch$";
pub(crate) const CODEX_SESSION_START_MATCHER: &str = "^(startup|resume)$";
pub(crate) const CODEX_SESSION_START_COMMANDS: &[&str] =
    &["agent-lens codex-hook session-start summary"];
pub(crate) const CODEX_PRE_TOOL_USE_COMMANDS: &[&str] = &[
    "agent-lens codex-hook pre-tool-use complexity",
    "agent-lens codex-hook pre-tool-use cohesion",
];
pub(crate) const CODEX_POST_TOOL_USE_COMMANDS: &[&str] = &[
    "agent-lens codex-hook post-tool-use similarity",
    "agent-lens codex-hook post-tool-use wrapper",
];

/// Resolve `$HOME/<relative>` as a [`PathBuf`], or `None` if `$HOME` is
/// unset. Each caller maps `None` to its own scope-specific error.
pub(crate) fn home_scoped_path(relative: &str) -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(relative))
}

/// Outcome of computing a setup plan against an existing settings/config
/// file. `T` is the file's payload representation — parsed JSON for the
/// Claude Code settings file, raw TOML text for the Codex config — and
/// the setup modules expose their concrete shape as a `SetupPlan` type
/// alias.
#[derive(Debug)]
pub struct SetupPlan<T> {
    pub path: PathBuf,
    pub before: Option<T>,
    pub after: T,
    pub added_commands: Vec<String>,
}

impl<T: PartialEq> SetupPlan<T> {
    /// Whether applying this plan would change the file on disk.
    pub fn changed(&self) -> bool {
        match &self.before {
            None => true,
            Some(before) => before != &self.after,
        }
    }
}

/// True when `existing` is the same handler invocation as `wanted`,
/// modulo trailing arguments and the path the binary is invoked through.
///
/// Used by both the Claude Code and Codex setup paths so that:
/// - an already-installed
///   `agent-lens hook post-tool-use similarity --threshold 0.9` is not
///   re-installed without the user-added flag, and
/// - a command wired through an explicit binary path — e.g.
///   `"$CLAUDE_PROJECT_DIR"/target/debug/agent-lens hook post-tool-use similarity`
///   — is recognised as the same handler as the bare
///   `agent-lens hook post-tool-use similarity` we install, so re-running
///   setup stays a no-op instead of appending a duplicate block.
///
/// The binary token is matched on its basename, so only the leading path
/// (not the handler arguments) is normalised away.
pub(crate) fn has_command_prefix(existing: &str, wanted: &str) -> bool {
    let (existing_bin, existing_args) = split_binary(existing);
    let (wanted_bin, wanted_args) = split_binary(wanted);
    binary_basename(existing_bin) == binary_basename(wanted_bin)
        && args_prefix_matches(existing_args, wanted_args)
}

/// Split a shell command into its leading binary token and the remaining
/// argument string. Splitting is whitespace-based, so a binary path that
/// itself contains whitespace is not handled — none of the commands we
/// install do.
fn split_binary(command: &str) -> (&str, &str) {
    let command = command.trim_start();
    match command.find(char::is_whitespace) {
        Some(end) => (&command[..end], command[end..].trim_start()),
        None => (command, ""),
    }
}

/// The final path segment of a binary token, so `.../target/debug/agent-lens`
/// and a bare `agent-lens` compare equal. Handles both Unix and Windows
/// separators.
fn binary_basename(bin: &str) -> &str {
    bin.rsplit(['/', '\\']).next().unwrap_or(bin)
}

/// True when `existing_args` equals `wanted_args` or extends it with at
/// least one whitespace-separated trailing argument (a user-added flag).
fn args_prefix_matches(existing_args: &str, wanted_args: &str) -> bool {
    if existing_args == wanted_args {
        return true;
    }
    existing_args
        .strip_prefix(wanted_args)
        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_command_prefix_matches_exact() {
        assert!(has_command_prefix("a b c", "a b c"));
    }

    #[test]
    fn has_command_prefix_matches_trailing_args() {
        assert!(has_command_prefix("a b c --flag", "a b c"));
    }

    #[test]
    fn has_command_prefix_rejects_word_extension() {
        assert!(!has_command_prefix("a b cx", "a b c"));
    }

    #[test]
    fn has_command_prefix_ignores_binary_path_prefix() {
        // The command may invoke agent-lens through a project-relative or
        // absolute path; setup must still recognise it as the same handler
        // as the bare `agent-lens hook ...` it installs.
        assert!(has_command_prefix(
            "\"$CLAUDE_PROJECT_DIR\"/target/debug/agent-lens hook post-tool-use similarity",
            "agent-lens hook post-tool-use similarity",
        ));
        assert!(has_command_prefix(
            "/usr/local/bin/agent-lens hook pre-tool-use complexity",
            "agent-lens hook pre-tool-use complexity",
        ));
    }

    #[test]
    fn has_command_prefix_ignores_path_prefix_with_trailing_args() {
        assert!(has_command_prefix(
            "/opt/agent-lens hook post-tool-use similarity --threshold 0.9",
            "agent-lens hook post-tool-use similarity",
        ));
    }

    #[test]
    fn has_command_prefix_rejects_different_handler_under_same_binary() {
        // Same binary basename but a different handler must not match, so a
        // Claude command is never mistaken for its Codex counterpart.
        assert!(!has_command_prefix(
            "/path/agent-lens hook pre-tool-use complexity",
            "agent-lens codex-hook pre-tool-use complexity",
        ));
    }

    #[test]
    fn has_command_prefix_rejects_different_binary() {
        assert!(!has_command_prefix(
            "bash \"$CLAUDE_PROJECT_DIR\"/.claude/hooks/session-start.sh",
            "agent-lens hook session-start summary",
        ));
    }

    #[test]
    fn has_command_prefix_rejects_different_binary_with_identical_args() {
        // Same arguments but a different binary basename must not match:
        // this pins `binary_basename` as load-bearing, so normalising the
        // path can never collapse two genuinely different binaries.
        assert!(!has_command_prefix(
            "/opt/other-tool hook post-tool-use similarity",
            "agent-lens hook post-tool-use similarity",
        ));
    }
}
