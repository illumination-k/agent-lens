//! Hook protocol types and dispatch trait for coding agents.
//!
//! Each supported agent lives in its own module (currently [`claude_code`]).
//! A hook handler implements [`Hook`] for a specific `Input`/`Output` pair and
//! is responsible for the domain logic; this crate only deals with the schema.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod claude_code;

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
