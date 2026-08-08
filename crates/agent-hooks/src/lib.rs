//! Hook protocol types and dispatch trait for coding agents.
//!
//! Each supported agent lives in its own module ([`claude_code`], [`codex`]).
//! A hook handler implements [`Hook`] for a specific `Input`/`Output` pair and
//! is responsible for the domain logic; this crate only deals with the schema.
//!
//! Both engines model every event they document, not just the ones
//! `agent-lens` currently handles. `UserPromptSubmit`, `Stop`,
//! `SubagentStop`, and Codex's `PermissionRequest` have no handler on the
//! CLI yet; they are kept deliberately so a new handler is a domain-logic
//! change rather than a schema change, and so the tagged input enums
//! round-trip every payload an agent can send. See
//! <https://github.com/illumination-k/agent-lens/issues/71> (Phase 4) for
//! the handlers planned on top of them.
//!
//! [`common::CommonHookOutput`] is shared by both engines: the four
//! "what should the agent do next" keys are spelled identically in the
//! two protocols, so they are modeled once.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod claude_code;
pub mod codex;
pub mod common;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// A handler for a single hook event.
///
/// `Input` is deserialized from the agent's stdin payload and `Output` is
/// serialized back to stdout. The associated `Error` type lets implementors
/// surface domain-specific failures without forcing a common error crate.
pub trait Hook {
    type Input: DeserializeOwned;
    type Output: Serialize;
    type Error: std::error::Error + 'static;

    fn handle(&self, input: Self::Input) -> Result<Self::Output, Self::Error>;
}
