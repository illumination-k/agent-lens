//! Shared data model for the function call graph substrate.
//!
//! These types are the crate-internal currency between the graph
//! builder ([`super::CallGraph`]), the symbol resolver
//! ([`super::resolve::Resolver`]), and every analyzer that consumes
//! the graph. They double as the serialized report shapes for
//! `analyze function-graph`, so field names and ordering here are
//! part of the JSON schema.

use lens_domain::{FunctionShape, SyntaxFact, VisibilityShape};
use serde::Serialize;

/// One function definition in the call graph.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CallGraphNode {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) qualified_name: String,
    pub(crate) file: String,
    pub(crate) module: String,
    pub(crate) impl_owner: Option<String>,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) is_test: bool,
    /// Syntactic visibility. Correct for Rust and Go; `Unknown` for
    /// TypeScript and Python until their adapters extract export
    /// status.
    pub(crate) visibility: NodeVisibility,
    pub(crate) weights: NodeWeights,
    /// Per-node counts of outgoing call sites by resolution outcome.
    /// Non-resolved edges carry no target endpoint to tag, so this is
    /// the per-node confidence signal: a node with many unresolved
    /// outgoing calls has an undercounted fan-out.
    pub(crate) outgoing_calls: ResolutionCallCounts,
}

/// Language-neutral projection of [`VisibilityShape`] for graph nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NodeVisibility {
    Public,
    Restricted,
    Private,
    Exported,
    Unexported,
    Unknown,
}

impl NodeVisibility {
    pub(crate) fn from_shape(fact: &SyntaxFact<VisibilityShape>) -> Self {
        match fact {
            SyntaxFact::Known(VisibilityShape::Public) => Self::Public,
            SyntaxFact::Known(VisibilityShape::Restricted(_)) => Self::Restricted,
            SyntaxFact::Known(VisibilityShape::Private) => Self::Private,
            SyntaxFact::Known(VisibilityShape::Exported) => Self::Exported,
            SyntaxFact::Known(VisibilityShape::Unexported) => Self::Unexported,
            SyntaxFact::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct NodeWeights {
    pub(crate) incoming_call_count: usize,
    pub(crate) outgoing_call_count: usize,
    pub(crate) fan_in: usize,
    pub(crate) fan_out: usize,
    pub(crate) loc: usize,
    pub(crate) cyclomatic_complexity: Option<u32>,
    pub(crate) cognitive_complexity: Option<u32>,
    pub(crate) max_nesting: Option<u32>,
    pub(crate) maintainability_index: Option<f64>,
    pub(crate) halstead_volume: Option<f64>,
    pub(crate) total_time_ms: Option<f64>,
    pub(crate) self_time_ms: Option<f64>,
    pub(crate) error_count: Option<u64>,
}

/// One aggregated call edge. Edges group call sites that share the
/// same caller, target (or candidate set), callee name, and
/// resolution.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct CallGraphEdge {
    pub(crate) from: Option<String>,
    pub(crate) to: Option<String>,
    pub(crate) callee_name: Option<String>,
    pub(crate) resolution: Resolution,
    /// Candidate target node ids for ambiguous edges. Empty (and
    /// omitted from JSON) for every other resolution.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) candidates: Vec<String>,
    /// Which resolver strategy produced `to` (or `candidates`).
    /// Absent for unresolved and anonymous edges. When grouped call
    /// sites reached the target through different strategies, the
    /// most direct one (the [`ResolutionMethod`] ordering) is kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) resolution_method: Option<ResolutionMethod>,
    pub(crate) call_count: usize,
    pub(crate) call_lines: Vec<usize>,
    pub(crate) weights: EdgeWeights,
}

#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct EdgeWeights {
    pub(crate) call_count: usize,
    pub(crate) total_transition_time_ms: Option<f64>,
    pub(crate) error_count: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Resolution {
    Resolved,
    Unresolved,
    Ambiguous,
    Anonymous,
}

/// Provenance of a resolved or ambiguous edge: which heuristic the
/// resolver used to pick the target (or candidate set).
///
/// Declaration order doubles as the `Ord` ranking, most direct
/// strategy first; edge aggregation keeps the minimum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ResolutionMethod {
    /// Matched a lexical candidate path (absolute, `self::`/`super::`,
    /// `Self::`, imports/aliases) against a qualified name.
    Lexical,
    /// `self.method()` resolved to `Owner::method` in the caller's
    /// module.
    SelfMethod,
    /// Fallback match on the last path segment of the callee name.
    LastSegment,
    /// Last-segment candidates narrowed by a multi-segment callee path
    /// suffix (e.g. `Foo::new` through a glob import).
    PathSuffix,
    /// Last-segment candidates narrowed to the caller's crate.
    CrateNarrowed,
}

/// Call-site counts bucketed by resolution outcome. Used per node
/// (outgoing sites) and per module (graph-confidence summary).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ResolutionCallCounts {
    pub(crate) resolved_call_count: usize,
    pub(crate) unresolved_call_count: usize,
    pub(crate) ambiguous_call_count: usize,
    pub(crate) anonymous_call_count: usize,
}

impl ResolutionCallCounts {
    pub(crate) fn record(&mut self, resolution: Resolution, call_count: usize) {
        match resolution {
            Resolution::Resolved => self.resolved_call_count += call_count,
            Resolution::Unresolved => self.unresolved_call_count += call_count,
            Resolution::Ambiguous => self.ambiguous_call_count += call_count,
            Resolution::Anonymous => self.anonymous_call_count += call_count,
        }
    }

    pub(crate) fn total(&self) -> usize {
        self.resolved_call_count
            + self.unresolved_call_count
            + self.ambiguous_call_count
            + self.anonymous_call_count
    }
}

/// Graph-confidence calibration for one module: how many of its call
/// sites resolved. Downstream reports cite this to bound their claims
/// ("this module's edges are 40% unresolved — treat results as lower
/// bounds").
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ModuleResolutionSummary {
    pub(crate) module: String,
    #[serde(flatten)]
    pub(crate) calls: ResolutionCallCounts,
    pub(crate) total_call_count: usize,
}

/// Stable node id: `file:Owner::name:start_line`.
pub(crate) fn node_id(file: &str, f: &FunctionShape) -> String {
    format!("{}:{}:{}", file, node_local_name(f), f.span.start_line)
}

/// File-local display name used in node ids and complexity matching:
/// `Owner::name` for methods, bare `name` otherwise.
pub(crate) fn node_local_name(f: &FunctionShape) -> String {
    f.owner
        .known_value()
        .and_then(|owner| owner.as_ref())
        .map_or_else(
            || f.display_name.clone(),
            |owner| format!("{}::{}", owner.display_name, f.display_name),
        )
}

pub(crate) fn name_last_segment(name: &str) -> &str {
    name.rsplit_once("::").map_or(name, |(_, last)| last)
}
