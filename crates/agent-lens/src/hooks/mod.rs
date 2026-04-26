//! Hook handlers, grouped by agent and then by event.
//!
//! The Claude Code handlers live directly under `post_tool_use` for
//! historical reasons (they were the first to land); Codex handlers are
//! namespaced under [`codex`]. The CLI wires each handler to a clap
//! subcommand so typos surface at parse time.

pub mod codex;
pub(crate) mod core;
pub mod post_tool_use;
