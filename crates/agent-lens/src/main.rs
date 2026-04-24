//! `agent-lens` CLI entry point.
//!
//! Each PostToolUse handler is a clap subcommand, so `agent-lens hook
//! post-tool-use similarity` is parsed statically instead of routed by
//! a runtime name string. Stdout is reserved for the hook's JSON
//! response; diagnostics go to stderr via `tracing`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::{self, Read, Write as _};
use std::process::ExitCode;

use agent_hooks::Hook;
use agent_hooks::claude_code::{ClaudeCodeHookInput, PostToolUseInput, PostToolUseOutput};
use agent_lens::hooks::post_tool_use::SimilarityHook;
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
    /// Handle a `PostToolUse` event.
    #[command(subcommand)]
    PostToolUse(PostToolUseCommand),
}

#[derive(Debug, Subcommand)]
enum PostToolUseCommand {
    /// Report similar function pairs in the file that was just edited.
    ///
    /// The parser is chosen from the file extension (`.rs` today).
    /// Files with an unsupported extension are ignored silently.
    Similarity,
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
    // Ignore the init result — a second call would only happen in tests
    // and would silently re-use the first subscriber.
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(filter)
        .try_init();
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Hook(HookCommand::PostToolUse(sub)) => run_post_tool_use(sub),
    }
}

fn run_post_tool_use(cmd: PostToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    let input = read_post_tool_use_from_stdin()?;
    let output = match cmd {
        PostToolUseCommand::Similarity => SimilarityHook::new().handle(input)?,
    };
    write_output_to_stdout(&output)
}

fn read_post_tool_use_from_stdin() -> Result<PostToolUseInput, Box<dyn std::error::Error>> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    let event: ClaudeCodeHookInput = serde_json::from_str(&buf)?;
    match event {
        ClaudeCodeHookInput::PostToolUse(input) => Ok(input),
        _ => Err("expected a PostToolUse hook payload on stdin".into()),
    }
}

fn write_output_to_stdout(output: &PostToolUseOutput) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, output)?;
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
