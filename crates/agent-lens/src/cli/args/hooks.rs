//! `agent-lens hook` / `agent-lens codex-hook` subcommands and their
//! argument structs.

use agent_lens::hooks::setup_engine::{HookSelection, SetupScope};
use clap::{Args, Subcommand};

use crate::cli::examples;

#[derive(Debug, Subcommand)]
pub(in crate::cli) enum HookCommand {
    /// Handle a `SessionStart` event.
    #[command(subcommand)]
    SessionStart(SessionStartCommand),
    /// Handle a `PreToolUse` event.
    #[command(subcommand)]
    PreToolUse(PreToolUseCommand),
    /// Handle a `PostToolUse` event.
    #[command(subcommand)]
    PostToolUse(PostToolUseCommand),
    /// Handle a `Stop` event.
    #[command(subcommand)]
    Stop(StopCommand),
    /// Handle a `SubagentStop` event.
    #[command(subcommand)]
    SubagentStop(StopCommand),
    /// Wire `agent-lens`'s hook handlers into a Claude Code
    /// `settings.json`.
    ///
    /// The merge is conservative: existing entries are preserved, and a
    /// new block is appended only with the commands that aren't already
    /// wired up. Re-running the command is a no-op once every handler
    /// is installed.
    #[command(after_long_help = examples::HOOK_SETUP)]
    Setup(SetupArgs),
}

#[derive(Debug, Args)]
pub(in crate::cli) struct SetupArgs {
    /// Where to install the hooks: `project` writes
    /// `<cwd>/.claude/settings.json`, `user` writes
    /// `$HOME/.claude/settings.json`.
    #[arg(long, value_enum, default_value_t = SetupScope::Project)]
    pub(in crate::cli) scope: SetupScope,
    /// Show the resulting JSON without touching disk.
    #[arg(long)]
    pub(in crate::cli) dry_run: bool,
    #[command(flatten)]
    pub(in crate::cli) selection: HookSelectionArgs,
}

/// `--only` / `--skip`, shared by both setup commands.
#[derive(Debug, Args)]
pub(in crate::cli) struct HookSelectionArgs {
    /// Install only these handlers. A value is a hook id such as
    /// `post-tool-use:similarity`, or a bare event such as
    /// `post-tool-use` for all of its handlers. Repeatable and
    /// comma-separated. A stop `delta` handler also installs
    /// `session-start:snapshot`, which it compares against.
    #[arg(long, value_name = "HOOK", value_delimiter = ',')]
    pub(in crate::cli) only: Vec<String>,
    /// Leave these handlers out, in the same form as `--only`. Applied
    /// after `--only`.
    #[arg(long, value_name = "HOOK", value_delimiter = ',')]
    pub(in crate::cli) skip: Vec<String>,
}

impl HookSelectionArgs {
    pub(in crate::cli) fn into_selection(self) -> HookSelection {
        HookSelection {
            only: self.only,
            skip: self.skip,
        }
    }
}

#[derive(Debug, Subcommand)]
pub(in crate::cli) enum SessionStartCommand {
    /// Inject a one-shot summary of the project's hotspots and
    /// coupling thumbnail into the new Claude Code session.
    ///
    /// Runs once per session against `cwd`. Pieces that don't apply
    /// (cwd outside a git working tree, or not anchored at a Rust
    /// crate) are silently omitted; if neither applies, the hook
    /// returns a no-op and Claude Code starts unchanged.
    Summary,
    /// Record the session checkpoint snapshot that `stop delta`
    /// compares against.
    ///
    /// Hashes every production source file under `cwd` and records its
    /// per-function cognitive complexity and body hash, its
    /// forwarding-only wrappers, and — from whole-tree runs — the
    /// near-duplicate pairs, the confirmed/likely unreachable functions
    /// and the call-graph hubs. Writes
    /// `<repo-root>/target/agent-lens/session-<id>.json` (with a
    /// `.gitignore` beside it) and injects nothing. A snapshot that
    /// already exists for the session — a resume or a compaction — is
    /// kept, so the baseline stays the session's start.
    Snapshot,
}

#[derive(Debug, Subcommand)]
pub(in crate::cli) enum PreToolUseCommand {
    /// Report functions whose pre-edit complexity (cyclomatic /
    /// cognitive / nesting) crosses a non-trivial threshold in the
    /// file the agent is about to edit.
    ///
    /// The parser is chosen from the file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently. `Write` against a brand-new path
    /// is a silent no-op (no current state to read).
    Complexity,
    /// Report cohesion units (`impl` blocks, classes, or module units)
    /// whose pre-edit LCOM4 is above 1 in the file the agent is about
    /// to edit.
    ///
    /// The parser is chosen from the file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Cohesion,
}

#[derive(Debug, Subcommand)]
pub(in crate::cli) enum PostToolUseCommand {
    /// Report clusters of similar functions in the file that was just edited.
    ///
    /// The parser is chosen from the file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Similarity,
    /// Report functions whose body, after stripping a short chain of
    /// trivial adapters, is just a forwarding call to another function.
    ///
    /// The parser is chosen from the file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Wrapper,
    /// Report the pending diff's footprint flags that land in the file
    /// that was just edited.
    ///
    /// Runs `analyze footprint` over the working-tree diff under `cwd`
    /// (untracked files count as added) and keeps only the rows in the
    /// edited file: functions outside the change's impact closure,
    /// complexity increases, new wrappers, and added functions nothing
    /// calls. Silent outside a git working tree and when nothing in the
    /// file is flagged.
    Footprint,
}

#[derive(Debug, Subcommand)]
pub(in crate::cli) enum StopCommand {
    /// Report what got worse since the session's `snapshot`, and
    /// nothing when nothing did.
    ///
    /// Recomputes only what the session could have changed: function
    /// facts and wrappers for files whose content hash moved, duplicate
    /// pairs scored for the added or modified functions against the
    /// whole tree, and whole-tree unreachable and hub runs. Reports new
    /// near-duplicate pairs, functions at or above cognitive 8 that got
    /// more complex, new forwarding-only wrappers, newly unreachable
    /// functions, and edited hubs. A regression no earlier stop reported
    /// blocks the stop once, handing the report to the agent as the
    /// reason to keep going; a repeat, or a stop that is already that
    /// continuation, is only a message. Silent without a snapshot.
    Delta(DeltaArgs),
}

#[derive(Debug, Args)]
pub(in crate::cli) struct DeltaArgs {
    /// Never block: report regressions as a message only, which the
    /// agent does not read.
    #[arg(long)]
    pub(in crate::cli) no_block: bool,
}

#[derive(Debug, Subcommand)]
pub(in crate::cli) enum CodexHookCommand {
    /// Handle a Codex `SessionStart` event.
    #[command(subcommand)]
    SessionStart(CodexSessionStartCommand),
    /// Handle a Codex `PreToolUse` event.
    #[command(subcommand)]
    PreToolUse(CodexPreToolUseCommand),
    /// Handle a Codex `PostToolUse` event.
    #[command(subcommand)]
    PostToolUse(CodexPostToolUseCommand),
    /// Handle a Codex `Stop` event.
    #[command(subcommand)]
    Stop(StopCommand),
    /// Wire `agent-lens`'s Codex hook handlers into a Codex
    /// `config.toml`.
    ///
    /// The merge is conservative: existing keys and comments are
    /// preserved, and `[[hooks.SessionStart]]`, `[[hooks.PreToolUse]]`,
    /// `[[hooks.PostToolUse]]`, and `[[hooks.Stop]]` blocks are appended
    /// only for handlers that aren't already wired up. Re-running the
    /// command is a no-op once every handler is installed.
    #[command(after_long_help = examples::CODEX_HOOK_SETUP)]
    Setup(CodexSetupArgs),
}

#[derive(Debug, Args)]
pub(in crate::cli) struct CodexSetupArgs {
    /// Where to install the hooks: `project` writes
    /// `<repo-root>/.codex/config.toml` — the nearest ancestor holding a
    /// `.git` entry, falling back to the current directory outside a git
    /// tree — and `user` writes `$HOME/.codex/config.toml`.
    #[arg(long, value_enum, default_value_t = SetupScope::User)]
    pub(in crate::cli) scope: SetupScope,
    /// Show the resulting TOML without touching disk.
    #[arg(long)]
    pub(in crate::cli) dry_run: bool,
    #[command(flatten)]
    pub(in crate::cli) selection: HookSelectionArgs,
}

#[derive(Debug, Subcommand)]
pub(in crate::cli) enum CodexPostToolUseCommand {
    /// Report clusters of similar functions across every file Codex's
    /// `apply_patch` just touched.
    ///
    /// The parser is chosen from each file's extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Similarity,
    /// Report functions whose body, after stripping a short chain of
    /// trivial adapters, is just a forwarding call to another function.
    ///
    /// Runs against every file Codex's `apply_patch` just touched. The
    /// parser is chosen from each file's extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Wrapper,
    /// Report the pending diff's footprint flags that land in the files
    /// Codex's `apply_patch` just touched.
    ///
    /// Runs `analyze footprint` over the working-tree diff under `cwd`
    /// (untracked files count as added) and keeps only the rows in the
    /// patched files. Silent outside a git working tree and when nothing
    /// in them is flagged.
    Footprint,
}

#[derive(Debug, Subcommand)]
pub(in crate::cli) enum CodexPreToolUseCommand {
    /// Report functions whose pre-patch complexity crosses a
    /// non-trivial threshold across every file Codex's `apply_patch`
    /// is about to update.
    ///
    /// `*** Add File:` entries are skipped (no current state on disk);
    /// only `*** Update File:` paths are inspected.
    /// The parser is chosen from each updated file's extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Complexity,
    /// Report cohesion units (`impl` blocks, classes, or module units)
    /// whose pre-patch LCOM4 is above 1 across every file Codex's
    /// `apply_patch` is about to update.
    ///
    /// `*** Add File:` entries are skipped (no current state on disk);
    /// only `*** Update File:` paths are inspected.
    /// The parser is chosen from each updated file's extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Cohesion,
}

#[derive(Debug, Subcommand)]
pub(in crate::cli) enum CodexSessionStartCommand {
    /// Inject a one-shot summary of the project's hotspots and
    /// coupling thumbnail into the new Codex session.
    ///
    /// Runs once per session against `cwd`. Pieces that don't apply
    /// (cwd outside a git working tree, or not anchored at a Rust
    /// crate) are silently omitted; if neither applies, the hook
    /// returns a no-op and Codex starts unchanged.
    Summary,
    /// Record the session checkpoint snapshot that `stop delta`
    /// compares against.
    ///
    /// Hashes every production source file under `cwd` and records its
    /// per-function cognitive complexity and body hash, its
    /// forwarding-only wrappers, and — from whole-tree runs — the
    /// near-duplicate pairs, the confirmed/likely unreachable functions
    /// and the call-graph hubs. Writes
    /// `<repo-root>/target/agent-lens/session-<id>.json` (with a
    /// `.gitignore` beside it) and injects nothing. A snapshot that
    /// already exists for the session — a resume or a compaction — is
    /// kept, so the baseline stays the session's start.
    Snapshot,
}
