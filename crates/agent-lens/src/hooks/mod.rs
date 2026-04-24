//! Claude Code hook handlers, grouped by event.
//!
//! Each submodule exposes a `dispatch(name, input)` entry point that the
//! binary calls after reading JSON from stdin and routing on the
//! subcommand name (e.g. `rust-similarity`).

pub mod post_tool_use;
