//! `agent-lens` CLI entry point.
//!
//! The binary reads JSON from stdin when invoked as a hook handler,
//! dispatches to the named handler, and writes the handler's JSON response
//! back to stdout. All diagnostics go to stderr via `tracing` — stdout is
//! reserved for the protocol response so Claude Code can parse it.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::{self, Read, Write as _};
use std::process::ExitCode;

use agent_hooks::claude_code::ClaudeCodeHookInput;
use agent_lens::hooks::post_tool_use;
use clap::{Parser, Subcommand};
use tracing::error;
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
}

#[derive(Debug, Subcommand)]
enum HookCommand {
    /// Handle a `PostToolUse` event by dispatching to the named handler.
    ///
    /// Reads the hook payload from stdin, writes the JSON response to
    /// stdout, and logs diagnostics to stderr.
    PostToolUse {
        /// Handler name (e.g. `rust-similarity`).
        name: String,
    },
}

fn main() -> ExitCode {
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
    // `try_init` so repeated calls in tests (should we ever use them) don't
    // panic; in `main` we ignore the result because a failure here just
    // means logging is silenced, not that the hook should abort.
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(filter)
        .try_init();
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Hook(HookCommand::PostToolUse { name }) => run_post_tool_use(&name),
    }
}

fn run_post_tool_use(name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;

    let event: ClaudeCodeHookInput = serde_json::from_str(&buf)?;
    let ClaudeCodeHookInput::PostToolUse(input) = event else {
        return Err("expected a PostToolUse hook payload on stdin".into());
    };

    let output = post_tool_use::dispatch(name, input)?;

    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, &output)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }
}
