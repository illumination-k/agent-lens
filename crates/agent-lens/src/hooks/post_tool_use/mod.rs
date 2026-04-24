//! `PostToolUse` hook handlers.
//!
//! Each submodule is one handler; the CLI wires them to clap
//! subcommands so that typos surface at parse time rather than at
//! runtime.

pub mod similarity;

pub use similarity::{SimilarityError, SimilarityHook};
