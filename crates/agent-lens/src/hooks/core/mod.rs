//! Engine-agnostic primitives shared by every PostToolUse hook.
//!
//! The Claude Code and Codex adapters each prepare their own
//! [`EditedSource`] list and then hand it off to a `…Core` here. The cores
//! own the actual analysis (parser dispatch, threshold handling, report
//! formatting) so each agent's hook module is just a thin trait
//! implementation that wires up the engine-specific input/output shapes.

pub mod cohesion;
pub mod complexity;
pub mod error;
pub mod runner;
pub mod similarity;
pub mod wrapper;

pub use error::{HookError, ReadEditedSourceError};
pub use runner::{CohesionHook, ComplexityHook, HookEnvelope, SimilarityHook, WrapperHook};

use crate::analyze::SourceLang;

/// One file the agent just edited, prepared for a hook to analyse.
///
/// The same struct is produced by both the Claude Code single-file path
/// (which yields zero or one entry) and the Codex `apply_patch` path
/// (which yields one entry per touched file).
#[derive(Debug)]
pub struct EditedSource {
    /// Path verbatim as it appeared in the agent's input — kept so reports
    /// quote it back without resolving it to an absolute path.
    pub rel_path: String,
    pub lang: SourceLang,
    pub source: String,
}
