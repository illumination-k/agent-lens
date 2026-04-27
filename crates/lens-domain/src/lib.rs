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
//! * [`complexity`] — per-function Cyclomatic / Cognitive / Nesting / Halstead
//!   counts, plus derived Maintainability Index. Adapters fill in the counts;
//!   the derived metrics live here so every language goes through the same
//!   formula.
//! * [`coupling`] — module-level Number of Couplings / Fan-In / Fan-Out /
//!   Henry-Kafura IFC / Inter-module coupling / Instability / dependency
//!   cycles. Adapters produce [`CouplingEdge`]s; this module folds them
//!   into the report.
//! * [`context_span`] — for each module, the transitive closure of its
//!   outgoing dependencies. Reuses the [`CouplingEdge`] graph and
//!   answers "how many other modules must I read to fully understand
//!   this one".
//! * [`hotspot`] — `commits × cognitive_max` scoring per file. Adapters
//!   feed in per-file complexity rollups and a churn table; this module
//!   merges them into a ranked list.
//! * [`wrapper`] — thin-wrapper finding shape. Adapters decide what
//!   counts as a trivial adapter in their grammar; the result type is
//!   shared so `agent-lens` can dispatch on language without per-adapter
//!   conversion.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod apted;
pub mod cohesion;
pub mod complexity;
pub mod context_span;
pub mod coupling;
pub mod function;
pub mod hotspot;
pub mod naming;
pub mod tree;
pub mod tsed;
pub mod wrapper;

pub use apted::{APTEDOptions, compute_edit_distance};
pub use cohesion::{
    CohesionUnit, CohesionUnitKind, MethodCohesion, compute_components, compute_lcom96,
};
pub use complexity::{FunctionComplexity, HalsteadCounts};
pub use context_span::{ContextSpanReport, ModuleContextSpan, compute_context_spans};
pub use coupling::{
    CouplingEdge, CouplingReport, DependencyCycle, EdgeKind, ModuleMetrics, ModulePath,
    PairCoupling, compute_report,
};
pub use function::{FunctionDef, LanguageParser, SimilarPair, find_similar_functions};
pub use hotspot::{FileChurn, FileComplexity, HotspotEntry, compute_hotspots};
pub use naming::qualify;
pub use tree::TreeNode;
pub use tsed::{TSEDOptions, calculate_tsed};
pub use wrapper::{WrapperFinding, args_pass_through_by};
