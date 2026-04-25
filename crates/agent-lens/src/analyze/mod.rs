//! On-demand analyzers that emit LLM-friendly context.
//!
//! Each submodule is one analyzer (cohesion, similarity, hotspot, …) and is
//! wired to a clap subcommand so typos surface at parse time. Output is
//! always written to stdout as JSON by default; analyzers can opt in to a
//! `--format md` mode for a more compact human-readable summary.

pub mod cohesion;

pub use cohesion::{CohesionAnalyzer, CohesionAnalyzerError, OutputFormat};
