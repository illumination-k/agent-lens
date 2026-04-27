//! Bits of setup logic shared between the Claude Code and Codex setup
//! commands.
//!
//! The two setup files diverge on file format (JSON vs TOML), error
//! types, and plan/summary shapes, but the path-resolution and
//! command-prefix matching logic is identical and is collected here.

use std::path::PathBuf;

/// Resolve `$HOME/<relative>` as a [`PathBuf`], or `None` if `$HOME` is
/// unset. Each caller maps `None` to its own scope-specific error.
pub(crate) fn home_scoped_path(relative: &str) -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(relative))
}

/// True when `existing` is the same handler invocation as `wanted`,
/// modulo trailing arguments.
///
/// Used by both the Claude Code and Codex setup paths so that an
/// already-installed `agent-lens hook post-tool-use similarity --threshold 0.9`
/// is not re-installed without the user-added flag.
pub(crate) fn has_command_prefix(existing: &str, wanted: &str) -> bool {
    if existing == wanted {
        return true;
    }
    existing
        .strip_prefix(wanted)
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
}
