//! Analyzers that read the git history rather than only the tree.
//!
//! `churn` runs the shared `git log` pass; the analyzers here and
//! `analyze hotspot` / `analyze risk` build on it.

pub mod change_entropy;
pub(super) mod churn;
pub mod co_change;
pub(super) mod error_from;
pub mod hidden_coupling;
pub mod ownership;
mod static_file_graph;
