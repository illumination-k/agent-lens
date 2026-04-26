//! `codex-hook setup` — wire `agent-lens`'s PostToolUse handlers into a
//! Codex `config.toml` so users don't have to hand-edit it.
//!
//! Codex's hook config lives under `$CODEX_HOME/config.toml` (or, when
//! `CODEX_HOME` is unset, `$HOME/.codex/config.toml`). Each handler is a
//! `[[hooks.post_tool_use]]` block whose `command` array is the argv we
//! want Codex to spawn.
//!
//! The merge mirrors the Claude Code setup: existing tables are
//! preserved, comments and formatting on adjacent keys survive thanks to
//! `toml_edit`, and a handler is installed only when no existing entry
//! already starts with the same argv prefix. Re-running is a no-op once
//! every handler is wired up.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, Value};

const CONFIG_RELATIVE: &str = ".codex/config.toml";
const CONFIG_FILENAME: &str = "config.toml";

/// Argv prefixes the setup writes into `[[hooks.post_tool_use]]`. Each
/// inner slice is one handler; presence is checked by prefix-matching
/// the existing `command` array, so user-added trailing flags don't
/// trigger a re-install.
pub const POST_TOOL_USE_COMMANDS: &[&[&str]] = &[
    &["agent-lens", "codex-hook", "post-tool-use", "similarity"],
    &["agent-lens", "codex-hook", "post-tool-use", "wrapper"],
];

/// Outcome of computing a setup plan against an existing config file.
#[derive(Debug)]
pub struct SetupPlan {
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: String,
    pub added_commands: Vec<Vec<String>>,
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
    pub added_commands: &'a [Vec<String>],
    pub config: &'a str,
}

#[derive(Debug)]
pub enum SetupError {
    /// Neither `CODEX_HOME` nor `HOME` is set, so the config path can't
    /// be resolved.
    HomeNotFound,
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    InvalidToml {
        path: PathBuf,
        source: toml_edit::TomlError,
    },
    /// A field along the `hooks.post_tool_use[].command` path has the
    /// wrong TOML type for us to merge into safely.
    UnexpectedShape { path: PathBuf, field: &'static str },
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HomeNotFound => {
                write!(
                    f,
                    "neither $CODEX_HOME nor $HOME is set; cannot resolve Codex config.toml"
                )
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

/// Resolve the on-disk Codex `config.toml` path.
///
/// `$CODEX_HOME/config.toml` wins if `CODEX_HOME` is set, falling back to
/// `$HOME/.codex/config.toml`. Codex itself uses the same precedence.
pub fn resolve_path() -> Result<PathBuf, SetupError> {
    if let Some(codex_home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(codex_home).join(CONFIG_FILENAME));
    }
    let home = std::env::var_os("HOME").ok_or(SetupError::HomeNotFound)?;
    Ok(PathBuf::from(home).join(CONFIG_RELATIVE))
}

/// Compute the post-merge TOML for `path` without touching the
/// filesystem.
///
/// A missing or empty file produces a plan that creates one. A file
/// that doesn't parse, or whose `hooks.post_tool_use` shape is
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

fn merge(path: &Path, doc: &mut DocumentMut) -> Result<Vec<Vec<String>>, SetupError> {
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
        .entry("post_tool_use")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
    let post_tool_use =
        post_tool_use_item
            .as_array_of_tables_mut()
            .ok_or_else(|| SetupError::UnexpectedShape {
                path: path.to_path_buf(),
                field: "hooks.post_tool_use",
            })?;

    let installed = collect_installed_commands(post_tool_use, path)?;
    let missing: Vec<&[&str]> = POST_TOOL_USE_COMMANDS
        .iter()
        .copied()
        .filter(|cmd| !installed.iter().any(|seen| has_command_prefix(seen, cmd)))
        .collect();

    for cmd in &missing {
        let mut table = Table::new();
        let mut argv = Array::new();
        for word in *cmd {
            argv.push(*word);
        }
        table.insert("command", Item::Value(Value::Array(argv)));
        post_tool_use.push(table);
    }

    Ok(missing
        .iter()
        .map(|cmd| cmd.iter().map(|s| (*s).to_string()).collect())
        .collect())
}

fn collect_installed_commands(
    post_tool_use: &ArrayOfTables,
    path: &Path,
) -> Result<Vec<Vec<String>>, SetupError> {
    let mut out = Vec::new();
    for entry in post_tool_use.iter() {
        let Some(cmd_item) = entry.get("command") else {
            continue;
        };
        let Some(arr) = cmd_item.as_array() else {
            return Err(SetupError::UnexpectedShape {
                path: path.to_path_buf(),
                field: "hooks.post_tool_use[].command",
            });
        };
        let mut argv = Vec::with_capacity(arr.len());
        for value in arr.iter() {
            let Some(s) = value.as_str() else {
                return Err(SetupError::UnexpectedShape {
                    path: path.to_path_buf(),
                    field: "hooks.post_tool_use[].command[]",
                });
            };
            argv.push(s.to_string());
        }
        out.push(argv);
    }
    Ok(out)
}

/// True when `existing` is the same handler as `wanted`, modulo
/// trailing arguments. e.g. `["agent-lens", "codex-hook",
/// "post-tool-use", "similarity", "--threshold", "0.9"]` counts as the
/// `similarity` handler already being installed.
fn has_command_prefix(existing: &[String], wanted: &[&str]) -> bool {
    if existing.len() < wanted.len() {
        return false;
    }
    existing
        .iter()
        .zip(wanted.iter())
        .all(|(a, b)| a.as_str() == *b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn parse(text: &str) -> DocumentMut {
        text.parse().unwrap()
    }

    fn argv_at(doc: &DocumentMut, index: usize) -> Vec<String> {
        let aot = doc["hooks"]["post_tool_use"].as_array_of_tables().unwrap();
        aot.get(index).unwrap()["command"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
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
        let aot = doc["hooks"]["post_tool_use"].as_array_of_tables().unwrap();
        assert_eq!(aot.len(), POST_TOOL_USE_COMMANDS.len());
        assert_eq!(
            argv_at(&doc, 0),
            POST_TOOL_USE_COMMANDS[0]
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            argv_at(&doc, 1),
            POST_TOOL_USE_COMMANDS[1]
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>(),
        );
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

[[hooks.post_tool_use]]
command = [\"echo\", \"done\"]
";
        fs::write(&path, existing).unwrap();

        let plan = plan(path.clone()).unwrap();
        apply(&plan).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("model = \"gpt-5\""));
        let doc = parse(&after);
        let aot = doc["hooks"]["post_tool_use"].as_array_of_tables().unwrap();
        assert_eq!(aot.len(), 1 + POST_TOOL_USE_COMMANDS.len());
        assert_eq!(
            argv_at(&doc, 0),
            vec!["echo".to_string(), "done".to_string()],
            "existing hook entry should still be in place",
        );
    }

    #[test]
    fn skips_command_already_installed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let existing = "\
[[hooks.post_tool_use]]
command = [\"agent-lens\", \"codex-hook\", \"post-tool-use\", \"similarity\"]
";
        fs::write(&path, existing).unwrap();

        let plan = plan(path).unwrap();
        assert_eq!(
            plan.added_commands,
            vec![vec![
                "agent-lens".to_string(),
                "codex-hook".to_string(),
                "post-tool-use".to_string(),
                "wrapper".to_string(),
            ],],
            "only the wrapper handler should be queued for install",
        );
    }

    #[test]
    fn tolerates_existing_command_with_trailing_args() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let existing = "\
[[hooks.post_tool_use]]
command = [\"agent-lens\", \"codex-hook\", \"post-tool-use\", \"similarity\", \"--threshold\", \"0.9\"]

[[hooks.post_tool_use]]
command = [\"agent-lens\", \"codex-hook\", \"post-tool-use\", \"wrapper\"]
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
        fs::write(&path, "[hooks]\npost_tool_use = \"oops\"\n").unwrap();

        let err = plan(path).unwrap_err();
        assert!(matches!(
            err,
            SetupError::UnexpectedShape {
                field: "hooks.post_tool_use",
                ..
            }
        ));
    }

    #[test]
    fn has_command_prefix_only_matches_at_argument_boundary() {
        assert!(has_command_prefix(
            &[
                "agent-lens".into(),
                "codex-hook".into(),
                "post-tool-use".into(),
                "similarity".into(),
            ],
            &["agent-lens", "codex-hook", "post-tool-use", "similarity"],
        ));
        assert!(has_command_prefix(
            &[
                "agent-lens".into(),
                "codex-hook".into(),
                "post-tool-use".into(),
                "similarity".into(),
                "--threshold".into(),
                "0.9".into(),
            ],
            &["agent-lens", "codex-hook", "post-tool-use", "similarity"],
        ));
        // Argv shorter than the wanted prefix isn't a match.
        assert!(!has_command_prefix(
            &["agent-lens".into(), "codex-hook".into()],
            &["agent-lens", "codex-hook", "post-tool-use", "similarity"],
        ));
        // Sibling handler whose name happens to share a prefix would
        // not be confused for the `wrapper` install — argv elements
        // are compared word-for-word, never as substrings.
        assert!(!has_command_prefix(
            &[
                "agent-lens".into(),
                "codex-hook".into(),
                "post-tool-use".into(),
                "wrapperx".into(),
            ],
            &["agent-lens", "codex-hook", "post-tool-use", "wrapper"],
        ));
    }
}
