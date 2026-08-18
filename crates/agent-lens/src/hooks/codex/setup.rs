//! Codex's `config.toml` format for the setup engine.
//!
//! Codex's hook config is the same shape as Claude Code's, just spelled
//! in TOML: a `[[hooks.PostToolUse]]` block declares an optional
//! `matcher` regex and a list of `[[hooks.PostToolUse.hooks]]` handlers
//! whose `command` is a single shell string (see
//! <https://developers.openai.com/codex/hooks> and `codex-rs/core/
//! config.schema.json` in `openai/codex`). Codex looks at four
//! locations: `~/.codex/config.toml`, `~/.codex/hooks.json`, and the
//! same two under `<repo>/.codex/`. We only touch `config.toml`.
//!
//! [`CodexConfig`] supplies the TOML-specific half to
//! [`crate::hooks::setup_engine`]; the merge control flow is shared with
//! the Claude Code setup. Existing tables are preserved, comments and
//! formatting on adjacent keys survive thanks to `toml_edit`, and a
//! handler is installed only when no existing
//! `[[hooks.PostToolUse.hooks]]` entry already starts with the same
//! command. Re-running is a no-op once every handler is wired up.

use std::path::Path;

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

use crate::hooks::setup_engine::{
    ConfigFormat, EventBlock, POST_TOOL_USE_EVENT, PRE_TOOL_USE_EVENT, SESSION_START_EVENT,
    SetupError,
};

const CONFIG_RELATIVE: &str = ".codex/config.toml";

/// Regex Codex matches the just-finished tool name against. `apply_patch`
/// is the only source-modifying tool today and is the one our handlers
/// care about; anchoring keeps a future `apply_patch_v2` from sneaking
/// in.
pub const POST_TOOL_USE_MATCHER: &str = APPLY_PATCH_MATCHER;

/// Commands the setup writes into `[[hooks.PostToolUse.hooks]]`. One
/// entry per installed handler; matching against the leading prefix of
/// an existing `command` string makes the merge tolerant of user-added
/// flags.
pub const POST_TOOL_USE_COMMANDS: &[&str] = &[
    "agent-lens codex-hook post-tool-use similarity",
    "agent-lens codex-hook post-tool-use wrapper",
];

/// Regex Codex matches the about-to-run tool name against. The pre-edit
/// handlers reason about the same `apply_patch` payload as the post-edit
/// handlers, so the matcher matches [`POST_TOOL_USE_MATCHER`] today.
pub const PRE_TOOL_USE_MATCHER: &str = APPLY_PATCH_MATCHER;

/// Commands the setup writes into `[[hooks.PreToolUse.hooks]]`.
pub const PRE_TOOL_USE_COMMANDS: &[&str] = &[
    "agent-lens codex-hook pre-tool-use complexity",
    "agent-lens codex-hook pre-tool-use cohesion",
];

/// Regex Codex matches the SessionStart `source` field against
/// (`startup` / `resume` / `clear`). A summary on every clear would be
/// noisy, so by default we only fire on a fresh start or a resumed
/// session.
pub const SESSION_START_MATCHER: &str = "^(startup|resume)$";

/// Commands the setup writes into `[[hooks.SessionStart.hooks]]`.
pub const SESSION_START_COMMANDS: &[&str] = &["agent-lens codex-hook session-start summary"];

const APPLY_PATCH_MATCHER: &str = "^apply_patch$";

/// Codex's `config.toml` as a setup target.
#[derive(Debug, Clone, Copy)]
pub struct CodexConfig;

impl ConfigFormat for CodexConfig {
    type Document = DocumentMut;
    /// `toml_edit` round-trips comments and formatting, so the payload
    /// stays raw text and `changed()` compares what would actually land
    /// on disk.
    type Payload = String;

    const RELATIVE_PATH: &'static str = CONFIG_RELATIVE;
    const FILE_LABEL: &'static str = "config.toml";
    const FORMAT: &'static str = "TOML";
    const SUMMARY_KEY: &'static str = "config";
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

    fn read_payload(_path: &Path, text: &str) -> Result<String, SetupError> {
        Ok(text.to_owned())
    }

    fn to_document(path: &Path, payload: Option<&String>) -> Result<DocumentMut, SetupError> {
        match payload {
            Some(text) => text
                .parse::<DocumentMut>()
                .map_err(|source| SetupError::parse(path, Self::FORMAT, source)),
            None => Ok(DocumentMut::new()),
        }
    }

    fn to_payload(document: &DocumentMut) -> String {
        document.to_string()
    }

    fn render(_path: &Path, payload: &String) -> Result<String, SetupError> {
        Ok(payload.clone())
    }

    fn installed_commands(
        document: &mut DocumentMut,
        path: &Path,
        block: &EventBlock,
    ) -> Result<Vec<String>, SetupError> {
        let entries = event_entries(document, path, block)?;
        let mut out = Vec::new();
        for group in entries.iter() {
            let Some(handlers_item) = group.get("hooks") else {
                continue;
            };
            let Some(handlers) = handlers_item.as_array_of_tables() else {
                return Err(SetupError::shape(
                    path,
                    format!("hooks.{}[].hooks", block.event),
                ));
            };
            for handler in handlers.iter() {
                let Some(cmd_item) = handler.get("command") else {
                    continue;
                };
                let Some(cmd) = cmd_item.as_str() else {
                    return Err(SetupError::shape(
                        path,
                        format!("hooks.{}[].hooks[].command", block.event),
                    ));
                };
                out.push(cmd.to_string());
            }
        }
        Ok(out)
    }

    fn append_matcher_group(
        document: &mut DocumentMut,
        path: &Path,
        block: &EventBlock,
        commands: &[String],
    ) -> Result<(), SetupError> {
        let entries = event_entries(document, path, block)?;
        let mut group = Table::new();
        group.insert("matcher", value(block.matcher));
        let mut handlers = ArrayOfTables::new();
        for cmd in commands {
            let mut handler = Table::new();
            handler.insert("type", value("command"));
            handler.insert("command", value(cmd));
            handlers.push(handler);
        }
        group.insert("hooks", Item::ArrayOfTables(handlers));
        entries.push(group);
        Ok(())
    }
}

/// Navigate to the `hooks.<event>` array-of-tables, creating the
/// intermediate table/array when absent, and erroring when an existing
/// field along the path has an incompatible TOML type.
fn event_entries<'a>(
    doc: &'a mut DocumentMut,
    path: &Path,
    block: &EventBlock,
) -> Result<&'a mut ArrayOfTables, SetupError> {
    let hooks_item = doc.as_table_mut().entry("hooks").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    let hooks = hooks_item
        .as_table_mut()
        .ok_or_else(|| SetupError::shape(path, "hooks"))?;
    hooks
        .entry(block.event)
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .ok_or_else(|| SetupError::shape(path, format!("hooks.{}", block.event)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    use crate::hooks::setup_engine::{self, conformance};

    fn plan(path: PathBuf) -> Result<setup_engine::SetupPlan<String>, SetupError> {
        setup_engine::plan::<CodexConfig>(path)
    }

    fn apply(plan: &setup_engine::SetupPlan<String>) -> Result<(), SetupError> {
        setup_engine::apply::<CodexConfig>(plan)
    }

    fn parse(text: &str) -> DocumentMut {
        text.parse().unwrap()
    }

    /// The engine contract, run against the TOML format. Bodies live in
    /// [`conformance`] so Claude Code's `settings.json` is held to the
    /// same ones.
    #[rstest]
    #[case::missing_file_installs_everything(
        conformance::plan_for_missing_file_installs_every_command::<CodexConfig>
    )]
    #[case::apply_creates_parent_dir(
        conformance::apply_creates_parent_dir_and_writes_file::<CodexConfig>
    )]
    #[case::rerun_is_idempotent(conformance::rerunning_setup_is_idempotent::<CodexConfig>)]
    #[case::empty_file_is_missing(conformance::empty_file_is_treated_as_missing::<CodexConfig>)]
    #[case::directory_in_place_of_file(
        conformance::directory_in_place_of_file_surfaces_io_error::<CodexConfig>
    )]
    #[case::project_scope_path(conformance::resolve_path_project_joins_relative::<CodexConfig>)]
    fn engine_contract(#[case] assertion: fn()) {
        assertion();
    }

    #[test]
    fn invalid_toml_is_reported() {
        conformance::unparsable_file_is_reported::<CodexConfig>("this = is = not = toml");
    }

    #[rstest]
    #[case::hooks_is_not_a_table("hooks = \"nope\"\n", "hooks")]
    #[case::event_is_not_an_array_of_tables(
        "[hooks]\nPostToolUse = \"oops\"\n",
        "hooks.PostToolUse"
    )]
    fn unexpected_shape_is_reported(#[case] contents: &str, #[case] field: &str) {
        conformance::unexpected_shape_is_reported::<CodexConfig>(contents, field);
    }

    #[test]
    fn plan_for_missing_file_writes_every_handler() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".codex/config.toml");

        let plan = plan(path.clone()).unwrap();
        assert!(plan.before.is_none());
        assert!(plan.changed());
        assert_eq!(
            plan.added_commands.len(),
            SESSION_START_COMMANDS.len()
                + PRE_TOOL_USE_COMMANDS.len()
                + POST_TOOL_USE_COMMANDS.len(),
        );

        let doc = parse(&plan.after);
        for (event, matcher, expected_commands) in [
            (
                "SessionStart",
                SESSION_START_MATCHER,
                SESSION_START_COMMANDS,
            ),
            ("PreToolUse", PRE_TOOL_USE_MATCHER, PRE_TOOL_USE_COMMANDS),
            ("PostToolUse", POST_TOOL_USE_MATCHER, POST_TOOL_USE_COMMANDS),
        ] {
            let groups = doc["hooks"][event].as_array_of_tables().unwrap();
            assert_eq!(
                groups.len(),
                1,
                "all {event} handlers go under one matcher group",
            );
            assert_eq!(groups.get(0).unwrap()["matcher"].as_str().unwrap(), matcher);
            let handlers = groups.get(0).unwrap()["hooks"]
                .as_array_of_tables()
                .unwrap();
            assert_eq!(handlers.len(), expected_commands.len());
            for (handler, expected) in handlers.iter().zip(expected_commands.iter()) {
                assert_eq!(handler["type"].as_str().unwrap(), "command");
                assert_eq!(handler["command"].as_str().unwrap(), *expected);
            }
        }
    }

    #[test]
    fn preserves_unrelated_keys_and_existing_hooks() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let existing = "\
model = \"gpt-5\"

[[hooks.PostToolUse]]
matcher = \"^Bash$\"

[[hooks.PostToolUse.hooks]]
type = \"command\"
command = \"echo done\"
";
        fs::write(&path, existing).unwrap();

        let plan = plan(path.clone()).unwrap();
        apply(&plan).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("model = \"gpt-5\""));
        let doc = parse(&after);
        let groups = doc["hooks"]["PostToolUse"].as_array_of_tables().unwrap();
        assert_eq!(
            groups.len(),
            2,
            "existing matcher group should still be in place",
        );
        assert_eq!(
            groups.get(0).unwrap()["matcher"].as_str().unwrap(),
            "^Bash$"
        );
        assert_eq!(
            groups.get(0).unwrap()["hooks"]
                .as_array_of_tables()
                .unwrap()
                .get(0)
                .unwrap()["command"]
                .as_str()
                .unwrap(),
            "echo done",
        );
        assert_eq!(
            groups.get(1).unwrap()["matcher"].as_str().unwrap(),
            POST_TOOL_USE_MATCHER,
        );
    }

    /// What the merge queues against a file that already carries some
    /// of the handlers. The bodies are shared with the Claude Code
    /// setup, so only the TOML spelling of each situation lives here.
    #[rstest]
    // Every handler already installed, each under a non-canonical
    // matcher, except the post-tool-use wrapper.
    #[case::installed_under_other_matcher(
        "\
[[hooks.SessionStart]]
matcher = \"^startup$\"

[[hooks.SessionStart.hooks]]
type = \"command\"
command = \"agent-lens codex-hook session-start summary\"

[[hooks.PreToolUse]]
matcher = \"\"

[[hooks.PreToolUse.hooks]]
type = \"command\"
command = \"agent-lens codex-hook pre-tool-use complexity\"

[[hooks.PreToolUse.hooks]]
type = \"command\"
command = \"agent-lens codex-hook pre-tool-use cohesion\"

[[hooks.PostToolUse]]
matcher = \"\"

[[hooks.PostToolUse.hooks]]
type = \"command\"
command = \"agent-lens codex-hook post-tool-use similarity\"
",
        &["agent-lens codex-hook post-tool-use wrapper"]
    )]
    // User-added flags on an installed command must not trigger a
    // reinstall of the bare form.
    #[case::trailing_args(
        "\
[[hooks.SessionStart]]
matcher = \"^(startup|resume)$\"

[[hooks.SessionStart.hooks]]
type = \"command\"
command = \"agent-lens codex-hook session-start summary --quiet\"

[[hooks.PreToolUse]]
matcher = \"^apply_patch$\"

[[hooks.PreToolUse.hooks]]
type = \"command\"
command = \"agent-lens codex-hook pre-tool-use complexity --foo\"

[[hooks.PreToolUse.hooks]]
type = \"command\"
command = \"agent-lens codex-hook pre-tool-use cohesion\"

[[hooks.PostToolUse]]
matcher = \"^apply_patch$\"

[[hooks.PostToolUse.hooks]]
type = \"command\"
command = \"agent-lens codex-hook post-tool-use similarity --threshold 0.9\"

[[hooks.PostToolUse.hooks]]
type = \"command\"
command = \"agent-lens codex-hook post-tool-use wrapper\"
",
        &[]
    )]
    // A `type = "prompt"` or `type = "agent"` handler has no `command`
    // field; it is skipped rather than erroring out, so every command is
    // still missing.
    #[case::handler_without_command_field(
        "\
[[hooks.PostToolUse]]
matcher = \"^apply_patch$\"

[[hooks.PostToolUse.hooks]]
type = \"prompt\"
",
        &conformance::all_commands::<CodexConfig>()
    )]
    fn queues_exactly(#[case] existing: &str, #[case] expected: &[&str]) {
        conformance::queues_exactly::<CodexConfig>(existing, expected);
    }
}
