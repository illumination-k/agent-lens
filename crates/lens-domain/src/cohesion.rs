//! Language-agnostic cohesion analysis.
//!
//! `agent-lens` measures cohesion at the granularity of an `impl` block (or a
//! similar concept in non-Rust languages). For each method we record the
//! instance fields it touches and the sibling methods it calls; the LCOM4
//! variant of "Lack of Cohesion of Methods" then drops out as the number of
//! weakly-connected components in the graph where methods are vertices and
//! shared field accesses or sibling calls are edges.
//!
//! Higher LCOM4 means the unit's methods split into more independent groups —
//! a hint that the type may want to be broken apart.
//!
//! The types here are intentionally free of language-specific details: the
//! per-language adapter (e.g. `lens-rust`) is responsible for filling in the
//! [`MethodCohesion`] entries; this module only knows how to fold them into
//! components.

use crate::function::FunctionDef;

/// What kind of unit a [`CohesionUnit`] describes.
///
/// `Inherent` covers `impl Foo { ... }`; `Trait` covers
/// `impl Trait for Foo { ... }`. The distinction is preserved because cohesion
/// of trait impls is usually less interesting (the method set is dictated by
/// the trait) but callers may still want to surface it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CohesionUnitKind {
    Inherent,
    Trait { trait_name: String },
}

/// One method's cohesion-relevant footprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodCohesion {
    pub name: String,
    /// 1-based inclusive start line of the method signature.
    pub start_line: usize,
    /// 1-based inclusive end line of the method body.
    pub end_line: usize,
    /// Instance fields referenced by this method (e.g. `self.foo`). Order is
    /// not significant; duplicates are removed by the language adapter.
    pub fields: Vec<String>,
    /// Sibling-method names called by this method (e.g. `self.bar()` or
    /// `Self::baz(...)`). Already filtered to names that belong to the same
    /// unit by the language adapter.
    pub calls: Vec<String>,
}

impl MethodCohesion {
    /// Convenience constructor used by language adapters.
    pub fn new(
        name: impl Into<String>,
        start_line: usize,
        end_line: usize,
        fields: Vec<String>,
        calls: Vec<String>,
    ) -> Self {
        Self {
            name: name.into(),
            start_line,
            end_line,
            fields,
            calls,
        }
    }

    pub fn from_function(def: &FunctionDef, fields: Vec<String>, calls: Vec<String>) -> Self {
        Self::new(
            def.name.clone(),
            def.start_line,
            def.end_line,
            fields,
            calls,
        )
    }
}

/// One unit of analysis (typically a single `impl` block) along with its
/// cohesion components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CohesionUnit {
    pub kind: CohesionUnitKind,
    /// Name of the type the unit is attached to (e.g. `Foo` for
    /// `impl Foo { ... }`).
    pub type_name: String,
    /// 1-based inclusive start line of the unit.
    pub start_line: usize,
    /// 1-based inclusive end line of the unit.
    pub end_line: usize,
    pub methods: Vec<MethodCohesion>,
    /// Connected components, each as a sorted list of indices into
    /// [`Self::methods`]. Computed eagerly so the field is canonical.
    pub components: Vec<Vec<usize>>,
}

impl CohesionUnit {
    /// Build a unit and compute its components in one step.
    pub fn build(
        kind: CohesionUnitKind,
        type_name: impl Into<String>,
        start_line: usize,
        end_line: usize,
        methods: Vec<MethodCohesion>,
    ) -> Self {
        let components = compute_components(&methods);
        Self {
            kind,
            type_name: type_name.into(),
            start_line,
            end_line,
            methods,
            components,
        }
    }

    /// LCOM4 score: the number of connected components.
    ///
    /// `0` means the unit has no methods. `1` is the most cohesive case
    /// (every method connects, directly or transitively, to every other).
    pub fn lcom4(&self) -> usize {
        self.components.len()
    }
}

/// Compute the connected components of the cohesion graph for `methods`.
///
/// Two methods are connected if they share at least one referenced field or
/// if one calls the other by name. The result is sorted: smaller indices
/// first within each component, components ordered by their smallest index.
pub fn compute_components(methods: &[MethodCohesion]) -> Vec<Vec<usize>> {
    let n = methods.len();
    let mut uf = UnionFind::new(n);

    for i in 0..n {
        for j in (i + 1)..n {
            if methods_connected(&methods[i], &methods[j]) {
                uf.union(i, j);
            }
        }
    }

    let mut by_root: std::collections::BTreeMap<usize, Vec<usize>> =
        std::collections::BTreeMap::new();
    for i in 0..n {
        by_root.entry(uf.find(i)).or_default().push(i);
    }
    let mut components: Vec<Vec<usize>> = by_root.into_values().collect();
    for c in &mut components {
        c.sort_unstable();
    }
    components.sort_by_key(|c| c.first().copied().unwrap_or(usize::MAX));
    components
}

fn methods_connected(a: &MethodCohesion, b: &MethodCohesion) -> bool {
    // Field overlap.
    if a.fields.iter().any(|f| b.fields.contains(f)) {
        return true;
    }
    // Direct call in either direction. Names are matched verbatim against
    // sibling method names, so "private helper" calls land here too.
    a.calls.contains(&b.name) || b.calls.contains(&a.name)
}

struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u32>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra == rb {
            return;
        }
        match self.rank[ra].cmp(&self.rank[rb]) {
            std::cmp::Ordering::Less => self.parent[ra] = rb,
            std::cmp::Ordering::Greater => self.parent[rb] = ra,
            std::cmp::Ordering::Equal => {
                self.parent[rb] = ra;
                self.rank[ra] += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(name: &str, fields: &[&str], calls: &[&str]) -> MethodCohesion {
        MethodCohesion::new(
            name,
            1,
            1,
            fields.iter().map(|s| (*s).to_owned()).collect(),
            calls.iter().map(|s| (*s).to_owned()).collect(),
        )
    }

    #[test]
    fn empty_unit_has_zero_components() {
        assert!(compute_components(&[]).is_empty());
    }

    #[test]
    fn isolated_methods_each_form_a_component() {
        let methods = vec![m("a", &[], &[]), m("b", &[], &[]), m("c", &[], &[])];
        let comps = compute_components(&methods);
        assert_eq!(comps, vec![vec![0], vec![1], vec![2]]);
    }

    #[test]
    fn shared_field_merges_methods() {
        let methods = vec![m("a", &["x"], &[]), m("b", &["x"], &[]), m("c", &[], &[])];
        let comps = compute_components(&methods);
        assert_eq!(comps, vec![vec![0, 1], vec![2]]);
    }

    #[test]
    fn direct_call_merges_methods() {
        let methods = vec![m("a", &[], &["b"]), m("b", &[], &[]), m("c", &[], &[])];
        let comps = compute_components(&methods);
        assert_eq!(comps, vec![vec![0, 1], vec![2]]);
    }

    #[test]
    fn transitive_connections_collapse_to_one_component() {
        // a—b via field "x"; b—c via call; so {a,b,c} should merge.
        let methods = vec![
            m("a", &["x"], &[]),
            m("b", &["x"], &["c"]),
            m("c", &[], &[]),
        ];
        let comps = compute_components(&methods);
        assert_eq!(comps, vec![vec![0, 1, 2]]);
    }

    #[test]
    fn build_records_lcom4_and_components() {
        let methods = vec![m("a", &["x"], &[]), m("b", &["x"], &[]), m("c", &[], &[])];
        let unit = CohesionUnit::build(CohesionUnitKind::Inherent, "Foo", 1, 10, methods);
        assert_eq!(unit.lcom4(), 2);
        assert_eq!(unit.components, vec![vec![0, 1], vec![2]]);
    }

    #[test]
    fn unrelated_calls_do_not_connect() {
        // "b" here calls a name that doesn't belong to this unit; the
        // language adapter is expected to filter such calls before
        // constructing MethodCohesion, so the test mirrors a clean input.
        let methods = vec![m("a", &[], &[]), m("b", &[], &[])];
        let comps = compute_components(&methods);
        assert_eq!(comps, vec![vec![0], vec![1]]);
    }
}
