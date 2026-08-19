//! Claude Code's `settings.json` format for the setup engine.
//!
//! [`ClaudeSettings`] teaches [`crate::hooks::setup_engine`] how to read,
//! navigate and render a `.claude/settings.json`; the merge itself — which
//! commands are missing, where a fresh matcher group goes, the plan/apply
//! split — lives in the engine and is shared with the Codex setup.
//!
//! The merge is conservative: every existing key is preserved, and a
//! fresh block is appended only with the commands that aren't already
//! wired up anywhere under that event. Re-running the command is a
//! no-op once everything is installed.

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::hooks::setup_engine::{
    ConfigFormat, EventBlock, POST_TOOL_USE_EVENT, PRE_TOOL_USE_EVENT, SESSION_START_EVENT,
    SetupError,
};

const SETTINGS_RELATIVE: &str = ".claude/settings.json";

/// Tool matcher used for the PostToolUse block. Mirrors the
/// `EDITING_TOOL_NAMES` constant the handlers themselves filter on.
pub const POST_TOOL_USE_MATCHER: &str = EDITING_TOOL_MATCHER;

/// Commands the setup writes into `hooks.PostToolUse`. One entry per
/// installed handler; matching against the leading prefix of an existing
/// `command` string makes the merge tolerant of user-added flags.
pub const POST_TOOL_USE_COMMANDS: &[&str] = &[
    "agent-lens hook post-tool-use similarity",
    "agent-lens hook post-tool-use wrapper",
];

/// Tool matcher used for the PreToolUse block. Mirrors the
/// `EDITING_TOOL_NAMES` constant the handlers themselves filter on; the
/// value happens to match [`POST_TOOL_USE_MATCHER`] today because the
/// pre/post handlers act on the same set of editing tools.
pub const PRE_TOOL_USE_MATCHER: &str = EDITING_TOOL_MATCHER;

/// Commands the setup writes into `hooks.PreToolUse`. One entry per
/// installed handler; matching against the leading prefix of an existing
/// `command` string makes the merge tolerant of user-added flags.
pub const PRE_TOOL_USE_COMMANDS: &[&str] = &[
    "agent-lens hook pre-tool-use complexity",
    "agent-lens hook pre-tool-use cohesion",
];

/// Source matcher for the SessionStart block. Claude Code dispatches on
/// the `source` field (`startup` / `resume` / `clear` / `compact`); a
/// summary on every clear/compact would be noisy, so by default we only
/// fire on a fresh start or a resumed session.
pub const SESSION_START_MATCHER: &str = "startup|resume";

/// Commands the setup writes into `hooks.SessionStart`.
pub const SESSION_START_COMMANDS: &[&str] = &["agent-lens hook session-start summary"];

const EDITING_TOOL_MATCHER: &str = "Edit|Write|MultiEdit";

/// Claude Code's `settings.json` as a setup target.
#[derive(Debug, Clone, Copy)]
pub struct ClaudeSettings;

impl ConfigFormat for ClaudeSettings {
    /// `serde_json` parses to an owned tree, so the document and the
    /// payload are the same type; `changed()` then compares parsed JSON
    /// and stays blind to reformatting.
    type Document = Value;
    type Payload = Value;

    const RELATIVE_PATH: &'static str = SETTINGS_RELATIVE;
    const FILE_LABEL: &'static str = "settings.json";
    const FORMAT: &'static str = "JSON";
    const SUMMARY_KEY: &'static str = "settings";
    const EVENTS: &'static [EventBlock] = &[
        EventBlock {
            event: SESSION_START_EVENT,
            matcher: SESSION_START_MATCHER,
            commands: SESSION_START_COMMANDS,
        },
        EventBlock {
            event: PRE_TOOL_USE_EVENT,
            matcher: PRE_TOOL_USE_MATCHER,
            commands: PRE_TOOL_USE_COMMANDS,
        },
        EventBlock {
            event: POST_TOOL_USE_EVENT,
            matcher: POST_TOOL_USE_MATCHER,
            commands: POST_TOOL_USE_COMMANDS,
        },
    ];

    fn read_payload(path: &Path, text: &str) -> Result<Value, SetupError> {
        serde_json::from_str(text).map_err(|source| SetupError::parse(path, Self::FORMAT, source))
    }

    fn to_document(_path: &Path, payload: Option<&Value>) -> Result<Value, SetupError> {
        Ok(payload
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new())))
    }

    fn to_payload(document: &Value) -> Value {
        document.clone()
    }

    fn render(path: &Path, payload: &Value) -> Result<String, SetupError> {
        let mut text = serde_json::to_string_pretty(payload)
            .map_err(|source| SetupError::parse(path, Self::FORMAT, source))?;
        text.push('\n');
        Ok(text)
    }

    fn installed_commands(
        document: &mut Value,
        path: &Path,
        block: &EventBlock,
    ) -> Result<Vec<String>, SetupError> {
        let entries = event_entries(document, path, block)?;
        let mut out = Vec::new();
        for entry in entries.iter() {
            let Some(entry_obj) = entry.as_object() else {
                return Err(SetupError::shape(path, format!("hooks.{}[]", block.event)));
            };
            let Some(hooks) = entry_obj.get("hooks") else {
                continue;
            };
            let Some(hooks) = hooks.as_array() else {
                return Err(SetupError::shape(
                    path,
                    format!("hooks.{}[].hooks", block.event),
                ));
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
        document: &mut Value,
        path: &Path,
        block: &EventBlock,
        commands: &[String],
    ) -> Result<(), SetupError> {
        let entries = event_entries(document, path, block)?;
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
        .ok_or_else(|| SetupError::shape(path, "(root)"))?;
    let hooks = root_obj
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| SetupError::shape(path, "hooks"))?;
    hooks
        .entry(block.event)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .ok_or_else(|| SetupError::shape(path, format!("hooks.{}", block.event)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use serde_json::json;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    use crate::hooks::setup_engine::{self, conformance};

    fn plan(path: PathBuf) -> Result<setup_engine::SetupPlan<Value>, SetupError> {
        setup_engine::plan::<ClaudeSettings>(path)
    }

    fn apply(plan: &setup_engine::SetupPlan<Value>) -> Result<(), SetupError> {
        setup_engine::apply::<ClaudeSettings>(plan)
    }

    fn read(path: &Path) -> Value {
        let text = fs::read_to_string(path).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// The engine contract, run against the JSON format. Bodies live in
    /// [`conformance`] so Codex's `config.toml` is held to the same ones.
    #[rstest]
    #[case::missing_file_installs_everything(
        conformance::plan_for_missing_file_installs_every_command::<ClaudeSettings>
    )]
    #[case::apply_creates_parent_dir(
        conformance::apply_creates_parent_dir_and_writes_file::<ClaudeSettings>
    )]
    #[case::rerun_is_idempotent(conformance::rerunning_setup_is_idempotent::<ClaudeSettings>)]
    #[case::empty_file_is_missing(conformance::empty_file_is_treated_as_missing::<ClaudeSettings>)]
    #[case::directory_in_place_of_file(
        conformance::directory_in_place_of_file_surfaces_io_error::<ClaudeSettings>
    )]
    #[case::project_scope_path(
        conformance::resolve_path_project_joins_relative::<ClaudeSettings>
    )]
    fn engine_contract(#[case] assertion: fn()) {
        assertion();
    }

    #[test]
    fn invalid_json_is_reported() {
        conformance::unparsable_file_is_reported::<ClaudeSettings>("{not json");
    }

    #[rstest]
    #[case::hooks_is_not_an_object(r#"{"hooks": "nope"}"#, "hooks")]
    #[case::event_is_not_an_array(r#"{"hooks": {"PostToolUse": {}}}"#, "hooks.PostToolUse")]
    fn unexpected_shape_is_reported(#[case] contents: &str, #[case] field: &str) {
        conformance::unexpected_shape_is_reported::<ClaudeSettings>(contents, field);
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

    /// What the merge queues against a file that already carries some
    /// of the handlers. The bodies are shared with the Codex setup, so
    /// only the JSON spelling of each situation lives here.
    #[rstest]
    // Every handler already installed, each under a non-canonical
    // matcher, except the post-tool-use wrapper.
    #[case::installed_under_other_matcher(
        json!({
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
        }),
        &["agent-lens hook post-tool-use wrapper"]
    )]
    // User-added flags on an installed command must not trigger a
    // reinstall of the bare form.
    #[case::trailing_args(
        json!({
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
        }),
        &[]
    )]
    // A handler entry with no `command` field — Claude Code's own
    // `"type": "prompt"` hooks — is skipped rather than erroring, so
    // every command is still missing.
    #[case::handler_without_command_field(
        json!({
            "hooks": {
                "PostToolUse": [{
                    "matcher": POST_TOOL_USE_MATCHER,
                    "hooks": [{"type": "prompt"}],
                }],
            },
        }),
        &conformance::all_commands::<ClaudeSettings>()
    )]
    fn queues_exactly(#[case] existing: Value, #[case] expected: &[&str]) {
        let text = serde_json::to_string_pretty(&existing).unwrap();
        conformance::queues_exactly::<ClaudeSettings>(&text, expected);
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
}
