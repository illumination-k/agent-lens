//! Transitive dependency closure ("context span") over the module
//! graph.
//!
//! The metric answers: "to fully understand module `M`, how many other
//! modules must I follow?" It is the count of nodes reachable from `M`
//! along outgoing edges in the dependency graph, excluding `M` itself.
//!
//! `agent-lens` exposes this as a "files I'd need to read" estimate.
//! Since each module typically maps to one source file, the number of
//! transitively reachable modules is a reasonable upper bound on the
//! number of files an agent must load into context to reason about
//! `M`. The CLI layer dedupes by source file so inline modules sharing
//! a parent file collapse to one.
//!
//! Like [`crate::coupling::compute_report`], this module is
//! language-neutral: a per-language adapter (e.g. `lens-rust`) is
//! responsible for producing the [`crate::coupling::CouplingEdge`]
//! list.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::coupling::{CouplingEdge, ModulePath};

/// Per-module transitive closure result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleContextSpan {
    pub path: ModulePath,
    /// Distinct modules `path` directly depends on (= `fan_out`).
    pub direct: usize,
    /// Distinct modules reachable from `path` via one or more outgoing
    /// edges, excluding `path` itself.
    pub transitive: usize,
    /// The reachable modules, sorted lexicographically.
    pub reachable: Vec<ModulePath>,
}

/// Full context-span report. One [`ModuleContextSpan`] per module in
/// the input slice; the input order is preserved so callers that want
/// a different presentation can sort the slice up front.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSpanReport {
    pub modules: Vec<ModuleContextSpan>,
}

/// Compute per-module context spans.
///
/// `modules` is the universe of nodes — every module appears in the
/// report, even one with no edges (`direct = 0`, `transitive = 0`).
/// `edges` may contain self-loops or duplicates; both are ignored
/// before the BFS so cycle handling stays correct without the caller
/// having to pre-clean the edge list.
///
/// The closure follows outgoing edges only: starting from `M`, every
/// `M → X` extends the search through `X`'s out-edges. Cycles are
/// handled — the start module itself never appears in its own
/// transitive set, even when the graph loops back to it. Edges whose
/// endpoints are not in `modules` are dropped so a partial graph
/// never poisons the result.
pub fn compute_context_spans(modules: &[ModulePath], edges: &[CouplingEdge]) -> ContextSpanReport {
    if modules.is_empty() {
        return ContextSpanReport {
            modules: Vec::new(),
        };
    }
    let index_of: BTreeMap<&ModulePath, usize> =
        modules.iter().enumerate().map(|(i, m)| (m, i)).collect();
    let mut adj: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); modules.len()];
    for e in edges {
        let (Some(&u), Some(&v)) = (index_of.get(&e.from), index_of.get(&e.to)) else {
            continue;
        };
        if u != v {
            adj[u].insert(v);
        }
    }

    let spans = (0..modules.len())
        .map(|i| {
            let direct = adj[i].len();
            let visited = reachable_from(i, &adj);
            let mut reachable: Vec<ModulePath> =
                visited.iter().map(|&j| modules[j].clone()).collect();
            reachable.sort();
            ModuleContextSpan {
                path: modules[i].clone(),
                direct,
                transitive: visited.len(),
                reachable,
            }
        })
        .collect();
    ContextSpanReport { modules: spans }
}

/// BFS from `start` along `adj`, returning the set of reached nodes
/// with `start` itself excluded even when a cycle leads back to it.
fn reachable_from(start: usize, adj: &[BTreeSet<usize>]) -> BTreeSet<usize> {
    let mut visited: BTreeSet<usize> = BTreeSet::new();
    let mut queue: VecDeque<usize> = VecDeque::new();
    for &nb in &adj[start] {
        if nb != start && visited.insert(nb) {
            queue.push_back(nb);
        }
    }
    while let Some(u) = queue.pop_front() {
        for &v in &adj[u] {
            if v != start && visited.insert(v) {
                queue.push_back(v);
            }
        }
    }
    visited
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coupling::EdgeKind;

    fn m(s: &str) -> ModulePath {
        ModulePath::new(s)
    }

    fn e(from: &str, to: &str) -> CouplingEdge {
        CouplingEdge {
            from: m(from),
            to: m(to),
            symbol: "x".to_owned(),
            kind: EdgeKind::Use,
        }
    }

    fn span<'a>(report: &'a ContextSpanReport, path: &str) -> &'a ModuleContextSpan {
        report
            .modules
            .iter()
            .find(|s| s.path == m(path))
            .expect("module present")
    }

    #[test]
    fn empty_input_yields_empty_report() {
        let r = compute_context_spans(&[], &[]);
        assert!(r.modules.is_empty());
    }

    #[test]
    fn module_with_no_edges_has_zero_span() {
        let mods = vec![m("a"), m("b")];
        let r = compute_context_spans(&mods, &[]);
        assert_eq!(r.modules.len(), 2);
        for s in &r.modules {
            assert_eq!(s.direct, 0);
            assert_eq!(s.transitive, 0);
            assert!(s.reachable.is_empty());
        }
    }

    #[test]
    fn single_edge_records_one_direct_and_transitive_dep() {
        let mods = vec![m("a"), m("b")];
        let r = compute_context_spans(&mods, &[e("a", "b")]);
        let a = span(&r, "a");
        assert_eq!(a.direct, 1);
        assert_eq!(a.transitive, 1);
        assert_eq!(a.reachable, vec![m("b")]);
        let b = span(&r, "b");
        assert_eq!(b.direct, 0);
        assert_eq!(b.transitive, 0);
        assert!(b.reachable.is_empty());
    }

    #[test]
    fn chain_accumulates_transitive_deps() {
        // a → b → c. a's closure is {b, c}; b's closure is {c}.
        let mods = vec![m("a"), m("b"), m("c")];
        let edges = vec![e("a", "b"), e("b", "c")];
        let r = compute_context_spans(&mods, &edges);
        let a = span(&r, "a");
        assert_eq!(a.direct, 1);
        assert_eq!(a.transitive, 2);
        assert_eq!(a.reachable, vec![m("b"), m("c")]);
        let b = span(&r, "b");
        assert_eq!(b.direct, 1);
        assert_eq!(b.transitive, 1);
        assert_eq!(b.reachable, vec![m("c")]);
    }

    #[test]
    fn diamond_does_not_double_count_shared_descendants() {
        // a → b, a → c, b → d, c → d. a's closure is {b, c, d} = 3.
        let mods = vec![m("a"), m("b"), m("c"), m("d")];
        let edges = vec![e("a", "b"), e("a", "c"), e("b", "d"), e("c", "d")];
        let r = compute_context_spans(&mods, &edges);
        let a = span(&r, "a");
        assert_eq!(a.direct, 2);
        assert_eq!(a.transitive, 3);
        assert_eq!(a.reachable, vec![m("b"), m("c"), m("d")]);
    }

    #[test]
    fn cycle_does_not_count_start_in_its_own_span() {
        // a → b, b → a. a reaches {b}; b reaches {a}.
        let mods = vec![m("a"), m("b")];
        let edges = vec![e("a", "b"), e("b", "a")];
        let r = compute_context_spans(&mods, &edges);
        let a = span(&r, "a");
        assert_eq!(a.transitive, 1);
        assert_eq!(a.reachable, vec![m("b")]);
        let b = span(&r, "b");
        assert_eq!(b.transitive, 1);
        assert_eq!(b.reachable, vec![m("a")]);
    }

    #[test]
    fn three_node_cycle_yields_full_other_set_for_each_member() {
        // a → b → c → a. From any node, the other two are reachable.
        let mods = vec![m("a"), m("b"), m("c")];
        let edges = vec![e("a", "b"), e("b", "c"), e("c", "a")];
        let r = compute_context_spans(&mods, &edges);
        for name in ["a", "b", "c"] {
            let s = span(&r, name);
            assert_eq!(s.transitive, 2, "{name}");
            assert_eq!(s.reachable.len(), 2);
            assert!(!s.reachable.contains(&m(name)));
        }
    }

    #[test]
    fn duplicate_edges_are_collapsed_for_direct_count() {
        let mods = vec![m("a"), m("b")];
        // Two parallel edges with different symbols/kinds — direct
        // count is over distinct targets, so it stays at 1.
        let edges = vec![
            e("a", "b"),
            CouplingEdge {
                from: m("a"),
                to: m("b"),
                symbol: "y".to_owned(),
                kind: EdgeKind::Type,
            },
        ];
        let r = compute_context_spans(&mods, &edges);
        let a = span(&r, "a");
        assert_eq!(a.direct, 1);
        assert_eq!(a.transitive, 1);
    }

    #[test]
    fn self_loops_are_ignored() {
        let mods = vec![m("a")];
        let edges = vec![e("a", "a")];
        let r = compute_context_spans(&mods, &edges);
        let a = span(&r, "a");
        assert_eq!(a.direct, 0);
        assert_eq!(a.transitive, 0);
    }

    #[test]
    fn edges_referencing_unknown_modules_are_ignored() {
        let mods = vec![m("a")];
        // b is not in the module set; the edge is silently dropped.
        let r = compute_context_spans(&mods, &[e("a", "b")]);
        let a = span(&r, "a");
        assert_eq!(a.direct, 0);
        assert_eq!(a.transitive, 0);
    }

    #[test]
    fn module_input_order_is_preserved() {
        let mods = vec![m("z"), m("a"), m("m")];
        let r = compute_context_spans(&mods, &[]);
        let paths: Vec<&str> = r.modules.iter().map(|s| s.path.as_str()).collect();
        assert_eq!(paths, vec!["z", "a", "m"]);
    }
}
