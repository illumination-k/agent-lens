//! `agent-lens` — hook handlers and analyzers for coding agents.
//!
//! The binary bundles two families of subcommands:
//!
//! * [`hooks`] — handlers that speak Claude Code's stdin/stdout hook
//!   protocol. Each handler is addressed by a short name so that the same
//!   binary can serve many hooks from `settings.json`.
//! * analyzers (forthcoming) — on-demand code analyses that produce
//!   LLM-friendly context.
//!
//! Only the pieces exercised by the current CLI live here today; the rest
//! will land as new subcommands are added.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod hooks;
