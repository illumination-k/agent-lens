//! On-demand analyzers that emit LLM-friendly context.
//!
//! Each submodule is one analyzer (cohesion, complexity, similarity,
//! hotspot, …) and is wired to a clap subcommand so typos surface at
//! parse time. Output is always written to stdout as JSON by default;
//! analyzers can opt in to a `--format md` mode for a more compact
//! human-readable summary.

pub mod cohesion;
pub mod complexity;

pub use cohesion::{CohesionAnalyzer, CohesionAnalyzerError};
pub use complexity::{ComplexityAnalyzer, ComplexityAnalyzerError};

/// Output format shared across analyzers.
///
/// Lives at the module root so every analyzer's `--format` flag
/// resolves to the same enum, both in clap and in the public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Json,
    Md,
}
