//! Codex hook handlers, grouped by event.
//!
//! Each submodule is one hook event; the CLI wires individual handlers
//! to clap subcommands so that typos surface at parse time rather than at
//! runtime. `setup` is a separate one-shot command that writes the
//! handler entries into the user's `~/.codex/config.toml`.

pub mod post_tool_use;
pub mod setup;
