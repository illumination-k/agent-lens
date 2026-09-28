//! Raw-reference targets shared by `analyze single-use` and `analyze
//! parameters`: a function whose bare name is searched for in the
//! scanned sources, and the spans whose mentions of it the graph already
//! accounts for. Every other occurrence is a possible hidden caller.

use super::call_graph::model::CallGraphNode;
use super::span_references::{AllowedSpan, SpanReferenceIndex};
use super::unreachable::identifiers;

/// One function whose bare name is searched for, and the spans whose
/// mentions of it are accounted for.
pub(super) struct RawTarget {
    pub(super) name: String,
    /// `(file, start_line, end_line)` spans of the definition and its
    /// known callers.
    pub(super) allowed: Vec<AllowedSpan>,
}

impl RawTarget {
    /// A target for `node` whose mentions inside its own definition and
    /// inside each of `callers` are accounted for.
    pub(super) fn new<'a>(
        node: &CallGraphNode,
        callers: impl IntoIterator<Item = &'a CallGraphNode>,
    ) -> Self {
        let span_of = |n: &CallGraphNode| (n.file.clone(), n.start_line, n.end_line);
        let mut allowed = vec![span_of(node)];
        allowed.extend(callers.into_iter().map(span_of));
        Self {
            name: node.name.clone(),
            allowed,
        }
    }

    /// Index `targets` for a token-by-token scan; each target's slot is
    /// its position in the slice.
    pub(super) fn index(targets: &[Self]) -> SpanReferenceIndex<'_> {
        SpanReferenceIndex::new(
            targets
                .iter()
                .enumerate()
                .map(|(slot, t)| (slot, t.name.as_str(), t.allowed.as_slice())),
        )
    }
}

/// Every identifier token of `source` with its 1-based line number.
pub(super) fn identifiers_by_line(source: &str) -> impl Iterator<Item = (usize, &str)> {
    source
        .lines()
        .enumerate()
        .flat_map(|(offset, line)| identifiers(line).map(move |token| (offset + 1, token)))
}
