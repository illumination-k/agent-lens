//! The engine behind `hook setup` and `codex-hook setup`.
//!
//! Claude Code stores its hook wiring in `.claude/settings.json` and
//! Codex stores the same shape in `.codex/config.toml`. Only the document
//! operations differ: how a file's text becomes a mutable document, how
//! the `hooks.<event>` container is navigated, and how the merged
//! document is rendered back to text. Everything else — path resolution,
//! reading the existing file, the merge loop, command-prefix matching,
//! the plan/apply split, and the JSON summary — is format-independent and
//! lives here.
//!
//! A format plugs in by implementing [`ConfigFormat`]; [`plan`] and
//! [`apply`] are then generic over it, so adding a third agent means
//! writing one impl rather than a third copy of the merge engine.

use std::path::{Path, PathBuf};

use serde::Serialize;
use serde::ser::SerializeStruct as _;

/// Where to install the hook entries. Both setups accept the same two
/// scopes; the concrete file each one resolves to comes from the
/// [`ConfigFormat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum SetupScope {
    /// The project's own config file, under the project root (created if
    /// missing).
    Project,
    /// The user-wide config file under `$HOME` (created if missing).
    User,
}

pub(crate) const SESSION_START_EVENT: &str = "SessionStart";
pub(crate) const PRE_TOOL_USE_EVENT: &str = "PreToolUse";
pub(crate) const POST_TOOL_USE_EVENT: &str = "PostToolUse";

/// Per-event metadata driving the merge loop: which key under `hooks.`
/// the event lives at, the matcher written for a fresh block, and the
/// handler commands the setup may install there.
pub struct EventBlock {
    pub event: &'static str,
    pub matcher: &'static str,
    pub commands: &'static [&'static str],
}

/// Anything that can go wrong while planning or applying a setup.
///
/// The parse failure is boxed rather than generic over the format: both
/// `serde_json::Error` and `toml_edit::TomlError` are only ever rendered
/// for the user, and one error type keeps every signature in this module
/// free of a format parameter.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    /// `$HOME` is not set, so the user-scope path can't be resolved.
    #[error("$HOME is not set; cannot resolve user-scope {file}")]
    HomeNotFound { file: &'static str },
    #[error("failed to access {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path:?} is not valid {format}: {source}")]
    Parse {
        path: PathBuf,
        format: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// A field along the `hooks.<event>[].hooks[].command` path has the
    /// wrong type for us to merge into safely.
    #[error("{path:?} has an unexpected shape at .{field}")]
    UnexpectedShape { path: PathBuf, field: String },
}

impl SetupError {
    /// A parse failure for `path`, tagged with the format that rejected
    /// it so the message names JSON or TOML.
    pub fn parse(
        path: &Path,
        format: &'static str,
        source: impl Into<Box<dyn std::error::Error + Send + Sync>>,
    ) -> Self {
        Self::Parse {
            path: path.to_path_buf(),
            format,
            source: source.into(),
        }
    }

    /// An existing field along the `hooks.<event>` path whose type we
    /// refuse to clobber.
    pub fn shape(path: &Path, field: impl Into<String>) -> Self {
        Self::UnexpectedShape {
            path: path.to_path_buf(),
            field: field.into(),
        }
    }

    fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// A config file format the setup engine can merge hook blocks into.
///
/// Implemented once per agent (JSON `settings.json` for Claude Code,
/// TOML `config.toml` for Codex) by a marker type that carries no data.
/// Both document operations are expected to create the `hooks.<event>`
/// container on demand and to report an incompatible existing shape
/// through [`SetupError::shape`] rather than clobbering it.
pub trait ConfigFormat {
    /// Mutable, in-memory form of the config file the merge operates on.
    type Document;
    /// What a [`SetupPlan`] records as `before` / `after`: parsed JSON
    /// for Claude Code, raw text for the comment-preserving TOML editor.
    type Payload: PartialEq + Serialize;

    /// Config-file path relative to the project root or `$HOME`.
    const RELATIVE_PATH: &'static str;
    /// File name used in error and log messages.
    const FILE_LABEL: &'static str;
    /// Format name used in parse-error messages.
    const FORMAT: &'static str;
    /// Key the setup summary publishes the merged document under.
    const SUMMARY_KEY: &'static str;
    /// Events this format's config file wires up.
    const EVENTS: &'static [EventBlock];

    /// Record the file's current text as the plan's `before` payload.
    fn read_payload(path: &Path, text: &str) -> Result<Self::Payload, SetupError>;

    /// Open a payload for editing, or start an empty document when the
    /// file doesn't exist yet.
    fn to_document(
        path: &Path,
        payload: Option<&Self::Payload>,
    ) -> Result<Self::Document, SetupError>;

    /// Snapshot the merged document as the plan's `after` payload.
    fn to_payload(document: &Self::Document) -> Self::Payload;

    /// Render a payload as the text to write to disk.
    fn render(path: &Path, payload: &Self::Payload) -> Result<String, SetupError>;

    /// Collect every handler command currently installed under
    /// `hooks.<event>`, across all matcher groups.
    fn installed_commands(
        document: &mut Self::Document,
        path: &Path,
        block: &EventBlock,
    ) -> Result<Vec<String>, SetupError>;

    /// Append a fresh matcher group carrying `commands` under
    /// `hooks.<event>`.
    fn append_matcher_group(
        document: &mut Self::Document,
        path: &Path,
        block: &EventBlock,
        commands: &[String],
    ) -> Result<(), SetupError>;
}

/// Outcome of computing a setup plan against an existing config file.
/// `T` is the format's payload representation.
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

/// Compact summary of a setup run, suitable for JSON-on-stdout output.
///
/// The merged document is published under [`ConfigFormat::SUMMARY_KEY`]
/// (`settings` for Claude Code, `config` for Codex), which is why this
/// serializes by hand instead of deriving.
#[derive(Debug)]
pub struct SetupSummary<'a, F: ConfigFormat> {
    pub path: &'a Path,
    pub wrote: bool,
    pub added_commands: &'a [String],
    pub document: &'a F::Payload,
}

impl<F: ConfigFormat> Serialize for SetupSummary<'_, F> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut state = serializer.serialize_struct("SetupSummary", 4)?;
        state.serialize_field("path", self.path)?;
        state.serialize_field("wrote", &self.wrote)?;
        state.serialize_field("added_commands", self.added_commands)?;
        state.serialize_field(F::SUMMARY_KEY, self.document)?;
        state.end()
    }
}

/// Resolve the on-disk config path for the requested scope.
///
/// `project_root` is only consulted for [`SetupScope::Project`]; the
/// caller decides what "the project" means (the current directory for
/// Claude Code, the git working-tree root for Codex).
pub fn resolve_path<F: ConfigFormat>(
    scope: SetupScope,
    project_root: &Path,
) -> Result<PathBuf, SetupError> {
    match scope {
        SetupScope::Project => Ok(project_root.join(F::RELATIVE_PATH)),
        SetupScope::User => {
            crate::paths::home_scoped_path(F::RELATIVE_PATH).ok_or(SetupError::HomeNotFound {
                file: F::FILE_LABEL,
            })
        }
    }
}

/// Compute the post-merge document for `path` without touching the
/// filesystem.
///
/// A missing or empty file produces a plan that creates one. A file that
/// doesn't parse, or with an unexpected shape along the `hooks.<event>`
/// path, is reported as an error so the user can inspect it before we
/// clobber anything.
pub fn plan<F: ConfigFormat>(path: PathBuf) -> Result<SetupPlan<F::Payload>, SetupError> {
    let before = read_existing_text(&path)?
        .map(|text| F::read_payload(&path, &text))
        .transpose()?;
    let mut document = F::to_document(&path, before.as_ref())?;
    let added_commands = merge_hook_commands::<F>(&path, &mut document)?;
    Ok(SetupPlan {
        path,
        before,
        after: F::to_payload(&document),
        added_commands,
    })
}

/// Write the planned document to disk, creating parent directories if
/// needed.
pub fn apply<F: ConfigFormat>(plan: &SetupPlan<F::Payload>) -> Result<(), SetupError> {
    let text = F::render(&plan.path, &plan.after)?;
    write_with_parents(&plan.path, &text)
}

/// The merge itself: for each event, install the commands that aren't
/// already wired up anywhere under that event (modulo trailing arguments
/// and binary path — see [`has_command_prefix`]) as one fresh matcher
/// group. Returns the commands that were added; an empty result means the
/// document was left untouched.
fn merge_hook_commands<F: ConfigFormat>(
    path: &Path,
    document: &mut F::Document,
) -> Result<Vec<String>, SetupError> {
    let mut added: Vec<String> = Vec::new();
    for block in F::EVENTS {
        let installed = F::installed_commands(document, path, block)?;
        let missing: Vec<String> = block
            .commands
            .iter()
            .filter(|cmd| !installed.iter().any(|seen| has_command_prefix(seen, cmd)))
            .map(|s| (*s).to_string())
            .collect();
        if missing.is_empty() {
            continue;
        }
        F::append_matcher_group(document, path, block, &missing)?;
        added.extend(missing);
    }
    Ok(added)
}

/// Read the current contents of a config file, treating a missing or
/// blank file as "not there yet".
fn read_existing_text(path: &Path) -> Result<Option<String>, SetupError> {
    match std::fs::read_to_string(path) {
        Ok(s) if s.trim().is_empty() => Ok(None),
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SetupError::io(path, source)),
    }
}

/// Write `text` to `path`, creating parent directories first.
fn write_with_parents(path: &Path, text: &str) -> Result<(), SetupError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| SetupError::io(parent, source))?;
    }
    std::fs::write(path, text).map_err(|source| SetupError::io(path, source))
}

/// True when `existing` is the same handler invocation as `wanted`,
/// modulo trailing arguments and the path the binary is invoked through.
///
/// Used by both setups so that:
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
fn has_command_prefix(existing: &str, wanted: &str) -> bool {
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
    use rstest::rstest;

    #[rstest]
    #[case::exact("a b c", "a b c", true)]
    #[case::trailing_args("a b c --flag", "a b c", true)]
    #[case::word_extension("a b cx", "a b c", false)]
    // The command may invoke agent-lens through a project-relative or
    // absolute path; setup must still recognise it as the same handler as
    // the bare `agent-lens hook ...` it installs.
    #[case::project_dir_path(
        "\"$CLAUDE_PROJECT_DIR\"/target/debug/agent-lens hook post-tool-use similarity",
        "agent-lens hook post-tool-use similarity",
        true
    )]
    #[case::absolute_path(
        "/usr/local/bin/agent-lens hook pre-tool-use complexity",
        "agent-lens hook pre-tool-use complexity",
        true
    )]
    #[case::path_prefix_with_trailing_args(
        "/opt/agent-lens hook post-tool-use similarity --threshold 0.9",
        "agent-lens hook post-tool-use similarity",
        true
    )]
    // Same binary basename but a different handler must not match, so a
    // Claude command is never mistaken for its Codex counterpart.
    #[case::different_handler(
        "/path/agent-lens hook pre-tool-use complexity",
        "agent-lens codex-hook pre-tool-use complexity",
        false
    )]
    #[case::different_binary(
        "bash \"$CLAUDE_PROJECT_DIR\"/.claude/hooks/session-start.sh",
        "agent-lens hook session-start summary",
        false
    )]
    // Same arguments but a different binary basename must not match: this
    // pins `binary_basename` as load-bearing, so normalising the path can
    // never collapse two genuinely different binaries.
    #[case::different_binary_same_args(
        "/opt/other-tool hook post-tool-use similarity",
        "agent-lens hook post-tool-use similarity",
        false
    )]
    fn has_command_prefix_cases(
        #[case] existing: &str,
        #[case] wanted: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(has_command_prefix(existing, wanted), expected);
    }

    #[test]
    fn home_not_found_display_is_descriptive() {
        let msg = SetupError::HomeNotFound {
            file: "settings.json",
        }
        .to_string();
        assert!(msg.contains("$HOME"), "got {msg}");
        assert!(msg.contains("user-scope"), "got {msg}");
        assert!(msg.contains("settings.json"), "got {msg}");
    }

    #[test]
    fn io_display_includes_path_and_source() {
        let err = SetupError::io(
            Path::new("/tmp/x"),
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        );
        let msg = err.to_string();
        assert!(msg.contains("/tmp/x"), "got {msg}");
        assert!(msg.contains("denied"), "got {msg}");
        assert!(msg.contains("failed to access"), "got {msg}");
    }

    #[test]
    fn parse_display_names_the_format_and_path() {
        let source = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        let err = SetupError::parse(Path::new("/tmp/settings.json"), "JSON", source);
        let msg = err.to_string();
        assert!(msg.contains("/tmp/settings.json"), "got {msg}");
        assert!(msg.contains("not valid JSON"), "got {msg}");
    }

    #[test]
    fn unexpected_shape_display_includes_field() {
        let msg =
            SetupError::shape(Path::new("/tmp/settings.json"), "hooks.PostToolUse").to_string();
        assert!(msg.contains("/tmp/settings.json"), "got {msg}");
        assert!(msg.contains(".hooks.PostToolUse"), "got {msg}");
    }

    #[test]
    fn io_and_parse_carry_a_source() {
        use std::error::Error as _;
        let io_err = SetupError::io(Path::new("/tmp/x"), std::io::Error::other("boom"));
        assert!(io_err.source().is_some());

        let source = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        assert!(
            SetupError::parse(Path::new("/tmp/x"), "JSON", source)
                .source()
                .is_some()
        );
    }

    #[test]
    fn variants_without_source_return_none() {
        use std::error::Error as _;
        assert!(
            SetupError::HomeNotFound {
                file: "config.toml"
            }
            .source()
            .is_none()
        );
        assert!(
            SetupError::shape(Path::new("/tmp/x"), "hooks")
                .source()
                .is_none()
        );
    }
}
