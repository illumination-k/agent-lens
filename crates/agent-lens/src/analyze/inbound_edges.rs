//! The per-callee edge walk shared by `analyze single-use` and `analyze
//! parameters`: every edge that could carry a call into a node, with its
//! endpoints already mapped to node indices.

use std::collections::HashMap;

use super::call_graph::CallGraph;
use super::call_graph::model::{CallGraphEdge, Resolution, ResolutionMethod};

/// One edge as seen from the callee it may reach.
pub(super) enum InboundEdge<'g> {
    /// An ambiguous edge naming node `to` as one of its candidates.
    AmbiguousCandidate { to: usize, edge: &'g CallGraphEdge },
    /// A resolved edge whose both endpoints are graph nodes.
    Resolved {
        from: usize,
        to: usize,
        edge: &'g CallGraphEdge,
    },
}

/// Call `visit` once per ambiguous candidate and once per resolved edge
/// of `graph`. Unresolved and anonymous edges, and endpoints naming no
/// node, are skipped.
pub(super) fn visit_inbound_edges<'g>(
    graph: &'g CallGraph,
    mut visit: impl FnMut(InboundEdge<'g>),
) {
    let index_by_id = graph.node_index_by_id();
    for edge in &graph.edges {
        match edge.resolution {
            Resolution::Ambiguous => edge
                .candidates
                .iter()
                .filter_map(|candidate| index_by_id.get(candidate.as_str()))
                .for_each(|&to| visit(InboundEdge::AmbiguousCandidate { to, edge })),
            Resolution::Resolved => {
                if let Some((from, to)) = endpoints(edge, &index_by_id) {
                    visit(InboundEdge::Resolved { from, to, edge });
                }
            }
            Resolution::Unresolved | Resolution::Anonymous => {}
        }
    }
}

/// `edge`'s caller and callee as node indices, when both name a node.
fn endpoints(edge: &CallGraphEdge, index_by_id: &HashMap<&str, usize>) -> Option<(usize, usize)> {
    let from = index_by_id.get(edge.from.as_deref()?)?;
    let to = index_by_id.get(edge.to.as_deref()?)?;
    Some((*from, *to))
}

/// Whether `edge` was resolved through the last-segment fallback
/// family, making the caller attribution itself heuristic.
pub(super) fn is_fallback_resolved(edge: &CallGraphEdge) -> bool {
    matches!(
        edge.resolution_method,
        Some(
            ResolutionMethod::LastSegment
                | ResolutionMethod::PathSuffix
                | ResolutionMethod::CrateNarrowed
        )
    )
}
