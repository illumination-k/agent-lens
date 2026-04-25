//! Codex hook handlers, grouped by event.
//!
//! Each submodule is one hook event; the CLI wires individual handlers
//! to clap subcommands so that typos surface at parse time rather than at
//! runtime.

pub mod post_tool_use;
