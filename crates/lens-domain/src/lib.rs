//! Shared analysis primitives for `agent-lens`.
//!
//! This crate holds language-agnostic building blocks that each
//! language-specific crate (currently [`lens-rust`](../lens_rust/index.html))
//! plugs into:
//!
//! * [`TreeNode`] — a small labelled tree used as a common currency for AST
//!   comparison.
//! * [`apted`] — tree edit distance (Zhang-Shasha-style with configurable
//!   operation costs), modelled after `similarity-ts-core`'s APTED.
//! * [`tsed`] — a normalised similarity score derived from the edit distance,
//!   with an optional size penalty for short functions.
//! * [`function`] — the [`LanguageParser`] trait, [`FunctionDef`] type, and
//!   [`find_similar_functions`] helper that drives pairwise comparison.
//! * [`cohesion`] — LCOM4-style cohesion metric over method graphs that the
//!   language adapters (e.g. `lens-rust`) populate.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod apted;
pub mod cohesion;
pub mod coupling;
pub mod function;
pub mod tree;
pub mod tsed;

pub use apted::{APTEDOptions, compute_edit_distance};
pub use cohesion::{
    CohesionUnit, CohesionUnitKind, MethodCohesion, compute_components, compute_lcom96,
};
pub use coupling::{
    CouplingEdge, CouplingReport, EdgeKind, ModuleMetrics, ModulePath, PairCoupling, compute_report,
};
pub use function::{FunctionDef, LanguageParser, SimilarPair, find_similar_functions};
pub use tree::TreeNode;
pub use tsed::{TSEDOptions, calculate_tsed};
