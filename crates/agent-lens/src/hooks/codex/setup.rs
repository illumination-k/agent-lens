//! `codex-hook setup` — wire `agent-lens`'s PostToolUse handlers into a
//! Codex `config.toml` so users don't have to hand-edit it.
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
//! The merge mirrors the Claude Code setup: existing tables are
//! preserved, comments and formatting on adjacent keys survive thanks
//! to `toml_edit`, and a handler is installed only when no existing
//! `[[hooks.PostToolUse.hooks]]` entry already starts with the same
//! command. Re-running is a no-op once every handler is wired up.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

const CONFIG_RELATIVE: &str = ".codex/config.toml";

/// Regex Codex matches the just-finished tool name against. `apply_patch`
/// is the only source-modifying tool today and is the one our handlers
/// care about; anchoring keeps a future `apply_patch_v2` from sneaking
/// in.
pub const POST_TOOL_USE_MATCHER: &str = "^apply_patch$";

/// Commands the setup writes into `[[hooks.PostToolUse.hooks]]`. One
/// entry per installed handler; matching against the leading prefix of
/// an existing `command` string makes the merge tolerant of user-added
/// flags.
pub const POST_TOOL_USE_COMMANDS: &[&str] = &[
    "agent-lens codex-hook post-tool-use similarity",
    "agent-lens codex-hook post-tool-use wrapper",
];

/// Where to install the hook entries.
#[derive(Debug, Clone, Copy)]
pub enum ConfigScope {
    /// `<project_root>/.codex/config.toml` (created if missing).
    Project,
    /// `$HOME/.codex/config.toml` (created if missing). This is Codex's
    /// canonical location and the default.
    User,
}

/// Outcome of computing a setup plan against an existing config file.
#[derive(Debug)]
pub struct SetupPlan {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: String,
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
    pub config: &'a str,
}

#[derive(Debug)]
pub enum SetupError {
    /// `$HOME` is not set, so the user-scope path can't be resolved.
    HomeNotFound,
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    InvalidToml {
        path: PathBuf,
        source: toml_edit::TomlError,
    },
    /// A field along the `hooks.PostToolUse[].hooks[].command` path has
    /// the wrong TOML type for us to merge into safely.
    UnexpectedShape { path: PathBuf, field: &'static str },
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HomeNotFound => {
                write!(f, "$HOME is not set; cannot resolve user-scope config.toml")
            }
            Self::Io { path, source } => {
                write!(f, "failed to access {}: {source}", path.display())
            }
            Self::InvalidToml { path, source } => {
                write!(f, "{} is not valid TOML: {source}", path.display())
            }
            Self::UnexpectedShape { path, field } => {
                write!(f, "{} has an unexpected shape at .{field}", path.display())
            }
        }
    }
}

impl std::error::Error for SetupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::InvalidToml { source, .. } => Some(source),
            Self::HomeNotFound | Self::UnexpectedShape { .. } => None,
        }
    }
}

/// Resolve the on-disk Codex `config.toml` path for the requested scope.
///
/// `project_root` is only consulted for [`ConfigScope::Project`]. The
/// caller is expected to supply the actual project root (e.g. `git
/// rev-parse --show-toplevel`, falling back to `cwd`); this function
/// just joins the relative path so it stays trivially testable.
pub fn resolve_path(scope: ConfigScope, project_root: &Path) -> Result<PathBuf, SetupError> {
    match scope {
        ConfigScope::Project => Ok(project_root.join(CONFIG_RELATIVE)),
        ConfigScope::User => {
            let home = std::env::var_os("HOME").ok_or(SetupError::HomeNotFound)?;
            Ok(PathBuf::from(home).join(CONFIG_RELATIVE))
        }
    }
}

/// Compute the post-merge TOML for `path` without touching the
/// filesystem.
///
/// A missing or empty file produces a plan that creates one. A file
/// that doesn't parse, or whose `hooks.PostToolUse` shape is
/// incompatible, is reported as an error so the user can inspect it
/// before we clobber anything.
pub fn plan(path: PathBuf) -> Result<SetupPlan, SetupError> {
    let before = read_existing(&path)?;
    let mut doc = match before.as_deref() {
        Some(s) => s
            .parse::<DocumentMut>()
            .map_err(|source| SetupError::InvalidToml {
                path: path.clone(),
                source,
            })?,
        None => DocumentMut::new(),
    };
    let added_commands = merge(&path, &mut doc)?;
    Ok(SetupPlan {
        path,
        before,
        after: doc.to_string(),
        added_commands,
    })
}

/// Write the planned TOML to disk, creating parent directories if
/// needed.
pub fn apply(plan: &SetupPlan) -> Result<(), SetupError> {
    if let Some(parent) = plan.path.parent() {
        fs::create_dir_all(parent).map_err(|source| SetupError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    fs::write(&plan.path, &plan.after).map_err(|source| SetupError::Io {
        path: plan.path.clone(),
        source,
    })
}

fn read_existing(path: &Path) -> Result<Option<String>, SetupError> {
    match fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(None),
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SetupError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn merge(path: &Path, doc: &mut DocumentMut) -> Result<Vec<String>, SetupError> {
    let hooks_item = doc.as_table_mut().entry("hooks").or_insert_with(|| {
        let mut t = Table::new();
        t.set_implicit(true);
        Item::Table(t)
    });
    let hooks = hooks_item
        .as_table_mut()
        .ok_or_else(|| SetupError::UnexpectedShape {
            path: path.to_path_buf(),
            field: "hooks",
        })?;

    let post_tool_use_item = hooks
        .entry("PostToolUse")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let post_tool_use =
        post_tool_use_item
            .as_array_of_tables_mut()
            .ok_or_else(|| SetupError::UnexpectedShape {
                path: path.to_path_buf(),
                field: "hooks.PostToolUse",
            })?;

    let installed = collect_installed_commands(post_tool_use, path)?;
    let missing: Vec<&str> = POST_TOOL_USE_COMMANDS
        .iter()
        .copied()
        .filter(|cmd| !installed.iter().any(|seen| has_command_prefix(seen, cmd)))
        .collect();

    if !missing.is_empty() {
        let mut group = Table::new();
        group.insert("matcher", value(POST_TOOL_USE_MATCHER));
        let mut handlers = ArrayOfTables::new();
        for cmd in &missing {
            let mut handler = Table::new();
            handler.insert("type", value("command"));
            handler.insert("command", value(*cmd));
            handlers.push(handler);
        }
        group.insert("hooks", Item::ArrayOfTables(handlers));
        post_tool_use.push(group);
    }

    Ok(missing.iter().map(|s| (*s).to_string()).collect())
}

fn collect_installed_commands(
    post_tool_use: &ArrayOfTables,
    path: &Path,
) -> Result<Vec<String>, SetupError> {
    let mut out = Vec::new();
    for group in post_tool_use.iter() {
        let Some(handlers_item) = group.get("hooks") else {
            continue;
        };
        let Some(handlers) = handlers_item.as_array_of_tables() else {
            return Err(SetupError::UnexpectedShape {
                path: path.to_path_buf(),
                field: "hooks.PostToolUse[].hooks",
            });
        };
        for handler in handlers.iter() {
            let Some(cmd_item) = handler.get("command") else {
                continue;
            };
            let Some(cmd) = cmd_item.as_str() else {
                return Err(SetupError::UnexpectedShape {
                    path: path.to_path_buf(),
                    field: "hooks.PostToolUse[].hooks[].command",
                });
            };
            out.push(cmd.to_string());
        }
    }
    Ok(out)
}

/// True when `existing` is the same handler as `wanted`, modulo trailing
/// arguments. e.g. `agent-lens codex-hook post-tool-use similarity --threshold 0.9`
/// counts as the `similarity` handler already being installed.
fn has_command_prefix(existing: &str, wanted: &str) -> bool {
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
    use tempfile::TempDir;

    fn parse(text: &str) -> DocumentMut {
        text.parse().unwrap()
    }

    #[test]
    fn plan_for_missing_file_writes_every_handler() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".codex/config.toml");

        let plan = plan(path.clone()).unwrap();
        assert!(plan.before.is_none());
        assert!(plan.changed());
        assert_eq!(plan.added_commands.len(), POST_TOOL_USE_COMMANDS.len());

        let doc = parse(&plan.after);
        let groups = doc["hooks"]["PostToolUse"].as_array_of_tables().unwrap();
        assert_eq!(groups.len(), 1, "all handlers go under one matcher group");
        assert_eq!(
            groups.get(0).unwrap()["matcher"].as_str().unwrap(),
            POST_TOOL_USE_MATCHER,
        );
        let handlers = groups.get(0).unwrap()["hooks"]
            .as_array_of_tables()
            .unwrap();
        assert_eq!(handlers.len(), POST_TOOL_USE_COMMANDS.len());
        for (handler, expected) in handlers.iter().zip(POST_TOOL_USE_COMMANDS.iter()) {
            assert_eq!(handler["type"].as_str().unwrap(), "command");
            assert_eq!(handler["command"].as_str().unwrap(), *expected);
        }
    }

    #[test]
    fn apply_creates_parent_dir_and_writes_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".codex/config.toml");

        let plan = plan(path.clone()).unwrap();
        apply(&plan).unwrap();

        assert!(path.exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), plan.after);
    }

    #[test]
    fn rerunning_setup_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(".codex/config.toml");

        let first = plan(path.clone()).unwrap();
        apply(&first).unwrap();

        let second = plan(path.clone()).unwrap();
        assert!(!second.changed(), "second plan should be a no-op");
        assert!(second.added_commands.is_empty());
        assert_eq!(second.before.as_deref(), Some(second.after.as_str()));
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

    #[test]
    fn skips_command_already_installed_under_other_matcher() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let existing = "\
[[hooks.PostToolUse]]
matcher = \"\"

[[hooks.PostToolUse.hooks]]
type = \"command\"
command = \"agent-lens codex-hook post-tool-use similarity\"
";
        fs::write(&path, existing).unwrap();

        let plan = plan(path).unwrap();
        assert_eq!(
            plan.added_commands,
            vec!["agent-lens codex-hook post-tool-use wrapper".to_string()],
            "only the wrapper handler should be queued for install",
        );
    }

    #[test]
    fn tolerates_existing_command_with_trailing_args() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let existing = "\
[[hooks.PostToolUse]]
matcher = \"^apply_patch$\"

[[hooks.PostToolUse.hooks]]
type = \"command\"
command = \"agent-lens codex-hook post-tool-use similarity --threshold 0.9\"

[[hooks.PostToolUse.hooks]]
type = \"command\"
command = \"agent-lens codex-hook post-tool-use wrapper\"
";
        fs::write(&path, existing).unwrap();

        let plan = plan(path).unwrap();
        assert!(
            plan.added_commands.is_empty(),
            "trailing args should not trigger reinstall, got {:?}",
            plan.added_commands,
        );
        assert!(!plan.changed());
    }

    #[test]
    fn handler_without_command_field_is_ignored() {
        // A `type = "prompt"` or `type = "agent"` handler has no
        // `command` field; we should skip it instead of erroring out.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let existing = "\
[[hooks.PostToolUse]]
matcher = \"^apply_patch$\"

[[hooks.PostToolUse.hooks]]
type = \"prompt\"
";
        fs::write(&path, existing).unwrap();

        let plan = plan(path).unwrap();
        assert_eq!(plan.added_commands.len(), POST_TOOL_USE_COMMANDS.len());
    }

    #[test]
    fn empty_file_is_treated_as_missing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "   \n").unwrap();

        let plan = plan(path).unwrap();
        assert!(plan.before.is_none());
        assert!(plan.changed());
    }

    #[test]
    fn invalid_toml_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "this = is = not = toml").unwrap();

        let err = plan(path).unwrap_err();
        assert!(matches!(err, SetupError::InvalidToml { .. }));
    }

    #[test]
    fn unexpected_shape_for_hooks_field_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "hooks = \"nope\"\n").unwrap();

        let err = plan(path).unwrap_err();
        assert!(
            matches!(err, SetupError::UnexpectedShape { field: "hooks", .. }),
            "expected UnexpectedShape at hooks, got {err:?}",
        );
    }

    #[test]
    fn unexpected_shape_for_post_tool_use_is_reported() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[hooks]\nPostToolUse = \"oops\"\n").unwrap();

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
        let p = resolve_path(ConfigScope::Project, root).unwrap();
        assert_eq!(p, root.join(".codex/config.toml"));
    }

    #[test]
    fn has_command_prefix_only_matches_at_argument_boundary() {
        assert!(has_command_prefix(
            "agent-lens codex-hook post-tool-use similarity",
            "agent-lens codex-hook post-tool-use similarity",
        ));
        assert!(has_command_prefix(
            "agent-lens codex-hook post-tool-use similarity --threshold 0.9",
            "agent-lens codex-hook post-tool-use similarity",
        ));
        assert!(!has_command_prefix(
            "agent-lens codex-hook post-tool-use wrapperx",
            "agent-lens codex-hook post-tool-use wrapper",
        ));
    }
}
