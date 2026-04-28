//! `agent-lens` CLI parsing and command dispatch.
//!
//! Each PostToolUse handler is a clap subcommand, so `agent-lens hook
//! post-tool-use similarity` is parsed statically instead of routed by
//! a runtime name string. Analyzers live under `agent-lens analyze ...`
//! and write their report to stdout. Stdout is otherwise reserved for the
//! hook's JSON response; diagnostics go to stderr via `tracing`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use agent_hooks::Hook;
use agent_hooks::claude_code::ClaudeCodeHookInput;
use agent_hooks::codex::CodexHookInput;
use agent_lens::analyze::{
    CohesionAnalyzer, ComplexityAnalyzer, ContextSpanAnalyzer, CouplingAnalyzer,
    DEFAULT_SIMILARITY_MIN_LINES, DEFAULT_SIMILARITY_THRESHOLD, HotspotAnalyzer, OutputFormat,
    SimilarityAnalyzer, WrapperAnalyzer,
};
use agent_lens::hooks::codex::post_tool_use::{
    SimilarityHook as CodexSimilarityHook, WrapperHook as CodexWrapperHook,
};
use agent_lens::hooks::codex::pre_tool_use::{
    CohesionHook as CodexPreCohesionHook, ComplexityHook as CodexPreComplexityHook,
};
use agent_lens::hooks::codex::session_start::SummaryHook as CodexSessionStartSummaryHook;
use agent_lens::hooks::codex::setup::{self as codex_setup, SetupSummary as CodexSetupSummary};
use agent_lens::hooks::post_tool_use::{SimilarityHook, WrapperHook};
use agent_lens::hooks::pre_tool_use::{CohesionHook, ComplexityHook};
use agent_lens::hooks::session_start::SummaryHook as SessionStartSummaryHook;
use agent_lens::hooks::setup::{self, SettingsScope, SetupSummary};
use clap::{Args, Parser, Subcommand, ValueEnum};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "agent-lens",
    about = "Hook handlers and analyzers that give coding agents a sharper view of the codebase.",
    version,
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a handler for one of Claude Code's hook events.
    #[command(subcommand)]
    Hook(HookCommand),
    /// Run a handler for one of Codex's hook events.
    #[command(subcommand)]
    CodexHook(CodexHookCommand),
    /// Run an on-demand analyzer that emits LLM-friendly context.
    #[command(subcommand)]
    Analyze(AnalyzeCommand),
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    /// Handle a `SessionStart` event.
    #[command(subcommand)]
    SessionStart(SessionStartCommand),
    /// Handle a `PreToolUse` event.
    #[command(subcommand)]
    PreToolUse(PreToolUseCommand),
    /// Handle a `PostToolUse` event.
    #[command(subcommand)]
    PostToolUse(PostToolUseCommand),
    /// Wire `agent-lens`'s hook handlers into a Claude Code
    /// `settings.json`.
    ///
    /// The merge is conservative: existing entries are preserved, and a
    /// new block is appended only with the commands that aren't already
    /// wired up. Re-running the command is a no-op once every handler
    /// is installed.
    Setup(SetupArgs),
}

#[derive(Debug, Args)]
struct SetupArgs {
    /// Where to install the hooks. `project` writes to
    /// `<cwd>/.claude/settings.json`; `user` writes to
    /// `$HOME/.claude/settings.json`.
    #[arg(long, value_enum, default_value_t = SetupScope::Project)]
    scope: SetupScope,
    /// Show the resulting JSON without touching disk.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SetupScope {
    Project,
    User,
}

impl From<SetupScope> for SettingsScope {
    fn from(value: SetupScope) -> Self {
        match value {
            SetupScope::Project => Self::Project,
            SetupScope::User => Self::User,
        }
    }
}

#[derive(Debug, Subcommand)]
enum SessionStartCommand {
    /// Inject a one-shot summary of the project's hotspots and
    /// coupling thumbnail into the new Claude Code session.
    ///
    /// Runs once per session against `cwd`. Pieces that don't apply
    /// (cwd outside a git working tree, or not anchored at a Rust
    /// crate) are silently omitted; if neither applies, the hook
    /// returns a no-op and Claude Code starts unchanged.
    Summary,
}

#[derive(Debug, Subcommand)]
enum PreToolUseCommand {
    /// Report functions whose pre-edit complexity (cyclomatic /
    /// cognitive / nesting) crosses a non-trivial threshold in the
    /// file the agent is about to edit.
    ///
    /// The parser is chosen from the file extension (`.rs` / `.ts` /
    /// `.py`). Files with an unsupported extension are ignored
    /// silently. `Write` against a brand-new path is a silent no-op
    /// (no current state to read).
    Complexity,
    /// Report `impl` blocks whose pre-edit LCOM4 cohesion is above 1
    /// (split-personality types) in the file the agent is about to
    /// edit.
    ///
    /// The parser is chosen from the file extension (`.rs` / `.ts` /
    /// `.py`). Files with an unsupported extension are ignored
    /// silently.
    Cohesion,
}

#[derive(Debug, Subcommand)]
enum PostToolUseCommand {
    /// Report clusters of similar functions in the file that was just edited.
    ///
    /// The parser is chosen from the file extension (`.rs` / `.ts` /
    /// `.py`). Files with an unsupported extension are ignored silently.
    Similarity,
    /// Report functions whose body, after stripping a short chain of
    /// trivial adapters, is just a forwarding call to another function.
    ///
    /// The parser is chosen from the file extension (`.rs` / `.ts` /
    /// `.py`). Files with an unsupported extension are ignored silently.
    Wrapper,
}

#[derive(Debug, Subcommand)]
enum CodexHookCommand {
    /// Handle a Codex `SessionStart` event.
    #[command(subcommand)]
    SessionStart(CodexSessionStartCommand),
    /// Handle a Codex `PreToolUse` event.
    #[command(subcommand)]
    PreToolUse(CodexPreToolUseCommand),
    /// Handle a Codex `PostToolUse` event.
    #[command(subcommand)]
    PostToolUse(CodexPostToolUseCommand),
    /// Wire `agent-lens`'s Codex PostToolUse handlers into a Codex
    /// `config.toml`.
    ///
    /// The merge is conservative: existing keys and comments are
    /// preserved, and a `[[hooks.PostToolUse]]` block is appended only
    /// for handlers that aren't already wired up. Re-running the
    /// command is a no-op once every handler is installed.
    Setup(CodexSetupArgs),
}

#[derive(Debug, Args)]
struct CodexSetupArgs {
    /// Where to install the hooks. `user` writes to
    /// `$HOME/.codex/config.toml` (Codex's canonical location);
    /// `project` writes to `<repo-root>/.codex/config.toml`, where
    /// `repo-root` comes from `git rev-parse --show-toplevel` and
    /// falls back to the current directory outside a git tree.
    #[arg(long, value_enum, default_value_t = CodexSetupScope::User)]
    scope: CodexSetupScope,
    /// Show the resulting TOML without touching disk.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CodexSetupScope {
    Project,
    User,
}

impl From<CodexSetupScope> for codex_setup::ConfigScope {
    fn from(value: CodexSetupScope) -> Self {
        match value {
            CodexSetupScope::Project => Self::Project,
            CodexSetupScope::User => Self::User,
        }
    }
}

#[derive(Debug, Subcommand)]
enum CodexPostToolUseCommand {
    /// Report clusters of similar functions across every file Codex's
    /// `apply_patch` just touched.
    ///
    /// The parser is chosen from each file's extension (`.rs` / `.ts` /
    /// `.py`). Files with an unsupported extension are ignored silently.
    Similarity,
    /// Report functions whose body, after stripping a short chain of
    /// trivial adapters, is just a forwarding call to another function.
    ///
    /// Runs against every file Codex's `apply_patch` just touched. The
    /// parser is chosen from each file's extension (`.rs` / `.ts` /
    /// `.py`). Files with an unsupported extension are ignored silently.
    Wrapper,
}

#[derive(Debug, Subcommand)]
enum CodexPreToolUseCommand {
    /// Report functions whose pre-patch complexity crosses a
    /// non-trivial threshold across every file Codex's `apply_patch`
    /// is about to update.
    ///
    /// `*** Add File:` entries are skipped (no current state on disk);
    /// only `*** Update File:` paths are inspected.
    Complexity,
    /// Report `impl` blocks whose pre-patch LCOM4 cohesion is above 1
    /// across every file Codex's `apply_patch` is about to update.
    ///
    /// `*** Add File:` entries are skipped (no current state on disk);
    /// only `*** Update File:` paths are inspected.
    Cohesion,
}

#[derive(Debug, Subcommand)]
enum CodexSessionStartCommand {
    /// Inject a one-shot summary of the project's hotspots and
    /// coupling thumbnail into the new Codex session.
    ///
    /// Runs once per session against `cwd`. Pieces that don't apply
    /// (cwd outside a git working tree, or not anchored at a Rust
    /// crate) are silently omitted; if neither applies, the hook
    /// returns a no-op and Codex starts unchanged.
    Summary,
}

#[derive(Debug, Subcommand)]
enum AnalyzeCommand {
    /// Report LCOM4 cohesion units (one per `impl` block).
    ///
    /// Accepts either a single source file or a directory; in directory
    /// mode the analyzer walks recursively (respecting `.gitignore` like
    /// ripgrep) and groups findings per file. The parser is chosen from
    /// each file extension (`.rs` / `.ts` / `.py`). The JSON format is the
    /// default machine-readable output; `--format md` emits a compact
    /// summary tuned for LLM context.
    Cohesion {
        /// Path to a source file or a directory to analyze.
        path: PathBuf,
        /// Output format. Defaults to JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
        /// Restrict the report to `impl` blocks touching unstaged
        /// changed lines in `git diff -U0`.
        #[arg(long)]
        diff_only: bool,
        /// Cap the markdown ranking to the top-N units. JSON output
        /// always carries the full list.
        #[arg(long)]
        top: Option<usize>,
        /// Minimum LCOM4 score included in the markdown ranking. The
        /// markdown default is 2, which hides cohesive LCOM4=1 units;
        /// pass `--min-score 1` to include them.
        #[arg(long)]
        min_score: Option<usize>,
        #[command(flatten)]
        path_filter: AnalyzePathArgs,
    },
    /// Report per-function complexity metrics (Cyclomatic, Cognitive,
    /// Max Nesting, Halstead Volume, Maintainability Index).
    ///
    /// Accepts either a single source file or a directory; in directory
    /// mode the analyzer walks recursively (respecting `.gitignore` like
    /// ripgrep), groups findings per file, and aggregates the top-level
    /// summary across the whole corpus. The parser is chosen from each
    /// file extension (`.rs` / `.ts` / `.py`). The JSON format is the
    /// default machine-readable output; `--format md` emits a compact
    /// summary tuned for LLM context.
    Complexity {
        /// Path to a source file or a directory to analyze.
        path: PathBuf,
        /// Output format. Defaults to JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
        /// Restrict the report to functions touching unstaged changed
        /// lines in `git diff -U0`.
        #[arg(long)]
        diff_only: bool,
        /// Cap the markdown ranking to the top-N functions. JSON output
        /// always carries the full list.
        #[arg(long)]
        top: Option<usize>,
        /// Minimum cognitive complexity score included in the markdown
        /// ranking. JSON output always carries the full list.
        #[arg(long)]
        min_score: Option<u32>,
        #[command(flatten)]
        path_filter: AnalyzePathArgs,
    },
    /// Report module-level coupling metrics for a Rust crate.
    ///
    /// Number of Couplings, Fan-In, Fan-Out, simplified Henry-Kafura
    /// IFC ((fan_in*fan_out)^2), per-pair shared-symbol counts,
    /// Robert C. Martin's Instability `Ce/(Ca+Ce)`, and the strongly
    /// connected components of the dependency graph (cycles). `path`
    /// may be a `.rs` crate root (e.g. `src/lib.rs`) or a directory
    /// containing one.
    Coupling {
        /// Path to a `.rs` crate root or a directory containing
        /// `src/lib.rs` or `src/main.rs`.
        path: PathBuf,
        /// Output format. Defaults to JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
        #[command(flatten)]
        path_filter: AnalyzePathArgs,
    },
    /// Report each module's transitive outgoing dependency closure
    /// (its "context span").
    ///
    /// For every module in a Rust crate, lists the directly-depended
    /// modules, the modules reachable through one or more outgoing
    /// edges, and the count of distinct source files those modules
    /// span. Useful as an "onboarding cost" estimate — how many files
    /// an agent must open to reason about a given module. `path` may
    /// be a `.rs` crate root (e.g. `src/lib.rs`) or a directory
    /// containing one.
    ContextSpan {
        /// Path to a `.rs` crate root or a directory containing
        /// `src/lib.rs` or `src/main.rs`.
        path: PathBuf,
        /// Output format. Defaults to JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
        #[command(flatten)]
        path_filter: AnalyzePathArgs,
    },
    /// Rank files by `commits × cognitive_max` to surface hotspots.
    ///
    /// Walks `path` for supported source files (`.rs` / `.ts` / `.py`),
    /// asks `git` how many commits each file has been touched in
    /// (optionally scoped by `--since`), and joins the two with
    /// cognitive complexity. The resulting ranking points at
    /// "frequently changed *and* complex" code — where bugs concentrate
    /// and where a refactor is most likely to pay off. `path` must be
    /// inside a git working tree.
    Hotspot {
        /// File or directory to score. Must lie inside a git repo.
        path: PathBuf,
        /// Output format. Defaults to JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
        /// Restrict churn to commits in this `--since=` window. Accepts
        /// anything git's approxidate parser does (e.g. `90.days.ago`,
        /// `2024-01-01`).
        #[arg(long)]
        since: Option<String>,
        /// Cap the markdown table to the top-N entries (JSON always
        /// carries the full list).
        #[arg(long)]
        top: Option<usize>,
        #[command(flatten)]
        path_filter: AnalyzePathArgs,
    },
    /// Report clusters of near-duplicate functions.
    ///
    /// Accepts either a single source file or a directory; in directory
    /// mode the analyzer walks recursively (respecting `.gitignore` like
    /// ripgrep) and reports cross-file clusters alongside in-file ones.
    /// Function bodies are compared via TSED on their normalised AST;
    /// pairs scoring at or above `--threshold` are folded into complete-link
    /// clusters where every member is similar to every other (no chaining
    /// through weaker links). The parser is chosen from each file extension
    /// (`.rs` / `.ts` / `.py`). The JSON format is the default
    /// machine-readable output; `--format md` emits a compact summary
    /// tuned for LLM context.
    Similarity {
        /// Path to a source file or a directory to analyze.
        path: PathBuf,
        /// Output format. Defaults to JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
        /// Restrict the report to functions touching unstaged changed
        /// lines in `git diff -U0`.
        #[arg(long)]
        diff_only: bool,
        /// Shared path filters. `--exclude-tests` also drops
        /// language-level test functions for similarity.
        #[command(flatten)]
        path_filter: AnalyzePathArgs,
        /// Similarity threshold in [0.0, 1.0]. Pairs scoring at or above
        /// this value are eligible for clustering, and the same threshold
        /// is the complete-link cut so every pair inside a reported cluster
        /// stays at or above it. Defaults to the same cutoff used by the
        /// PostToolUse `similarity` hook.
        #[arg(long, visible_alias = "min-score", default_value_t = DEFAULT_SIMILARITY_THRESHOLD)]
        threshold: f64,
        /// Minimum source line count for a function to be considered.
        /// Functions shorter than this are dropped before pairwise
        /// comparison; mirrors `similarity-ts`'s `--min-lines` knob and
        /// keeps trivial getters / one-liners out of the report.
        #[arg(long, default_value_t = DEFAULT_SIMILARITY_MIN_LINES)]
        min_lines: usize,
        /// Cap the markdown report to the top-N similar clusters. JSON
        /// output always carries the full list.
        #[arg(long)]
        top: Option<usize>,
    },
    /// Report functions whose body, after stripping a short chain of
    /// trivial adapters, is just a forwarding call to another function.
    ///
    /// Accepts either a single source file or a directory; in directory
    /// mode the analyzer walks recursively (respecting `.gitignore` like
    /// ripgrep) and groups findings per file. The parser is chosen from
    /// each file extension (`.rs` / `.ts` / `.py`). The JSON format is the
    /// default machine-readable output; `--format md` emits a compact
    /// summary tuned for LLM context.
    Wrapper {
        /// Path to a source file or a directory to analyze.
        path: PathBuf,
        /// Output format. Defaults to JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
        /// Restrict the report to wrappers touching unstaged changed
        /// lines in `git diff -U0`.
        #[arg(long)]
        diff_only: bool,
        #[command(flatten)]
        path_filter: AnalyzePathArgs,
    },
}

#[derive(Debug, Clone, Args, Default)]
struct AnalyzePathArgs {
    /// Analyze only files that look like tests (`tests/`, `*_test.*`,
    /// `*.test.*`, `test_*`, etc.).
    #[arg(long, conflicts_with = "exclude_tests")]
    only_tests: bool,
    /// Exclude files that look like tests. For `similarity`, this also
    /// drops language-level test functions such as Rust `#[cfg(test)]`
    /// modules.
    #[arg(long, conflicts_with = "only_tests")]
    exclude_tests: bool,
    /// Exclude paths matching this glob. Repeatable. Bare patterns also
    /// match at any depth, so `--exclude generated.rs` matches
    /// `src/generated.rs`.
    #[arg(long = "exclude", value_name = "GLOB")]
    exclude: Vec<String>,
}

pub fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(error = %err, "agent-lens failed");
            ExitCode::from(1)
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Ignore the init result — a second call would only happen in tests
    // and would silently re-use the first subscriber.
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(filter)
        .try_init();
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Hook(HookCommand::SessionStart(sub)) => run_session_start(sub),
        Command::Hook(HookCommand::PreToolUse(sub)) => run_pre_tool_use(sub),
        Command::Hook(HookCommand::PostToolUse(sub)) => run_post_tool_use(sub),
        Command::Hook(HookCommand::Setup(args)) => run_hook_setup(args),
        Command::CodexHook(CodexHookCommand::SessionStart(sub)) => run_codex_session_start(sub),
        Command::CodexHook(CodexHookCommand::PreToolUse(sub)) => run_codex_pre_tool_use(sub),
        Command::CodexHook(CodexHookCommand::PostToolUse(sub)) => run_codex_post_tool_use(sub),
        Command::CodexHook(CodexHookCommand::Setup(args)) => run_codex_hook_setup(args),
        Command::Analyze(sub) => run_analyze(sub),
    }
}

fn run_analyze(cmd: AnalyzeCommand) -> Result<(), Box<dyn std::error::Error>> {
    write_stdout_line(&cmd.run()?)
}

impl AnalyzeCommand {
    /// Pick the right analyzer for this CLI variant and produce its
    /// report. Each arm constructs the analyzer inline; per-handler
    /// helper functions used to live here but were 100% similar to
    /// each other and added cyclomatic surface without insight.
    fn run(self) -> Result<String, Box<dyn std::error::Error>> {
        Ok(match self {
            Self::Cohesion {
                path,
                format,
                diff_only,
                top,
                min_score,
                path_filter,
            } => CohesionAnalyzer::new()
                .with_diff_only(diff_only)
                .with_top(top)
                .with_min_score(min_score)
                .with_only_tests(path_filter.only_tests)
                .with_exclude_tests(path_filter.exclude_tests)
                .with_exclude_patterns(path_filter.exclude)
                .analyze(&path, format)?,
            Self::Complexity {
                path,
                format,
                diff_only,
                top,
                min_score,
                path_filter,
            } => ComplexityAnalyzer::new()
                .with_diff_only(diff_only)
                .with_top(top)
                .with_min_score(min_score)
                .with_only_tests(path_filter.only_tests)
                .with_exclude_tests(path_filter.exclude_tests)
                .with_exclude_patterns(path_filter.exclude)
                .analyze(&path, format)?,
            Self::Coupling {
                path,
                format,
                path_filter,
            } => CouplingAnalyzer::new()
                .with_only_tests(path_filter.only_tests)
                .with_exclude_tests(path_filter.exclude_tests)
                .with_exclude_patterns(path_filter.exclude)
                .analyze(&path, format)?,
            Self::ContextSpan {
                path,
                format,
                path_filter,
            } => ContextSpanAnalyzer::new()
                .with_only_tests(path_filter.only_tests)
                .with_exclude_tests(path_filter.exclude_tests)
                .with_exclude_patterns(path_filter.exclude)
                .analyze(&path, format)?,
            Self::Hotspot {
                path,
                format,
                since,
                top,
                path_filter,
            } => HotspotAnalyzer::new()
                .with_top(top)
                .with_since_opt(since)
                .with_only_tests(path_filter.only_tests)
                .with_exclude_tests(path_filter.exclude_tests)
                .with_exclude_patterns(path_filter.exclude)
                .analyze(&path, format)?,
            Self::Similarity {
                path,
                format,
                diff_only,
                path_filter,
                threshold,
                min_lines,
                top,
            } => SimilarityAnalyzer::new()
                .with_threshold(threshold)
                .with_diff_only(diff_only)
                .with_only_tests(path_filter.only_tests)
                .with_exclude_tests(path_filter.exclude_tests)
                .with_exclude_patterns(path_filter.exclude)
                .with_min_lines(min_lines)
                .with_top(top)
                .analyze(&path, format)?,
            Self::Wrapper {
                path,
                format,
                diff_only,
                path_filter,
            } => WrapperAnalyzer::new()
                .with_diff_only(diff_only)
                .with_only_tests(path_filter.only_tests)
                .with_exclude_tests(path_filter.exclude_tests)
                .with_exclude_patterns(path_filter.exclude)
                .analyze(&path, format)?,
        })
    }
}

fn write_stdout_line(report: &str) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(report.as_bytes())?;
    if !report.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

fn run_session_start(cmd: SessionStartCommand) -> Result<(), Box<dyn std::error::Error>> {
    let ClaudeCodeHookInput::SessionStart(input) = read_stdin_json::<ClaudeCodeHookInput>()? else {
        return Err("expected a SessionStart hook payload on stdin".into());
    };
    let output = match cmd {
        SessionStartCommand::Summary => SessionStartSummaryHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

fn run_pre_tool_use(cmd: PreToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    let ClaudeCodeHookInput::PreToolUse(input) = read_stdin_json::<ClaudeCodeHookInput>()? else {
        return Err("expected a PreToolUse hook payload on stdin".into());
    };
    let output = match cmd {
        PreToolUseCommand::Complexity => ComplexityHook::new().handle(input)?,
        PreToolUseCommand::Cohesion => CohesionHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

fn run_post_tool_use(cmd: PostToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    let ClaudeCodeHookInput::PostToolUse(input) = read_stdin_json::<ClaudeCodeHookInput>()? else {
        return Err("expected a PostToolUse hook payload on stdin".into());
    };
    let output = match cmd {
        PostToolUseCommand::Similarity => SimilarityHook::new().handle(input)?,
        PostToolUseCommand::Wrapper => WrapperHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

fn run_hook_setup(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let path = setup::resolve_path(args.scope.into(), &cwd)?;
    let plan = setup::plan(path)?;
    let wrote = apply_setup_plan(
        args.dry_run,
        plan.changed(),
        SetupApplyContext {
            path: &plan.path,
            added_commands: plan.added_commands.len(),
            dry_run_message: "dry-run: leaving settings.json untouched",
            wrote_message: "wrote settings.json",
            unchanged_message: "settings.json already configured; nothing to do",
        },
        || setup::apply(&plan).map_err(Into::into),
    )?;
    write_stdout_json(&SetupSummary {
        path: &plan.path,
        wrote,
        added_commands: &plan.added_commands,
        settings: &plan.after,
    })
}

fn run_codex_hook_setup(args: CodexSetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let project_root = git_top_level(&cwd).unwrap_or(cwd);
    let path = codex_setup::resolve_path(args.scope.into(), &project_root)?;
    let plan = codex_setup::plan(path)?;
    let wrote = apply_setup_plan(
        args.dry_run,
        plan.changed(),
        SetupApplyContext {
            path: &plan.path,
            added_commands: plan.added_commands.len(),
            dry_run_message: "dry-run: leaving config.toml untouched",
            wrote_message: "wrote config.toml",
            unchanged_message: "config.toml already configured; nothing to do",
        },
        || codex_setup::apply(&plan).map_err(Into::into),
    )?;
    write_stdout_json(&CodexSetupSummary {
        path: &plan.path,
        wrote,
        added_commands: &plan.added_commands,
        config: &plan.after,
    })
}

fn apply_setup_plan(
    dry_run: bool,
    changed: bool,
    context: SetupApplyContext<'_>,
    apply: impl FnOnce() -> Result<(), Box<dyn std::error::Error>>,
) -> Result<bool, Box<dyn std::error::Error>> {
    if dry_run {
        info!(path = %context.path.display(), "{}", context.dry_run_message);
        return Ok(false);
    }

    if !changed {
        info!(path = %context.path.display(), "{}", context.unchanged_message);
        return Ok(false);
    }

    apply()?;
    info!(
        path = %context.path.display(),
        added = context.added_commands,
        "{}",
        context.wrote_message,
    );
    Ok(true)
}

struct SetupApplyContext<'a> {
    path: &'a Path,
    added_commands: usize,
    dry_run_message: &'static str,
    wrote_message: &'static str,
    unchanged_message: &'static str,
}

fn run_codex_pre_tool_use(cmd: CodexPreToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    let CodexHookInput::PreToolUse(input) = read_stdin_json::<CodexHookInput>()? else {
        return Err("expected a Codex PreToolUse hook payload on stdin".into());
    };
    let output = match cmd {
        CodexPreToolUseCommand::Complexity => CodexPreComplexityHook::new().handle(input)?,
        CodexPreToolUseCommand::Cohesion => CodexPreCohesionHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

fn run_codex_post_tool_use(cmd: CodexPostToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    let CodexHookInput::PostToolUse(input) = read_stdin_json::<CodexHookInput>()? else {
        return Err("expected a Codex PostToolUse hook payload on stdin".into());
    };
    let output = match cmd {
        CodexPostToolUseCommand::Similarity => CodexSimilarityHook::new().handle(input)?,
        CodexPostToolUseCommand::Wrapper => CodexWrapperHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

fn run_codex_session_start(
    cmd: CodexSessionStartCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let CodexHookInput::SessionStart(input) = read_stdin_json::<CodexHookInput>()? else {
        return Err("expected a Codex SessionStart hook payload on stdin".into());
    };
    let output = match cmd {
        CodexSessionStartCommand::Summary => CodexSessionStartSummaryHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

/// Resolve the enclosing git repository's top-level directory, or
/// `None` when `cwd` is not inside a git tree (or `git` isn't on
/// `PATH`). Used to anchor `--scope project` so the hook lands at the
/// repo root no matter which subdirectory the user invoked from.
fn git_top_level(cwd: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn read_stdin_json<T: serde::de::DeserializeOwned>() -> Result<T, Box<dyn std::error::Error>> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    Ok(serde_json::from_str(&buf)?)
}

fn write_stdout_json<T: serde::Serialize>(value: &T) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_hook_session_start_summary() {
        let cli = Cli::try_parse_from(["agent-lens", "hook", "session-start", "summary"])
            .expect("clean parse");
        assert!(matches!(
            cli.command,
            Command::Hook(HookCommand::SessionStart(SessionStartCommand::Summary)),
        ));
    }

    #[test]
    fn parses_hook_pre_tool_use_complexity() {
        let cli = Cli::try_parse_from(["agent-lens", "hook", "pre-tool-use", "complexity"])
            .expect("clean parse");
        assert!(matches!(
            cli.command,
            Command::Hook(HookCommand::PreToolUse(PreToolUseCommand::Complexity)),
        ));
    }

    #[test]
    fn parses_hook_pre_tool_use_cohesion() {
        let cli = Cli::try_parse_from(["agent-lens", "hook", "pre-tool-use", "cohesion"])
            .expect("clean parse");
        assert!(matches!(
            cli.command,
            Command::Hook(HookCommand::PreToolUse(PreToolUseCommand::Cohesion)),
        ));
    }

    #[test]
    fn parses_hook_post_tool_use_similarity() {
        let cli = Cli::try_parse_from(["agent-lens", "hook", "post-tool-use", "similarity"])
            .expect("clean parse");
        assert!(matches!(
            cli.command,
            Command::Hook(HookCommand::PostToolUse(PostToolUseCommand::Similarity)),
        ));
    }

    #[test]
    fn parses_hook_post_tool_use_wrapper() {
        let cli = Cli::try_parse_from(["agent-lens", "hook", "post-tool-use", "wrapper"])
            .expect("clean parse");
        assert!(matches!(
            cli.command,
            Command::Hook(HookCommand::PostToolUse(PostToolUseCommand::Wrapper)),
        ));
    }

    #[test]
    fn parses_hook_setup_with_default_scope() {
        let cli = Cli::try_parse_from(["agent-lens", "hook", "setup"]).expect("clean parse");
        let Command::Hook(HookCommand::Setup(args)) = cli.command else {
            panic!("expected hook setup");
        };
        assert!(matches!(args.scope, SetupScope::Project));
        assert!(!args.dry_run);
    }

    #[test]
    fn parses_hook_setup_with_user_scope_and_dry_run() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "hook",
            "setup",
            "--scope",
            "user",
            "--dry-run",
        ])
        .expect("clean parse");
        let Command::Hook(HookCommand::Setup(args)) = cli.command else {
            panic!("expected hook setup");
        };
        assert!(matches!(args.scope, SetupScope::User));
        assert!(args.dry_run);
    }

    #[test]
    fn parses_codex_hook_post_tool_use_similarity() {
        let cli = Cli::try_parse_from(["agent-lens", "codex-hook", "post-tool-use", "similarity"])
            .expect("clean parse");
        assert!(matches!(
            cli.command,
            Command::CodexHook(CodexHookCommand::PostToolUse(
                CodexPostToolUseCommand::Similarity,
            )),
        ));
    }

    #[test]
    fn parses_codex_hook_pre_tool_use_complexity() {
        let cli = Cli::try_parse_from(["agent-lens", "codex-hook", "pre-tool-use", "complexity"])
            .expect("clean parse");
        assert!(matches!(
            cli.command,
            Command::CodexHook(CodexHookCommand::PreToolUse(
                CodexPreToolUseCommand::Complexity,
            )),
        ));
    }

    #[test]
    fn parses_codex_hook_pre_tool_use_cohesion() {
        let cli = Cli::try_parse_from(["agent-lens", "codex-hook", "pre-tool-use", "cohesion"])
            .expect("clean parse");
        assert!(matches!(
            cli.command,
            Command::CodexHook(CodexHookCommand::PreToolUse(
                CodexPreToolUseCommand::Cohesion
            )),
        ));
    }

    #[test]
    fn parses_codex_hook_session_start_summary() {
        let cli = Cli::try_parse_from(["agent-lens", "codex-hook", "session-start", "summary"])
            .expect("clean parse");
        assert!(matches!(
            cli.command,
            Command::CodexHook(CodexHookCommand::SessionStart(
                CodexSessionStartCommand::Summary,
            )),
        ));
    }

    #[test]
    fn parses_codex_hook_setup_defaults_to_user_scope() {
        let cli = Cli::try_parse_from(["agent-lens", "codex-hook", "setup"]).expect("clean parse");
        let Command::CodexHook(CodexHookCommand::Setup(args)) = cli.command else {
            panic!("expected codex-hook setup");
        };
        assert!(matches!(args.scope, CodexSetupScope::User));
        assert!(!args.dry_run);
    }

    #[test]
    fn parses_analyze_similarity_with_threshold() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--threshold",
            "0.85",
            "--format",
            "md",
            "--diff-only",
            "--exclude-tests",
            "--exclude",
            "generated/**",
            "--min-lines",
            "8",
            "--top",
            "3",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity {
            path,
            format,
            diff_only,
            path_filter,
            threshold,
            min_lines,
            top,
        }) = cli.command
        else {
            panic!("expected analyze similarity");
        };
        assert_eq!(path, PathBuf::from("src/lib.rs"));
        assert_eq!(format, OutputFormat::Md);
        assert!(diff_only);
        assert!(path_filter.exclude_tests);
        assert_eq!(path_filter.exclude, ["generated/**"]);
        assert!((threshold - 0.85).abs() < f64::EPSILON);
        assert_eq!(min_lines, 8);
        assert_eq!(top, Some(3));
    }

    #[test]
    fn parses_analyze_similarity_min_score_alias() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--min-score",
            "0.91",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity { threshold, .. }) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert!((threshold - 0.91).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_analyze_complexity_with_top_and_min_score() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "complexity",
            "src/lib.rs",
            "--top",
            "12",
            "--min-score",
            "8",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Complexity { top, min_score, .. }) = cli.command
        else {
            panic!("expected analyze complexity");
        };
        assert_eq!(top, Some(12));
        assert_eq!(min_score, Some(8));
    }

    #[test]
    fn parses_analyze_cohesion_with_top_and_min_score() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "cohesion",
            "src/lib.rs",
            "--top",
            "7",
            "--min-score",
            "2",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Cohesion { top, min_score, .. }) = cli.command else {
            panic!("expected analyze cohesion");
        };
        assert_eq!(top, Some(7));
        assert_eq!(min_score, Some(2));
    }

    #[test]
    fn analyze_command_run_executes_analyzer_with_markdown_options() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
fn quiet() {}
fn branchy(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }
fn dispatch(n: i32) -> i32 {
    match n { 0 => 0, 1 => 1, 2 => 2, _ => 3 }
}
"#,
        );
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "complexity",
            file.to_str().unwrap(),
            "--format",
            "md",
            "--top",
            "1",
            "--min-score",
            "2",
        ])
        .expect("clean parse");
        let Command::Analyze(cmd) = cli.command else {
            panic!("expected analyze command");
        };
        let out = cmd.run().unwrap();
        assert!(out.contains("Top 1 by complexity"), "got: {out}");
        assert!(out.contains("`branchy`"), "got: {out}");
        assert!(!out.contains("`dispatch`"), "got: {out}");
    }

    #[test]
    fn parses_analyze_hotspot_with_since_and_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "hotspot",
            ".",
            "--since",
            "90.days.ago",
            "--top",
            "5",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Hotspot {
            since, top, format, ..
        }) = cli.command
        else {
            panic!("expected analyze hotspot");
        };
        assert_eq!(since.as_deref(), Some("90.days.ago"));
        assert_eq!(top, Some(5));
        assert_eq!(format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_coupling_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "coupling", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Coupling { path, format, .. }) = cli.command else {
            panic!("expected analyze coupling");
        };
        assert_eq!(path, PathBuf::from("."));
        assert_eq!(format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_context_span_with_md_format() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "context-span",
            "src/lib.rs",
            "--format",
            "md",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::ContextSpan { path, format, .. }) = cli.command else {
            panic!("expected analyze context-span");
        };
        assert_eq!(path, PathBuf::from("src/lib.rs"));
        assert_eq!(format, OutputFormat::Md);
    }

    #[test]
    fn analyze_command_requires_a_subcommand() {
        let err = Cli::try_parse_from(["agent-lens", "analyze"]).expect_err("missing subcommand");
        // clap reports this as DisplayHelpOnMissingArgumentOrSubcommand
        // because the parent command has no default behaviour without a
        // subcommand.
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
        );
    }

    #[test]
    fn analyze_cohesion_requires_path() {
        let err =
            Cli::try_parse_from(["agent-lens", "analyze", "cohesion"]).expect_err("missing path");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument,);
    }

    #[test]
    fn invalid_format_value_is_rejected() {
        let err = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "cohesion",
            "src/lib.rs",
            "--format",
            "yaml",
        ])
        .expect_err("yaml is not a known format");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn invalid_setup_scope_is_rejected() {
        let err = Cli::try_parse_from(["agent-lens", "hook", "setup", "--scope", "global"])
            .expect_err("global is not a known scope");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        let err = Cli::try_parse_from(["agent-lens", "lint"]).expect_err("no lint subcommand");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn unknown_post_tool_use_handler_is_rejected() {
        let err = Cli::try_parse_from(["agent-lens", "hook", "post-tool-use", "complexity"])
            .expect_err("complexity is not a hook handler");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn version_flag_short_circuits_parsing() {
        let err = Cli::try_parse_from(["agent-lens", "--version"]).expect_err("version exits");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    #[test]
    fn setup_scope_into_settings_scope_round_trip() {
        let project: SettingsScope = SetupScope::Project.into();
        let user: SettingsScope = SetupScope::User.into();
        assert!(matches!(project, SettingsScope::Project));
        assert!(matches!(user, SettingsScope::User));
    }

    #[test]
    fn codex_setup_scope_into_config_scope_round_trip() {
        let project: codex_setup::ConfigScope = CodexSetupScope::Project.into();
        let user: codex_setup::ConfigScope = CodexSetupScope::User.into();
        assert!(matches!(project, codex_setup::ConfigScope::Project));
        assert!(matches!(user, codex_setup::ConfigScope::User));
    }

    #[test]
    fn git_top_level_returns_none_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        // tempdir() returns a fresh path; nothing inside it is git-tracked.
        assert!(git_top_level(dir.path()).is_none());
    }

    #[test]
    fn git_top_level_finds_repo_root_from_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let nested = dir.path().join("nested/inner");
        std::fs::create_dir_all(&nested).unwrap();
        let resolved = git_top_level(&nested).expect("inside the new repo");
        // Resolve symlinks on both sides — macOS tempdirs live under
        // /private/var/... while git emits /var/..., so a literal
        // comparison is fragile.
        let canonical_dir = std::fs::canonicalize(dir.path()).unwrap();
        let canonical_resolved = std::fs::canonicalize(&resolved).unwrap();
        assert_eq!(canonical_resolved, canonical_dir);
    }
}
