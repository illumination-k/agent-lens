//! Language-agnostic module coupling metrics.
//!
//! `agent-lens` measures coupling at the granularity of a logical module
//! (e.g. a Rust `mod` path like `crate::analyze::coupling`). For each
//! directed edge `from → to` carrying a referenced `symbol` we derive
//! several metrics:
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
//! * **Instability** — Robert C. Martin's `I = Ce / (Ca + Ce)` where
//!   `Ce = fan_out` (efferent) and `Ca = fan_in` (afferent). `1.0` is
//!   maximally unstable (only depends on others); `0.0` is maximally
//!   stable (only depended upon). Modules with no edges have `instability
//!   = None`.
//! * **Cycles** — the strongly-connected components of the dependency
//!   graph with at least two members. Reported as
//!   [`DependencyCycle`]s, each carrying its participating modules in
//!   ascending lexicographic order.
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
#[derive(Debug, Clone, PartialEq)]
pub struct ModuleMetrics {
    pub path: ModulePath,
    pub fan_in: usize,
    pub fan_out: usize,
    /// Henry-Kafura IFC, simplified: `(fan_in × fan_out)^2`. Computed with
    /// saturating arithmetic so a pathological graph cannot panic.
    pub ifc: u64,
    /// Robert C. Martin's instability `I = Ce / (Ca + Ce)`. `None` when
    /// the module participates in no edges (so the ratio is undefined).
    pub instability: Option<f64>,
}

impl Eq for ModuleMetrics {}

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

/// One non-trivial strongly-connected component of the module
/// dependency graph.
///
/// Components of size 1 are ignored (a single module is only a "cycle"
/// when it self-references, which `compute_report` already filters out).
/// Members are sorted lexicographically so cycle output is stable across
/// runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyCycle {
    pub members: Vec<ModulePath>,
}

/// Full coupling report. Edges are deduplicated and sorted; modules and
/// pairs are sorted into a canonical order (see [`compute_report`]).
#[derive(Debug, Clone, PartialEq)]
pub struct CouplingReport {
    pub modules: Vec<ModuleMetrics>,
    pub edges: Vec<CouplingEdge>,
    pub pairs: Vec<PairCoupling>,
    /// Strongly-connected components of size ≥ 2. Sorted by descending
    /// member count, lexicographic tiebreak.
    pub cycles: Vec<DependencyCycle>,
    /// Total directed edges after dedup on `(from, to, symbol, kind)`.
    pub number_of_couplings: usize,
}

impl Eq for CouplingReport {}

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
            let instability = instability_of(fan_in_n, fan_out_n);
            ModuleMetrics {
                path: m.clone(),
                fan_in: fan_in_n,
                fan_out: fan_out_n,
                ifc,
                instability,
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

    let cycles = detect_cycles(modules, &edges);

    let number_of_couplings = edges.len();
    CouplingReport {
        modules: module_metrics,
        edges,
        pairs,
        cycles,
        number_of_couplings,
    }
}

/// Robert C. Martin's instability ratio. Returns `None` when both
/// fan-in and fan-out are zero (the module participates in no edges).
fn instability_of(fan_in: usize, fan_out: usize) -> Option<f64> {
    let total = fan_in + fan_out;
    if total == 0 {
        return None;
    }
    Some(fan_out as f64 / total as f64)
}

/// Tarjan-style SCC discovery against the directed dependency graph.
///
/// Components of size 1 are dropped: a one-element SCC means "no
/// cycle", and self-loops are already removed by [`dedup_edges`].
/// Inside each surviving component members are sorted; the component
/// list itself is sorted by descending size with a lexicographic
/// tiebreak so output is deterministic.
fn detect_cycles(modules: &[ModulePath], edges: &[CouplingEdge]) -> Vec<DependencyCycle> {
    if modules.is_empty() {
        return Vec::new();
    }
    let index_of: BTreeMap<&ModulePath, usize> =
        modules.iter().enumerate().map(|(i, m)| (m, i)).collect();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); modules.len()];
    for e in edges {
        let (Some(&u), Some(&v)) = (index_of.get(&e.from), index_of.get(&e.to)) else {
            continue;
        };
        adj[u].push(v);
    }

    let mut state = TarjanState::new(modules.len());
    for v in 0..modules.len() {
        if state.indices[v].is_none() {
            state.strongconnect(v, &adj);
        }
    }

    let mut cycles: Vec<DependencyCycle> = state
        .components
        .into_iter()
        .filter(|c| c.len() >= 2)
        .map(|c| {
            let mut members: Vec<ModulePath> = c.into_iter().map(|i| modules[i].clone()).collect();
            members.sort();
            DependencyCycle { members }
        })
        .collect();
    cycles.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then_with(|| a.members.cmp(&b.members))
    });
    cycles
}

/// Mutable bookkeeping for Tarjan's SCC algorithm. Kept in its own
/// struct so the recursive `strongconnect` call doesn't have to thread
/// six arguments through every frame.
struct TarjanState {
    indices: Vec<Option<usize>>,
    lowlinks: Vec<usize>,
    on_stack: Vec<bool>,
    stack: Vec<usize>,
    next_index: usize,
    components: Vec<Vec<usize>>,
}

impl TarjanState {
    fn new(n: usize) -> Self {
        Self {
            indices: vec![None; n],
            lowlinks: vec![0; n],
            on_stack: vec![false; n],
            stack: Vec::new(),
            next_index: 0,
            components: Vec::new(),
        }
    }

    fn strongconnect(&mut self, v: usize, adj: &[Vec<usize>]) {
        self.indices[v] = Some(self.next_index);
        self.lowlinks[v] = self.next_index;
        self.next_index += 1;
        self.stack.push(v);
        self.on_stack[v] = true;

        for &w in &adj[v] {
            match self.indices[w] {
                None => {
                    self.strongconnect(w, adj);
                    self.lowlinks[v] = self.lowlinks[v].min(self.lowlinks[w]);
                }
                Some(w_idx) if self.on_stack[w] => {
                    self.lowlinks[v] = self.lowlinks[v].min(w_idx);
                }
                Some(_) => {}
            }
        }

        if Some(self.lowlinks[v]) == self.indices[v] {
            let mut component = Vec::new();
            while let Some(w) = self.stack.pop() {
                self.on_stack[w] = false;
                component.push(w);
                if w == v {
                    break;
                }
            }
            self.components.push(component);
        }
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

    #[test]
    fn instability_is_none_for_isolated_modules() {
        let mods = vec![m("a"), m("b")];
        let r = compute_report(&mods, vec![]);
        assert!(r.modules.iter().all(|x| x.instability.is_none()));
    }

    #[test]
    fn instability_one_for_pure_consumer_zero_for_pure_provider() {
        // a depends on b: a is fully unstable (only Ce), b fully stable (only Ca).
        let mods = vec![m("a"), m("b")];
        let edges = vec![e("a", "b", "Foo", EdgeKind::Use)];
        let r = compute_report(&mods, edges);
        let a = r.modules.iter().find(|x| x.path == m("a")).unwrap();
        let b = r.modules.iter().find(|x| x.path == m("b")).unwrap();
        assert_eq!(a.instability, Some(1.0));
        assert_eq!(b.instability, Some(0.0));
    }

    #[test]
    fn instability_is_balanced_when_fan_in_and_fan_out_match() {
        // c sits between {a, b} and {d, e}: Ca = 2, Ce = 2, I = 0.5.
        let mods = vec![m("a"), m("b"), m("c"), m("d"), m("e")];
        let edges = vec![
            e("c", "a", "x", EdgeKind::Use),
            e("c", "b", "y", EdgeKind::Use),
            e("d", "c", "z", EdgeKind::Use),
            e("e", "c", "w", EdgeKind::Use),
        ];
        let r = compute_report(&mods, edges);
        let c = r.modules.iter().find(|x| x.path == m("c")).unwrap();
        assert_eq!(c.instability, Some(0.5));
    }

    #[test]
    fn dag_has_no_cycles() {
        // a → b → c is a chain.
        let mods = vec![m("a"), m("b"), m("c")];
        let edges = vec![
            e("a", "b", "x", EdgeKind::Use),
            e("b", "c", "y", EdgeKind::Use),
        ];
        let r = compute_report(&mods, edges);
        assert!(r.cycles.is_empty());
    }

    #[test]
    fn two_node_back_edge_is_a_cycle() {
        let mods = vec![m("a"), m("b")];
        let edges = vec![
            e("a", "b", "x", EdgeKind::Use),
            e("b", "a", "y", EdgeKind::Use),
        ];
        let r = compute_report(&mods, edges);
        assert_eq!(r.cycles.len(), 1);
        assert_eq!(r.cycles[0].members, vec![m("a"), m("b")]);
    }

    #[test]
    fn three_node_cycle_includes_all_members_sorted() {
        let mods = vec![m("a"), m("b"), m("c")];
        let edges = vec![
            e("a", "b", "x", EdgeKind::Use),
            e("b", "c", "y", EdgeKind::Use),
            e("c", "a", "z", EdgeKind::Use),
        ];
        let r = compute_report(&mods, edges);
        assert_eq!(r.cycles.len(), 1);
        assert_eq!(r.cycles[0].members, vec![m("a"), m("b"), m("c")]);
    }

    #[test]
    fn separate_cycles_are_listed_independently() {
        // a↔b and c↔d↔e: two disjoint SCCs.
        let mods = vec![m("a"), m("b"), m("c"), m("d"), m("e")];
        let edges = vec![
            e("a", "b", "x", EdgeKind::Use),
            e("b", "a", "y", EdgeKind::Use),
            e("c", "d", "p", EdgeKind::Use),
            e("d", "e", "q", EdgeKind::Use),
            e("e", "c", "r", EdgeKind::Use),
        ];
        let r = compute_report(&mods, edges);
        assert_eq!(r.cycles.len(), 2);
        // Larger component first (c-d-e then a-b).
        assert_eq!(r.cycles[0].members.len(), 3);
        assert_eq!(r.cycles[1].members.len(), 2);
        assert_eq!(r.cycles[1].members, vec![m("a"), m("b")]);
    }

    #[test]
    fn self_loop_alone_is_not_a_cycle() {
        // The dedup pass strips self-loops, so they never produce a
        // size-1 SCC with a back edge.
        let mods = vec![m("a")];
        let edges = vec![e("a", "a", "Foo", EdgeKind::Call)];
        let r = compute_report(&mods, edges);
        assert!(r.cycles.is_empty());
    }
}
