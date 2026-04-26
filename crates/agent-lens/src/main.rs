//! `agent-lens` CLI entry point.
//!
//! Each PostToolUse handler is a clap subcommand, so `agent-lens hook
//! post-tool-use similarity` is parsed statically instead of routed by
//! a runtime name string. Analyzers live under `agent-lens analyze ...`
//! and write their report to stdout. Stdout is otherwise reserved for the
//! hook's JSON response; diagnostics go to stderr via `tracing`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::{self, Read, Write as _};
use std::path::PathBuf;
use std::process::ExitCode;

use agent_hooks::Hook;
use agent_hooks::claude_code::ClaudeCodeHookInput;
use agent_hooks::codex::CodexHookInput;
use agent_lens::analyze::{
    CohesionAnalyzer, ComplexityAnalyzer, CouplingAnalyzer, DEFAULT_SIMILARITY_THRESHOLD,
    OutputFormat, SimilarityAnalyzer, WrapperAnalyzer,
};
use agent_lens::hooks::codex::post_tool_use::{
    SimilarityHook as CodexSimilarityHook, WrapperHook as CodexWrapperHook,
};
use agent_lens::hooks::post_tool_use::{SimilarityHook, WrapperHook};
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
    /// Run a handler for one of Codex's hook events.
    #[command(subcommand)]
    CodexHook(CodexHookCommand),
    /// Run an on-demand analyzer that emits LLM-friendly context.
    #[command(subcommand)]
    Analyze(AnalyzeCommand),
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
    /// Report functions whose body, after stripping a short chain of
    /// trivial adapters, is just a forwarding call to another function.
    ///
    /// The parser is chosen from the file extension (`.rs` today).
    /// Files with an unsupported extension are ignored silently.
    Wrapper,
}

#[derive(Debug, Subcommand)]
enum CodexHookCommand {
    /// Handle a Codex `PostToolUse` event.
    #[command(subcommand)]
    PostToolUse(CodexPostToolUseCommand),
}

#[derive(Debug, Subcommand)]
enum CodexPostToolUseCommand {
    /// Report similar function pairs across every file Codex's
    /// `apply_patch` just touched.
    ///
    /// The parser is chosen from each file's extension (`.rs` today).
    /// Files with an unsupported extension are ignored silently.
    Similarity,
    /// Report functions whose body, after stripping a short chain of
    /// trivial adapters, is just a forwarding call to another function.
    ///
    /// Runs against every file Codex's `apply_patch` just touched.
    /// Files with an unsupported extension are ignored silently.
    Wrapper,
}

#[derive(Debug, Subcommand)]
enum AnalyzeCommand {
    /// Report LCOM4 cohesion units (one per `impl` block) for a source file.
    ///
    /// The parser is chosen from the file extension (`.rs` today). The JSON
    /// format is the default machine-readable output; `--format md` emits a
    /// compact summary tuned for LLM context.
    Cohesion {
        /// Path to a source file to analyze.
        path: PathBuf,
        /// Output format. Defaults to JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
    },
    /// Report per-function complexity metrics (Cyclomatic, Cognitive,
    /// Max Nesting, Halstead Volume, Maintainability Index) for a source
    /// file.
    ///
    /// The parser is chosen from the file extension (`.rs` today). The JSON
    /// format is the default machine-readable output; `--format md` emits a
    /// compact summary tuned for LLM context.
    Complexity {
        /// Path to a source file to analyze.
        path: PathBuf,
        /// Output format. Defaults to JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
    },
    /// Report module-level coupling metrics for a Rust crate.
    ///
    /// Number of Couplings, Fan-In, Fan-Out, simplified Henry-Kafura
    /// IFC ((fan_in*fan_out)^2), and per-pair shared-symbol counts.
    /// `path` may be a `.rs` crate root (e.g. `src/lib.rs`) or a
    /// directory containing one.
    Coupling {
        /// Path to a `.rs` crate root or a directory containing
        /// `src/lib.rs` or `src/main.rs`.
        path: PathBuf,
        /// Output format. Defaults to JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
    },
    /// Report near-duplicate function pairs in a source file.
    ///
    /// Function bodies are compared via TSED on their normalised AST and
    /// reported when their similarity is at or above `--threshold`. The
    /// parser is chosen from the file extension (`.rs` today). The JSON
    /// format is the default machine-readable output; `--format md` emits
    /// a compact summary tuned for LLM context.
    Similarity {
        /// Path to a source file to analyze.
        path: PathBuf,
        /// Output format. Defaults to JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
        /// Similarity threshold in [0.0, 1.0]. Pairs scoring at or above
        /// this value are reported. Defaults to the same cutoff used by
        /// the PostToolUse `similarity` hook.
        #[arg(long, default_value_t = DEFAULT_SIMILARITY_THRESHOLD)]
        threshold: f64,
    },
    /// Report functions whose body, after stripping a short chain of
    /// trivial adapters, is just a forwarding call to another function.
    ///
    /// The parser is chosen from the file extension (`.rs` today). The JSON
    /// format is the default machine-readable output; `--format md` emits
    /// a compact summary tuned for LLM context.
    Wrapper {
        /// Path to a source file to analyze.
        path: PathBuf,
        /// Output format. Defaults to JSON.
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
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
        Command::CodexHook(CodexHookCommand::PostToolUse(sub)) => run_codex_post_tool_use(sub),
        Command::Analyze(sub) => run_analyze(sub),
    }
}

fn run_analyze(cmd: AnalyzeCommand) -> Result<(), Box<dyn std::error::Error>> {
    write_stdout_line(&cmd.run()?)
}

impl AnalyzeCommand {
    /// Pick the right analyzer for this CLI variant and produce its
    /// report. Kept on the enum so `run_analyze` is a one-liner and
    /// adding a new analyzer is a localised arm here.
    fn run(self) -> Result<String, Box<dyn std::error::Error>> {
        Ok(match self {
            Self::Cohesion { path, format } => CohesionAnalyzer::new().analyze(&path, format)?,
            Self::Complexity { path, format } => {
                ComplexityAnalyzer::new().analyze(&path, format)?
            }
            Self::Coupling { path, format } => CouplingAnalyzer::new().analyze(&path, format)?,
            Self::Similarity {
                path,
                format,
                threshold,
            } => SimilarityAnalyzer::new()
                .with_threshold(threshold)
                .analyze(&path, format)?,
            Self::Wrapper { path, format } => WrapperAnalyzer::new().analyze(&path, format)?,
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

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }
}
