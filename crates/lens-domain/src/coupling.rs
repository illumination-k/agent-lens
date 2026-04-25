//! Language-agnostic module coupling metrics.
//!
//! `agent-lens` measures coupling at the granularity of a logical module
//! (e.g. a Rust `mod` path like `crate::analyze::coupling`). For each
//! directed edge `from → to` carrying a referenced `symbol` we derive five
//! metrics:
//!
//! * **Number of Couplings** — total directed edges, deduplicated by
//!   `(from, to, symbol, kind)`.
//! * **Fan-In** — for module `m`, the number of *distinct* source modules
//!   pointing at `m`.
//! * **Fan-Out** — for module `m`, the number of *distinct* target modules
//!   `m` points at.
//! * **Information Flow Complexity** — Henry-Kafura's metric, simplified to
//!   `(fan_in × fan_out)^2` (the classical formula multiplies by length;
//!   this crate omits the LOC factor on purpose so the score reflects
//!   coupling structure alone).
//! * **Inter-module coupling** — for each unordered pair `{a, b}`, the
//!   number of distinct symbols crossing the boundary in either direction.
//!
//! The types in this module are language-neutral: a per-language adapter
//! (e.g. `lens-rust`) is responsible for producing the [`CouplingEdge`]
//! list; this module only knows how to fold it into a report.

use std::collections::{BTreeMap, BTreeSet};

/// Dotted module path (e.g. `crate::analyze::coupling`).
///
/// Stored as a single `String` for cheap hashing and ordered comparison;
/// the `::` separator is part of the convention but never inspected by
/// metrics computation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModulePath(String);

impl ModulePath {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Append a child segment (`crate::a` + `b` → `crate::a::b`).
    pub fn child(&self, name: &str) -> Self {
        Self(format!("{}::{name}", self.0))
    }

    /// Strip the last segment, if any. `crate::a::b` → `crate::a`;
    /// `crate` → `None`.
    pub fn parent(&self) -> Option<Self> {
        self.0.rsplit_once("::").map(|(p, _)| Self(p.to_owned()))
    }
}

impl std::fmt::Display for ModulePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What kind of cross-module reference produced an edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EdgeKind {
    /// `use crate::other::Foo;`
    Use,
    /// `other::foo()` or `Self::foo()` resolved across modules.
    Call,
    /// `let x: other::Foo = ...;` / generic / return type.
    Type,
    /// `impl OtherTrait for MyType` where the trait or self type lives in
    /// a different module than the `impl` block.
    ImplFor,
}

impl EdgeKind {
    /// Snake-case name suitable for JSON serialisation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Use => "use",
            Self::Call => "call",
            Self::Type => "type",
            Self::ImplFor => "impl_for",
        }
    }
}

/// One directed coupling edge between two modules.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CouplingEdge {
    pub from: ModulePath,
    pub to: ModulePath,
    /// Tail segment of the resolved path, e.g. the type or function name.
    /// Glob imports use `"*"`.
    pub symbol: String,
    pub kind: EdgeKind,
}

/// Per-module summary derived from the edge set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleMetrics {
    pub path: ModulePath,
    pub fan_in: usize,
    pub fan_out: usize,
    /// Henry-Kafura IFC, simplified: `(fan_in × fan_out)^2`. Computed with
    /// saturating arithmetic so a pathological graph cannot panic.
    pub ifc: u64,
}

/// Coupling between an unordered pair of modules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairCoupling {
    /// Lexicographically-smaller endpoint.
    pub a: ModulePath,
    /// Lexicographically-larger endpoint.
    pub b: ModulePath,
    /// Number of distinct symbols crossing the boundary in either direction.
    pub shared_symbols: usize,
}

/// Full coupling report. Edges are deduplicated and sorted; modules and
/// pairs are sorted into a canonical order (see [`compute_report`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CouplingReport {
    pub modules: Vec<ModuleMetrics>,
    pub edges: Vec<CouplingEdge>,
    pub pairs: Vec<PairCoupling>,
    /// Total directed edges after dedup on `(from, to, symbol, kind)`.
    pub number_of_couplings: usize,
}

/// Build a [`CouplingReport`] from a module list and its edges.
///
/// `modules` is the universe of nodes — every module appears in the
/// `modules` slice of the report, even if it has no edges. `edges` may
/// contain duplicates and self-loops; both are filtered before metrics
/// are computed.
///
/// Module metrics preserve the input order of `modules`, so callers that
/// want a specific presentation order can sort the slice up front.
/// Edges are sorted by `(from, to, symbol, kind)`; pairs are sorted by
/// `shared_symbols` descending with a lexicographic tiebreak.
pub fn compute_report(modules: &[ModulePath], edges: Vec<CouplingEdge>) -> CouplingReport {
    let edges = dedup_edges(edges);

    let mut fan_in: BTreeMap<ModulePath, BTreeSet<ModulePath>> = BTreeMap::new();
    let mut fan_out: BTreeMap<ModulePath, BTreeSet<ModulePath>> = BTreeMap::new();
    let mut pair_syms: BTreeMap<(ModulePath, ModulePath), BTreeSet<String>> = BTreeMap::new();
    for e in &edges {
        fan_out
            .entry(e.from.clone())
            .or_default()
            .insert(e.to.clone());
        fan_in
            .entry(e.to.clone())
            .or_default()
            .insert(e.from.clone());
        pair_syms
            .entry(unordered(&e.from, &e.to))
            .or_default()
            .insert(e.symbol.clone());
    }

    let module_metrics: Vec<ModuleMetrics> = modules
        .iter()
        .map(|m| {
            let fan_in_n = fan_in.get(m).map_or(0, |s| s.len());
            let fan_out_n = fan_out.get(m).map_or(0, |s| s.len());
            let product = (fan_in_n as u64).saturating_mul(fan_out_n as u64);
            let ifc = product.saturating_mul(product);
            ModuleMetrics {
                path: m.clone(),
                fan_in: fan_in_n,
                fan_out: fan_out_n,
                ifc,
            }
        })
        .collect();

    let mut pairs: Vec<PairCoupling> = pair_syms
        .into_iter()
        .map(|((a, b), syms)| PairCoupling {
            a,
            b,
            shared_symbols: syms.len(),
        })
        .collect();
    pairs.sort_by(|p, q| {
        q.shared_symbols
            .cmp(&p.shared_symbols)
            .then_with(|| p.a.cmp(&q.a))
            .then_with(|| p.b.cmp(&q.b))
    });

    let number_of_couplings = edges.len();
    CouplingReport {
        modules: module_metrics,
        edges,
        pairs,
        number_of_couplings,
    }
}

fn dedup_edges(mut edges: Vec<CouplingEdge>) -> Vec<CouplingEdge> {
    edges.retain(|e| e.from != e.to);
    edges.sort();
    edges.dedup();
    edges
}

fn unordered(a: &ModulePath, b: &ModulePath) -> (ModulePath, ModulePath) {
    if a <= b {
        (a.clone(), b.clone())
    } else {
        (b.clone(), a.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(s: &str) -> ModulePath {
        ModulePath::new(s)
    }

    fn e(from: &str, to: &str, symbol: &str, kind: EdgeKind) -> CouplingEdge {
        CouplingEdge {
            from: m(from),
            to: m(to),
            symbol: symbol.to_owned(),
            kind,
        }
    }

    #[test]
    fn module_path_child_and_parent_round_trip() {
        let p = m("crate::a");
        assert_eq!(p.child("b").as_str(), "crate::a::b");
        assert_eq!(p.child("b").parent().unwrap().as_str(), "crate::a");
        assert!(m("crate").parent().is_none());
    }

    #[test]
    fn empty_graph_yields_zeroed_metrics_for_every_module() {
        let mods = vec![m("crate::a"), m("crate::b")];
        let r = compute_report(&mods, vec![]);
        assert_eq!(r.number_of_couplings, 0);
        assert_eq!(r.modules.len(), 2);
        assert!(
            r.modules
                .iter()
                .all(|x| x.fan_in == 0 && x.fan_out == 0 && x.ifc == 0)
        );
        assert!(r.pairs.is_empty());
        assert!(r.edges.is_empty());
    }

    #[test]
    fn single_edge_sets_directional_fan_counts() {
        let mods = vec![m("crate::a"), m("crate::b")];
        let edges = vec![e("crate::a", "crate::b", "Foo", EdgeKind::Use)];
        let r = compute_report(&mods, edges);
        assert_eq!(r.number_of_couplings, 1);
        let a = r.modules.iter().find(|x| x.path == m("crate::a")).unwrap();
        let b = r.modules.iter().find(|x| x.path == m("crate::b")).unwrap();
        assert_eq!((a.fan_in, a.fan_out), (0, 1));
        assert_eq!((b.fan_in, b.fan_out), (1, 0));
        // IFC is zero whenever either side is zero.
        assert_eq!(a.ifc, 0);
        assert_eq!(b.ifc, 0);
    }

    #[test]
    fn ifc_uses_product_squared() {
        // c depends on a and b; d depends on c; e depends on c.
        // → c.fan_in = 2 (d, e), c.fan_out = 2 (a, b), IFC = (2*2)^2 = 16.
        let mods = vec![m("a"), m("b"), m("c"), m("d"), m("e")];
        let edges = vec![
            e("c", "a", "x", EdgeKind::Use),
            e("c", "b", "y", EdgeKind::Use),
            e("d", "c", "z", EdgeKind::Use),
            e("e", "c", "w", EdgeKind::Use),
        ];
        let r = compute_report(&mods, edges);
        let c = r.modules.iter().find(|x| x.path == m("c")).unwrap();
        assert_eq!(c.fan_in, 2);
        assert_eq!(c.fan_out, 2);
        assert_eq!(c.ifc, 16);
    }

    #[test]
    fn duplicate_edges_are_deduped_on_full_key() {
        let mods = vec![m("a"), m("b")];
        let edges = vec![
            e("a", "b", "Foo", EdgeKind::Use),
            e("a", "b", "Foo", EdgeKind::Use), // exact dup → dropped
            e("a", "b", "Foo", EdgeKind::Type), // same symbol, different kind → kept
        ];
        let r = compute_report(&mods, edges);
        assert_eq!(r.number_of_couplings, 2);
    }

    #[test]
    fn self_loops_are_dropped() {
        let mods = vec![m("a")];
        let edges = vec![e("a", "a", "Foo", EdgeKind::Call)];
        let r = compute_report(&mods, edges);
        assert_eq!(r.number_of_couplings, 0);
        assert_eq!(r.modules[0].fan_in, 0);
        assert_eq!(r.modules[0].fan_out, 0);
    }

    #[test]
    fn pairs_are_unordered_and_count_distinct_symbols() {
        // a → b uses {Foo, Bar}; b → a uses {Foo, Baz}.
        // Distinct symbols crossing the boundary: {Foo, Bar, Baz} = 3.
        let mods = vec![m("a"), m("b")];
        let edges = vec![
            e("a", "b", "Foo", EdgeKind::Use),
            e("a", "b", "Bar", EdgeKind::Type),
            e("b", "a", "Foo", EdgeKind::Call),
            e("b", "a", "Baz", EdgeKind::Use),
        ];
        let r = compute_report(&mods, edges);
        assert_eq!(r.pairs.len(), 1);
        assert_eq!(r.pairs[0].a, m("a"));
        assert_eq!(r.pairs[0].b, m("b"));
        assert_eq!(r.pairs[0].shared_symbols, 3);
    }

    #[test]
    fn pairs_sort_by_shared_symbols_desc() {
        let mods = vec![m("a"), m("b"), m("c"), m("d")];
        let edges = vec![
            // a-b: 1 symbol
            e("a", "b", "x", EdgeKind::Use),
            // c-d: 2 symbols
            e("c", "d", "y", EdgeKind::Use),
            e("c", "d", "z", EdgeKind::Use),
        ];
        let r = compute_report(&mods, edges);
        assert_eq!(r.pairs[0].shared_symbols, 2);
        assert_eq!(r.pairs[0].a, m("c"));
        assert_eq!(r.pairs[1].shared_symbols, 1);
    }

    #[test]
    fn diamond_graph_metrics() {
        // a→b, a→c, b→d, c→d.
        let mods = vec![m("a"), m("b"), m("c"), m("d")];
        let edges = vec![
            e("a", "b", "x", EdgeKind::Use),
            e("a", "c", "y", EdgeKind::Use),
            e("b", "d", "z", EdgeKind::Use),
            e("c", "d", "w", EdgeKind::Use),
        ];
        let r = compute_report(&mods, edges);
        let a = r.modules.iter().find(|x| x.path == m("a")).unwrap();
        let b = r.modules.iter().find(|x| x.path == m("b")).unwrap();
        let d = r.modules.iter().find(|x| x.path == m("d")).unwrap();
        assert_eq!((a.fan_in, a.fan_out), (0, 2));
        assert_eq!((b.fan_in, b.fan_out), (1, 1));
        assert_eq!((d.fan_in, d.fan_out), (2, 0));
        assert_eq!(b.ifc, 1); // (1*1)^2
        assert_eq!(r.number_of_couplings, 4);
    }

    #[test]
    fn module_order_is_preserved() {
        let mods = vec![m("z"), m("a"), m("m")];
        let r = compute_report(&mods, vec![]);
        let paths: Vec<&str> = r.modules.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(paths, vec!["z", "a", "m"]);
    }

    #[test]
    fn edge_kind_string_names_are_stable() {
        assert_eq!(EdgeKind::Use.as_str(), "use");
        assert_eq!(EdgeKind::Call.as_str(), "call");
        assert_eq!(EdgeKind::Type.as_str(), "type");
        assert_eq!(EdgeKind::ImplFor.as_str(), "impl_for");
    }
}
