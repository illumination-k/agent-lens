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
use serde::ser::SerializeMap as _;

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
pub(crate) const STOP_EVENT: &str = "Stop";
pub(crate) const SUBAGENT_STOP_EVENT: &str = "SubagentStop";
pub(crate) const CWD_CHANGED_EVENT: &str = "CwdChanged";

/// Per-event metadata driving the merge loop: which key under `hooks.`
/// the event lives at, the matcher written for a fresh block, the
/// handler commands the setup may install there, and the commands that
/// must be installed whenever one of them is (the stop `delta` handlers
/// compare against the snapshot `session-start snapshot` records).
/// `background` marks a block whose handlers print nothing the agent
/// needs, so a format that can run hooks without blocking (Claude
/// Code's `async`) installs them that way.
pub struct EventBlock {
    pub event: &'static str,
    pub matcher: &'static str,
    pub commands: &'static [&'static str],
    pub requires: &'static [&'static str],
    pub background: bool,
}

/// Which handlers a setup run installs, as `--only` / `--skip`
/// selectors.
///
/// A selector names one handler by its hook id — the command's
/// subcommand path joined with `:`, e.g. `post-tool-use:similarity` —
/// or every handler of one hook event by the id's first segment, e.g.
/// `post-tool-use`. An empty `only` selects everything; `skip` is
/// applied after it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookSelection {
    pub only: Vec<String>,
    pub skip: Vec<String>,
}

impl HookSelection {
    fn selects(&self, id: &str) -> bool {
        (self.only.is_empty() || self.only.iter().any(|sel| selector_matches(sel, id)))
            && !self.skips(id)
    }

    fn skips(&self, id: &str) -> bool {
        self.skip.iter().any(|sel| selector_matches(sel, id))
    }
}

/// The binary every handler command names unless `--command` says
/// otherwise.
pub const DEFAULT_COMMAND: &str = "agent-lens";

/// `--command`: what a setup run writes in place of the leading
/// `agent-lens` of every handler command — an absolute path, or a
/// launcher such as `mise exec -- agent-lens` when the binary is not on
/// the `PATH` hooks run with.
///
/// Parsing trims surrounding whitespace and rejects a blank value, so a
/// prefix is never empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPrefix(String);

impl CommandPrefix {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::str::FromStr for CommandPrefix {
    type Err = &'static str;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim() {
            "" => Err("the command must not be empty"),
            prefix => Ok(Self(prefix.to_owned())),
        }
    }
}

/// The hook id of an installed command: everything after the binary and
/// its `hook` / `codex-hook` subcommand, joined with `:`.
pub fn hook_id(command: &str) -> String {
    command
        .split_whitespace()
        .skip(2)
        .collect::<Vec<_>>()
        .join(":")
}

/// Every hook id `F` can install, in install order.
pub fn hook_ids<F: ConfigFormat>() -> Vec<String> {
    F::EVENTS
        .iter()
        .flat_map(|block| block.commands.iter().map(|cmd| hook_id(cmd)))
        .collect()
}

fn selector_matches(selector: &str, id: &str) -> bool {
    id == selector
        || id
            .strip_prefix(selector)
            .is_some_and(|rest| rest.starts_with(':'))
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
    /// An `--only` / `--skip` selector that names no handler.
    #[error("unknown hook {selector:?}; expected one of: {}", known.join(", "))]
    UnknownHook {
        selector: String,
        known: Vec<String>,
    },
    /// A selected handler depends on one the selection skips.
    #[error("hook {hook:?} requires {required:?}, which is skipped")]
    MissingRequirement { hook: String, required: String },
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

    /// Hand every handler command currently installed under
    /// `hooks.<event>`, across all matcher groups, to `visit`, and
    /// replace in place each one it returns a new command for. Nothing
    /// else about the handler may change.
    fn visit_commands(
        document: &mut Self::Document,
        path: &Path,
        block: &EventBlock,
        visit: &mut dyn FnMut(&str) -> Option<String>,
    ) -> Result<(), SetupError>;

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
    /// Handlers that were already installed and whose command a
    /// `--command` run rewrote, as the commands now read.
    pub rewritten_commands: Vec<String>,
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
/// serializes by hand instead of deriving. `rewritten_commands` is
/// present only when a `--command` run rewrote an installed handler.
#[derive(Debug)]
pub struct SetupSummary<'a, F: ConfigFormat> {
    pub path: &'a Path,
    pub wrote: bool,
    pub added_commands: &'a [String],
    pub rewritten_commands: &'a [String],
    pub document: &'a F::Payload,
}

impl<F: ConfigFormat> Serialize for SetupSummary<'_, F> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // A map, not a struct: the number of keys depends on the run.
        let mut state = serializer.serialize_map(None)?;
        state.serialize_entry("path", self.path)?;
        state.serialize_entry("wrote", &self.wrote)?;
        state.serialize_entry("added_commands", self.added_commands)?;
        if !self.rewritten_commands.is_empty() {
            state.serialize_entry("rewritten_commands", self.rewritten_commands)?;
        }
        state.serialize_entry(F::SUMMARY_KEY, self.document)?;
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
/// clobber anything. Only the handlers `selection` picks (plus whatever
/// they require) are installed; handlers it leaves out are left alone
/// either way.
///
/// Without `command`, new handlers are written as bare `agent-lens ...`
/// and the selected handlers already in the file keep whatever they are
/// invoked through. With it, new handlers are written behind that prefix
/// and the selected ones already in the file are rewritten to it,
/// keeping their trailing arguments.
pub fn plan<F: ConfigFormat>(
    path: PathBuf,
    selection: &HookSelection,
    command: Option<&CommandPrefix>,
) -> Result<SetupPlan<F::Payload>, SetupError> {
    let wanted = selected_commands::<F>(selection)?;
    let before = read_existing_text(&path)?
        .map(|text| F::read_payload(&path, &text))
        .transpose()?;
    let mut document = F::to_document(&path, before.as_ref())?;
    let merged = merge_hook_commands::<F>(&path, &mut document, &wanted, command)?;
    Ok(SetupPlan {
        path,
        before,
        after: F::to_payload(&document),
        added_commands: merged.added,
        rewritten_commands: merged.rewritten,
    })
}

/// Write the planned document to disk, creating parent directories if
/// needed.
pub fn apply<F: ConfigFormat>(plan: &SetupPlan<F::Payload>) -> Result<(), SetupError> {
    let text = F::render(&plan.path, &plan.after)?;
    write_with_parents(&plan.path, &text)
}

/// What [`merge_hook_commands`] did to the document.
#[derive(Debug, Default)]
struct Merged {
    added: Vec<String>,
    rewritten: Vec<String>,
}

/// The merge itself: for each event, install the commands that aren't
/// already wired up anywhere under that event (modulo trailing arguments
/// and what the binary is invoked through — see [`handler_tail`]) as one
/// fresh matcher group. Only commands in `wanted` are considered. With a
/// `command` prefix, the wanted handlers already present are rewritten
/// to it in place and the fresh ones are written behind it. Empty
/// results mean the document was left untouched.
fn merge_hook_commands<F: ConfigFormat>(
    path: &Path,
    document: &mut F::Document,
    wanted: &[&str],
    command: Option<&CommandPrefix>,
) -> Result<Merged, SetupError> {
    let custom = command.map(CommandPrefix::as_str);
    let prefix = custom.unwrap_or(DEFAULT_COMMAND);
    let mut merged = Merged::default();
    for block in F::EVENTS {
        let candidates: Vec<&str> = block
            .commands
            .iter()
            .copied()
            .filter(|cmd| wanted.contains(cmd))
            .collect();
        let mut installed: Vec<&str> = Vec::new();
        F::visit_commands(document, path, block, &mut |existing| {
            let (cmd, tail) = candidates
                .iter()
                .find_map(|cmd| Some((*cmd, handler_tail(existing, cmd, custom)?)))?;
            installed.push(cmd);
            let rewritten = format!("{}{tail}", with_prefix(custom?, cmd));
            (rewritten != existing).then(|| {
                merged.rewritten.push(rewritten.clone());
                rewritten
            })
        })?;
        let missing: Vec<String> = candidates
            .iter()
            .filter(|cmd| !installed.contains(cmd))
            .map(|cmd| with_prefix(prefix, cmd))
            .collect();
        if missing.is_empty() {
            continue;
        }
        F::append_matcher_group(document, path, block, &missing)?;
        merged.added.extend(missing);
    }
    Ok(merged)
}

/// `command` — one of the `agent-lens hook ...` strings a format lists —
/// with its leading `agent-lens` replaced by `prefix`.
fn with_prefix(prefix: &str, command: &str) -> String {
    format!("{prefix} {}", split_binary(command).1)
}

/// Resolve `selection` against `F`'s handlers: reject a selector that
/// names nothing, then pull in every requirement of a selected handler.
/// A requirement the selection explicitly skips is an error rather than
/// a silently broken install.
fn selected_commands<F: ConfigFormat>(
    selection: &HookSelection,
) -> Result<Vec<&'static str>, SetupError> {
    reject_unknown_selectors::<F>(selection)?;
    let mut wanted: Vec<&'static str> = Vec::new();
    for block in F::EVENTS {
        let picked: Vec<&'static str> = block
            .commands
            .iter()
            .copied()
            .filter(|cmd| selection.selects(&hook_id(cmd)))
            .collect();
        let Some(first) = picked.first() else {
            continue;
        };
        push_requirements(block, first, selection, &mut wanted)?;
        wanted.extend(picked);
    }
    Ok(wanted)
}

/// Queue `block`'s requirements ahead of its selected handler `picked`,
/// once each.
fn push_requirements(
    block: &EventBlock,
    picked: &str,
    selection: &HookSelection,
    wanted: &mut Vec<&'static str>,
) -> Result<(), SetupError> {
    for required in block.requires {
        let required_id = hook_id(required);
        if selection.skips(&required_id) {
            return Err(SetupError::MissingRequirement {
                hook: hook_id(picked),
                required: required_id,
            });
        }
        if !wanted.contains(required) {
            wanted.push(required);
        }
    }
    Ok(())
}

/// Fail on the first `--only` / `--skip` selector that names none of
/// `F`'s handlers, listing the ones it could have named.
fn reject_unknown_selectors<F: ConfigFormat>(selection: &HookSelection) -> Result<(), SetupError> {
    let known = hook_ids::<F>();
    match selection
        .only
        .iter()
        .chain(&selection.skip)
        .find(|sel| !known.iter().any(|id| selector_matches(sel, id)))
    {
        Some(selector) => Err(SetupError::UnknownHook {
            selector: selector.clone(),
            known,
        }),
        None => Ok(()),
    }
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

/// When `existing` is the same handler invocation as `wanted`, modulo
/// trailing arguments and what the binary is invoked through, the
/// trailing arguments (with their leading whitespace; empty when there
/// are none). `None` when it is some other command.
///
/// `wanted` is one of the bare `agent-lens hook ...` commands a format
/// lists. `existing` is that handler when `wanted`'s arguments appear in
/// it as whole words, and what precedes them is one of ours: it ends in
/// a word whose basename is `agent-lens`, or it is exactly `custom`, the
/// `--command` prefix of this run. So that:
/// - an already-installed
///   `agent-lens hook post-tool-use similarity --threshold 0.9` is not
///   re-installed without the user-added flag;
/// - a command wired through an explicit binary path — e.g.
///   `"$CLAUDE_PROJECT_DIR"/target/debug/agent-lens hook post-tool-use similarity`
///   — or through a launcher — `mise exec -- agent-lens hook ...` — is
///   recognised as the same handler as the bare
///   `agent-lens hook post-tool-use similarity`, so re-running setup
///   never appends a duplicate block;
/// - a prefix that does not name `agent-lens` at all (`--command
///   /opt/bin/al`) is still recognised by a re-run with the same
///   `--command`.
///
/// Someone else's command is never matched: the arguments alone are not
/// enough, the prefix has to be ours.
fn handler_tail<'a>(existing: &'a str, wanted: &str, custom: Option<&str>) -> Option<&'a str> {
    let wanted_args = split_binary(wanted).1;
    word_starts(existing).skip(1).find_map(|start| {
        let tail = args_tail(&existing[start..], wanted_args)?;
        is_our_prefix(existing[..start].trim(), custom).then_some(tail)
    })
}

/// Byte offset of every whitespace-separated word in `text`.
fn word_starts(text: &str) -> impl Iterator<Item = usize> + '_ {
    let mut after_space = true;
    text.char_indices().filter_map(move |(at, ch)| {
        let starts = after_space && !ch.is_whitespace();
        after_space = ch.is_whitespace();
        starts.then_some(at)
    })
}

/// True when `prefix` — everything a command puts before the handler
/// arguments — invokes agent-lens: its last word is the binary, possibly
/// behind a path and inside quotes, or it is the `--command` prefix of
/// this run verbatim.
fn is_our_prefix(prefix: &str, custom: Option<&str>) -> bool {
    custom == Some(prefix)
        || prefix.split_whitespace().next_back().is_some_and(|binary| {
            binary_basename(binary.trim_end_matches(['"', '\''])) == DEFAULT_COMMAND
        })
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

/// What follows `wanted_args` in `existing_args`, when the latter
/// equals the former or extends it with whitespace-separated trailing
/// arguments (a user-added flag).
fn args_tail<'a>(existing_args: &'a str, wanted_args: &str) -> Option<&'a str> {
    existing_args
        .strip_prefix(wanted_args)
        .filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// The format-agnostic half of every [`ConfigFormat`] test suite.
///
/// [`plan`] and [`apply`] promise things no format implementation gets
/// to choose: a missing file plans every command, applying creates the
/// parent directory, re-running is a no-op, a blank file reads as
/// absent, a directory in the file's place is an IO error rather than
/// "no config yet", and a malformed document is reported instead of
/// clobbered. Those are properties of the engine, so they belong to the
/// engine, and each format supplies only the text that spells them.
///
/// They used to be written out once per format. Nothing declared a
/// dependency between the two copies, so nothing kept them in step, and
/// they had already drifted: only Claude Code's suite pinned the
/// directory-as-file case, and only Codex's pinned a handler entry with
/// no `command` field. Both formats now run both.
#[cfg(test)]
pub(crate) mod conformance {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{
        CommandPrefix, ConfigFormat, HookSelection, SetupError, SetupPlan, SetupScope, apply,
        resolve_path,
    };

    /// Plan with the default selection: every handler.
    fn plan<F: ConfigFormat>(path: PathBuf) -> Result<SetupPlan<F::Payload>, SetupError> {
        super::plan::<F>(path, &HookSelection::default(), None)
    }

    /// Plan every handler behind `--command prefix`.
    fn plan_with<F: ConfigFormat>(path: &Path, prefix: &str) -> SetupPlan<F::Payload> {
        let prefix: CommandPrefix = prefix.parse().unwrap();
        match super::plan::<F>(path.to_path_buf(), &HookSelection::default(), Some(&prefix)) {
            Ok(plan) => plan,
            Err(err) => panic!("planning {path:?} failed: {err}"),
        }
    }

    /// [`all_commands`] as a `--command prefix` run writes them.
    pub(crate) fn all_commands_with<F: ConfigFormat>(prefix: &str) -> Vec<String> {
        all_commands::<F>()
            .iter()
            .map(|cmd| cmd.replacen(super::DEFAULT_COMMAND, prefix, 1))
            .collect()
    }

    /// Every handler command in the config file at `path`, one per
    /// handler, in event order — ours or not.
    pub(crate) fn commands_on_disk<F: ConfigFormat>(path: &Path) -> Vec<String> {
        let text = fs::read_to_string(path).unwrap();
        let payload = F::read_payload(path, &text).ok().unwrap();
        let mut document = F::to_document(path, Some(&payload)).ok().unwrap();
        let mut events: Vec<&str> = Vec::new();
        let mut commands = Vec::new();
        for block in F::EVENTS {
            if events.contains(&block.event) {
                continue;
            }
            events.push(block.event);
            F::visit_commands(&mut document, path, block, &mut |cmd| {
                commands.push(cmd.to_owned());
                None
            })
            .unwrap();
        }
        commands
    }

    /// [`commands_on_disk`], sorted, for comparing against a list in
    /// install order.
    fn sorted_commands_on_disk<F: ConfigFormat>(path: &Path) -> Vec<String> {
        let mut commands = commands_on_disk::<F>(path);
        commands.sort();
        commands
    }

    fn sorted(mut commands: Vec<String>) -> Vec<String> {
        commands.sort();
        commands
    }

    /// `--command` on a fresh file writes every handler behind the
    /// prefix and rewrites nothing.
    pub(crate) fn custom_prefix_is_written_for_every_command<F: ConfigFormat>(prefix: &str) {
        let (_dir, path) = scratch::<F>();

        let plan = plan_with::<F>(&path, prefix);
        assert_eq!(plan.added_commands, all_commands_with::<F>(prefix));
        assert!(plan.rewritten_commands.is_empty());
        apply::<F>(&plan).unwrap();
        assert_eq!(
            sorted_commands_on_disk::<F>(&path),
            sorted(all_commands_with::<F>(prefix)),
        );
    }

    /// Re-running with the same `--command` is a no-op, whether or not
    /// the prefix names `agent-lens`.
    pub(crate) fn rerunning_with_the_same_prefix_is_idempotent<F: ConfigFormat>(prefix: &str) {
        let (_dir, path) = scratch::<F>();
        apply::<F>(&plan_with::<F>(&path, prefix)).unwrap();

        let second = plan_with::<F>(&path, prefix);
        assert!(!second.changed(), "second plan should be a no-op");
        assert!(second.added_commands.is_empty());
        assert!(second.rewritten_commands.is_empty());
    }

    /// A run without `--command` over handlers wired through a custom
    /// prefix recognises them and leaves the prefix alone.
    pub(crate) fn default_rerun_keeps_a_custom_prefix<F: ConfigFormat>(prefix: &str) {
        let (_dir, path) = scratch::<F>();
        apply::<F>(&plan_with::<F>(&path, prefix)).unwrap();

        let second = plan::<F>(path).unwrap();
        assert!(!second.changed(), "a default re-run must not touch them");
        assert!(second.added_commands.is_empty());
        assert!(second.rewritten_commands.is_empty());
    }

    /// Re-running with another `--command` — over a bare install when
    /// `first` is `None` — rewrites every handler in place: nothing is
    /// appended, and no handler is left behind the old prefix.
    pub(crate) fn rerunning_with_another_prefix_rewrites_in_place<F: ConfigFormat>(
        first: Option<&str>,
        second: &str,
    ) {
        let (_dir, path) = scratch::<F>();
        let installed = match first {
            Some(prefix) => plan_with::<F>(&path, prefix),
            None => plan::<F>(path.clone()).unwrap(),
        };
        apply::<F>(&installed).unwrap();

        let rerun = plan_with::<F>(&path, second);
        assert!(rerun.changed());
        assert!(rerun.added_commands.is_empty(), "nothing may be appended");
        assert_eq!(rerun.rewritten_commands, all_commands_with::<F>(second));
        apply::<F>(&rerun).unwrap();
        assert_eq!(
            sorted_commands_on_disk::<F>(&path),
            sorted(all_commands_with::<F>(second)),
            "one handler each, all behind the new prefix",
        );
        assert!(!plan_with::<F>(&path, second).changed());
    }

    /// Every command the format's events would install, in the order
    /// [`plan`] queues them.
    pub(crate) fn all_commands<F: ConfigFormat>() -> Vec<&'static str> {
        F::EVENTS
            .iter()
            .flat_map(|block| block.commands.iter().copied())
            .collect()
    }

    /// A scratch config file at the path the format actually resolves
    /// to, so the parent-directory handling is exercised for real. The
    /// `TempDir` is returned alongside because dropping it deletes the
    /// tree.
    fn scratch<F: ConfigFormat>() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(F::RELATIVE_PATH);
        (dir, path)
    }

    /// A config file whose text is `contents`, at the format's own path.
    fn scratch_with<F: ConfigFormat>(contents: &str) -> (TempDir, PathBuf) {
        let (dir, path) = scratch::<F>();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, contents).unwrap();
        (dir, path)
    }

    /// The error [`plan`] fails with. Spelled out rather than
    /// `unwrap_err`, whose `Debug` bound would force every
    /// [`ConfigFormat::Payload`] to implement it just for these
    /// assertions.
    fn plan_err<F: ConfigFormat>(path: PathBuf) -> SetupError {
        match plan::<F>(path) {
            Ok(plan) => panic!(
                "expected planning {:?} to fail, got {} queued command(s)",
                plan.path,
                plan.added_commands.len(),
            ),
            Err(err) => err,
        }
    }

    /// A file that does not exist yet plans in every handler command.
    pub(crate) fn plan_for_missing_file_installs_every_command<F: ConfigFormat>() {
        let (_dir, path) = scratch::<F>();

        let plan = plan::<F>(path).unwrap();
        assert!(plan.before.is_none(), "no file means no `before` payload");
        assert!(plan.changed());
        assert_eq!(plan.added_commands, all_commands::<F>());
    }

    /// Applying a plan creates the parent directory and writes exactly
    /// what the format renders for the planned document.
    pub(crate) fn apply_creates_parent_dir_and_writes_file<F: ConfigFormat>() {
        let (_dir, path) = scratch::<F>();

        let plan = plan::<F>(path.clone()).unwrap();
        apply::<F>(&plan).unwrap();

        assert!(path.exists(), "apply must create {:?}", path.parent());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            F::render(&path, &plan.after).unwrap(),
        );
    }

    /// Planning against a file this setup already wrote changes nothing.
    pub(crate) fn rerunning_setup_is_idempotent<F: ConfigFormat>() {
        let (_dir, path) = scratch::<F>();

        let first = plan::<F>(path.clone()).unwrap();
        apply::<F>(&first).unwrap();

        let second = plan::<F>(path).unwrap();
        assert!(!second.changed(), "second plan should be a no-op");
        assert!(second.added_commands.is_empty());
        assert!(
            second.before.as_ref() == Some(&second.after),
            "an unchanged plan must round-trip its payload",
        );
    }

    /// A file that exists but holds only whitespace reads as absent
    /// rather than as an empty document to merge into.
    pub(crate) fn empty_file_is_treated_as_missing<F: ConfigFormat>() {
        let (_dir, path) = scratch_with::<F>("   \n");

        let plan = plan::<F>(path).unwrap();
        assert!(plan.before.is_none());
        assert!(plan.changed());
    }

    /// A document the format cannot parse is reported, naming the
    /// format, instead of being overwritten.
    pub(crate) fn unparsable_file_is_reported<F: ConfigFormat>(contents: &str) {
        let (_dir, path) = scratch_with::<F>(contents);

        let err = plan_err::<F>(path);
        assert!(
            matches!(err, SetupError::Parse { format, .. } if format == F::FORMAT),
            "expected a {} parse error, got {err:?}",
            F::FORMAT,
        );
    }

    /// A field along the `hooks.<event>` path whose type the merge
    /// cannot navigate is reported against that field.
    pub(crate) fn unexpected_shape_is_reported<F: ConfigFormat>(contents: &str, field: &str) {
        let (_dir, path) = scratch_with::<F>(contents);

        let err = plan_err::<F>(path);
        assert!(
            matches!(&err, SetupError::UnexpectedShape { field: got, .. } if got == field),
            "expected UnexpectedShape at {field}, got {err:?}",
        );
    }

    /// A directory where the config file should be is an IO error. The
    /// `NotFound` guard in the read must not swallow every other
    /// `ErrorKind` as "no config yet" and then write over the path.
    pub(crate) fn directory_in_place_of_file_surfaces_io_error<F: ConfigFormat>() {
        let (_dir, path) = scratch::<F>();
        fs::create_dir_all(&path).unwrap();

        let err = plan_err::<F>(path);
        assert!(
            matches!(err, SetupError::Io { .. }),
            "expected Io error for directory-as-file, got {err:?}",
        );
    }

    /// Project scope resolves under the caller's root, at the format's
    /// own relative path.
    pub(crate) fn resolve_path_project_joins_relative<F: ConfigFormat>() {
        let root = Path::new("/tmp/proj");
        assert_eq!(
            resolve_path::<F>(SetupScope::Project, root).unwrap(),
            root.join(F::RELATIVE_PATH),
        );
    }

    /// Planning against `existing` queues exactly `expected` and nothing
    /// else — the assertion behind "already installed under a different
    /// matcher", "installed with a user-added flag", and "installed as a
    /// handler the merge cannot read a command out of".
    pub(crate) fn queues_exactly<F: ConfigFormat>(existing: &str, expected: &[&str]) {
        let (_dir, path) = scratch_with::<F>(existing);

        let plan = plan::<F>(path).unwrap();
        let queued: Vec<&str> = plan.added_commands.iter().map(String::as_str).collect();
        assert_eq!(queued, expected);
        assert_eq!(
            plan.changed(),
            !expected.is_empty(),
            "the document must move exactly when something was queued",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::exact("agent-lens b c", "agent-lens b c", true)]
    #[case::trailing_args("agent-lens b c --flag", "agent-lens b c", true)]
    #[case::word_extension("agent-lens b cx", "agent-lens b c", false)]
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
    // A launcher in front of the binary is still our handler.
    #[case::launcher(
        "mise exec -- agent-lens hook stop delta",
        "agent-lens hook stop delta",
        true
    )]
    #[case::launcher_with_path_and_trailing_args(
        "nix run .# -- ./result/bin/agent-lens hook stop delta --no-block",
        "agent-lens hook stop delta",
        true
    )]
    // A path with spaces has to be quoted; the closing quote is not part
    // of the basename.
    #[case::double_quoted_path(
        "\"/my tools/agent-lens\" hook stop delta",
        "agent-lens hook stop delta",
        true
    )]
    #[case::single_quoted_path(
        "'/my tools/agent-lens' hook stop delta",
        "agent-lens hook stop delta",
        true
    )]
    #[case::leading_whitespace("  agent-lens  hook stop delta", "agent-lens hook stop delta", true)]
    // `agent-lens` earlier in the prefix is not the word that runs the
    // handler.
    #[case::binary_is_not_the_last_prefix_word(
        "agent-lens-wrapper --bin agent-lens other hook stop delta",
        "agent-lens hook stop delta",
        false
    )]
    #[case::basename_only_shares_a_suffix(
        "/opt/not-agent-lens hook stop delta",
        "agent-lens hook stop delta",
        false
    )]
    // The handler arguments alone, with nothing invoking them.
    #[case::no_prefix("hook stop delta", "agent-lens hook stop delta", false)]
    #[case::no_prefix_after_whitespace(" hook stop delta", "agent-lens hook stop delta", false)]
    // `codex-hook` is one word: it never reads as `hook`.
    #[case::codex_is_not_claude(
        "agent-lens codex-hook stop delta",
        "agent-lens hook stop delta",
        false
    )]
    #[case::arguments_mid_word("agent-lens xhook stop delta", "agent-lens hook stop delta", false)]
    fn handler_tail_recognises_our_handlers(
        #[case] existing: &str,
        #[case] wanted: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(handler_tail(existing, wanted, None).is_some(), expected);
    }

    /// The tail is what a rewrite carries over: the user's trailing
    /// arguments, whitespace included, and nothing of the prefix.
    #[rstest]
    #[case::none("agent-lens hook stop delta", "")]
    #[case::flag("mise exec -- agent-lens hook stop delta --no-block", " --no-block")]
    #[case::spacing("/bin/agent-lens hook stop delta   --no-block  ", "   --no-block  ")]
    fn handler_tail_is_the_trailing_arguments(#[case] existing: &str, #[case] tail: &str) {
        assert_eq!(
            handler_tail(existing, "agent-lens hook stop delta", None),
            Some(tail),
        );
    }

    /// A prefix that does not name `agent-lens` is ours only when it is
    /// this run's `--command`, verbatim and whole.
    #[rstest]
    #[case::same(Some("/opt/bin/al"), true)]
    #[case::none(None, false)]
    #[case::another(Some("/opt/bin/other"), false)]
    #[case::only_its_last_word(Some("al"), false)]
    #[case::longer(Some("env X=1 /opt/bin/al"), false)]
    fn a_custom_prefix_is_recognised_verbatim(#[case] custom: Option<&str>, #[case] ours: bool) {
        assert_eq!(
            handler_tail(
                "/opt/bin/al hook stop delta",
                "agent-lens hook stop delta",
                custom
            )
            .is_some(),
            ours,
        );
    }

    #[rstest]
    #[case::plain("agent-lens", Some("agent-lens"))]
    #[case::launcher("mise exec -- agent-lens", Some("mise exec -- agent-lens"))]
    #[case::trimmed("  /opt/bin/al\n", Some("/opt/bin/al"))]
    #[case::empty("", None)]
    #[case::blank(" \t ", None)]
    fn command_prefix_parses_to_a_non_empty_trimmed_value(
        #[case] raw: &str,
        #[case] expected: Option<&str>,
    ) {
        let parsed = raw.parse::<CommandPrefix>();
        assert_eq!(parsed.as_ref().ok().map(CommandPrefix::as_str), expected);
    }

    #[rstest]
    #[case::bare("agent-lens", "agent-lens hook stop delta")]
    #[case::launcher("mise exec -- agent-lens", "mise exec -- agent-lens hook stop delta")]
    fn with_prefix_replaces_only_the_binary(#[case] prefix: &str, #[case] expected: &str) {
        assert_eq!(with_prefix(prefix, "agent-lens hook stop delta"), expected);
    }

    #[rstest]
    #[case::words("a  bc\td", &[0, 3, 6])]
    #[case::leading_space("  a b", &[2, 4])]
    #[case::multibyte("é ü", &[0, 3])]
    #[case::empty("", &[])]
    fn word_starts_are_the_offsets_after_whitespace(
        #[case] text: &str,
        #[case] expected: &[usize],
    ) {
        assert_eq!(word_starts(text).collect::<Vec<_>>(), expected);
    }

    /// `rewritten_commands` is published only when a run rewrote
    /// something, so a default run's summary keeps its four keys.
    #[rstest]
    #[case::nothing_rewritten(&[], &["added_commands", "path", "settings", "wrote"])]
    #[case::rewritten(
        &["x agent-lens hook stop delta"],
        &["added_commands", "path", "rewritten_commands", "settings", "wrote"]
    )]
    fn summary_lists_rewrites_only_when_there_are_some(
        #[case] rewritten: &[&str],
        #[case] keys: &[&str],
    ) {
        use crate::hooks::setup::ClaudeSettings;

        let rewritten: Vec<String> = rewritten.iter().map(|s| (*s).to_string()).collect();
        let summary = SetupSummary::<ClaudeSettings> {
            path: Path::new("settings.json"),
            wrote: true,
            added_commands: &[],
            rewritten_commands: &rewritten,
            document: &serde_json::json!({}),
        };
        let json = serde_json::to_value(&summary).unwrap();
        let mut published: Vec<&str> = json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        published.sort_unstable();
        assert_eq!(published, keys);
        if !rewritten.is_empty() {
            assert_eq!(json["rewritten_commands"], serde_json::json!(rewritten));
        }
    }

    mod matcher_properties {
        use proptest::prelude::*;

        use super::super::{DEFAULT_COMMAND, handler_tail, with_prefix};

        const WANTED: &str = "agent-lens hook post-tool-use similarity";

        /// A shell word that is not `agent-lens` and holds no separator.
        fn word() -> impl Strategy<Value = String> {
            "[a-zA-Z0-9_.=$-]{1,8}".prop_filter("the binary name is not a filler word", |word| {
                word != DEFAULT_COMMAND
            })
        }

        /// Whitespace-joined words; possibly empty.
        fn words(max: usize) -> impl Strategy<Value = String> {
            prop::collection::vec(word(), 0..=max).prop_map(|words| words.join(" "))
        }

        /// A prefix that ends in the `agent-lens` binary: a launcher,
        /// then the binary behind an optional path.
        fn our_prefix() -> impl Strategy<Value = String> {
            (words(3), prop::collection::vec(word(), 0..3)).prop_map(|(launcher, dirs)| {
                let binary = dirs
                    .into_iter()
                    .chain([DEFAULT_COMMAND.to_owned()])
                    .collect::<Vec<_>>()
                    .join("/");
                format!("{launcher} {binary}").trim_start().to_owned()
            })
        }

        /// Trailing arguments as a user would append them.
        fn tail() -> impl Strategy<Value = String> {
            words(3).prop_map(|args| {
                if args.is_empty() {
                    args
                } else {
                    format!(" {args}")
                }
            })
        }

        proptest! {
            /// Whatever agent-lens is invoked through, the handler is
            /// recognised and its trailing arguments come back intact.
            #[test]
            fn any_prefix_ending_in_the_binary_is_recognised(
                prefix in our_prefix(),
                tail in tail(),
            ) {
                let existing = format!("{}{tail}", with_prefix(&prefix, WANTED));
                prop_assert_eq!(handler_tail(&existing, WANTED, None), Some(tail.as_str()));
            }

            /// What a `--command` run writes, a re-run with that same
            /// `--command` recognises — and rewrites to itself, so the
            /// second run changes nothing. Holds for prefixes that never
            /// mention `agent-lens`.
            #[test]
            fn a_rewrite_is_recognised_by_its_own_prefix(
                prefix in words(4).prop_filter("--command is never blank", |p| !p.is_empty()),
                tail in tail(),
            ) {
                let written = format!("{}{tail}", with_prefix(&prefix, WANTED));
                let found = handler_tail(&written, WANTED, Some(&prefix));
                prop_assert_eq!(found, Some(tail.as_str()));
                prop_assert_eq!(
                    format!("{}{}", with_prefix(&prefix, WANTED), found.unwrap()),
                    written,
                );
            }

            /// A command that never invokes `agent-lens` is not ours,
            /// even with our handler's arguments in it.
            #[test]
            fn a_foreign_command_is_never_recognised(
                prefix in words(4),
                tail in tail(),
            ) {
                let existing = format!("{}{tail}", with_prefix(&prefix, WANTED));
                prop_assert_eq!(handler_tail(&existing, WANTED, None), None);
            }
        }
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
