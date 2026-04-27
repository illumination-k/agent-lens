//! `setup` — wire `agent-lens`'s hooks into a Claude Code
//! `settings.json` so users don't have to hand-edit it.
//!
//! The merge is conservative: every existing key is preserved, and a
//! fresh block is appended only with the commands that aren't already
//! wired up anywhere under that event. Re-running the command is a
//! no-op once everything is installed.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::hooks::setup_common;

const SETTINGS_RELATIVE: &str = ".claude/settings.json";

/// Tool matcher used for the PostToolUse block. Mirrors the
/// `EDITING_TOOL_NAMES` constant the handlers themselves filter on.
pub const POST_TOOL_USE_MATCHER: &str = "Edit|Write|MultiEdit";

/// Commands the setup writes into `hooks.PostToolUse`. One entry per
/// installed handler; matching against the leading prefix of an existing
/// `command` string makes the merge tolerant of user-added flags.
pub const POST_TOOL_USE_COMMANDS: &[&str] = &[
    "agent-lens hook post-tool-use similarity",
    "agent-lens hook post-tool-use wrapper",
];

/// Source matcher for the SessionStart block. Claude Code dispatches on
/// the `source` field (`startup` / `resume` / `clear` / `compact`); a
/// summary on every clear/compact would be noisy, so by default we only
/// fire on a fresh start or a resumed session.
pub const SESSION_START_MATCHER: &str = "startup|resume";

/// Commands the setup writes into `hooks.SessionStart`.
pub const SESSION_START_COMMANDS: &[&str] = &["agent-lens hook session-start summary"];

/// Per-event metadata used by [`merge`]. Field labels are baked in as
/// `&'static str` so they can flow into [`SetupError::UnexpectedShape`]
/// without an allocation.
struct EventBlock {
    /// Key under `hooks.` in settings.json (e.g. `"PostToolUse"`).
    event: &'static str,
    /// Field path used in error messages for the outer array.
    array_field: &'static str,
    /// Field path used in error messages for an entry in the array.
    entry_field: &'static str,
    /// Field path used in error messages for an entry's `hooks` array.
    inner_array_field: &'static str,
    /// Matcher string written for this event's block.
    matcher: &'static str,
    /// Commands the setup may install under this event.
    commands: &'static [&'static str],
}

const EVENTS: &[EventBlock] = &[
    EventBlock {
        event: "SessionStart",
        array_field: "hooks.SessionStart",
        entry_field: "hooks.SessionStart[]",
        inner_array_field: "hooks.SessionStart[].hooks",
        matcher: SESSION_START_MATCHER,
        commands: SESSION_START_COMMANDS,
    },
    EventBlock {
        event: "PostToolUse",
        array_field: "hooks.PostToolUse",
        entry_field: "hooks.PostToolUse[]",
        inner_array_field: "hooks.PostToolUse[].hooks",
        matcher: POST_TOOL_USE_MATCHER,
        commands: POST_TOOL_USE_COMMANDS,
    },
];

/// Where to install the hook entries.
#[derive(Debug, Clone, Copy)]
pub enum SettingsScope {
    /// `<project_root>/.claude/settings.json` (created if missing).
    Project,
    /// `$HOME/.claude/settings.json` (created if missing).
    User,
}

/// Outcome of computing a setup plan against an existing settings file.
#[derive(Debug)]
pub struct SetupPlan {
    pub path: PathBuf,
    pub before: Option<Value>,
    pub after: Value,
    pub added_commands: Vec<String>,
}

impl SetupPlan {
    /// Whether applying this plan would change the file on disk.
    pub fn changed(&self) -> bool {
        match &self.before {
            None => true,
            Some(before) => before != &self.after,
        }
    }
}

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
    UnexpectedShape { path: PathBuf, field: &'static str },
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
    let before = read_existing(&path)?;
    let mut after = before.clone().unwrap_or_else(|| Value::Object(Map::new()));
    let added_commands = merge(&path, &mut after)?;
    Ok(SetupPlan {
        path,
        before,
        after,
        added_commands,
    })
}

/// Write the planned JSON to disk, creating parent directories if needed.
pub fn apply(plan: &SetupPlan) -> Result<(), SetupError> {
    if let Some(parent) = plan.path.parent() {
        fs::create_dir_all(parent).map_err(|source| SetupError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let mut text =
        serde_json::to_string_pretty(&plan.after).map_err(|source| SetupError::InvalidJson {
            path: plan.path.clone(),
            source,
        })?;
    text.push('\n');
    fs::write(&plan.path, text).map_err(|source| SetupError::Io {
        path: plan.path.clone(),
        source,
    })
}

fn read_existing(path: &Path) -> Result<Option<Value>, SetupError> {
    match fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(None),
        Ok(s) => serde_json::from_str(&s)
            .map(Some)
            .map_err(|source| SetupError::InvalidJson {
                path: path.to_path_buf(),
                source,
            }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SetupError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn merge(path: &Path, root: &mut Value) -> Result<Vec<String>, SetupError> {
    let root_obj = root
        .as_object_mut()
        .ok_or_else(|| SetupError::UnexpectedShape {
            path: path.to_path_buf(),
            field: "(root)",
        })?;

    let hooks = root_obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| SetupError::UnexpectedShape {
            path: path.to_path_buf(),
            field: "hooks",
        })?;

    let mut added: Vec<String> = Vec::new();
    for block in EVENTS {
        let entries = hooks
            .entry(block.event)
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| SetupError::UnexpectedShape {
                path: path.to_path_buf(),
                field: block.array_field,
            })?;

        let installed = collect_installed_commands(entries, path, block)?;
        let missing: Vec<String> = block
            .commands
            .iter()
            .filter(|cmd| {
                !installed
                    .iter()
                    .any(|seen| setup_common::has_command_prefix(seen, cmd))
            })
            .map(|s| (*s).to_string())
            .collect();

        if !missing.is_empty() {
            entries.push(json!({
                "matcher": block.matcher,
                "hooks": missing
                    .iter()
                    .map(|cmd| json!({ "type": "command", "command": cmd }))
                    .collect::<Vec<_>>(),
            }));
            added.extend(missing);
        }
    }

    Ok(added)
}

fn collect_installed_commands(
    entries: &[Value],
    path: &Path,
    block: &EventBlock,
) -> Result<Vec<String>, SetupError> {
    let mut out = Vec::new();
    for entry in entries {
        let Some(entry_obj) = entry.as_object() else {
            return Err(SetupError::UnexpectedShape {
                path: path.to_path_buf(),
                field: block.entry_field,
            });
        };
        let Some(hooks) = entry_obj.get("hooks") else {
            continue;
        };
        let Some(hooks) = hooks.as_array() else {
            return Err(SetupError::UnexpectedShape {
                path: path.to_path_buf(),
                field: block.inner_array_field,
            });
        };
        for hook in hooks {
            if let Some(cmd) = hook.get("command").and_then(Value::as_str) {
                out.push(cmd.to_string());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
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
            POST_TOOL_USE_COMMANDS.len() + SESSION_START_COMMANDS.len(),
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
    fn plan_installs_session_start_alongside_existing_post_tool_use() {
        // When a user has only the older PostToolUse block, re-running
        // setup should add the new SessionStart block without disturbing
        // the existing one.
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
            vec!["agent-lens hook session-start summary".to_string()],
        );
        assert!(plan.changed());
        let session_start = plan.after["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session_start.len(), 1);
        assert_eq!(session_start[0]["matcher"], SESSION_START_MATCHER);
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
        assert_eq!(
            after["hooks"]["PreToolUse"],
            existing["hooks"]["PreToolUse"]
        );
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
            matches!(err, SetupError::UnexpectedShape { field: "hooks", .. }),
            "expected UnexpectedShape at hooks, got {err:?}",
        );
    }

    #[test]
    fn unexpected_shape_for_post_tool_use_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, r#"{"hooks": {"PostToolUse": {}}}"#).unwrap();

        let err = plan(path).unwrap_err();
        assert!(matches!(
            err,
            SetupError::UnexpectedShape {
                field: "hooks.PostToolUse",
                ..
            }
        ));
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
            field: "hooks.PostToolUse",
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
            field: "hooks",
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
