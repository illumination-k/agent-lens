//! `agent-lens` — hook handlers and analyzers for coding agents.
//!
//! The binary bundles two families of subcommands:
//!
//! * [`hooks`] — handlers that speak Claude Code's stdin/stdout hook
//!   protocol. Each handler is addressed by a short name so that the same
//!   binary can serve many hooks from `settings.json`.
//! * [`analyze`] — on-demand code analyses that produce LLM-friendly context
//!   (e.g. cohesion reports).
//! * [`config`] — `agent-lens.toml` parsing: named analysis profiles that
//!   the `run` subcommand fans out across several analyzers.
//! * [`config_schema`] — renders the `agent-lens.toml` schema as
//!   agent-friendly Markdown for `config schema`.
//! * [`skills`] — the Claude Code skills bundled into the binary plus the
//!   `skills install` plan/apply logic.
//! * [`help_md`] — renders the whole command tree as an agent-friendly
//!   Markdown reference for `help --md`.
//!
//! Only the pieces exercised by the current CLI live here today; the rest
//! will land as new subcommands are added.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod analyze;
pub mod config;
pub mod config_schema;
pub mod help_md;
pub mod hooks;
pub mod paths;
pub mod skills;

#[doc(hidden)]
pub mod test_support;
