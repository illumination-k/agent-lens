//! `agent-lens` CLI entry point.

use std::process::ExitCode;

mod cli;

fn main() -> ExitCode {
    cli::main()
}
