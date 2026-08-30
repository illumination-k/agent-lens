//! `analyze test-only` — production functions only tests keep alive.
//!
//! A function that lives in production code but is reachable only from
//! test entry points is paying production costs (it is read, compiled,
//! shipped, and preserved as if callers depended on it) for a test-side
//! job. The honest place for it is the language's test scope — a
//! `#[cfg(test)]` module, a `_test.go` file — or, when the tests that
//! call it exist only to exercise it, deletion together with them.
//!
//! This is the gap between two siblings on the same call graph:
//! `analyze unreachable` roots its traversal in *every* entry point,
//! tests included, so code only tests reach is "reached" and never
//! reported; `analyze untested` walks the other direction and reports
//! production code no test reaches. The code both of them bless —
//! reached, but only from tests — is this analyzer's subject.
//!
//! Two finding kinds, because they are different edits:
//!
//! - **test-only**: no resolved call path from any production entry
//!   point (`main`, Go `init`, a public/exported declaration, a live
//!   annotation) reaches it, while a test does. These rows are private
//!   or crate-restricted by construction — anything public is itself a
//!   production entry.
//! - **test-only entry**: a public/exported declaration whose resolved
//!   callers are all tests. It cannot appear in the first kind (an
//!   entry point trivially reaches itself), and in a library a consumer
//!   outside the analyzed tree cannot be ruled out — the kind itself
//!   carries that weakness, so these rows are listed separately.
//!
//! Soundness bounds mirror `analyze unreachable`, and every reason to
//! doubt a row demotes it with a caveat rather than hiding it. The
//! raw-name backstop distinguishes *where* a bare-name occurrence sits:
//! inside a production function body it is a possible hidden caller and
//! caveats the row; outside every function span (an import, an
//! attribute, a top-level macro) it is counted as an unattributed
//! reference and reported, but does not caveat — tests import what they
//! call, so treating every `use` line as a hidden caller would demote
//! most true findings, and a caller this misses fails the compile
//! rather than silently breaking. Occurrences inside test functions and
//! inside other candidates are expected (the tests are the callers; the
//! candidates move together) and never count.
//!
//! Doubt also flows downstream. A caveat says "this row may be
//! production-live after all", and a live caller keeps its callees
//! alive, so every candidate the resolved calls of a caveated row can
//! reach inherits a caveat — the same propagation rule as `analyze
//! unreachable`'s reference scan. The two transitive sources this
//! catches: a helper family entered through a function-pointer
//! reference — no call edge, so the root demotes textually and the
//! mention edges between candidates carry the doubt down — and the
//! closure under a production trait/interface method, which dispatch
//! can enter without any call site naming it.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;

use serde::Serialize;

use super::call_graph::algo::bfs;
use super::call_graph::model::{ModuleResolutionSummary, NodeVisibility, Resolution};
use super::call_graph::{CallGraph, CallGraphBuilder, delegate_call_graph_builders};
use super::export_lang::{ExportLang, InterfaceIndex};
use super::format::render_module_confidence;
use super::options::analyzer_options;
use super::runner::render_report;
use super::unreachable::{EntryKind, entry_kind_of, identifiers};
use super::{AnalyzeRoots, AnalyzerError, OutputFormat};

const SCHEMA_VERSION: u32 = 1;

/// Markdown listing cap when `--top` is not given. JSON always carries
/// every finding.
const DEFAULT_TOP: usize = 20;

const NOTE: &str = "Each row is a production function only tests keep alive: no resolved call \
     path from any production entry point (`main`, a public/exported declaration, a live \
     annotation) reaches it, while a test does. The candidate edit is to move it into the \
     language's test scope — a `#[cfg(test)]` module, a `_test.go` file — or, when its tests \
     exist only to exercise it, to delete both together; it is a candidate, not a verdict. \
     Reachability walks resolved call edges only, so a caller hidden in a macro body, an \
     unresolved call site, or a raw name reference can make a row wrong; a row a production \
     trait/interface method can reach says so, since dispatch into that method is invisible to \
     the graph. The raw-name scan backs \
     each row up: bare-name occurrences inside production function bodies carry the raw-reference \
     caveat; occurrences outside every function span (imports, attributes, top-level macros) are \
     counted as unattributed references — check them before moving, though a caller this scan \
     misses fails the compile rather than silently breaking. Only files the graph scanned are \
     searched, so a name reached from a config file, a template, or another language is \
     invisible here. Public rows are a separate section: in a library, callers outside the \
     analyzed tree cannot be ruled out.";

analyzer_options! {
    /// `analyze test-only` flags, and the `[profile.<name>.test-only]`
    /// table.
    pub struct TestOnlyOptions {
        @shared(ranking);
    }
}

/// Analyzer entry point for `analyze test-only`.
#[derive(Debug, Default, Clone)]
pub struct TestOnlyAnalyzer {
    builder: CallGraphBuilder,
    top: Option<usize>,
    /// Mirrored from the path filter: dropping test files removes the
    /// very entry points this analyzer measures against, so the report
    /// has to say it happened.
    exclude_tests: bool,
}

impl TestOnlyAnalyzer {
    /// Apply a whole [`TestOnlyOptions`] group. The CLI flags and the
    /// `[profile.<name>.test-only]` table are the same type, so this is
    /// the only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: TestOnlyOptions) -> Self {
        self.with_top(opts.top)
    }

    pub fn new() -> Self {
        Self::default()
    }

    delegate_call_graph_builders! {
        builder,
        /// Accepted for CLI uniformity. Keeping only test files leaves
        /// no production function to judge, so the report is empty.
        only_tests,
        /// Drops test files — and with them every test entry point this
        /// analyzer measures against, so the report is empty and says
        /// the entry set was cut.
        exclude_tests => exclude_tests,
    }

    /// Cap each markdown section to the top-N entries. JSON output
    /// always carries every finding.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, AnalyzerError> {
        let roots = roots.into();
        // Interface method sets keep a Go method whose calls can
        // dispatch through an interface caveated, so this analyzer
        // always pays for their extraction (Go only).
        let graph = self
            .builder
            .clone()
            .with_interface_facts(true)
            .build(&roots)?;
        let collected = Findings::collect(&graph);
        let scan = ReferenceScan::run(&self.builder, &roots, &graph, &collected)?;
        let report = Report::build(&roots, &graph, collected, &scan, self.exclude_tests);
        render_report(&report, format, || format_markdown(&report, self.top))
    }
}

/// Which question a row answers — the two are different edits (move
/// into test scope vs. reconsider a public surface).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    /// No production call path reaches it; a test does.
    TestOnly,
    /// A public/exported entry whose resolved callers are all tests.
    TestOnlyEntry,
}

/// A reason the test-only claim is weaker for this row than its
/// presence suggests. Caveats demote; they never remove a row from the
/// JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Caveat {
    /// Visible beyond its own module (`pub(crate)`, `pub(super)`): a
    /// production caller outside the analyzed path can exist.
    WiderThanPrivate,
    /// A Rust trait `impl` method or trait default body: a production
    /// call site can name the trait and never this definition.
    TraitMethod,
    /// A Go method matching an in-scope interface's method set: calls
    /// can dispatch through the interface.
    InterfaceMethod,
    /// A production trait/interface method's body reaches it. Dispatch
    /// into that method is invisible to the graph, so a production call
    /// path may exist end to end.
    DispatchReachable,
    /// An ambiguous call site in live production code names it as a
    /// candidate — a production caller may exist.
    AmbiguousInbound,
    /// The bare name is written inside a production function's body —
    /// a macro argument, a closure, a string — which can be, or become,
    /// a production caller.
    RawReference,
    /// A row whose own claim already carries a caveat can reach this
    /// one through resolved calls, so if that row turns out to be
    /// production-live, this one is too.
    CaveatedCallerPath,
}

impl Caveat {
    fn as_str(self) -> &'static str {
        match self {
            Self::WiderThanPrivate => "visible outside its module",
            Self::TraitMethod => "trait method: callers name the trait",
            Self::InterfaceMethod => "may satisfy an in-scope interface",
            Self::DispatchReachable => "a production trait/interface method reaches it",
            Self::AmbiguousInbound => "an ambiguous production call site names it",
            Self::RawReference => "its bare name is written in production code",
            Self::CaveatedCallerPath => "reached from a row whose claim is already in doubt",
        }
    }
}

/// One finding.
#[derive(Debug, Clone, Serialize)]
struct FindingEntry {
    id: String,
    qualified_name: String,
    file: String,
    start_line: usize,
    end_line: usize,
    module: String,
    loc: usize,
    visibility: NodeVisibility,
    kind: Kind,
    /// Distinct test functions calling it directly.
    test_caller_count: usize,
    /// Distinct production functions calling it directly. For a
    /// `test_only` row every one of these is itself not
    /// production-reachable — they go wherever this row goes.
    production_caller_count: usize,
    /// Bare-name occurrences inside production function bodies outside
    /// every candidate span. Non-zero carries the raw-reference caveat.
    production_reference_count: usize,
    /// Bare-name occurrences outside every function span (imports,
    /// attributes, top-level macros), in files that hold production
    /// code. Informational: check them before moving.
    unattributed_reference_count: usize,
    /// Ambiguous call sites in production-reached code naming it.
    ambiguous_inbound_count: usize,
    /// Why this row is weaker than its presence suggests, sorted.
    caveats: Vec<Caveat>,
}

/// What was examined and skipped, so the counts have a denominator.
#[derive(Debug, Default, Serialize)]
struct Audit {
    /// Non-test Rust and Go functions in scope.
    judged_function_count: usize,
    /// Non-test functions skipped because their language carries no
    /// export status (TypeScript, Python): without it, "not a
    /// production entry" cannot be established.
    unjudged_function_count: usize,
    /// Public/exported entries whose resolved callers are all tests but
    /// which carry a live annotation — the annotation may itself be a
    /// production caller, so they are not listed.
    annotated_entry_count: usize,
    /// Source files tokenized for the raw-reference scan.
    reference_scan_file_count: usize,
}

/// The traversal's two starting sets, emitted rather than assumed.
#[derive(Debug, Serialize)]
struct EntrySet {
    production_entry_count: usize,
    test_entry_count: usize,
    /// `--exclude-tests` was in force: the test entry points this
    /// analyzer measures against are missing, so the report is empty by
    /// construction.
    tests_excluded: bool,
}

#[derive(Debug, Serialize)]
struct Summary {
    test_only_count: usize,
    test_only_entry_count: usize,
    /// Source lines held by the `test_only` rows — what moving them
    /// would take out of production code.
    test_only_loc: usize,
    /// Rows with no caveat at all.
    clean_count: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    note: &'static str,
    node_count: usize,
    entries: EntrySet,
    audit: Audit,
    summary: Summary,
    /// All findings, both kinds, strongest first.
    findings: Vec<FindingEntry>,
    /// Per-module call-site resolution counts. A module whose call
    /// sites mostly failed to resolve can hide production callers,
    /// which weakens every row inside it.
    modules: Vec<ModuleResolutionSummary>,
}

/// The candidate set before the reference scan.
struct Findings {
    entries: Vec<FindingEntry>,
    /// Bare names, parallel to `entries` — what the reference scan
    /// tokenizes for (`FindingEntry` carries only the qualified name).
    names: Vec<String>,
    /// Graph node index per entry, parallel to `entries` — what caveat
    /// propagation walks from.
    node_indices: Vec<usize>,
    /// Resolved adjacency with edges into test nodes cut, kept for the
    /// same propagation.
    prod_adjacency: Vec<Vec<usize>>,
    audit: Audit,
    entry_set: EntrySet,
}

impl Findings {
    fn collect(graph: &CallGraph) -> Self {
        let interfaces = InterfaceIndex::new(&graph.interfaces);
        let entry_kind: Vec<Option<EntryKind>> = graph.nodes.iter().map(entry_kind_of).collect();

        // Production reachability must not pass through a test: a
        // production entry calling a test helper (rare, but a graph can
        // contain it) does not make what the helper calls production
        // code. Edges into test nodes are cut before the walk.
        let mut prod_adjacency = graph.resolved_adjacency();
        for targets in &mut prod_adjacency {
            targets.retain(|&target| !graph.nodes[target].is_test);
        }
        let prod_roots: Vec<usize> = entry_kind
            .iter()
            .enumerate()
            .filter(|&(idx, kind)| kind.is_some() && !graph.nodes[idx].is_test)
            .map(|(idx, _)| idx)
            .collect();
        let mut prod_reached = vec![false; graph.nodes.len()];
        for visit in bfs(&prod_adjacency, &prod_roots) {
            prod_reached[visit.node] = true;
        }

        let test_roots: Vec<usize> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.is_test)
            .map(|(idx, _)| idx)
            .collect();
        let adjacency = graph.resolved_adjacency();
        let mut test_reached = vec![false; graph.nodes.len()];
        for visit in bfs(&adjacency, &test_roots) {
            test_reached[visit.node] = true;
        }

        // Production trait/interface methods are syntactically private
        // (a Rust trait impl method carries no visibility of its own),
        // so everything only they reach looks test-only — while
        // production dispatch into them is exactly what the graph
        // cannot see. Their whole downstream closure is therefore
        // caveated, not just the methods themselves.
        let dispatch_roots: Vec<usize> = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| {
                !node.is_test
                    && (matches!(
                        node.owner_kind,
                        Some(lens_domain::OwnerKind::TraitImpl | lens_domain::OwnerKind::Trait)
                    ) || (ExportLang::of(node) == Some(ExportLang::Go)
                        && !interfaces.matching(node, ExportLang::Go).is_empty()))
            })
            .map(|(idx, _)| idx)
            .collect();
        let mut dispatch_reached = vec![false; graph.nodes.len()];
        for visit in bfs(&prod_adjacency, &dispatch_roots) {
            dispatch_reached[visit.node] = true;
        }

        let callers = graph.resolved_callers();
        let ambiguous = ambiguous_inbound_counts(graph, &prod_reached);

        let mut audit = Audit::default();
        let mut entries = Vec::new();
        let mut names = Vec::new();
        let mut node_indices = Vec::new();
        for (idx, node) in graph.nodes.iter().enumerate() {
            if node.is_test {
                continue;
            }
            let Some(lang) = ExportLang::of(node) else {
                audit.unjudged_function_count += 1;
                continue;
            };
            audit.judged_function_count += 1;

            let empty = BTreeSet::new();
            let all_callers = callers.get(&idx).unwrap_or(&empty);
            let test_caller_count = all_callers
                .iter()
                .filter(|&&caller| graph.nodes[caller].is_test)
                .count();
            let production_caller_count = all_callers.len() - test_caller_count;

            let kind = if !prod_reached[idx] && test_reached[idx] {
                Kind::TestOnly
            } else if matches!(
                entry_kind[idx],
                Some(EntryKind::Public | EntryKind::Exported)
            ) && production_caller_count == 0
                && test_caller_count > 0
            {
                Kind::TestOnlyEntry
            } else if entry_kind[idx] == Some(EntryKind::Annotated)
                && production_caller_count == 0
                && test_caller_count > 0
            {
                audit.annotated_entry_count += 1;
                continue;
            } else {
                continue;
            };

            let mut caveats = Vec::new();
            // A `test_only_entry` row is public by definition — its
            // section says so, and repeating it per row would be noise.
            if kind == Kind::TestOnly && node.visibility != lang.private() {
                caveats.push(Caveat::WiderThanPrivate);
            }
            if matches!(
                node.owner_kind,
                Some(lens_domain::OwnerKind::TraitImpl | lens_domain::OwnerKind::Trait)
            ) {
                caveats.push(Caveat::TraitMethod);
            }
            if lang == ExportLang::Go && !interfaces.matching(node, ExportLang::Go).is_empty() {
                caveats.push(Caveat::InterfaceMethod);
            }
            // A dispatch root trivially reaches itself, and it already
            // carries the sharper trait/interface caveat.
            if dispatch_reached[idx]
                && !caveats.contains(&Caveat::TraitMethod)
                && !caveats.contains(&Caveat::InterfaceMethod)
            {
                caveats.push(Caveat::DispatchReachable);
            }
            let ambiguous_inbound_count = ambiguous.get(&idx).copied().unwrap_or(0);
            if ambiguous_inbound_count > 0 {
                caveats.push(Caveat::AmbiguousInbound);
            }
            caveats.sort_unstable();

            names.push(node.name.clone());
            node_indices.push(idx);
            entries.push(FindingEntry {
                id: node.id.clone(),
                qualified_name: node.qualified_name.clone(),
                file: node.file.clone(),
                start_line: node.start_line,
                end_line: node.end_line,
                module: node.module.clone(),
                loc: node.weights.loc,
                visibility: node.visibility,
                kind,
                test_caller_count,
                production_caller_count,
                production_reference_count: 0,
                unattributed_reference_count: 0,
                ambiguous_inbound_count,
                caveats,
            });
        }

        let entry_set = EntrySet {
            production_entry_count: prod_roots.len(),
            test_entry_count: test_roots.len(),
            tests_excluded: false,
        };
        Self {
            entries,
            names,
            node_indices,
            prod_adjacency,
            audit,
            entry_set,
        }
    }
}

/// Ambiguous call sites in production-reached code, per candidate
/// target. A test's ambiguous call is an expected caller; a dead
/// function's says nothing — only live production code weakens a row.
fn ambiguous_inbound_counts(graph: &CallGraph, prod_reached: &[bool]) -> BTreeMap<usize, usize> {
    let index_by_id = graph.node_index_by_id();
    let mut counts = BTreeMap::new();
    for edge in &graph.edges {
        if edge.resolution != Resolution::Ambiguous {
            continue;
        }
        // An unattributed caller (`from: None`) could be live
        // production code, so it counts.
        let from_is_live_production = edge.from.as_deref().is_none_or(|from| {
            index_by_id
                .get(from)
                .is_some_and(|&from| !graph.nodes[from].is_test && prod_reached[from])
        });
        if !from_is_live_production {
            continue;
        }
        for candidate in &edge.candidates {
            if let Some(&to) = index_by_id.get(candidate.as_str()) {
                *counts.entry(to).or_default() += edge.call_count;
            }
        }
    }
    counts
}

/// Where each candidate's bare name is written, classified by who wrote
/// it: a production function body (a possible hidden caller — caveat),
/// no function at all in a production-holding file (an import, an
/// attribute — informational), a test or another candidate (expected —
/// ignored).
struct ReferenceScan {
    file_count: usize,
    /// Parallel to [`Findings::entries`]: `(production, unattributed)`.
    counts: Vec<(usize, usize)>,
    /// `(mentioning candidate, mentioned candidate)` for bare names
    /// written inside another candidate's span — a function-pointer
    /// reference produces no call edge, so caveat propagation needs
    /// these as extra edges.
    internal: BTreeSet<(usize, usize)>,
}

/// Line ownership classes for the scan, in precedence order where spans
/// overlap. A candidate span carries its slot so an internal mention
/// knows who wrote it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineOwner {
    Candidate(usize),
    Test,
    Production,
}

impl ReferenceScan {
    fn run(
        builder: &CallGraphBuilder,
        roots: &AnalyzeRoots,
        graph: &CallGraph,
        collected: &Findings,
    ) -> Result<Self, AnalyzerError> {
        let mut counts = vec![(0usize, 0usize); collected.entries.len()];
        if collected.entries.is_empty() {
            return Ok(Self {
                file_count: 0,
                counts,
                internal: BTreeSet::new(),
            });
        }

        let mut slots_by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut slot_by_id: HashMap<&str, usize> = HashMap::new();
        for (slot, entry) in collected.entries.iter().enumerate() {
            slots_by_name
                .entry(collected.names[slot].as_str())
                .or_default()
                .push(slot);
            slot_by_id.insert(entry.id.as_str(), slot);
        }

        // Span tables per file, with per-class precedence at lookup.
        // Candidate spans carry their slot so an internal mention knows
        // who wrote it.
        let mut spans_by_file: HashMap<&str, Vec<(usize, usize, LineOwner)>> = HashMap::new();
        let mut file_has_production: HashMap<&str, bool> = HashMap::new();
        for node in &graph.nodes {
            let owner = if let Some(&slot) = slot_by_id.get(node.id.as_str()) {
                LineOwner::Candidate(slot)
            } else if node.is_test {
                LineOwner::Test
            } else {
                LineOwner::Production
            };
            spans_by_file.entry(node.file.as_str()).or_default().push((
                node.start_line,
                node.end_line,
                owner,
            ));
            let has_production = file_has_production.entry(node.file.as_str()).or_default();
            *has_production |= !node.is_test;
        }

        let mut internal: BTreeSet<(usize, usize)> = BTreeSet::new();
        let file_count = builder.visit_source_texts(roots, |file, source| {
            let spans = spans_by_file.get(file).map(Vec::as_slice).unwrap_or(&[]);
            // A file the graph found no production function in is test
            // scaffolding (or holds no functions at all to hide a call
            // in a body); unowned mentions there are the expected
            // imports and helpers of the callers themselves.
            let file_holds_production = file_has_production.get(file).copied().unwrap_or(true);
            for (offset, line) in source.lines().enumerate() {
                let line_no = offset + 1;
                let owner = line_owner(spans, line_no);
                if owner == Some(LineOwner::Test) {
                    continue;
                }
                for token in identifiers(line) {
                    let Some(slots) = slots_by_name.get(token) else {
                        continue;
                    };
                    for &slot in slots {
                        match owner {
                            Some(LineOwner::Candidate(writer)) => {
                                if writer != slot {
                                    internal.insert((writer, slot));
                                }
                            }
                            Some(LineOwner::Production) => counts[slot].0 += 1,
                            Some(LineOwner::Test) => {}
                            None if file_holds_production => counts[slot].1 += 1,
                            None => {}
                        }
                    }
                }
            }
        })?;
        Ok(Self {
            file_count,
            counts,
            internal,
        })
    }
}

/// The highest-precedence class among the spans covering `line`, if
/// any. Candidate wins over test wins over production, so a mention
/// inside a candidate nested in anything is never counted against it.
fn line_owner(spans: &[(usize, usize, LineOwner)], line: usize) -> Option<LineOwner> {
    let rank = |class: LineOwner| match class {
        LineOwner::Candidate(_) => 2,
        LineOwner::Test => 1,
        LineOwner::Production => 0,
    };
    spans
        .iter()
        .filter(|&&(start, end, _)| start <= line && line <= end)
        .map(|&(_, _, class)| class)
        .max_by_key(|&class| rank(class))
}

impl Report {
    fn build(
        roots: &AnalyzeRoots,
        graph: &CallGraph,
        collected: Findings,
        scan: &ReferenceScan,
        tests_excluded: bool,
    ) -> Self {
        let Findings {
            mut entries,
            names: _,
            node_indices,
            prod_adjacency,
            mut audit,
            mut entry_set,
        } = collected;
        audit.reference_scan_file_count = scan.file_count;
        entry_set.tests_excluded = tests_excluded;

        for (entry, &(production, unattributed)) in entries.iter_mut().zip(&scan.counts) {
            entry.production_reference_count = production;
            entry.unattributed_reference_count = unattributed;
            if production > 0 {
                entry.caveats.push(Caveat::RawReference);
                entry.caveats.sort_unstable();
            }
        }

        // Doubt flows downstream: every caveat above says "this row may
        // be production-live after all", and a live caller keeps its
        // callees alive — so everything a caveated row can reach
        // through resolved calls *or* through a bare-name mention in
        // its body (a function-pointer reference produces no call
        // edge) inherits a caveat. The mention edges are slot-level and
        // the call edges node-level, so the closure is a small
        // fixpoint.
        let slot_of: HashMap<usize, usize> = node_indices
            .iter()
            .enumerate()
            .map(|(slot, &idx)| (idx, slot))
            .collect();
        let mut suspect: Vec<bool> = entries.iter().map(|e| !e.caveats.is_empty()).collect();
        loop {
            let roots: Vec<usize> = suspect
                .iter()
                .enumerate()
                .filter(|&(_, &s)| s)
                .map(|(slot, _)| node_indices[slot])
                .collect();
            let mut changed = false;
            for visit in bfs(&prod_adjacency, &roots) {
                if let Some(&slot) = slot_of.get(&visit.node)
                    && !suspect[slot]
                {
                    suspect[slot] = true;
                    changed = true;
                }
            }
            for &(writer, mentioned) in &scan.internal {
                if suspect[writer] && !suspect[mentioned] {
                    suspect[mentioned] = true;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for (slot, entry) in entries.iter_mut().enumerate() {
            if suspect[slot] && entry.caveats.is_empty() {
                entry.caveats.push(Caveat::CaveatedCallerPath);
            }
        }

        entries.sort_by(|a, b| {
            a.caveats
                .len()
                .cmp(&b.caveats.len())
                .then_with(|| b.loc.cmp(&a.loc))
                .then_with(|| a.id.cmp(&b.id))
        });

        let summary = Summary {
            test_only_count: entries.iter().filter(|e| e.kind == Kind::TestOnly).count(),
            test_only_entry_count: entries
                .iter()
                .filter(|e| e.kind == Kind::TestOnlyEntry)
                .count(),
            test_only_loc: entries
                .iter()
                .filter(|e| e.kind == Kind::TestOnly)
                .map(|e| e.loc)
                .sum(),
            clean_count: entries.iter().filter(|e| e.caveats.is_empty()).count(),
        };

        Self {
            schema_version: SCHEMA_VERSION,
            root: roots.display(),
            language: graph.language,
            note: NOTE,
            node_count: graph.nodes.len(),
            entries: entry_set,
            audit,
            summary,
            findings: entries,
            modules: graph.module_summary.clone(),
        }
    }
}

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    let mut out = format!(
        "# Test-only report: {} ({} judged function(s), {} test-only, {} public called only \
         by tests)\n",
        report.root,
        report.audit.judged_function_count,
        report.summary.test_only_count,
        report.summary.test_only_entry_count,
    );
    let _ = writeln!(out, "\n{NOTE}\n");
    let _ = writeln!(
        out,
        "Entry set: {} production, {} test. Skipped: {} unjudged-language function(s), \
         {} annotated entries.",
        report.entries.production_entry_count,
        report.entries.test_entry_count,
        report.audit.unjudged_function_count,
        report.audit.annotated_entry_count,
    );
    if report.entries.tests_excluded {
        out.push_str(
            "\n**`--exclude-tests` removed every test entry point this analyzer measures \
             against — an empty report here is by construction, not evidence.**\n",
        );
    }
    if report.audit.judged_function_count == 0 {
        out.push_str("\n_No functions to analyze._\n");
        return out;
    }

    let (test_only, entry_rows): (Vec<_>, Vec<_>) = report
        .findings
        .iter()
        .partition(|e| e.kind == Kind::TestOnly);
    let _ = writeln!(
        out,
        "\n## Test-only functions (top {limit}, {} loc total)",
        report.summary.test_only_loc,
    );
    out.push_str(
        "\nNo production call path reaches these; tests do. Move each into test scope, or \
         delete it with its tests — after checking any reference counts on the row.\n",
    );
    render_entries(&mut out, &test_only, limit);

    let _ = writeln!(out, "\n## Public surface only tests call (top {limit})");
    out.push_str(
        "\nEach is a public/exported declaration whose only resolved callers are tests. In a \
         binary this is a test-only function wearing `pub`; in a library, consumers outside \
         the analyzed tree cannot be ruled out.\n",
    );
    render_entries(&mut out, &entry_rows, limit);

    render_module_confidence(
        &mut out,
        &report.modules,
        "Call sites in these modules often failed to resolve; a hidden production caller is \
         likeliest there, so treat their rows with extra suspicion.",
    );
    out
}

fn render_entries(out: &mut String, entries: &[&FindingEntry], limit: usize) {
    if entries.is_empty() {
        out.push_str("\n_None._\n");
        return;
    }
    out.push('\n');
    for entry in entries.iter().take(limit) {
        let _ = write!(
            out,
            "- `{}` ({}:{}): loc={}, {} test caller(s)",
            entry.qualified_name, entry.file, entry.start_line, entry.loc, entry.test_caller_count,
        );
        if entry.production_caller_count > 0 {
            let _ = write!(
                out,
                ", {} production caller(s) (themselves not production-reachable)",
                entry.production_caller_count,
            );
        }
        if entry.production_reference_count > 0 || entry.unattributed_reference_count > 0 {
            let _ = write!(
                out,
                ", refs: production={}, unattributed={}",
                entry.production_reference_count, entry.unattributed_reference_count,
            );
        }
        if !entry.caveats.is_empty() {
            let caveats: Vec<&str> = entry.caveats.iter().map(|c| c.as_str()).collect();
            let _ = write!(out, " — {}", caveats.join("; "));
        }
        out.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use serde_json::Value;
    use std::path::Path;

    fn analyze_json(path: &Path) -> Value {
        analyze_json_with(path, TestOnlyAnalyzer::new())
    }

    fn analyze_json_with(path: &Path, analyzer: TestOnlyAnalyzer) -> Value {
        let json = analyzer.analyze(path, OutputFormat::Json).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn finding<'a>(report: &'a Value, name_suffix: &str) -> Option<&'a Value> {
        report["findings"].as_array().unwrap().iter().find(|e| {
            e["qualified_name"]
                .as_str()
                .is_some_and(|q| q.ends_with(name_suffix))
        })
    }

    #[test]
    fn a_private_helper_called_only_by_tests_is_a_clean_finding() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn fixture() -> usize { 1 }\n\
             pub fn api() -> usize { 2 }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { let v = crate::fixture(); assert_eq!(v, 1); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let entry = finding(&report, "::fixture").expect("fixture listed");
        assert_eq!(entry["kind"], "test_only");
        assert_eq!(entry["caveats"], serde_json::json!([]));
        assert_eq!(entry["test_caller_count"], 1);
        assert!(
            finding(&report, "::api").is_none(),
            "uncalled pub is unreachable's finding"
        );
        assert_eq!(report["summary"]["test_only_count"], 1);
    }

    #[test]
    fn production_reachable_functions_are_not_findings() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn helper() {}\n\
             pub fn api() { helper(); }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::helper(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        assert!(
            finding(&report, "::helper").is_none(),
            "helper has a production path"
        );
        assert_eq!(report["findings"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn the_transitive_closure_of_a_test_helper_is_reported() {
        // `inner` has no direct test caller, but its only path from any
        // entry runs through `outer`, which only tests call.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn inner() {}\n\
             fn outer() { inner(); }\n\
             pub fn api() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::outer(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let outer = finding(&report, "::outer").expect("outer listed");
        assert_eq!(outer["kind"], "test_only");
        let inner = finding(&report, "::inner").expect("inner listed");
        assert_eq!(inner["kind"], "test_only");
        assert_eq!(inner["test_caller_count"], 0);
        assert_eq!(inner["production_caller_count"], 1);
    }

    #[test]
    fn a_public_function_called_only_by_tests_is_a_test_only_entry() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn seam() -> usize { 1 }\n\
             pub fn api() -> usize { 2 }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { let v = crate::seam(); assert_eq!(v, 1); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let entry = finding(&report, "::seam").expect("seam listed");
        assert_eq!(entry["kind"], "test_only_entry");
        assert_eq!(entry["test_caller_count"], 1);
        // Not double-reported as test_only: an entry reaches itself.
        assert_eq!(report["summary"]["test_only_entry_count"], 1);
        assert_eq!(report["summary"]["test_only_count"], 0);
    }

    #[test]
    fn a_public_function_with_a_production_caller_is_not_an_entry_finding() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn used() {}\n\
             pub fn api() { used(); }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::used(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        assert!(finding(&report, "::used").is_none());
    }

    #[test]
    fn a_crate_visible_helper_carries_the_visibility_caveat() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub(crate) fn fixture() {}\n\
             pub fn api() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::fixture(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let entry = finding(&report, "::fixture").expect("listed");
        assert!(
            entry["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "wider_than_private"),
            "got {entry:?}",
        );
    }

    #[test]
    fn a_mention_in_a_production_body_is_the_raw_reference_caveat() {
        // `format!` arguments produce no call edge, so the graph sees
        // only the test caller; the scan says production code writes
        // the name.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn fixture() -> usize { 1 }\n\
             pub fn api() -> String { format!(\"{}\", fixture()) }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::fixture(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let entry = finding(&report, "::fixture").expect("listed");
        assert_eq!(entry["production_reference_count"], 1);
        assert!(
            entry["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "raw_reference"),
            "got {entry:?}",
        );
    }

    #[test]
    fn an_import_line_is_an_unattributed_reference_not_a_caveat() {
        // `use crate::fixture;` inside the test module sits outside
        // every function span. Tests import what they call, so this is
        // informational, not a demotion.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn fixture() -> usize { 1 }\n\
             pub fn api() -> usize { 2 }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 use crate::fixture;\n\
                 #[test]\n\
                 fn t() { let v = fixture(); assert_eq!(v, 1); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let entry = finding(&report, "::fixture").expect("listed");
        assert_eq!(entry["production_reference_count"], 0);
        assert!(
            entry["unattributed_reference_count"].as_u64().unwrap() >= 1,
            "got {entry:?}",
        );
        assert!(
            !entry["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "raw_reference"),
            "got {entry:?}",
        );
    }

    #[test]
    fn mentions_inside_tests_and_fellow_candidates_do_not_count() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn fixture() -> usize { 1 }\n\
             fn wrapper() -> usize {\n\
                 fixture()\n\
             }\n\
             pub fn api() -> usize { 2 }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { let v = crate::wrapper(); assert_eq!(v, 1); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        // `wrapper` mentions `fixture`, but both are candidates — they
        // move together, so neither demotes the other.
        let entry = finding(&report, "::fixture").expect("listed");
        assert_eq!(entry["production_reference_count"], 0);
        assert_eq!(entry["caveats"], serde_json::json!([]));
    }

    #[test]
    fn a_trait_impl_method_carries_the_trait_caveat() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "struct S;\n\
             trait Probe { fn probe(&self) -> usize; }\n\
             impl Probe for S { fn probe(&self) -> usize { 1 } }\n\
             pub fn api() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 use super::*;\n\
                 #[test]\n\
                 fn t() { let v = S.probe(); assert_eq!(v, 1); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let entry = finding(&report, "Probe::probe").or_else(|| finding(&report, "S::probe"));
        let entry = entry.expect("probe listed");
        assert!(
            entry["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "trait_method"),
            "got {entry:?}",
        );
    }

    #[test]
    fn the_downstream_of_a_production_trait_method_is_caveated() {
        // `helper` is only reachable through `Runner::run`, a trait
        // impl method that production dispatch could enter without any
        // call site naming it — so the row survives but says so.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn helper() {}\n\
             struct S;\n\
             trait Runner { fn run(&self); }\n\
             impl Runner for S { fn run(&self) { helper(); } }\n\
             pub fn api() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 use super::*;\n\
                 #[test]\n\
                 fn t() { S.run(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let entry = finding(&report, "::helper").expect("helper listed");
        assert!(
            entry["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "dispatch_reachable"),
            "got {entry:?}",
        );
        // The trait method itself carries the sharper caveat, not this
        // one.
        let run = finding(&report, "::run").expect("run listed");
        let caveats = run["caveats"].as_array().unwrap();
        assert!(caveats.iter().any(|c| c == "trait_method"), "got {run:?}");
        assert!(
            !caveats.iter().any(|c| c == "dispatch_reachable"),
            "got {run:?}",
        );
    }

    #[test]
    fn a_caveated_rows_callees_inherit_the_doubt() {
        // `hidden` is written inside a production `format!` (a possible
        // hidden caller), and `helper` is only reachable through
        // `hidden` — so if `hidden` is production-live, `helper` is
        // too, and its row must say so.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn helper() -> usize { 1 }\n\
             fn hidden() -> usize { helper() }\n\
             pub fn api() -> String { format!(\"x{}\", hidden()) }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { let v = crate::hidden(); assert_eq!(v, 1); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let hidden = finding(&report, "::hidden").expect("hidden listed");
        assert!(
            hidden["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "raw_reference"),
            "got {hidden:?}",
        );
        let helper = finding(&report, "::helper").expect("helper listed");
        assert_eq!(
            helper["caveats"],
            serde_json::json!(["caveated_caller_path"]),
            "got {helper:?}",
        );
    }

    #[test]
    fn doubt_flows_through_a_function_pointer_mention() {
        // `table` hands `leaf` out as a function pointer — no call edge
        // — and `table` itself is written inside a production macro
        // body. If `table` is production-live, so is `leaf`, and the
        // only path between them is the bare-name mention.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn leaf() {}\n\
             fn table() -> fn() {\n\
                 leaf\n\
             }\n\
             pub fn api() -> String { format!(\"x{:?}\", table as usize) }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::table()(); crate::leaf(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let table = finding(&report, "::table").expect("table listed");
        assert!(
            table["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "raw_reference"),
            "got {table:?}",
        );
        let leaf = finding(&report, "::leaf").expect("leaf listed");
        assert_eq!(
            leaf["caveats"],
            serde_json::json!(["caveated_caller_path"]),
            "got {leaf:?}",
        );
    }

    #[test]
    fn the_downstream_of_a_go_interface_method_is_caveated() {
        // `grinder` is only reachable through `greet`, which the
        // in-scope interface can dispatch into from production; `plain`
        // matches no interface, so `quiet` below it stays clean.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.go",
            "package p\n\n\
             type greeter interface {\n\
             \tgreet(x int) int\n\
             }\n\n\
             type T struct{}\n\n\
             func (t T) greet(x int) int { return grinder() }\n\n\
             func grinder() int { return 1 }\n\n\
             func (t T) plain(x int) int { return quiet() }\n\n\
             func quiet() int { return 2 }\n\n\
             func Use() int { return 3 }\n",
        );
        write_file(
            dir.path(),
            "src/lib_test.go",
            "package p\n\n\
             import \"testing\"\n\n\
             func TestAll(t *testing.T) {\n\
             \tv := T{}\n\
             \t_ = v.greet(1)\n\
             \t_ = v.plain(2)\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let grinder = finding(&report, "::grinder").expect("grinder listed");
        assert!(
            grinder["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "dispatch_reachable"),
            "got {grinder:?}",
        );
        let quiet = finding(&report, "::quiet").expect("quiet listed");
        assert_eq!(quiet["caveats"], serde_json::json!([]), "got {quiet:?}");
        let plain = finding(&report, "::plain").expect("plain listed");
        assert_eq!(plain["caveats"], serde_json::json!([]), "got {plain:?}");
    }

    #[test]
    fn markdown_renders_refs_callers_and_caveat_text() {
        // `hidden` carries a production raw reference; `inner` has a
        // production caller that is itself unreachable. Both facts must
        // render, with the caveat text verbatim.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn inner() -> usize { 1 }\n\
             fn hidden() -> usize { inner() }\n\
             pub fn api() -> String { format!(\"x{}\", hidden()) }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { let v = crate::hidden(); assert_eq!(v, 1); }\n\
             }\n",
        );

        let md = TestOnlyAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains(", refs: production=1, unattributed=0"),
            "got: {md}",
        );
        assert!(
            md.contains("1 production caller(s) (themselves not production-reachable)"),
            "got: {md}",
        );
        assert!(
            md.contains("its bare name is written in production code"),
            "got: {md}",
        );
        assert!(
            md.contains("reached from a row whose claim is already in doubt"),
            "got: {md}",
        );
    }

    #[test]
    fn a_main_called_by_tests_is_neither_finding_nor_annotated_skip() {
        // `main` reaches itself as an entry, so it is no finding — and
        // it must not be miscounted as a skipped annotated entry.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn main() {}\n\
             pub fn api() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::main(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        assert!(finding(&report, "::main").is_none());
        assert_eq!(report["audit"]["annotated_entry_count"], 0);
    }

    #[test]
    fn an_ambiguous_production_call_site_is_a_caveat() {
        // The bare `dup()` in reachable production code could target
        // either module's `dup`, so both rows carry the caveat; the
        // same bare call from `dead` (which nothing reaches) says
        // nothing and must not add to the count.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub(crate) fn dup() {} }\n\
             mod b { pub(crate) fn dup() {} }\n\
             pub fn wild() { dup(); }\n\
             fn dead() { dup(); }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::a::dup(); crate::b::dup(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let entry = finding(&report, "a::dup").expect("a::dup listed");
        assert_eq!(entry["ambiguous_inbound_count"], 1, "got {entry:?}");
        assert!(
            entry["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "ambiguous_inbound"),
            "got {entry:?}",
        );
    }

    #[test]
    fn unattributed_mentions_in_test_only_files_do_not_count() {
        // The import inside the pure test file is the caller's own
        // scaffolding; the comment in the production-only file is not,
        // and is the one unattributed reference.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn fixture() -> usize { 1 }\n\
             pub fn api() -> usize { 2 }\n",
        );
        write_file(
            dir.path(),
            "src/other.rs",
            "// fixture is re-exported nowhere\n\
             pub fn other_api() {}\n",
        );
        write_file(
            dir.path(),
            "tests/it.rs",
            "use agent_lens_fixture::fixture;\n\
             #[test]\n\
             fn t() { let v = fixture(); assert_eq!(v, 1); }\n",
        );

        let report = analyze_json(dir.path());
        let entry = finding(&report, "::fixture").expect("listed");
        assert_eq!(
            entry["unattributed_reference_count"], 1,
            "only the production-file comment counts: {entry:?}",
        );
    }

    #[test]
    fn an_annotated_public_entry_is_skipped_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "#[no_mangle]\npub fn hook() {}\n\
             #[no_mangle]\npub fn uncalled_hook() {}\n\
             pub fn api() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::hook(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        assert!(finding(&report, "::hook").is_none());
        // Only the annotated entry tests actually call counts as a
        // skipped would-be finding; the uncalled one is nothing here.
        assert_eq!(report["audit"]["annotated_entry_count"], 1);
    }

    #[test]
    fn go_interface_matching_methods_carry_the_interface_caveat() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.go",
            "package p\n\n\
             type Greeter interface {\n\
             \tgreet(x int) int\n\
             }\n\n\
             type T struct{}\n\n\
             func (t T) greet(x int) int { return x }\n\n\
             func Use() int { return 1 }\n",
        );
        write_file(
            dir.path(),
            "src/lib_test.go",
            "package p\n\n\
             import \"testing\"\n\n\
             func TestGreet(t *testing.T) {\n\
             \tv := T{}\n\
             \t_ = v.greet(1)\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let entry = finding(&report, "::greet").expect("greet listed");
        assert!(
            entry["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "interface_method"),
            "got {entry:?}",
        );
    }

    #[test]
    fn unjudged_languages_are_counted_not_judged() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.py",
            "def helper():\n    return 1\n\n\
             def test_helper():\n    assert helper() == 1\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["findings"].as_array().unwrap().len(), 0);
        assert!(
            report["audit"]["unjudged_function_count"].as_u64().unwrap() >= 1,
            "got {report:?}",
        );
    }

    #[test]
    fn exclude_tests_empties_the_report_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn fixture() {}\n\
             pub fn api() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::fixture(); }\n\
             }\n",
        );

        let analyzer = TestOnlyAnalyzer::new().with_exclude_tests(true);
        let report = analyze_json_with(dir.path(), analyzer.clone());
        assert_eq!(report["findings"].as_array().unwrap().len(), 0);
        assert_eq!(report["entries"]["tests_excluded"], true);
        let md = analyzer.analyze(dir.path(), OutputFormat::Md).unwrap();
        assert!(md.contains("--exclude-tests"), "got: {md}");
    }

    #[test]
    fn markdown_renders_both_sections_and_the_loc_total() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn fixture() -> usize {\n    1\n}\n\
             pub fn seam() -> usize { 2 }\n\
             pub fn api() -> usize { 3 }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { let v = crate::fixture() + crate::seam(); assert_eq!(v, 3); }\n\
             }\n",
        );

        let md = TestOnlyAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Test-only report:"), "got: {md}");
        assert!(!md.contains("_No functions to analyze._"), "got: {md}");
        // Section membership: the private fixture in the first section,
        // the public seam in the second, and the loc total counts only
        // the first kind (fixture spans 3 lines).
        assert!(md.contains("3 loc total"), "got: {md}");
        let test_only_section = md
            .split("## Test-only functions")
            .nth(1)
            .and_then(|rest| rest.split("## Public surface only tests call").next())
            .expect("both sections rendered");
        let entry_section = md
            .split("## Public surface only tests call")
            .nth(1)
            .expect("entry section rendered");
        assert!(test_only_section.contains("`crate::fixture`"), "got: {md}");
        assert!(!test_only_section.contains("`crate::seam`"), "got: {md}");
        assert!(entry_section.contains("`crate::seam`"), "got: {md}");
        // Row shape: zero counts render nothing, clean rows carry no
        // caveat separator.
        assert!(!md.contains("0 production caller"), "got: {md}");
        assert!(!md.contains(", refs:"), "got: {md}");
        let fixture_line = md
            .lines()
            .find(|l| l.contains("`crate::fixture`"))
            .expect("row rendered");
        assert!(!fixture_line.contains(" — "), "got: {fixture_line}");
        // The candidate framing must survive rendering.
        assert!(md.contains("not a verdict"), "got: {md}");
    }

    #[test]
    fn top_caps_each_markdown_section() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn f1() {}\n\
             fn f2() {\n\
                 let _ = 1;\n\
                 let _ = 2;\n\
             }\n\
             pub fn api() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::f1(); crate::f2(); }\n\
             }\n",
        );

        let md = TestOnlyAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("top 1"), "got: {md}");
        // Bigger rows first, so the cap keeps `f2` and drops `f1`.
        assert!(md.contains("`crate::f2`"), "got: {md}");
        assert!(!md.contains("`crate::f1`"), "got: {md}");
    }

    #[test]
    fn markdown_reports_empty_input_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "README.md", "no source here\n");

        let md = TestOnlyAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("_No functions to analyze._"), "got: {md}");
    }

    #[test]
    fn analysis_is_deterministic_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn f1() {}\nfn f2() {}\npub fn api() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::f1(); crate::f2(); }\n\
             }\n",
        );

        let analyzer = TestOnlyAnalyzer::new();
        let a = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        let b = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn an_unreachable_function_nothing_calls_is_not_a_finding() {
        // Reached by nothing at all — that is `analyze unreachable`'s
        // report, not this one's.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn orphan() {}\n\
             pub fn api() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 #[test]\n\
                 fn t() { crate::api(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        assert!(finding(&report, "::orphan").is_none());
    }
}
