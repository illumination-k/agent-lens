//! `setup` — wire `agent-lens`'s hooks into a Claude Code
//! `settings.json` so users don't have to hand-edit it.
//!
//! The merge is conservative: every existing key is preserved, and a
//! fresh block is appended only with the commands that aren't already
//! wired up anywhere under that event. Re-running the command is a
//! no-op once everything is installed.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::hooks::setup_common::{self, EventBlock, HooksDocument};

const SETTINGS_RELATIVE: &str = ".claude/settings.json";

/// Tool matcher used for the PostToolUse block. Mirrors the
/// `EDITING_TOOL_NAMES` constant the handlers themselves filter on.
pub const POST_TOOL_USE_MATCHER: &str = setup_common::CLAUDE_EDITING_TOOL_MATCHER;

/// Commands the setup writes into `hooks.PostToolUse`. One entry per
/// installed handler; matching against the leading prefix of an existing
/// `command` string makes the merge tolerant of user-added flags.
pub const POST_TOOL_USE_COMMANDS: &[&str] = setup_common::CLAUDE_POST_TOOL_USE_COMMANDS;

/// Tool matcher used for the PreToolUse block. Mirrors the
/// `EDITING_TOOL_NAMES` constant the handlers themselves filter on; the
/// value happens to match [`POST_TOOL_USE_MATCHER`] today because the
/// pre/post handlers act on the same set of editing tools.
pub const PRE_TOOL_USE_MATCHER: &str = setup_common::CLAUDE_EDITING_TOOL_MATCHER;

/// Commands the setup writes into `hooks.PreToolUse`. One entry per
/// installed handler; matching against the leading prefix of an existing
/// `command` string makes the merge tolerant of user-added flags.
pub const PRE_TOOL_USE_COMMANDS: &[&str] = setup_common::CLAUDE_PRE_TOOL_USE_COMMANDS;

/// Source matcher for the SessionStart block. Claude Code dispatches on
/// the `source` field (`startup` / `resume` / `clear` / `compact`); a
/// summary on every clear/compact would be noisy, so by default we only
/// fire on a fresh start or a resumed session.
pub const SESSION_START_MATCHER: &str = setup_common::CLAUDE_SESSION_START_MATCHER;

/// Commands the setup writes into `hooks.SessionStart`.
pub const SESSION_START_COMMANDS: &[&str] = setup_common::CLAUDE_SESSION_START_COMMANDS;

/// Where to install the hook entries.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum SettingsScope {
    /// `<project_root>/.claude/settings.json` (created if missing).
    Project,
    /// `$HOME/.claude/settings.json` (created if missing).
    User,
}

/// Outcome of computing a setup plan against an existing settings file.
/// The payload is the parsed `settings.json` document.
pub type SetupPlan = setup_common::SetupPlan<Value>;

/// Compact summary of a setup run, suitable for JSON-on-stdout output.
#[derive(Debug, Serialize)]
pub struct SetupSummary<'a> {
    pub path: &'a Path,
    pub wrote: bool,
    pub added_commands: &'a [String],
    pub settings: &'a Value,
}

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    /// `$HOME` is not set, so the user-scope path can't be resolved.
    #[error("$HOME is not set; cannot resolve user-scope settings.json")]
    HomeNotFound,
    #[error("failed to access {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path:?} is not valid JSON: {source}")]
    InvalidJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    /// A field along the `hooks.PostToolUse[].hooks[].command` path has
    /// the wrong JSON type for us to merge into safely.
    #[error("{path:?} has an unexpected shape at .{field}")]
    UnexpectedShape { path: PathBuf, field: String },
}

fn io_error(path: PathBuf, source: std::io::Error) -> SetupError {
    SetupError::Io { path, source }
}

fn shape_error(path: &Path, field: impl Into<String>) -> SetupError {
    SetupError::UnexpectedShape {
        path: path.to_path_buf(),
        field: field.into(),
    }
}

/// Resolve the on-disk `settings.json` path for the requested scope.
///
/// `project_root` is only consulted for [`SettingsScope::Project`].
pub fn resolve_path(scope: SettingsScope, project_root: &Path) -> Result<PathBuf, SetupError> {
    match scope {
        SettingsScope::Project => Ok(project_root.join(SETTINGS_RELATIVE)),
        SettingsScope::User => {
            setup_common::home_scoped_path(SETTINGS_RELATIVE).ok_or(SetupError::HomeNotFound)
        }
    }
}

/// Compute the post-merge JSON for `path` without touching the filesystem.
///
/// A missing or empty file produces a plan that creates one. A file with
/// invalid JSON, or with an unexpected non-object/non-array shape along
/// the `hooks.PostToolUse` path, is reported as an error so the user can
/// inspect it before we clobber anything.
pub fn plan(path: PathBuf) -> Result<SetupPlan, SetupError> {
    let before = setup_common::read_existing_text(&path, io_error)?
        .map(|text| {
            serde_json::from_str::<Value>(&text).map_err(|source| SetupError::InvalidJson {
                path: path.clone(),
                source,
            })
        })
        .transpose()?;
    let mut after = before.clone().unwrap_or_else(|| Value::Object(Map::new()));
    let added_commands =
        setup_common::merge_hook_commands(&path, &mut after, setup_common::CLAUDE_EVENTS)?;
    Ok(SetupPlan {
        path,
        before,
        after,
        added_commands,
    })
}

/// Write the planned JSON to disk, creating parent directories if needed.
pub fn apply(plan: &SetupPlan) -> Result<(), SetupError> {
    let mut text =
        serde_json::to_string_pretty(&plan.after).map_err(|source| SetupError::InvalidJson {
            path: plan.path.clone(),
            source,
        })?;
    text.push('\n');
    setup_common::write_with_parents(&plan.path, &text, io_error)
}

impl HooksDocument for Value {
    type Error = SetupError;

    fn installed_commands(
        &mut self,
        path: &Path,
        block: &EventBlock,
    ) -> Result<Vec<String>, SetupError> {
        let entries = event_entries(self, path, block)?;
        let mut out = Vec::new();
        for entry in entries.iter() {
            let Some(entry_obj) = entry.as_object() else {
                return Err(shape_error(path, format!("hooks.{}[]", block.event)));
            };
            let Some(hooks) = entry_obj.get("hooks") else {
                continue;
            };
            let Some(hooks) = hooks.as_array() else {
                return Err(shape_error(path, format!("hooks.{}[].hooks", block.event)));
            };
            for hook in hooks {
                if let Some(cmd) = hook.get("command").and_then(Value::as_str) {
                    out.push(cmd.to_string());
                }
            }
        }
        Ok(out)
    }

    fn append_matcher_group(
        &mut self,
        path: &Path,
        block: &EventBlock,
        commands: &[String],
    ) -> Result<(), SetupError> {
        let entries = event_entries(self, path, block)?;
        entries.push(json!({
            "matcher": block.matcher,
            "hooks": commands
                .iter()
                .map(|cmd| json!({ "type": "command", "command": cmd }))
                .collect::<Vec<_>>(),
        }));
        Ok(())
    }
}

/// Navigate to the `hooks.<event>` array, creating the intermediate
/// object/array when absent, and erroring when an existing field along
/// the path has an incompatible JSON type.
fn event_entries<'a>(
    root: &'a mut Value,
    path: &Path,
    block: &EventBlock,
) -> Result<&'a mut Vec<Value>, SetupError> {
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| shape_error(path, "(root)"))?;
    let hooks = root_obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| shape_error(path, "hooks"))?;
    hooks
        .entry(block.event)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| shape_error(path, format!("hooks.{}", block.event)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use tempfile::TempDir;

    fn read(path: &Path) -> Value {
        let text = fs::read_to_string(path).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn plan_for_missing_file_creates_full_block() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".claude/settings.json");

        let plan = plan(path.clone()).unwrap();
        assert!(plan.before.is_none());
        assert!(plan.changed());
        assert_eq!(
            plan.added_commands.len(),
            SESSION_START_COMMANDS.len()
                + PRE_TOOL_USE_COMMANDS.len()
                + POST_TOOL_USE_COMMANDS.len(),
        );
        assert_eq!(
            plan.after,
            json!({
                "hooks": {
                    "SessionStart": [{
                        "matcher": SESSION_START_MATCHER,
                        "hooks": [
                            {"type": "command", "command": "agent-lens hook session-start summary"},
                        ],
                    }],
                    "PreToolUse": [{
                        "matcher": PRE_TOOL_USE_MATCHER,
                        "hooks": [
                            {"type": "command", "command": "agent-lens hook pre-tool-use complexity"},
                            {"type": "command", "command": "agent-lens hook pre-tool-use cohesion"},
                        ],
                    }],
                    "PostToolUse": [{
                        "matcher": POST_TOOL_USE_MATCHER,
                        "hooks": [
                            {"type": "command", "command": "agent-lens hook post-tool-use similarity"},
                            {"type": "command", "command": "agent-lens hook post-tool-use wrapper"},
                        ],
                    }],
                }
            })
        );
    }

    #[test]
    fn plan_installs_missing_blocks_alongside_existing_post_tool_use() {
        // When a user has only the older PostToolUse block, re-running
        // setup should add the SessionStart and PreToolUse blocks
        // without disturbing the existing one.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        let existing = json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": POST_TOOL_USE_MATCHER,
                    "hooks": [
                        {"type": "command", "command": "agent-lens hook post-tool-use similarity"},
                        {"type": "command", "command": "agent-lens hook post-tool-use wrapper"},
                    ],
                }],
            },
        });
        fs::write(&path, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

        let plan = plan(path).unwrap();
        assert_eq!(
            plan.added_commands,
            vec![
                "agent-lens hook session-start summary".to_string(),
                "agent-lens hook pre-tool-use complexity".to_string(),
                "agent-lens hook pre-tool-use cohesion".to_string(),
            ],
        );
        assert!(plan.changed());
        let session_start = plan.after["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session_start.len(), 1);
        assert_eq!(session_start[0]["matcher"], SESSION_START_MATCHER);
        let pre_tool_use = plan.after["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool_use.len(), 1);
        assert_eq!(pre_tool_use[0]["matcher"], PRE_TOOL_USE_MATCHER);
        let pre_hooks = pre_tool_use[0]["hooks"].as_array().unwrap();
        assert_eq!(pre_hooks.len(), PRE_TOOL_USE_COMMANDS.len());
    }

    #[test]
    fn apply_creates_parent_dir_and_writes_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".claude/settings.json");

        let plan = plan(path.clone()).unwrap();
        apply(&plan).unwrap();

        assert!(path.exists());
        assert_eq!(read(&path), plan.after);
    }

    #[test]
    fn rerunning_setup_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".claude/settings.json");

        let first = plan(path.clone()).unwrap();
        apply(&first).unwrap();

        let second = plan(path.clone()).unwrap();
        assert!(!second.changed(), "second plan should be a no-op");
        assert!(second.added_commands.is_empty());
        assert_eq!(second.before.as_ref(), Some(&second.after));
    }

    #[test]
    fn preserves_unrelated_keys_and_existing_hooks() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        let existing = json!({
            "theme": "dark",
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "/usr/local/bin/audit"}],
                }],
                "PostToolUse": [{
                    "matcher": "Edit",
                    "hooks": [{"type": "command", "command": "echo done"}],
                }],
            },
        });
        fs::write(&path, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

        let plan = plan(path.clone()).unwrap();
        apply(&plan).unwrap();

        let after = read(&path);
        assert_eq!(after["theme"], "dark");
        let pre = after["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(
            pre.len(),
            2,
            "existing PreToolUse entry should still be present"
        );
        assert_eq!(pre[0], existing["hooks"]["PreToolUse"][0]);
        assert_eq!(pre[1]["matcher"], PRE_TOOL_USE_MATCHER);
        let post = after["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(
            post.len(),
            2,
            "existing PostToolUse entry should still be present"
        );
        assert_eq!(post[0], existing["hooks"]["PostToolUse"][0]);
        assert_eq!(post[1]["matcher"], POST_TOOL_USE_MATCHER);
    }

    #[test]
    fn skips_command_already_installed_under_other_matcher() {
        // Pre-installs every handler the setup writes — under a
        // non-canonical matcher in each block — so the only queued
        // command is the post-tool-use wrapper.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        let existing = json!({
            "hooks": {
                "SessionStart": [{
                    "matcher": "startup",
                    "hooks": [{
                        "type": "command",
                        "command": "agent-lens hook session-start summary",
                    }],
                }],
                "PreToolUse": [{
                    "matcher": "Write",
                    "hooks": [
                        {"type": "command", "command": "agent-lens hook pre-tool-use complexity"},
                        {"type": "command", "command": "agent-lens hook pre-tool-use cohesion"},
                    ],
                }],
                "PostToolUse": [{
                    "matcher": "Write",
                    "hooks": [{
                        "type": "command",
                        "command": "agent-lens hook post-tool-use similarity",
                    }],
                }],
            },
        });
        fs::write(&path, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

        let plan = plan(path).unwrap();
        assert_eq!(
            plan.added_commands,
            vec!["agent-lens hook post-tool-use wrapper".to_string()],
            "only the wrapper handler should be queued for install"
        );
    }

    #[test]
    fn tolerates_existing_command_with_trailing_args() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        let existing = json!({
            "hooks": {
                "SessionStart": [{
                    "matcher": SESSION_START_MATCHER,
                    "hooks": [
                        {"type": "command", "command": "agent-lens hook session-start summary --quiet"},
                    ],
                }],
                "PreToolUse": [{
                    "matcher": "Edit|Write",
                    "hooks": [
                        {"type": "command", "command": "agent-lens hook pre-tool-use complexity --foo"},
                        {"type": "command", "command": "agent-lens hook pre-tool-use cohesion"},
                    ],
                }],
                "PostToolUse": [{
                    "matcher": "Edit|Write",
                    "hooks": [
                        {"type": "command", "command": "agent-lens hook post-tool-use similarity --threshold 0.9"},
                        {"type": "command", "command": "agent-lens hook post-tool-use wrapper"},
                    ],
                }],
            },
        });
        fs::write(&path, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

        let plan = plan(path).unwrap();
        assert!(
            plan.added_commands.is_empty(),
            "trailing args should not trigger reinstall, got {:?}",
            plan.added_commands
        );
        assert!(!plan.changed());
    }

    #[test]
    fn path_qualified_commands_are_not_reinstalled() {
        // A settings file that invokes agent-lens through an explicit
        // binary path (as this repo's own settings.json does for the dev
        // build) must not have its pre/post handlers duplicated on
        // re-setup — only the genuinely-absent session-start summary is
        // queued.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        let existing = json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": PRE_TOOL_USE_MATCHER,
                    "hooks": [
                        {"type": "command", "command": "\"$CLAUDE_PROJECT_DIR\"/target/debug/agent-lens hook pre-tool-use complexity"},
                        {"type": "command", "command": "\"$CLAUDE_PROJECT_DIR\"/target/debug/agent-lens hook pre-tool-use cohesion"},
                    ],
                }],
                "PostToolUse": [{
                    "matcher": POST_TOOL_USE_MATCHER,
                    "hooks": [
                        {"type": "command", "command": "\"$CLAUDE_PROJECT_DIR\"/target/debug/agent-lens hook post-tool-use similarity"},
                        {"type": "command", "command": "\"$CLAUDE_PROJECT_DIR\"/target/debug/agent-lens hook post-tool-use wrapper"},
                    ],
                }],
            },
        });
        fs::write(&path, serde_json::to_string_pretty(&existing).unwrap()).unwrap();

        let plan = plan(path).unwrap();
        assert_eq!(
            plan.added_commands,
            vec!["agent-lens hook session-start summary".to_string()],
            "path-qualified pre/post handlers must not be reinstalled",
        );
        let pre = plan.after["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1, "PreToolUse must not gain a duplicate block");
        let post = plan.after["hooks"]["PostToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 1, "PostToolUse must not gain a duplicate block");
    }

    #[test]
    fn empty_file_is_treated_as_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "   \n").unwrap();

        let plan = plan(path).unwrap();
        assert!(plan.before.is_none());
        assert!(plan.changed());
    }

    #[test]
    fn invalid_json_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, "{not json").unwrap();

        let err = plan(path).unwrap_err();
        assert!(matches!(err, SetupError::InvalidJson { .. }));
    }

    #[test]
    fn unexpected_shape_for_hooks_field_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"hooks": "nope"}"#).unwrap();

        let err = plan(path).unwrap_err();
        assert!(
            matches!(err, SetupError::UnexpectedShape { ref field, .. } if field == "hooks"),
            "expected UnexpectedShape at hooks, got {err:?}",
        );
    }

    #[test]
    fn unexpected_shape_for_post_tool_use_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"hooks": {"PostToolUse": {}}}"#).unwrap();

        let err = plan(path).unwrap_err();
        assert!(
            matches!(
                err,
                SetupError::UnexpectedShape { ref field, .. } if field == "hooks.PostToolUse"
            ),
            "expected UnexpectedShape at hooks.PostToolUse, got {err:?}",
        );
    }

    #[test]
    fn resolve_path_project_joins_relative() {
        let root = Path::new("/tmp/proj");
        let p = resolve_path(SettingsScope::Project, root).unwrap();
        assert_eq!(p, root.join(".claude/settings.json"));
    }

    #[test]
    fn setup_error_home_not_found_display_is_descriptive() {
        let err = SetupError::HomeNotFound;
        let msg = err.to_string();
        assert!(msg.contains("$HOME"), "got {msg}");
        assert!(msg.contains("user-scope"), "got {msg}");
    }

    #[test]
    fn setup_error_io_display_includes_path_and_source() {
        let err = SetupError::Io {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/x"), "got {msg}");
        assert!(msg.contains("denied"), "got {msg}");
        assert!(msg.contains("failed to access"), "got {msg}");
    }

    #[test]
    fn setup_error_invalid_json_display_includes_path() {
        let serde_err = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        let err = SetupError::InvalidJson {
            path: PathBuf::from("/tmp/settings.json"),
            source: serde_err,
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/settings.json"), "got {msg}");
        assert!(msg.contains("not valid JSON"), "got {msg}");
    }

    #[test]
    fn setup_error_unexpected_shape_display_includes_field() {
        let err = SetupError::UnexpectedShape {
            path: PathBuf::from("/tmp/settings.json"),
            field: "hooks.PostToolUse".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/settings.json"), "got {msg}");
        assert!(msg.contains(".hooks.PostToolUse"), "got {msg}");
    }

    #[test]
    fn setup_error_io_and_invalid_json_have_source() {
        use std::error::Error as _;
        let io_err = SetupError::Io {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::other("boom"),
        };
        assert!(io_err.source().is_some());

        let serde_err = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        let json_err = SetupError::InvalidJson {
            path: PathBuf::from("/tmp/x"),
            source: serde_err,
        };
        assert!(json_err.source().is_some());
    }

    #[test]
    fn setup_error_variants_without_source_return_none() {
        use std::error::Error as _;
        let err = SetupError::HomeNotFound;
        assert!(err.source().is_none());
        let err = SetupError::UnexpectedShape {
            path: PathBuf::from("/tmp/x"),
            field: "hooks".into(),
        };
        assert!(err.source().is_none());
    }

    #[test]
    fn read_existing_propagates_non_not_found_io_errors() {
        // Pointing at a directory rather than a file makes
        // `fs::read_to_string` fail with an ErrorKind other than NotFound
        // (typically IsADirectory on Linux). The match guard must NOT
        // swallow this as "no settings file" — it has to surface as Io.
        let dir = TempDir::new().unwrap();
        let plan_dir = dir.path().join(".claude/settings.json");
        std::fs::create_dir_all(&plan_dir).unwrap();
        let err = plan(plan_dir).unwrap_err();
        assert!(
            matches!(err, SetupError::Io { .. }),
            "expected Io error for directory-as-file, got {err:?}",
        );
    }
}
