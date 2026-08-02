//! `agent-lens` CLI parsing and command dispatch.
//!
//! Each hook handler is a clap subcommand, so `agent-lens hook
//! post-tool-use similarity` and `agent-lens codex-hook pre-tool-use
//! complexity` are parsed statically instead of routed by runtime name
//! strings. Analyzers live under `agent-lens analyze ...` and write their
//! report to stdout. Stdout is otherwise reserved for the hook's JSON
//! response; diagnostics go to stderr via `tracing`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::{self, Write as _};
use std::process::ExitCode;

use agent_lens::{config_schema, help_md, skills};
use clap::{CommandFactory, Parser};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

mod analyze;
mod args;
mod examples;
mod hooks;
mod profile;

use analyze::run_analyze;
use args::{
    Cli, CodexHookCommand, Command, ConfigCommand, HelpArgs, HookCommand, SkillsCommand,
    SkillsInstallArgs,
};
use hooks::{
    run_codex_hook_setup, run_codex_post_tool_use, run_codex_pre_tool_use, run_codex_session_start,
    run_hook_setup, run_post_tool_use, run_pre_tool_use, run_session_start,
};
use profile::run_profile;

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
        Command::Run(args) => run_profile(args),
        Command::Skills(sub) => run_skills(sub),
        Command::Config(sub) => run_config(sub),
        Command::Help(args) => run_help(args),
    }
}

/// Emit the `agent-lens.toml` schema reference on stdout.
fn run_config(cmd: ConfigCommand) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ConfigCommand::Schema => write_stdout_line(&config_schema::render()),
    }
}

/// Print the command reference. `--md` renders the agent-friendly
/// Markdown document; otherwise we defer to clap's own long help so
/// `agent-lens help` matches `agent-lens --help`.
fn run_help(args: HelpArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Cli::command();
    if args.md {
        let report = help_md::render(&command);
        write_stdout_line(&report)
    } else {
        write_stdout_line(&command.render_long_help().to_string())
    }
}

fn run_skills(cmd: SkillsCommand) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        SkillsCommand::List => write_stdout_line(&skills::render_list()),
        SkillsCommand::Install(args) => run_skills_install(args),
    }
}

/// Diff the bundled skills against the chosen scope and install the
/// missing (or, with `--force`, the changed) ones. Conflicts are logged
/// and reflected in the JSON summary so the agent can decide whether to
/// re-run with `--force`.
fn run_skills_install(args: SkillsInstallArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let root = skills::resolve_root(args.scope, &cwd)?;
    let plan = skills::plan(root, args.force)?;

    for conflict in plan.conflicts() {
        warn!(
            skill = conflict.name,
            path = %conflict.path.display(),
            "skill already exists with different content; re-run with --force to overwrite",
        );
    }

    let wrote = if args.dry_run {
        info!(root = %plan.root.display(), "dry-run: leaving skills untouched");
        false
    } else if plan.changed() {
        skills::apply(&plan)?;
        info!(root = %plan.root.display(), "installed skills");
        true
    } else {
        info!(root = %plan.root.display(), "skills already installed; nothing to do");
        false
    };

    write_stdout_json(&plan.summary(wrote))
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

    #[test]
    fn help_md_render_covers_the_whole_command_surface() {
        // The `help --md` document must reach the deepest analyzer leaves,
        // not just the top-level command trees.
        let md = help_md::render(&Cli::command());
        assert!(md.starts_with("# agent-lens\n"), "got: {md}");
        assert!(md.contains("## `agent-lens analyze`"), "got: {md}");
        assert!(
            md.contains("### `agent-lens analyze similarity`"),
            "got: {md}",
        );
        assert!(md.contains("## `agent-lens skills`"), "got: {md}");
        assert!(md.contains("## `agent-lens config`"), "got: {md}");
        assert!(md.contains("### `agent-lens config schema`"), "got: {md}");
        // Analyzer-specific options surface in the table.
        assert!(md.contains("`--threshold <THRESHOLD>`"), "got: {md}");
    }
}
