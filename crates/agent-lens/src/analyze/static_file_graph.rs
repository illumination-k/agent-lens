//! The file-level projection of the static dependency graphs.
//!
//! `analyze hidden-coupling` compares what history says against what the
//! code declares, and the second half of that comparison has to be in
//! git's units: git attributes commits to **files**, so a co-change edge
//! is between two files and the static side has to answer in the same
//! currency.
//!
//! Nothing here extracts anything. Both inputs already exist and are
//! only re-keyed:
//!
//! * the [`CallGraph`](super::call_graph::CallGraph)'s **resolved** call
//!   edges, rolled up from `caller → callee` to `file → file`, and
//! * the [`ModuleGraph`](super::module_graph::ModuleGraph)'s `use` /
//!   `import` edges, rolled up from module to the file each module was
//!   read from.
//!
//! The two are complementary and both are needed: a Rust file that only
//! imports a type from another file has no call edge to it, and a file
//! whose functions are reached across a crate boundary has no module
//! edge (the module graph is single-crate by construction). Neither is
//! complete on its own, and the union is still a **lower bound** — an
//! unresolved call site is an edge nobody can see, which is why a
//! "no static path" verdict is reported as the upper bound it is.
//!
//! Every path is keyed repo-root-relative, the space
//! [`ChurnScope`](super::churn::ChurnScope) puts git's paths in, so a
//! static verdict joins against a co-change row by key lookup.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use serde::Serialize;
use tracing::debug;

use super::call_graph::CallGraph;
use super::call_graph::model::Resolution;
use super::cargo_meta::CrateNameCache;
use super::churn::ChurnScope;
use super::module_graph::{GraphPolicy, ModuleGraph, build_graph};

/// Which of the two projections put an edge in the graph.
///
/// Reported per edge because the two carry different evidence: a call
/// edge says one file's code runs another's, an import edge says one
/// file names another's declarations. A "declared but never
/// co-changing" verdict reads differently for each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct EdgeSource {
    pub(crate) call: bool,
    pub(crate) module: bool,
}

impl EdgeSource {
    fn merge(self, other: Self) -> Self {
        Self {
            call: self.call || other.call,
            module: self.module || other.module,
        }
    }

    /// How the edge is named in a report.
    pub(crate) fn label(self) -> &'static str {
        match (self.call, self.module) {
            (true, true) => "call+import",
            (true, false) => "call",
            // An edge exists because one of the two projections produced
            // it, so the all-false case is unreachable; naming it
            // `import` keeps the function total without inventing a
            // third label nothing can produce.
            _ => "import",
        }
    }
}

/// How the static graph relates two files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StaticRelation {
    /// Neither file reaches the other over any chain of file-level
    /// edges. An upper bound: unresolved call sites hide edges.
    NoPath,
    /// A file-level edge runs directly between the two.
    Direct,
    /// One reaches the other, but only through intermediate files.
    Transitive,
}

/// Which way the shortest chain between two files runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Direction {
    AToB,
    BToA,
    /// Each reaches the other at the reported distance.
    Both,
}

/// The static side of one file pair's evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct StaticVerdict {
    pub(crate) relation: StaticRelation,
    /// Hops on the shortest chain — `1` for a direct edge. Absent when
    /// there is no chain at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) distance: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) direction: Option<Direction>,
}

/// One declared file-level dependency, in the pair spelling co-change
/// uses (`a` lexicographically first) so a static edge and a history row
/// name the same thing the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DirectEdge<'a> {
    pub(crate) a: &'a str,
    pub(crate) b: &'a str,
    pub(crate) direction: Direction,
    pub(crate) source: EdgeSource,
}

/// The assembled file-level static graph.
#[derive(Debug, Default)]
pub(crate) struct StaticFileGraph {
    /// Files the static view covers, repo-root-relative and sorted. A
    /// file is here because a language backend read it — which is the
    /// difference between "no declared dependency" and "no static view
    /// of this file at all".
    files: Vec<String>,
    index: HashMap<String, usize>,
    /// Directed adjacency by file index, sorted and deduplicated.
    out: Vec<Vec<usize>>,
    /// Provenance per directed edge.
    sources: BTreeMap<(usize, usize), EdgeSource>,
}

impl StaticFileGraph {
    pub(crate) fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Directed file-level edges. Two files depending on each other
    /// count twice here and once in [`Self::direct_edges`].
    pub(crate) fn edge_count(&self) -> usize {
        self.sources.len()
    }

    /// Whether the static view covers `file` at all. A `.md`, `.toml`,
    /// or fixture path is absent by construction — no backend reads it —
    /// and that is a different answer from "covered, and nothing depends
    /// on it".
    pub(crate) fn contains(&self, file: &str) -> bool {
        self.index.contains_key(file)
    }

    /// What the static graph says about a pair, or `None` when either
    /// file is outside the view.
    pub(crate) fn verdict(&self, a: &str, b: &str) -> Option<StaticVerdict> {
        let from = *self.index.get(a)?;
        let to = *self.index.get(b)?;
        let forward = self.distance(from, to);
        let backward = self.distance(to, from);
        let (distance, direction) = match (forward, backward) {
            (None, None) => {
                return Some(StaticVerdict {
                    relation: StaticRelation::NoPath,
                    distance: None,
                    direction: None,
                });
            }
            (Some(d), None) => (d, Direction::AToB),
            (None, Some(d)) => (d, Direction::BToA),
            (Some(f), Some(b)) => match f.cmp(&b) {
                std::cmp::Ordering::Less => (f, Direction::AToB),
                std::cmp::Ordering::Greater => (b, Direction::BToA),
                std::cmp::Ordering::Equal => (f, Direction::Both),
            },
        };
        Some(StaticVerdict {
            relation: if distance == 1 {
                StaticRelation::Direct
            } else {
                StaticRelation::Transitive
            },
            distance: Some(distance),
            direction: Some(direction),
        })
    }

    /// Every declared dependency as an unordered pair, so the suspect
    /// bucket reports `a ↔ b` once whether the code depends one way or
    /// both.
    pub(crate) fn direct_edges(&self) -> Vec<DirectEdge<'_>> {
        let mut folded: BTreeMap<(usize, usize), (Direction, EdgeSource)> = BTreeMap::new();
        for (&(from, to), &source) in &self.sources {
            let (key, direction) = unordered(from, to);
            folded
                .entry(key)
                .and_modify(|entry| {
                    if entry.0 != direction {
                        entry.0 = Direction::Both;
                    }
                    entry.1 = entry.1.merge(source);
                })
                .or_insert((direction, source));
        }
        folded
            .into_iter()
            .map(|((a, b), (direction, source))| DirectEdge {
                a: self.files[a].as_str(),
                b: self.files[b].as_str(),
                direction,
                source,
            })
            .collect()
    }

    /// Hops on the shortest directed chain from `from` to `to`, or
    /// `None` when none exists. Breadth-first, so the first sighting is
    /// the shortest.
    fn distance(&self, from: usize, to: usize) -> Option<usize> {
        if from == to {
            return Some(0);
        }
        let mut seen = vec![false; self.files.len()];
        seen[from] = true;
        let mut queue: VecDeque<(usize, usize)> = VecDeque::from([(from, 0)]);
        while let Some((node, depth)) = queue.pop_front() {
            for &next in &self.out[node] {
                if next == to {
                    return Some(depth + 1);
                }
                if !seen[next] {
                    seen[next] = true;
                    queue.push_back((next, depth + 1));
                }
            }
        }
        None
    }
}

/// A directed edge's unordered key, plus which way it runs.
///
/// Split out and excluded from cargo-mutants (`.cargo/mutants.toml`):
/// self-edges are dropped at insertion, so `from == to` cannot reach
/// here and `<` and `<=` are observationally identical. Keeping it a
/// named function of its own means the rest of
/// [`StaticFileGraph::direct_edges`] — the fold that merges provenance
/// and promotes a reciprocal pair to [`Direction::Both`] — stays under
/// mutation testing.
fn unordered(from: usize, to: usize) -> ((usize, usize), Direction) {
    if from < to {
        ((from, to), Direction::AToB)
    } else {
        ((to, from), Direction::BToA)
    }
}

/// Accumulates the two projections before they are indexed.
///
/// Kept separate from [`StaticFileGraph`] so the graph itself is
/// immutable once built: every verdict a report carries is then drawn
/// from the same graph, and no traversal can observe a half-folded one.
#[derive(Debug, Default)]
pub(crate) struct StaticFileGraphBuilder {
    files: BTreeSet<String>,
    edges: BTreeMap<(String, String), EdgeSource>,
}

impl StaticFileGraphBuilder {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record that a language backend read `file`, whether or not it
    /// produced any edge. A parsed file with no dependencies is covered
    /// by the static view; an unparsed one is not, and only this
    /// separates the two.
    pub(crate) fn observe_file(&mut self, file: String) {
        self.files.insert(file);
    }

    fn add_edge(&mut self, from: String, to: String, source: EdgeSource) {
        // A file does not depend on itself, and an intra-file call or a
        // `mod` block importing its sibling in the same file would
        // otherwise become a self-loop no traversal can use.
        if from == to {
            self.observe_file(from);
            return;
        }
        self.files.insert(from.clone());
        self.files.insert(to.clone());
        self.edges
            .entry((from, to))
            .and_modify(|existing| *existing = existing.merge(source))
            .or_insert(source);
    }

    /// Fold in the call graph's resolved caller → callee edges, keyed by
    /// the file each endpoint was declared in.
    ///
    /// Only resolved edges contribute: an unresolved or ambiguous call
    /// site names no target file, and guessing one would invent a
    /// declared dependency. That is the same restriction every other
    /// call-graph analyzer applies, and it is why the result is a lower
    /// bound.
    pub(crate) fn add_call_graph(&mut self, graph: &CallGraph, scope: &ChurnScope) {
        let mut key_of: HashMap<&str, String> = HashMap::new();
        for node in &graph.nodes {
            let key = scope.key_for_display(&node.file);
            key_of.insert(node.file.as_str(), key.clone());
            self.observe_file(key);
        }
        let index_by_id = graph.node_index_by_id();
        for edge in &graph.edges {
            if edge.resolution != Resolution::Resolved {
                continue;
            }
            let (Some(&from), Some(&to)) = (
                edge.from.as_deref().and_then(|id| index_by_id.get(id)),
                edge.to.as_deref().and_then(|id| index_by_id.get(id)),
            ) else {
                continue;
            };
            let (Some(from), Some(to)) = (
                key_of.get(graph.nodes[from].file.as_str()),
                key_of.get(graph.nodes[to].file.as_str()),
            ) else {
                continue;
            };
            self.add_edge(
                from.clone(),
                to.clone(),
                EdgeSource {
                    call: true,
                    module: false,
                },
            );
        }
    }

    /// Fold in a module graph's `use` / `import` edges, keyed by the
    /// file each module was read from.
    pub(crate) fn add_module_graph(&mut self, graph: &ModuleGraph, scope: &ChurnScope) {
        let mut file_of: HashMap<&str, String> = HashMap::new();
        for module in &graph.modules {
            let key = scope.key_for_absolute(&module.file);
            self.observe_file(key.clone());
            file_of.insert(module.path.as_str(), key);
        }
        for edge in &graph.edges {
            let (Some(from), Some(to)) = (
                file_of.get(edge.from.as_str()),
                file_of.get(edge.to.as_str()),
            ) else {
                continue;
            };
            self.add_edge(
                from.clone(),
                to.clone(),
                EdgeSource {
                    call: false,
                    module: true,
                },
            );
        }
    }

    pub(crate) fn finish(self) -> StaticFileGraph {
        let files: Vec<String> = self.files.into_iter().collect();
        let index: HashMap<String, usize> = files
            .iter()
            .enumerate()
            .map(|(idx, file)| (file.clone(), idx))
            .collect();
        let mut out: Vec<Vec<usize>> = vec![Vec::new(); files.len()];
        let mut sources = BTreeMap::new();
        for ((from, to), source) in self.edges {
            // Both endpoints were inserted into `files` by `add_edge`.
            let (Some(&from), Some(&to)) = (index.get(&from), index.get(&to)) else {
                continue;
            };
            out[from].push(to);
            sources.insert((from, to), source);
        }
        for neighbors in &mut out {
            neighbors.sort_unstable();
            neighbors.dedup();
        }
        StaticFileGraph {
            files,
            index,
            out,
            sources,
        }
    }
}

/// How many module graphs were grown, and how many roots refused to
/// grow one. A root that resolves to no language backend is not an
/// error — a repository root usually is one — but a report that leans
/// on the module half has to be able to say how much of it there was.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ModuleGraphCoverage {
    pub(crate) roots: usize,
    pub(crate) unsupported_roots: usize,
}

/// Grow a module graph from every root under `targets` and fold each one
/// into `builder`.
///
/// [`build_graph`] resolves one language backend from one entry point,
/// so a workspace has no single root — this repository's own root
/// resolves to no crate at all. The roots are therefore the analysis
/// targets themselves, which is the whole answer for a single-crate run,
/// a TS/JS entry file, or a Go module, plus the nearest Rust manifest
/// and Go module directory of every scanned file, so a workspace
/// contributes one module graph per member instead of none.
pub(crate) fn add_module_graphs(
    builder: &mut StaticFileGraphBuilder,
    scope: &ChurnScope,
    files: &[PathBuf],
) -> ModuleGraphCoverage {
    let mut coverage = ModuleGraphCoverage::default();
    for root in module_graph_roots(scope.targets(), files) {
        match build_graph(&root, GraphPolicy::COUPLING) {
            Ok(graph) => {
                coverage.roots += 1;
                builder.add_module_graph(&graph, scope);
            }
            Err(source) => {
                coverage.unsupported_roots += 1;
                debug!(root = %root.display(), %source, "no module graph for this root");
            }
        }
    }
    coverage
}

fn module_graph_roots(targets: &[PathBuf], files: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots: BTreeSet<PathBuf> = targets.iter().cloned().collect();
    let mut crates = CrateNameCache::new();
    for file in files {
        match file.extension().and_then(|e| e.to_str()) {
            Some("rs") => roots.extend(rust_crate_roots(crates.lookup(file).crate_root)),
            Some("go") => roots.extend(nearest_ancestor_holding(file, "go.mod")),
            _ => {}
        }
    }
    roots.into_iter().collect()
}

/// A Rust manifest directory as module-graph roots.
///
/// `resolve_crate_root` probes `src/lib.rs` before `src/main.rs`, so a
/// crate that is both a library and a binary would contribute only its
/// library's module tree — and the binary's own modules (a `cli` tree,
/// say) would have no declared dependency between any two of them. The
/// binary root is added alongside, so both trees are covered.
fn rust_crate_roots(manifest_dir: Option<PathBuf>) -> Vec<PathBuf> {
    let Some(dir) = manifest_dir else {
        return Vec::new();
    };
    let main = dir.join("src/main.rs");
    let mut roots = vec![dir];
    if main.is_file() {
        roots.push(main);
    }
    roots
}

/// The closest ancestor directory of `file` that holds `marker`.
fn nearest_ancestor_holding(file: &Path, marker: &str) -> Option<PathBuf> {
    file.ancestors()
        .skip(1)
        .find(|dir| dir.join(marker).is_file())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::AnalyzeRoots;
    use crate::analyze::call_graph::CallGraphBuilder;
    use crate::test_support::{run_git, write_file};
    use rstest::rstest;

    /// Build a graph from `edges` alone, with every named file observed.
    fn graph(edges: &[(&str, &str)]) -> StaticFileGraph {
        let mut builder = StaticFileGraphBuilder::new();
        for (from, to) in edges {
            builder.add_edge(
                (*from).to_owned(),
                (*to).to_owned(),
                EdgeSource {
                    call: true,
                    module: false,
                },
            );
        }
        builder.finish()
    }

    #[test]
    fn a_direct_edge_is_direct_in_either_spelling() {
        let g = graph(&[("a", "b")]);
        let forward = g.verdict("a", "b").unwrap();
        assert_eq!(forward.relation, StaticRelation::Direct);
        assert_eq!(forward.distance, Some(1));
        assert_eq!(forward.direction, Some(Direction::AToB));

        let reverse = g.verdict("b", "a").unwrap();
        assert_eq!(reverse.relation, StaticRelation::Direct);
        assert_eq!(reverse.direction, Some(Direction::BToA));
    }

    #[test]
    fn mutual_edges_report_both_directions() {
        let g = graph(&[("a", "b"), ("b", "a")]);
        assert_eq!(
            g.verdict("a", "b").unwrap().direction,
            Some(Direction::Both)
        );
        assert_eq!(g.direct_edges().len(), 1, "one unordered pair");
        assert_eq!(g.direct_edges()[0].direction, Direction::Both);
        assert_eq!(g.edge_count(), 2, "two directed edges");
    }

    #[test]
    fn an_indirect_chain_is_transitive_and_carries_its_length() {
        let g = graph(&[("a", "m"), ("m", "n"), ("n", "b")]);
        let verdict = g.verdict("a", "b").unwrap();
        assert_eq!(verdict.relation, StaticRelation::Transitive);
        assert_eq!(verdict.distance, Some(3));
        assert_eq!(verdict.direction, Some(Direction::AToB));
    }

    /// The shortest chain wins, and its direction is the one that found
    /// it — a long way there and a short way back is a `b_to_a` verdict.
    #[test]
    fn the_shorter_direction_decides_the_distance() {
        let g = graph(&[("a", "m"), ("m", "n"), ("n", "b"), ("b", "a")]);
        let verdict = g.verdict("a", "b").unwrap();
        assert_eq!(verdict.distance, Some(1));
        assert_eq!(verdict.direction, Some(Direction::BToA));
        assert_eq!(verdict.relation, StaticRelation::Direct);
    }

    #[test]
    fn two_files_in_the_view_with_no_chain_between_them_have_no_path() {
        let mut builder = StaticFileGraphBuilder::new();
        builder.observe_file("a".to_owned());
        builder.observe_file("b".to_owned());
        let g = builder.finish();
        let verdict = g.verdict("a", "b").unwrap();
        assert_eq!(verdict.relation, StaticRelation::NoPath);
        assert_eq!(verdict.distance, None);
        assert_eq!(verdict.direction, None);
    }

    /// A file no backend read is not "undeclared dependency" — it is
    /// "no static view", and the graph has to say so by refusing a
    /// verdict rather than returning `NoPath`.
    #[test]
    fn a_file_outside_the_view_gets_no_verdict_at_all() {
        let g = graph(&[("a", "b")]);
        assert!(g.contains("a"));
        assert!(!g.contains("README.md"));
        assert!(g.verdict("a", "README.md").is_none());
        assert!(g.verdict("README.md", "a").is_none());
    }

    /// A self-edge would make every traversal from a file reach itself
    /// at distance 1, so it is dropped — but the file is still covered.
    #[test]
    fn a_self_edge_is_dropped_but_the_file_stays_in_the_view() {
        let g = graph(&[("a", "a")]);
        assert!(g.contains("a"));
        assert_eq!(g.edge_count(), 0);
        assert!(g.direct_edges().is_empty());
    }

    /// The two projections are folded, not layered: a pair the call
    /// graph and the module graph both connect is one edge whose
    /// provenance names both.
    #[test]
    fn an_edge_seen_by_both_projections_reports_both() {
        let mut builder = StaticFileGraphBuilder::new();
        let call = EdgeSource {
            call: true,
            module: false,
        };
        let import = EdgeSource {
            call: false,
            module: true,
        };
        builder.add_edge("a".to_owned(), "b".to_owned(), call);
        builder.add_edge("a".to_owned(), "b".to_owned(), import);
        builder.add_edge("a".to_owned(), "c".to_owned(), import);
        let g = builder.finish();
        let edges = g.direct_edges();
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].source.label(), "call+import");
        assert_eq!(edges[1].source.label(), "import");
    }

    #[rstest]
    #[case::call(EdgeSource { call: true, module: false }, "call")]
    #[case::import(EdgeSource { call: false, module: true }, "import")]
    #[case::both(EdgeSource { call: true, module: true }, "call+import")]
    fn edge_source_labels_name_the_evidence(#[case] source: EdgeSource, #[case] expected: &str) {
        assert_eq!(source.label(), expected);
    }

    /// A tiny two-crate workspace: `app` calls into `lib`, and inside
    /// `app` one module only *imports* another. The call projection
    /// carries the cross-crate edge, the module projection carries the
    /// import-only one, and neither would have found the other's.
    #[test]
    fn both_projections_contribute_edges_a_single_one_would_miss() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        write_file(
            dir.path(),
            "lib/Cargo.toml",
            "[package]\nname = \"lib\"\nversion = \"0.1.0\"\n",
        );
        write_file(dir.path(), "lib/src/lib.rs", "pub fn work() -> u8 { 1 }\n");
        write_file(
            dir.path(),
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.1.0\"\n",
        );
        write_file(
            dir.path(),
            "app/src/lib.rs",
            "pub mod caller;\npub mod names;\npub mod uses;\n",
        );
        write_file(
            dir.path(),
            "app/src/caller.rs",
            "pub fn run() -> u8 { lib::work() }\n",
        );
        // `uses` imports a constant from `names` and calls nothing:
        // invisible to the call graph, an edge in the module graph.
        write_file(
            dir.path(),
            "app/src/names.rs",
            "pub const NAME: &str = \"n\";\npub fn unused() -> u8 { 0 }\n",
        );
        write_file(
            dir.path(),
            "app/src/uses.rs",
            "use crate::names::NAME;\npub fn label() -> &'static str { NAME }\n",
        );

        let roots = AnalyzeRoots::from(dir.path());
        let scope = ChurnScope::resolve(&roots).unwrap();
        let call_graph = CallGraphBuilder::new().build(&roots).unwrap();
        let mut builder = StaticFileGraphBuilder::new();
        builder.add_call_graph(&call_graph, &scope);
        let call_only = {
            let mut probe = StaticFileGraphBuilder::new();
            probe.add_call_graph(&call_graph, &scope);
            probe.finish()
        };
        let files: Vec<PathBuf> = ["lib/src/lib.rs", "app/src/lib.rs", "app/src/uses.rs"]
            .iter()
            .map(|rel| std::fs::canonicalize(dir.path().join(rel)).unwrap())
            .collect();
        let coverage = add_module_graphs(&mut builder, &scope, &files);
        let both = builder.finish();

        assert!(coverage.roots >= 2, "got {coverage:?}");
        assert_eq!(
            call_only.verdict("app/src/caller.rs", "lib/src/lib.rs"),
            both.verdict("app/src/caller.rs", "lib/src/lib.rs"),
            "the cross-crate call edge comes from the call graph alone",
        );
        assert_eq!(
            call_only
                .verdict("app/src/uses.rs", "app/src/names.rs")
                .map(|v| v.relation),
            Some(StaticRelation::NoPath),
            "an import with no call is invisible to the call graph",
        );
        assert_eq!(
            both.verdict("app/src/uses.rs", "app/src/names.rs")
                .map(|v| v.relation),
            Some(StaticRelation::Direct),
            "the module graph is what declares it",
        );
    }

    /// A root nothing claims is a fact about the repository, not a
    /// failure: this project's own root resolves to no crate, and the
    /// report has to be able to say how many roots answered.
    #[test]
    fn an_unsupported_root_is_counted_rather_than_raised() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        write_file(dir.path(), "notes.md", "nothing to analyze\n");
        let roots = AnalyzeRoots::from(dir.path());
        let scope = ChurnScope::resolve(&roots).unwrap();
        let mut builder = StaticFileGraphBuilder::new();
        let coverage = add_module_graphs(&mut builder, &scope, &[]);
        assert_eq!(
            coverage,
            ModuleGraphCoverage {
                roots: 0,
                unsupported_roots: 1,
            },
        );
        assert_eq!(builder.finish().file_count(), 0);
    }

    #[test]
    fn module_graph_roots_include_the_targets_and_each_manifest_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "crates/one/Cargo.toml",
            "[package]\nname = \"one\"\nversion = \"0.1.0\"\n",
        );
        let file = write_file(dir.path(), "crates/one/src/lib.rs", "pub fn f() {}\n");
        let roots = module_graph_roots(&[dir.path().to_path_buf()], &[file]);
        assert_eq!(
            roots,
            [dir.path().to_path_buf(), dir.path().join("crates/one")],
        );
    }

    /// A crate that is both a library and a binary contributes two
    /// roots: `resolve_crate_root` would otherwise stop at `src/lib.rs`
    /// and leave every module the binary declares outside the view.
    #[test]
    fn a_crate_with_a_binary_contributes_its_binary_root_too() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"both\"\nversion = \"0.1.0\"\n",
        );
        let lib = write_file(dir.path(), "src/lib.rs", "pub fn f() {}\n");
        write_file(dir.path(), "src/main.rs", "mod cli;\nfn main() {}\n");
        assert_eq!(
            module_graph_roots(&[], &[lib]),
            [dir.path().to_path_buf(), dir.path().join("src/main.rs")],
        );
    }

    #[test]
    fn a_library_only_crate_contributes_one_root() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"lib\"\nversion = \"0.1.0\"\n",
        );
        let lib = write_file(dir.path(), "src/lib.rs", "pub fn f() {}\n");
        assert_eq!(module_graph_roots(&[], &[lib]), [dir.path().to_path_buf()]);
    }

    #[test]
    fn a_go_file_contributes_its_module_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "svc/go.mod", "module example.com/svc\n");
        let file = write_file(dir.path(), "svc/pkg/a.go", "package pkg\n");
        assert_eq!(
            module_graph_roots(&[], &[file]),
            [dir.path().join("svc")],
            "the nearest go.mod directory is the module root",
        );
    }

    #[test]
    fn a_file_with_no_marker_above_it_contributes_no_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "loose.go", "package main\n");
        assert!(module_graph_roots(&[], &[file]).is_empty());
    }
}
