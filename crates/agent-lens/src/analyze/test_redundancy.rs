//! `analyze test-redundancy` — which test functions are near-copies of
//! each other, and which one of each set is worth keeping.
//!
//! The shape is Pan et al.'s similarity-based test-suite minimization
//! (LTM): score every test against every other, then pick a subset that
//! keeps the suite's diversity. Two deliberate departures:
//!
//! - **No embeddings, no search.** LTM embeds each test method with a
//!   code language model and runs a genetic algorithm over the resulting
//!   dense similarity matrix. This analyzer scores bodies with the same
//!   structural methods `analyze similarity` uses, so candidate
//!   generation hands it a *sparse* graph — only the pairs that could
//!   clear the threshold were ever enumerated. On a sparse graph a
//!   greedy dominating-set pass gets the same answer as a population
//!   search for a fraction of the cost, and it is deterministic, which a
//!   seeded GA is not. Output that lands in an agent's context and in
//!   `agent-lens baseline` snapshots must not move between runs on
//!   unchanged sources.
//!
//! - **Fold, don't delete.** LTM minimizes to a time budget and measures
//!   what fault-detection that costs. This report has no budget: it
//!   names redundancy and stops. Tests whose bodies agree on everything
//!   the parser records are a merge; tests that share a shape but name
//!   different things are a parameterization (`rstest` `#[case]`, a
//!   table test), which removes the duplication without removing a
//!   single assertion.
//!
//! The guard is the part LTM structurally cannot have. LTM is black-box
//! by design — no coverage, no call graph — so its only evidence that a
//! test is redundant is that another test looks like it. Here the call
//! graph is already built for `analyze untested`, so a fold candidate is
//! checked against it: if the rest of the suite has no resolved call
//! path to something this test reaches, the test is the only static
//! exerciser of that code and is held back from the fold.
//!
//! What the guard does *not* establish is that a folded test is safe to
//! delete. Reaching the same functions is not making the same
//! assertions, and only resolved edges are traversable. A clean guard
//! means "nothing here disproves the fold", never "this fold is safe".
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use serde::Serialize;

use super::call_graph::algo::bfs;
use std::sync::Arc;

use super::call_graph::{CallGraph, CallGraphBuilder};
use super::runner::render_report;
use super::similarity::{ScoredCorpus, ScoredUnit, SimilarityAnalyzer, SimilarityMethod};
use super::{AnalyzeRoots, AnalyzerError, FunctionSelection, OutputFormat};

const SCHEMA_VERSION: u32 = 1;

/// Default similarity cut. Held at `analyze similarity`'s own default so
/// a group reported here is a cluster that report would also show — the
/// two views disagree about what to *do* with a set of near-copies, not
/// about which sets exist.
pub const DEFAULT_THRESHOLD: f64 = 0.85;

/// Default minimum body length. Test bodies run shorter than production
/// ones and a three-line test is usually three lines of the same
/// `assert_eq!` skeleton every test in the file uses, so the floor is
/// `analyze similarity`'s rather than something lower.
pub const DEFAULT_MIN_LINES: usize = 5;

/// Default floor on how much body the analyzer can see before a test is
/// eligible at all.
///
/// `--min-lines` cannot do this job. It measures source lines, and a
/// `#[rstest]` test carries most of its lines in a signature: the body
/// of `fn line_maps_offsets_to_one_based_lines(#[case] source: &str,
/// #[case] offset: u32, #[case] expected: usize)` is one `assert_eq!`,
/// which the Rust adapter keeps as a single opaque leaf because it
/// expands no macros. Every such test therefore has the *same* body as
/// far as any score can tell, and at a line floor of 5 a report would
/// group a line-index test with a calendar test and call it duplication.
/// The floor that means something here is on the body tree.
///
/// Eight nodes is the smallest body with structure of its own: a block,
/// a statement, and a call with a couple of arguments. Below it a
/// "near-copy" is a claim about a body the analyzer never saw.
pub const DEFAULT_MIN_BODY_NODES: usize = 8;

/// Groups rendered in markdown when `--top` is not given.
const DEFAULT_TOP: usize = 20;

/// Value-similarity at or above which two bodies agree on everything the
/// parser recorded, not just their shape. Above it there is nothing
/// varying left to lift into a parameter list, so the candidate edit is
/// a merge rather than a parameterization.
///
/// Read it as an upper bound on agreement, never a proof of it: an
/// adapter only compares what it put in the tree, and the Rust one keeps
/// literal text out entirely, so two `#[case]` rows differing only in
/// their numbers reach this cut.
const IDENTICAL_VALUE_SIMILARITY: f64 = 0.95;

/// Unique-reach examples named per held-back member before the row
/// falls back to the count alone.
const MAX_UNIQUE_REACH_EXAMPLES: usize = 3;

/// What every verdict on this report is relative to, stated in the
/// output because "redundant test" reads as "deletable test" unless it
/// says otherwise.
const NOTE: &str = "Structural, not behavioural: two tests scoring alike share a body shape, \
     not a set of assertions. A group names one test to keep and the ones it makes redundant; it \
     does not say the redundant ones are safe to delete. Where the bodies name different things \
     the candidate edit is a parameterization (rstest #[case], a table test), which removes the \
     duplication without removing an assertion; the verdict reads only what the parser put in \
     the tree, and the Rust adapter keeps literal text out of it. The call graph subtracts what it can \
     disprove, in two directions: a pair whose two tests reach no production function in common \
     is not a pair, and a test that is the sole static caller of something is never offered as \
     foldable. It traverses resolved edges only and cannot see through an unexpanded macro, so a \
     clean guard is not a coverage proof, and a suite whose tests call only through macros gets \
     no guard at all.";

/// `analyze test-redundancy` flags, and the
/// `[profile.<name>.test-redundancy]` table.
///
/// Hand-written rather than built by `analyzer_options!` for the same
/// reason [`super::similarity::SimilarityOptions`] is: the score cuts
/// carry non-trivial clap defaults, and a derived `Default` would be
/// free to disagree with them.
#[derive(Debug, Clone, clap::Args, serde::Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct TestRedundancyOptions {
    /// Cap the markdown group list to the top-N groups. JSON output
    /// always carries the full list.
    #[arg(long)]
    pub top: Option<usize>,
    /// Similarity at or above which two test bodies are near-copies.
    #[arg(long, default_value_t = DEFAULT_THRESHOLD)]
    pub threshold: f64,
    /// Body-scoring algorithm, as `analyze similarity --method`. `pdg`
    /// is worth reaching for on a suite whose tests were copied and
    /// then reordered: it scores dependence structure, so a moved
    /// arrange step does not read as a different test.
    #[arg(long, value_enum, default_value_t = SimilarityMethod::Tsed)]
    pub method: SimilarityMethod,
    /// Minimum source line count for a test to be considered.
    #[arg(long)]
    pub min_lines: Option<usize>,
    /// Minimum body-tree nodes a test needs before it is eligible.
    /// Guards against bodies the parser keeps opaque — a Rust test whose
    /// whole body is one `assert_eq!` is a two-node tree, identical to
    /// every other such test however different the assertion.
    #[arg(long)]
    pub min_body_nodes: Option<usize>,
    /// Offer a test as foldable even when the call graph shows it is
    /// the only one with a resolved path to some production function.
    /// Skips building the graph, which is the bulk of the run.
    #[arg(long)]
    pub no_reach_guard: bool,
}

impl Default for TestRedundancyOptions {
    fn default() -> Self {
        Self {
            top: None,
            threshold: DEFAULT_THRESHOLD,
            method: SimilarityMethod::default(),
            min_lines: None,
            min_body_nodes: None,
            no_reach_guard: false,
        }
    }
}

/// Analyzer entry point for `analyze test-redundancy`.
///
/// Holds the path-filter flags as plain values rather than a
/// [`super::runner::FilterConfig`]: there is no per-file walk here to
/// configure. The corpus comes from a [`SimilarityAnalyzer`] and the
/// guard from a [`CallGraphBuilder`], and each wants the flags in its
/// own shape.
#[derive(Debug, Clone)]
pub struct TestRedundancyAnalyzer {
    only_tests: bool,
    exclude_tests: bool,
    exclude: Vec<String>,
    threshold: f64,
    method: SimilarityMethod,
    min_lines: Option<usize>,
    min_body_nodes: Option<usize>,
    top: Option<usize>,
    reach_guard: bool,
}

impl Default for TestRedundancyAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl TestRedundancyAnalyzer {
    pub fn new() -> Self {
        Self {
            only_tests: false,
            exclude_tests: false,
            exclude: Vec::new(),
            threshold: DEFAULT_THRESHOLD,
            method: SimilarityMethod::default(),
            min_lines: None,
            min_body_nodes: None,
            top: None,
            reach_guard: true,
        }
    }

    /// Apply a whole [`TestRedundancyOptions`] group. The CLI flags and
    /// the `[profile.<name>.test-redundancy]` table are the same type,
    /// so this is the only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: TestRedundancyOptions) -> Self {
        self.with_threshold(opts.threshold)
            .with_method(opts.method)
            .with_min_lines_opt(opts.min_lines)
            .with_min_body_nodes_opt(opts.min_body_nodes)
            .with_reach_guard(!opts.no_reach_guard)
            .with_top(opts.top)
    }

    /// Accepted for CLI uniformity and already implied: the corpus is
    /// test functions either way. It still narrows the *walk* to
    /// test-like paths, which drops `#[test]` functions living beside
    /// the code they exercise.
    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.only_tests = only_tests;
        self
    }

    /// Accepted for CLI uniformity, but it excludes the files this
    /// analyzer exists to read. The report then says how few test
    /// functions were in scope rather than presenting an empty suite as
    /// a suite without redundancy.
    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.exclude_tests = exclude_tests;
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.exclude = exclude;
        self
    }

    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.threshold = threshold;
        self
    }

    pub fn with_method(mut self, method: SimilarityMethod) -> Self {
        self.method = method;
        self
    }

    pub fn with_min_lines_opt(mut self, min_lines: Option<usize>) -> Self {
        self.min_lines = min_lines;
        self
    }

    /// Skip tests whose body tree is smaller than this. `None` keeps
    /// [`DEFAULT_MIN_BODY_NODES`].
    pub fn with_min_body_nodes_opt(mut self, min_body_nodes: Option<usize>) -> Self {
        self.min_body_nodes = min_body_nodes;
        self
    }

    fn resolved_min_body_nodes(&self) -> usize {
        self.min_body_nodes.unwrap_or(DEFAULT_MIN_BODY_NODES)
    }

    /// Cap the markdown group list to the top-N groups.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    /// Whether to check fold candidates against the call graph. On by
    /// default; `--no-reach-guard` turns it off and skips the build.
    pub fn with_reach_guard(mut self, reach_guard: bool) -> Self {
        self.reach_guard = reach_guard;
        self
    }

    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, AnalyzerError> {
        let roots = roots.into();
        let scored = self.similarity().scored_corpus(&roots)?;
        let RedundancyGraph {
            mut adjacency,
            opaque_body_test_count,
        } = adjacency(&scored, self.resolved_min_body_nodes());
        let graph = self.build_graph(&roots, &adjacency)?;
        let guard = graph
            .as_ref()
            .map(|graph| ReachGuard::build(graph, &scored.units, &adjacency));
        let unrelated_pair_count = guard
            .as_ref()
            .map_or(0, |guard| prune_unrelated(&mut adjacency, guard));
        let groups = select_groups(&scored, &adjacency);
        let report = Report::build(ReportInputs {
            roots: &roots,
            analyzer: self,
            scored: &scored,
            adjacency: &adjacency,
            groups,
            guard: guard.as_ref(),
            unrelated_pair_count,
            opaque_body_test_count,
        });
        render_report(&report, format, || format_markdown(&report, self.top))
    }

    /// The similarity run behind the corpus: the same analyzer, pinned
    /// to test functions and to this run's score cuts.
    fn similarity(&self) -> SimilarityAnalyzer {
        SimilarityAnalyzer::new()
            .with_threshold(self.threshold)
            .with_method(self.method)
            .with_min_lines_opt(Some(self.min_lines.unwrap_or(DEFAULT_MIN_LINES)))
            .with_function_selection(FunctionSelection::OnlyTests)
            .with_only_tests(self.only_tests)
            .with_exclude_tests(self.exclude_tests)
            .with_exclude_patterns(self.exclude.clone())
    }

    /// Build the call graph the guard reads, unless it is switched off
    /// or no test has a near-copy for it to rule on.
    fn build_graph(
        &self,
        roots: &AnalyzeRoots,
        adjacency: &[Vec<Neighbor>],
    ) -> Result<Option<Arc<CallGraph>>, AnalyzerError> {
        if !self.reach_guard || adjacency.iter().all(Vec::is_empty) {
            return Ok(None);
        }
        // The guard asks what the *rest of the suite* reaches, so it
        // needs the whole graph: dropping test nodes would leave it no
        // roots, and dropping production nodes nothing to reach. Only
        // the glob excludes carry over, because those name files the run
        // was told to ignore.
        let graph = CallGraphBuilder::new()
            .with_exclude_patterns(self.exclude.clone())
            .build(roots)?;
        Ok(Some(graph))
    }
}

/// One edge of the redundancy graph, as seen from one of its endpoints.
#[derive(Debug, Clone, Copy)]
struct Neighbor {
    unit: usize,
    similarity: f64,
    value_similarity: f64,
}

/// A representative plus the near-copies it covers, before the guard
/// has ruled on them.
#[derive(Debug)]
struct Group {
    representative: usize,
    members: Vec<Neighbor>,
}

/// Greedy dominating-set selection over the sparse redundancy graph.
///
/// Repeatedly take the test that makes the most *still-uncovered* tests
/// redundant, mark it and them covered, and record the pair. That is the
/// textbook greedy for set cover, which is within `1 - 1/e` of optimal
/// and — unlike the population search LTM runs over its dense matrix —
/// returns the same answer every time it sees the same sources.
///
/// A group is therefore a star, not a clique: every member is a
/// near-copy of the representative, and two members need not be
/// near-copies of each other. That is the shape the question has — one
/// test to keep, the rest folded into it — and it is what separates this
/// from `analyze similarity`, whose complete-link clusters answer "which
/// of these are all alike".
///
/// Tests with no near-copy are never selected and never reported: a test
/// that duplicates nothing is not this report's business.
fn select_groups(scored: &ScoredCorpus, adjacency: &[Vec<Neighbor>]) -> Vec<Group> {
    // Location order decides every tie, so two runs over the same tree
    // pick the same representatives whatever order the walk produced.
    let mut order: Vec<usize> = (0..scored.units.len())
        .filter(|&i| !adjacency[i].is_empty())
        .collect();
    order.sort_by(|&a, &b| unit_key(&scored.units[a]).cmp(&unit_key(&scored.units[b])));
    let rank: HashMap<usize, usize> = order.iter().enumerate().map(|(r, &i)| (i, r)).collect();

    let mut covered = vec![false; scored.units.len()];
    let mut groups = Vec::new();
    loop {
        let mut best: Option<(usize, usize)> = None;
        for &i in &order {
            if covered[i] {
                continue;
            }
            let gain = adjacency[i].iter().filter(|n| !covered[n.unit]).count();
            if gain > 0 && best.is_none_or(|(_, top)| gain > top) {
                best = Some((i, gain));
            }
        }
        let Some((representative, _)) = best else {
            break;
        };
        covered[representative] = true;
        let mut members: Vec<Neighbor> = adjacency[representative]
            .iter()
            .filter(|n| !covered[n.unit])
            .copied()
            .collect();
        members.sort_by_key(|n| rank.get(&n.unit).copied().unwrap_or(usize::MAX));
        for member in &members {
            covered[member.unit] = true;
        }
        groups.push(Group {
            representative,
            members,
        });
    }
    groups.sort_by(|a, b| {
        b.members.len().cmp(&a.members.len()).then_with(|| {
            rank.get(&a.representative)
                .cmp(&rank.get(&b.representative))
        })
    });
    groups
}

/// Undirected adjacency over the scored pairs. Both endpoints carry the
/// same two scores, so a representative can read its members' scores
/// without going back to the pair list.
fn adjacency(scored: &ScoredCorpus, min_body_nodes: usize) -> RedundancyGraph {
    let visible = |i: usize| {
        scored
            .units
            .get(i)
            .is_some_and(|unit| unit.body_node_count >= min_body_nodes)
    };
    let mut opaque: BTreeSet<usize> = BTreeSet::new();
    let mut adjacency: Vec<Vec<Neighbor>> = vec![Vec::new(); scored.units.len()];
    for pair in &scored.pairs {
        if pair.i >= adjacency.len() || pair.j >= adjacency.len() {
            continue;
        }
        if !visible(pair.i) || !visible(pair.j) {
            opaque.extend([pair.i, pair.j].into_iter().filter(|&i| !visible(i)));
            continue;
        }
        adjacency[pair.i].push(Neighbor {
            unit: pair.j,
            similarity: pair.similarity,
            value_similarity: pair.value_similarity,
        });
        adjacency[pair.j].push(Neighbor {
            unit: pair.i,
            similarity: pair.similarity,
            value_similarity: pair.value_similarity,
        });
    }
    RedundancyGraph {
        adjacency,
        opaque_body_test_count: opaque.len(),
    }
}

/// The redundancy graph plus what building it discarded.
struct RedundancyGraph {
    adjacency: Vec<Vec<Neighbor>>,
    opaque_body_test_count: usize,
}

/// Drop the edges between two tests the call graph shows exercise
/// nothing in common, and report how many went.
///
/// This is the half of the guard that has no equivalent in LTM, which is
/// black-box by construction: its only evidence that two tests are
/// interchangeable is that they look alike. Test bodies look alike for a
/// living — every `#[rstest]` case in this repository is one `assert_eq!`
/// over a different function — so body score alone pairs a line-index
/// test with a calendar test and calls the result redundancy. Sharing a
/// reached function is the cheapest fact that separates "the same test
/// written twice" from "the same idiom applied to different code".
///
/// Only a *disproof* prunes: a pair survives whenever either side
/// reaches nothing the graph could resolve, because "no evidence they
/// overlap" is not evidence they do not.
fn prune_unrelated(adjacency: &mut [Vec<Neighbor>], guard: &ReachGuard) -> usize {
    let mut dropped = 0usize;
    for (unit, neighbors) in adjacency.iter_mut().enumerate() {
        let before = neighbors.len();
        neighbors.retain(|n| guard.related(unit, n.unit));
        dropped += before - neighbors.len();
    }
    // Every pruned edge was seen from both of its endpoints.
    dropped / 2
}

fn unit_key(unit: &ScoredUnit) -> (&str, usize, &str) {
    (&unit.rel_path, unit.start_line, &unit.name)
}

/// What the call graph can say about a set of near-copies: which
/// production functions each one reaches, and which of those nothing
/// else in the suite reaches.
#[derive(Debug)]
struct ReachGuard<'a> {
    graph: &'a CallGraph,
    /// Resolved call-graph adjacency, kept for the per-member traversals
    /// that run later.
    edges: Vec<Vec<usize>>,
    test_nodes: Vec<usize>,
    node_of_unit: HashMap<usize, usize>,
    /// Production nodes each located test reaches.
    reach: HashMap<usize, BTreeSet<usize>>,
    /// Tests the graph could not locate — a language it does not model,
    /// an unparsed file, a body it does not hold as a function. The guard
    /// has nothing to say about these, so they are counted rather than
    /// silently treated as clean.
    unmatched_count: usize,
}

impl<'a> ReachGuard<'a> {
    /// One forward traversal per test that has at least one near-copy.
    /// Tests with none are never reported, so paying for their reach
    /// would be paying for a row nobody reads.
    fn build(graph: &'a CallGraph, units: &[ScoredUnit], adjacency: &[Vec<Neighbor>]) -> Self {
        let edges = graph.resolved_adjacency();
        let node_by_location: HashMap<(&str, usize), usize> = graph
            .nodes
            .iter()
            .enumerate()
            .map(|(idx, node)| ((node.file.as_str(), node.start_line), idx))
            .collect();
        let test_nodes: Vec<usize> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.is_test)
            .map(|(idx, _)| idx)
            .collect();

        let mut node_of_unit = HashMap::new();
        let mut reach = HashMap::new();
        let mut unmatched_count = 0usize;
        for (unit, neighbors) in adjacency.iter().enumerate() {
            if neighbors.is_empty() {
                continue;
            }
            let Some(shape) = units.get(unit) else {
                continue;
            };
            let Some(&node) = node_by_location.get(&(shape.rel_path.as_str(), shape.start_line))
            else {
                unmatched_count += 1;
                continue;
            };
            node_of_unit.insert(unit, node);
            reach.insert(unit, production_reach(graph, &edges, &[node]));
        }
        Self {
            graph,
            edges,
            test_nodes,
            node_of_unit,
            reach,
            unmatched_count,
        }
    }

    /// Whether two tests exercise anything in common. Unknown counts as
    /// related — see [`prune_unrelated`].
    fn related(&self, a: usize, b: usize) -> bool {
        match (self.reach.get(&a), self.reach.get(&b)) {
            (Some(left), Some(right)) if !left.is_empty() && !right.is_empty() => {
                !left.is_disjoint(right)
            }
            _ => true,
        }
    }

    /// Production functions `unit` is the only static exerciser of.
    ///
    /// One more traversal, from every *other* test root at once: the
    /// difference against what this test reaches is what the suite would
    /// stop exercising if the test went away.
    fn unique_reach(&self, unit: usize) -> Vec<String> {
        let (Some(&node), Some(mine)) = (self.node_of_unit.get(&unit), self.reach.get(&unit))
        else {
            return Vec::new();
        };
        let others: Vec<usize> = self
            .test_nodes
            .iter()
            .copied()
            .filter(|&t| t != node)
            .collect();
        let theirs = production_reach(self.graph, &self.edges, &others);
        let mut unique: Vec<String> = mine
            .difference(&theirs)
            .filter_map(|&n| Some(self.graph.nodes.get(n)?.qualified_name.clone()))
            .collect();
        unique.sort();
        unique.dedup();
        unique
    }
}

/// Non-test nodes a forward traversal from `starts` reaches.
fn production_reach(graph: &CallGraph, edges: &[Vec<usize>], starts: &[usize]) -> BTreeSet<usize> {
    bfs(edges, starts)
        .into_iter()
        .map(|visit| visit.node)
        .filter(|&n| graph.nodes.get(n).is_some_and(|node| !node.is_test))
        .collect()
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    /// Input path: a single source file, or the root directory walked.
    root: String,
    /// What every verdict on this report is relative to.
    note: &'static str,
    /// Body-scoring algorithm used. Scores from different methods are
    /// not on the same scale.
    method: &'static str,
    threshold: f64,
    min_lines: usize,
    /// Body-tree floor applied on top of `min_lines`.
    min_body_nodes: usize,
    /// Whether fold candidates were checked against the call graph.
    reach_guard: bool,
    /// Redundancy groups, most members first.
    groups: Vec<GroupView>,
    summary: Summary,
}

/// Everything [`Report::build`] needs. A struct rather than six
/// positional arguments, which at this arity is one transposed pair of
/// `usize`s away from a silently wrong report.
struct ReportInputs<'a> {
    roots: &'a AnalyzeRoots,
    analyzer: &'a TestRedundancyAnalyzer,
    scored: &'a ScoredCorpus,
    adjacency: &'a [Vec<Neighbor>],
    groups: Vec<Group>,
    guard: Option<&'a ReachGuard<'a>>,
    unrelated_pair_count: usize,
    opaque_body_test_count: usize,
}

impl Report {
    fn build(inputs: ReportInputs<'_>) -> Self {
        let ReportInputs {
            roots,
            analyzer,
            scored,
            adjacency,
            groups,
            guard,
            unrelated_pair_count,
            opaque_body_test_count,
        } = inputs;
        let views: Vec<GroupView> = groups
            .iter()
            .map(|group| GroupView::build(group, &scored.units, guard))
            .collect();
        let summary = Summary::build(SummaryInputs {
            scored,
            adjacency,
            groups: &views,
            guard,
            unrelated_pair_count,
            opaque_body_test_count,
        });
        Self {
            schema_version: SCHEMA_VERSION,
            root: roots.display(),
            note: NOTE,
            method: analyzer.method.as_str(),
            threshold: analyzer.threshold,
            min_lines: analyzer.min_lines.unwrap_or(DEFAULT_MIN_LINES),
            min_body_nodes: analyzer.resolved_min_body_nodes(),
            reach_guard: analyzer.reach_guard,
            groups: views,
            summary,
        }
    }
}

/// What to do with a group, from how far its bodies agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Verdict {
    /// Every member agrees with the representative on everything the
    /// parser recorded — the same calls, the same names, in the same
    /// shape. Nothing separates the copies that the tree can see.
    Duplicate,
    /// The members share the representative's shape but name different
    /// things. The duplication is the scaffolding, not the cases, so the
    /// edit is one parameterized test over what varies — not a deletion.
    Parameterize,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Self::Duplicate => "duplicate",
            Self::Parameterize => "parameterize",
        }
    }
}

#[derive(Debug, Serialize)]
struct GroupView {
    /// The test to keep: the one that makes the most others redundant,
    /// ties broken by source location.
    representative: TestRef,
    /// Near-copies of the representative, in source order.
    members: Vec<MemberView>,
    verdict: Verdict,
    /// Weakest member score in the group — how far the group is from
    /// being one test written several times.
    min_similarity: f64,
    /// Weakest member score once each node's recorded value counts, not
    /// just its label. Far below `min_similarity` is what makes the
    /// group a parameterization rather than a merge.
    min_value_similarity: f64,
    /// Members the guard did not hold back.
    foldable_count: usize,
}

impl GroupView {
    fn build(group: &Group, units: &[ScoredUnit], guard: Option<&ReachGuard<'_>>) -> Self {
        let members: Vec<MemberView> = group
            .members
            .iter()
            .filter_map(|member| MemberView::build(member, units, guard))
            .collect();
        let min_similarity = fold_min(members.iter().map(|m| m.similarity));
        let min_value_similarity = fold_min(members.iter().map(|m| m.value_similarity));
        let verdict = if min_value_similarity >= IDENTICAL_VALUE_SIMILARITY {
            Verdict::Duplicate
        } else {
            Verdict::Parameterize
        };
        Self {
            representative: TestRef::build(&units[group.representative]),
            foldable_count: members.iter().filter(|m| m.folds).count(),
            members,
            verdict,
            min_similarity,
            min_value_similarity,
        }
    }
}

fn fold_min(scores: impl Iterator<Item = f64>) -> f64 {
    scores.fold(1.0, f64::min)
}

#[derive(Debug, Serialize)]
struct TestRef {
    name: String,
    file: String,
    start_line: usize,
    end_line: usize,
    loc: usize,
}

impl TestRef {
    fn build(unit: &ScoredUnit) -> Self {
        Self {
            name: unit.name.clone(),
            file: unit.rel_path.clone(),
            start_line: unit.start_line,
            end_line: unit.end_line,
            loc: unit.line_count,
        }
    }
}

#[derive(Debug, Serialize)]
struct MemberView {
    #[serde(flatten)]
    test: TestRef,
    similarity: f64,
    value_similarity: f64,
    /// Whether the guard let this one through. `false` is always
    /// explained by `unique_reach`.
    folds: bool,
    /// Production functions this test is the only static exerciser of.
    /// Present only when the guard held the member back.
    #[serde(skip_serializing_if = "Option::is_none")]
    unique_reach: Option<UniqueReach>,
}

impl MemberView {
    fn build(
        member: &Neighbor,
        units: &[ScoredUnit],
        guard: Option<&ReachGuard<'_>>,
    ) -> Option<Self> {
        let unique = guard
            .map(|guard| guard.unique_reach(member.unit))
            .filter(|functions| !functions.is_empty())
            .map(|functions| UniqueReach::build(&functions));
        Some(Self {
            test: TestRef::build(units.get(member.unit)?),
            similarity: member.similarity,
            value_similarity: member.value_similarity,
            folds: unique.is_none(),
            unique_reach: unique,
        })
    }
}

#[derive(Debug, Serialize)]
struct UniqueReach {
    function_count: usize,
    /// The first few by name; `function_count` is the whole set.
    functions: Vec<String>,
}

impl UniqueReach {
    fn build(functions: &[String]) -> Self {
        Self {
            function_count: functions.len(),
            functions: functions
                .iter()
                .take(MAX_UNIQUE_REACH_EXAMPLES)
                .cloned()
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct Summary {
    /// Test functions in scope after `--min-lines` — the denominator.
    test_function_count: usize,
    group_count: usize,
    /// Tests a group made redundant, guard or no guard.
    redundant_test_count: usize,
    /// Of those, how many the guard let through.
    foldable_count: usize,
    /// Of those, how many the guard held back.
    reach_guarded_count: usize,
    /// Tests with a near-copy that the call graph could not locate, so
    /// the guard could not rule on them. Non-zero means `foldable_count`
    /// counts rows nothing checked.
    graph_unmatched_test_count: usize,
    /// Near-copy pairs dropped before selection because the call graph
    /// showed the two tests exercise no production function in common.
    /// This is the noise floor of body scoring on a test suite, measured:
    /// a large number against a small `group_count` means most of what
    /// scored alike was one assertion idiom, not one test written twice.
    unrelated_pair_count: usize,
    /// Tests with a near-copy dropped because their body tree is smaller
    /// than `min_body_nodes` — a body the parser kept opaque, most often
    /// a Rust test that is one unexpanded `assert_eq!`. They are counted
    /// rather than reported because there is no evidence either way.
    opaque_body_test_count: usize,
    /// LTM's suite-level redundancy figure: the mean squared similarity
    /// between each test and its nearest near-copy, zero for a test with
    /// none. Rises toward 1 as the suite fills with copies of itself.
    ///
    /// A lower bound, and deliberately not LTM's own number. Candidate
    /// generation never scores the pairs it can prove are far apart, and
    /// the guard then drops the pairs it can disprove, so a test with no
    /// surviving neighbour is credited 0 rather than the sub-threshold
    /// score it really has. That makes the figure comparable across runs
    /// of this tool and against nothing else.
    redundancy_score: f64,
}

struct SummaryInputs<'a> {
    scored: &'a ScoredCorpus,
    adjacency: &'a [Vec<Neighbor>],
    groups: &'a [GroupView],
    guard: Option<&'a ReachGuard<'a>>,
    unrelated_pair_count: usize,
    opaque_body_test_count: usize,
}

impl Summary {
    fn build(inputs: SummaryInputs<'_>) -> Self {
        let SummaryInputs {
            scored,
            adjacency,
            groups,
            guard,
            unrelated_pair_count,
            opaque_body_test_count,
        } = inputs;
        let redundant_test_count: usize = groups.iter().map(|group| group.members.len()).sum();
        let foldable_count: usize = groups.iter().map(|group| group.foldable_count).sum();
        Self {
            test_function_count: scored.units.len(),
            group_count: groups.len(),
            redundant_test_count,
            foldable_count,
            reach_guarded_count: redundant_test_count - foldable_count,
            graph_unmatched_test_count: guard.map_or(0, |guard| guard.unmatched_count),
            unrelated_pair_count,
            opaque_body_test_count,
            redundancy_score: redundancy_score(adjacency),
        }
    }
}

/// LTM's fitness function over the graph this report believes in.
///
/// Read off the pruned adjacency rather than the raw pair list, so the
/// figure agrees with the groups underneath it: a suite whose only
/// near-copies turned out to exercise different code is not a redundant
/// suite, and a header claiming otherwise over an empty body would be
/// the first thing anyone stopped trusting.
fn redundancy_score(adjacency: &[Vec<Neighbor>]) -> f64 {
    if adjacency.is_empty() {
        return 0.0;
    }
    let total: f64 = adjacency
        .iter()
        .map(|neighbors| {
            let nearest = neighbors
                .iter()
                .map(|n| n.similarity)
                .fold(0.0f64, f64::max);
            nearest * nearest
        })
        .sum();
    total / adjacency.len() as f64
}

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let summary = &report.summary;
    let mut out = format!(
        "# Test redundancy report: {} ({} method, {} test function(s), threshold {:.2}, \
         min lines {}, min body nodes {})\n",
        report.root,
        report.method,
        summary.test_function_count,
        report.threshold,
        report.min_lines,
        report.min_body_nodes,
    );
    let _ = writeln!(
        out,
        "\nSuite redundancy score {:.3} (lower bound). {}",
        summary.redundancy_score, NOTE,
    );
    write_calibration(&mut out, report);
    if report.groups.is_empty() {
        let _ = writeln!(out, "\n_No test near-copies at or above threshold._");
        return out;
    }
    let limit = top.unwrap_or(DEFAULT_TOP).min(report.groups.len());
    let _ = writeln!(
        out,
        "\n## {} redundancy group(s), {} foldable of {} redundant test(s){}",
        summary.group_count,
        summary.foldable_count,
        summary.redundant_test_count,
        guard_suffix(report, summary),
    );
    if limit < report.groups.len() {
        let _ = writeln!(out, "\nShowing the {limit} largest.");
    }
    for group in &report.groups[..limit] {
        let keep = &group.representative;
        let _ = writeln!(
            out,
            "\n- keep `{}` ({}:L{}-{}) — {} near-copy(ies), {}, min similarity {:.2} \
             (with values {:.2})",
            keep.name,
            keep.file,
            keep.start_line,
            keep.end_line,
            group.members.len(),
            group.verdict.as_str(),
            group.min_similarity,
            group.min_value_similarity,
        );
        for member in &group.members {
            let _ = writeln!(out, "  - {}", member_line(member));
        }
    }
    out
}

/// What the run threw away before ranking anything.
///
/// Rendered whether or not any group survived, because an empty report
/// is exactly when "why did this find nothing?" gets asked, and these
/// two counts are the answer.
fn write_calibration(out: &mut String, report: &Report) {
    let summary = &report.summary;
    if summary.opaque_body_test_count > 0 {
        let _ = writeln!(
            out,
            "\n{} test(s) skipped: body under {} tree nodes, so a near-copy claim would be \
             about a body the parser kept opaque.",
            summary.opaque_body_test_count, report.min_body_nodes,
        );
    }
    if summary.unrelated_pair_count > 0 {
        let _ = writeln!(
            out,
            "\n{} near-copy pair(s) dropped: the call graph shows no production function in \
             common.",
            summary.unrelated_pair_count,
        );
    }
}

fn guard_suffix(report: &Report, summary: &Summary) -> String {
    if !report.reach_guard {
        return " (reach guard off)".to_owned();
    }
    let mut suffix = format!(", {} held by unique reach", summary.reach_guarded_count);
    if summary.graph_unmatched_test_count > 0 {
        let _ = write!(
            suffix,
            ", {} not found in the call graph",
            summary.graph_unmatched_test_count,
        );
    }
    suffix
}

fn member_line(member: &MemberView) -> String {
    let test = &member.test;
    let head = format!(
        "`{}` ({}:L{}-{}) {:.2} / {:.2} with values",
        test.name,
        test.file,
        test.start_line,
        test.end_line,
        member.similarity,
        member.value_similarity,
    );
    match &member.unique_reach {
        None => format!("{head} — fold"),
        Some(unique) => format!(
            "{head} — held: sole static caller of {} ({})",
            counted_functions(unique.function_count),
            unique.functions.join(", "),
        ),
    }
}

fn counted_functions(count: usize) -> String {
    match count {
        1 => "1 function".to_owned(),
        n => format!("{n} functions"),
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::Value;

    use super::*;
    use crate::test_support::write_file;

    fn analyze_json(dir: &std::path::Path, analyzer: TestRedundancyAnalyzer) -> Value {
        let out = analyzer
            .analyze(dir.to_path_buf(), OutputFormat::Json)
            .expect("analysis succeeds");
        serde_json::from_str(&out).expect("valid JSON")
    }

    fn analyze_md(dir: &std::path::Path, analyzer: TestRedundancyAnalyzer) -> String {
        analyzer
            .analyze(dir.to_path_buf(), OutputFormat::Md)
            .expect("analysis succeeds")
    }

    /// Bodies long enough to clear both floors, so the fixtures exercise
    /// selection rather than the eligibility cuts.
    fn test_body(target: &str, seed: u32) -> String {
        format!(
            "let first = {seed};\n\
             let second = first + 1;\n\
             let total = {target}(first, second);\n\
             let doubled = total * 2;\n\
             assert!(doubled >= total);\n"
        )
    }

    /// Four tests of one production function: three are near-copies of
    /// each other, the fourth has a body of its own.
    fn one_target_source() -> String {
        let mut src = String::from("pub fn shared(a: u32, b: u32) -> u32 { a + b }\n");
        src.push_str("#[cfg(test)]\nmod tests {\nuse super::*;\n");
        for (name, seed) in [("alpha", 1u32), ("beta", 2), ("gamma", 3)] {
            src.push_str(&format!(
                "#[test]\nfn {name}() {{\n{}}}\n",
                test_body("shared", seed)
            ));
        }
        src.push_str(
            "#[test]\nfn outlier() {\n\
             let mut seen = Vec::new();\n\
             for step in 0..4 {\n\
             seen.push(shared(step, step));\n\
             }\n\
             seen.sort();\n\
             seen.dedup();\n\
             assert!(!seen.is_empty());\n\
             }\n",
        );
        src.push_str("}\n");
        src
    }

    #[test]
    fn near_copies_of_one_target_become_one_group() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", &one_target_source());

        let report = analyze_json(dir.path(), TestRedundancyAnalyzer::new());
        assert_eq!(report["schema_version"], 1);
        assert_eq!(report["summary"]["group_count"], 1);
        // Three near-copies: one is kept, two fold into it.
        assert_eq!(report["summary"]["redundant_test_count"], 2);
        let group = &report["groups"][0];
        assert_eq!(group["representative"]["name"], "alpha");
        let members: Vec<&str> = group["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect();
        assert_eq!(members, ["beta", "gamma"]);
        // The outlier shares the target but not the body.
        assert!(!members.contains(&"outlier"));
    }

    /// The representative is the test that covers the most others, and
    /// ties fall to source order — the property that keeps a report
    /// stable across runs and therefore diffable.
    #[test]
    fn selection_is_stable_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", &one_target_source());

        let first = analyze_json(dir.path(), TestRedundancyAnalyzer::new());
        let second = analyze_json(dir.path(), TestRedundancyAnalyzer::new());
        assert_eq!(first, second);
    }

    /// Two tests that look alike but exercise disjoint production
    /// functions are not redundant, whatever their bodies score.
    #[test]
    fn tests_of_different_functions_are_not_a_group() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            &format!(
                "pub fn left(a: u32, b: u32) -> u32 {{ a + b }}\n\
                 pub fn right(a: u32, b: u32) -> u32 {{ a * b }}\n\
                 #[cfg(test)]\nmod tests {{\nuse super::*;\n\
                 #[test]\nfn exercises_left() {{\n{}}}\n\
                 #[test]\nfn exercises_right() {{\n{}}}\n\
                 }}\n",
                test_body("left", 1),
                test_body("right", 1),
            ),
        );

        let report = analyze_json(dir.path(), TestRedundancyAnalyzer::new());
        assert_eq!(report["summary"]["group_count"], 0);
        assert_eq!(report["summary"]["unrelated_pair_count"], 1);
        // Without the graph there is nothing to disprove the pair with.
        let unguarded = analyze_json(
            dir.path(),
            TestRedundancyAnalyzer::new().with_reach_guard(false),
        );
        assert_eq!(unguarded["summary"]["group_count"], 1);
        assert_eq!(unguarded["summary"]["unrelated_pair_count"], 0);
    }

    /// A near-copy that is the only static caller of something is kept,
    /// and the report names what would stop being exercised.
    #[test]
    fn sole_caller_of_a_function_is_held_back() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            &format!(
                "pub fn shared(a: u32, b: u32) -> u32 {{ a + b }}\n\
                 pub fn only_here(a: u32, b: u32) -> u32 {{ shared(a, b) }}\n\
                 #[cfg(test)]\nmod tests {{\nuse super::*;\n\
                 #[test]\nfn plain_one() {{\n{}}}\n\
                 #[test]\nfn plain_two() {{\n{}}}\n\
                 #[test]\nfn also_covers_only_here() {{\n{}}}\n\
                 }}\n",
                test_body("shared", 1),
                test_body("shared", 2),
                test_body("only_here", 3),
            ),
        );

        let report = analyze_json(dir.path(), TestRedundancyAnalyzer::new());
        let members = report["groups"][0]["members"].as_array().unwrap();
        let held: Vec<&Value> = members
            .iter()
            .filter(|m| m["folds"] == Value::Bool(false))
            .collect();
        assert_eq!(held.len(), 1, "report: {report}");
        assert_eq!(held[0]["name"], "also_covers_only_here");
        assert_eq!(held[0]["unique_reach"]["function_count"], 1);
        assert_eq!(held[0]["unique_reach"]["functions"][0], "crate::only_here");
        assert_eq!(report["summary"]["reach_guarded_count"], 1);
    }

    /// A Rust body that is one unexpanded `assert_eq!` is a two-node
    /// tree: identical to every other such body whatever it asserts.
    /// `--min-lines` cannot see that, because an `#[rstest]` signature
    /// carries the test past any line floor on its own.
    #[test]
    fn bodies_the_parser_keeps_opaque_are_skipped_not_grouped() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            // The signatures are spread over enough lines to clear
            // `--min-lines` on their own, which is what an `#[rstest]`
            // case list does in real code; each body is one macro.
            "pub fn left(a: u32, b: u32) -> u32 { a + b }\n\
             pub fn right(a: u32, b: u32) -> u32 { a * b }\n\
             #[cfg(test)]\nmod tests {\nuse super::*;\n\
             #[test]\nfn macro_only_left(\n) \n{\n\
             \nassert_eq!(left(1, 1), 2);\n\n}\n\
             #[test]\nfn macro_only_right(\n) \n{\n\
             \nassert_eq!(right(1, 1), 1);\n\n}\n\
             }\n",
        );

        let report = analyze_json(
            dir.path(),
            TestRedundancyAnalyzer::new().with_threshold(0.7),
        );
        assert_eq!(report["summary"]["group_count"], 0);
        assert_eq!(report["summary"]["opaque_body_test_count"], 2);

        // The same corpus with the floor lowered does group them, which
        // is the report this default exists to prevent.
        let lowered = analyze_json(
            dir.path(),
            TestRedundancyAnalyzer::new()
                .with_threshold(0.7)
                .with_min_body_nodes_opt(Some(1)),
        );
        assert_eq!(lowered["summary"]["group_count"], 1);
        assert_eq!(lowered["summary"]["opaque_body_test_count"], 0);
    }

    /// A body with different local names is the same test over
    /// different material; a body that matches name for name is the same
    /// test twice.
    #[rstest]
    #[case::same_names(&["first", "second", "total", "doubled"], "duplicate")]
    #[case::renamed_locals(&["alpha", "beta", "sum", "twice"], "parameterize")]
    fn verdict_follows_the_value_reading(#[case] names: &[&str], #[case] expected: &str) {
        let renamed = |body: String| {
            let mut out = body;
            for (from, to) in ["first", "second", "total", "doubled"].iter().zip(names) {
                out = out.replace(from, to);
            }
            out
        };
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            &format!(
                "pub fn shared(a: u32, b: u32) -> u32 {{ a + b }}\n\
                 #[cfg(test)]\nmod tests {{\nuse super::*;\n\
                 #[test]\nfn one() {{\n{}}}\n\
                 #[test]\nfn two() {{\n{}}}\n\
                 }}\n",
                test_body("shared", 1),
                renamed(test_body("shared", 1)),
            ),
        );

        let report = analyze_json(dir.path(), TestRedundancyAnalyzer::new());
        assert_eq!(report["groups"][0]["verdict"], expected, "report: {report}");
    }

    /// An empty report still says what it threw away — the run that
    /// finds nothing is exactly the one whose cuts get questioned.
    #[test]
    fn markdown_reports_its_cuts_even_with_no_groups() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "pub fn alone() -> u32 { 1 }\n");

        let md = analyze_md(dir.path(), TestRedundancyAnalyzer::new());
        assert!(md.contains("min body nodes 8"), "{md}");
        assert!(
            md.contains("_No test near-copies at or above threshold._"),
            "{md}"
        );
    }

    #[test]
    fn markdown_names_the_kept_test_and_its_copies() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", &one_target_source());

        let md = analyze_md(dir.path(), TestRedundancyAnalyzer::new());
        assert!(md.contains("keep `alpha`"), "{md}");
        assert!(md.contains("`beta`"), "{md}");
        assert!(md.contains("— fold"), "{md}");
    }

    /// The suite figure rises with redundancy and is zero for a suite
    /// whose tests have no scored neighbour at all.
    #[test]
    fn redundancy_score_is_zero_without_near_copies() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "pub fn alone() -> u32 { 1 }\n");

        let report = analyze_json(dir.path(), TestRedundancyAnalyzer::new());
        assert_eq!(report["summary"]["redundancy_score"], 0.0);

        let with_copies = analyze_json(
            &{
                let dir = tempfile::tempdir().unwrap();
                write_file(dir.path(), "src/lib.rs", &one_target_source());
                dir.keep()
            },
            TestRedundancyAnalyzer::new(),
        );
        assert!(
            with_copies["summary"]["redundancy_score"].as_f64().unwrap() > 0.0,
            "report: {with_copies}",
        );
    }

    /// Greedy takes the test that covers the most others first: a hub
    /// with three near-copies outranks a pair, whatever their order on
    /// disk.
    #[test]
    fn the_widest_representative_is_chosen_first() {
        let units = vec![
            unit("a.rs", 1),
            unit("a.rs", 20),
            unit("a.rs", 40),
            unit("b.rs", 1),
            unit("b.rs", 20),
        ];
        // 3 is a hub over 0/1/2; 4 only pairs with 3.
        let scored = corpus(units, &[(3, 0), (3, 1), (3, 2), (3, 4)]);
        let graph = adjacency(&scored, 0);
        let groups = select_groups(&scored, &graph.adjacency);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].representative, 3);
        assert_eq!(groups[0].members.len(), 4);
    }

    /// Two disjoint pairs are two groups, not one.
    #[test]
    fn disjoint_pairs_stay_separate_groups() {
        let units = vec![
            unit("a.rs", 1),
            unit("a.rs", 20),
            unit("b.rs", 1),
            unit("b.rs", 20),
        ];
        let scored = corpus(units, &[(0, 1), (2, 3)]);
        let graph = adjacency(&scored, 0);
        let groups = select_groups(&scored, &graph.adjacency);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].members.len(), 1);
        assert_eq!(groups[1].members.len(), 1);
    }

    fn unit(file: &str, start_line: usize) -> ScoredUnit {
        ScoredUnit {
            rel_path: file.to_owned(),
            name: format!("t{start_line}"),
            start_line,
            end_line: start_line + 5,
            line_count: 6,
            body_node_count: 20,
        }
    }

    fn corpus(units: Vec<ScoredUnit>, pairs: &[(usize, usize)]) -> ScoredCorpus {
        ScoredCorpus {
            units,
            pairs: pairs
                .iter()
                .map(|&(i, j)| crate::analyze::similarity::ScoredUnitPair {
                    i,
                    j,
                    similarity: 0.9,
                    value_similarity: 0.9,
                })
                .collect(),
        }
    }
}
