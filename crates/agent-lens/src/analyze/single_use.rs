//! `analyze single-use` — functions with exactly one resolved
//! production caller, reported as inline candidates.
//!
//! A function only one call site needs is indirection an agent pays for
//! on every read: understanding the caller means a jump to the
//! definition and back. When the body is small and simple, inlining it
//! removes that jump for free. This analyzer lists where that trade is
//! available; it never claims the trade is right — a well-named
//! extraction with one caller is often the better read, which is why
//! the size and complexity thresholds exist and are configurable.
//!
//! Division of labor with its siblings on the same call graph:
//! `wrapper` flags bodies that only forward, whatever their fan-in;
//! `delegation` measures forwarding *chains*; this analyzer flags
//! single-caller functions whatever their body shape. A forwarding-only
//! single-caller function legitimately appears in both `wrapper` and
//! here.
//!
//! Soundness runs the *opposite* way from `analyze unreachable`: fan-in
//! counts resolved call edges only, so "one caller" is a claim about
//! what the graph could see, and a hidden second caller (a macro body —
//! a call written inside `format!`/`write!` arguments produces no call
//! edge — an unresolved call site, a raw name reference) makes inlining
//! wrong.
//! Every way the graph is known to under-count callers therefore
//! demotes a row with a caveat rather than silently listing it, and
//! rows whose caller count cannot be trusted at all — trait and
//! interface methods, live annotations — are excluded outright since
//! those cannot be inlined anyway. The load-bearing demotion is the
//! raw-reference scan (the same tokenize-everything pass `unreachable`
//! runs, aimed the other way): a candidate whose bare name is written
//! anywhere outside its own definition and its known callers' spans is
//! caveated, which is what catches the callers a syntax-only graph
//! cannot see — macro arguments, `#[case]` attributes, imports, doc
//! references. The scan is textual and shares
//! `unreachable`'s bound: only files the graph scanned are searched.
//!
//! Thresholds are absolute and per-repository on purpose (unlike the
//! outlier rules in `hubs`): what "small enough to inline" means is a
//! house style. The report carries a calibration section — the loc and
//! cyclomatic distribution over *all* single-caller functions, and how
//! many the current thresholds keep — so an agent can read one run and
//! set `[profile.<name>.single-use]` for that repository instead of
//! trusting the defaults.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::Serialize;

use super::call_graph::model::{
    CallGraphNode, ModuleResolutionSummary, NodeVisibility, Resolution, ResolutionMethod,
};
use super::call_graph::{CallGraph, CallGraphBuilder, delegate_call_graph_builders};
use super::export_lang::{ExportLang, InterfaceIndex};
use super::format::render_module_confidence;
use super::options::analyzer_options;
use super::runner::render_report;
use super::unreachable::identifiers;
use super::{AnalyzeRoots, AnalyzerError, OutputFormat};
use std::collections::HashMap;

const SCHEMA_VERSION: u32 = 1;

/// Markdown ranking cap when `--top` is not given. JSON always carries
/// every candidate.
const DEFAULT_TOP: usize = 20;

/// Body-size ceiling for a candidate, in source lines. Calibrated on
/// this repository, where 58% of production functions have exactly one
/// resolved caller and their loc distribution runs p50=14, p75=29,
/// p90=51: the default sits at that p75 knee, past which an extraction
/// is usually carrying a name worth keeping. Override per repository
/// with `--max-loc` after reading the report's calibration section.
pub const DEFAULT_MAX_LOC: usize = 30;

/// Cyclomatic-complexity ceiling for a candidate. Calibrated on the
/// same run: this repository's single-caller functions run p50=2,
/// p75=4, p90=7, and a body branching more than the default is a unit
/// an agent may prefer to reason about in isolation. Override with
/// `--max-cyclomatic`.
pub const DEFAULT_MAX_CYCLOMATIC: u32 = 6;

const NOTE: &str = "Each row is a function with exactly one resolved production caller, small and \
     simple enough (per the thresholds echoed below) that inlining it into that caller is a \
     candidate edit, not a verdict: a single-caller function can be a deliberate, well-named \
     extraction, and keeping it is often right. Fan-in counts resolved call edges only, so \
     \"one caller\" is what the graph could see — a macro body (a call inside `format!` or \
     `write!` arguments produces no call edge), an unresolved call site, or a raw name reference \
     can hide a second caller, and inlining then breaks it. A raw-name scan backs the claim up: a row whose bare \
     name is written anywhere outside its definition and its known callers carries the \
     raw-reference caveat with the occurrence count. Only files the graph scanned are searched, \
     so a name reached from a config file, a template, or another language is still invisible — \
     check those before inlining. Rows whose single-caller claim is weaker carry \
     caveats; trait/interface methods and annotated functions are excluded outright because a \
     dispatch or annotation caller no call site names can reach them, and the impl surface could \
     not be inlined anyway.";

analyzer_options! {
    /// `analyze single-use` flags, and the `[profile.<name>.single-use]`
    /// table.
    pub struct SingleUseOptions {
        @shared(ranking);
        /// Body-size ceiling in source lines: a single-caller function
        /// larger than this is excluded from the candidate list (the
        /// calibration section still counts it). Defaults to 30.
        #[arg(long, value_name = "LINES")]
        pub max_loc: Option<usize>,
        /// Cyclomatic-complexity ceiling: a single-caller function
        /// branching more than this is excluded from the candidate list
        /// (the calibration section still counts it). Defaults to 6.
        #[arg(long, value_name = "N")]
        pub max_cyclomatic: Option<u32>,
    }
}

/// Analyzer entry point for `analyze single-use`.
#[derive(Debug, Default, Clone)]
pub struct SingleUseAnalyzer {
    builder: CallGraphBuilder,
    only_tests: bool,
    top: Option<usize>,
    max_loc: Option<usize>,
    max_cyclomatic: Option<u32>,
}

impl SingleUseAnalyzer {
    /// Apply a whole [`SingleUseOptions`] group. The CLI flags and the
    /// `[profile.<name>.single-use]` table are the same type, so this is
    /// the only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: SingleUseOptions) -> Self {
        self.with_top(opts.top)
            .with_max_loc(opts.max_loc)
            .with_max_cyclomatic(opts.max_cyclomatic)
    }

    pub fn new() -> Self {
        Self::default()
    }

    delegate_call_graph_builders! {
        builder,
        /// Analyze the test corpus instead: test helpers earn their
        /// indirection the same way, and in this mode test callers are
        /// the production callers.
        only_tests => only_tests,
        /// Drops test files. Test callers then stop being visible, so
        /// `test_fan_in` reads 0 for every row and the test-seam caveat
        /// cannot fire.
        exclude_tests,
    }

    /// Cap the markdown candidate list to the top-N entries. JSON
    /// output always carries every candidate.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    /// Body-size ceiling; `None` applies [`DEFAULT_MAX_LOC`].
    pub fn with_max_loc(mut self, max_loc: Option<usize>) -> Self {
        self.max_loc = max_loc;
        self
    }

    /// Complexity ceiling; `None` applies [`DEFAULT_MAX_CYCLOMATIC`].
    pub fn with_max_cyclomatic(mut self, max_cyclomatic: Option<u32>) -> Self {
        self.max_cyclomatic = max_cyclomatic;
        self
    }

    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, AnalyzerError> {
        let roots = roots.into();
        // Interface method sets keep a Go method whose calls can
        // dispatch through an interface off the candidate list, so this
        // analyzer always pays for their extraction (Go only).
        let graph = self
            .builder
            .clone()
            .with_interface_facts(true)
            .build(&roots)?;
        let thresholds = Thresholds {
            max_loc: self.max_loc.unwrap_or(DEFAULT_MAX_LOC),
            max_cyclomatic: self.max_cyclomatic.unwrap_or(DEFAULT_MAX_CYCLOMATIC),
        };
        let collected = SingleCallers::collect(&graph, self.only_tests);
        let raw = RawReferences::run(&self.builder, &roots, &collected.targets)?;
        let report = Report::build(&roots, &graph, collected, &raw, thresholds);
        render_report(&report, format, || format_markdown(&report, self.top))
    }
}

/// The absolute cuts this run applied, echoed so the JSON is
/// self-describing and the calibration section has something to be
/// relative to.
#[derive(Debug, Clone, Copy, Serialize)]
struct Thresholds {
    max_loc: usize,
    max_cyclomatic: u32,
}

/// A reason the single-caller claim, or the inline edit, is weaker for
/// this row than the row's presence suggests. Caveats demote; they never
/// remove a row from the JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Caveat {
    /// Visible beyond its own module (or compilation unit): a caller
    /// outside the analyzed path can exist.
    WiderThanPrivate,
    /// The adapter extracts no export status for this language
    /// (TypeScript, Python), so outside callers cannot be ruled out.
    UnknownVisibility,
    /// Tests call it directly. It may be kept as a test seam, and
    /// inlining moves those tests onto the caller.
    TestCallers,
    /// The one caller calls it at several sites, so inlining duplicates
    /// the body.
    MultipleCallSites,
    /// The caller lives in a different module: the indirection may be a
    /// module boundary rather than an accident.
    CrossModuleCaller,
    /// An ambiguous call site names this function as a candidate — a
    /// second caller may exist.
    AmbiguousInbound,
    /// The one incoming edge resolved through a name-fallback heuristic,
    /// so even the first caller is less certain.
    FallbackResolvedCall,
    /// The bare name is written somewhere outside the definition and its
    /// known callers — a macro body, a closure, a string, a doc comment,
    /// an import. Any of those can be, or become, a second caller. The
    /// scan is textual, so a same-named identifier anywhere in the
    /// scanned sources counts; a candidate with a ubiquitous name
    /// (`new`, `get`) will in practice always carry this caveat, which
    /// is honest — its callers cannot be established textually.
    RawReference,
}

impl Caveat {
    fn as_str(self) -> &'static str {
        match self {
            Self::WiderThanPrivate => "visible outside its module",
            Self::UnknownVisibility => "export status unknown for this language",
            Self::TestCallers => "tests call it directly",
            Self::MultipleCallSites => "several call sites in the one caller",
            Self::CrossModuleCaller => "caller is in another module",
            Self::AmbiguousInbound => "an ambiguous call site names it",
            Self::FallbackResolvedCall => "caller attributed by name fallback",
            Self::RawReference => "its bare name is written elsewhere",
        }
    }
}

/// The one caller, spelled out so the row is actionable without a
/// second query.
#[derive(Debug, Clone, Serialize)]
struct Caller {
    qualified_name: String,
    file: String,
    start_line: usize,
    module: String,
}

/// One inline candidate.
#[derive(Debug, Clone, Serialize)]
struct CandidateEntry {
    id: String,
    qualified_name: String,
    file: String,
    start_line: usize,
    end_line: usize,
    module: String,
    loc: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    cyclomatic_complexity: Option<u32>,
    visibility: NodeVisibility,
    caller: Caller,
    /// Call sites in the one caller. 1 is the clean case; more means
    /// inlining duplicates the body (and carries a caveat).
    call_count: usize,
    /// Source lines of those call sites in the caller's file.
    call_lines: Vec<usize>,
    /// Distinct test functions calling it (informational; a non-zero
    /// count carries the test-seam caveat).
    test_fan_in: usize,
    /// Occurrences of the bare name outside the definition and its known
    /// callers, across every source file the graph scanned. Non-zero
    /// carries the raw-reference caveat.
    raw_reference_count: usize,
    /// Why this row is weaker than its presence suggests, sorted. Empty
    /// means the graph saw nothing against the edit.
    caveats: Vec<Caveat>,
}

/// Functions never considered, by reason — emitted so "why is X not
/// listed?" has an answer in the report itself.
#[derive(Debug, Default, Serialize)]
struct Excluded {
    /// Rust trait `impl` methods and trait default bodies: callers can
    /// name the trait, and the method must exist to satisfy it.
    trait_method_count: usize,
    /// Go methods matching an in-scope interface's method set by name
    /// and arity: calls can dispatch through the interface.
    interface_method_count: usize,
    /// Carries an annotation not on the language's inert list: the
    /// annotation itself may be a caller.
    annotated_count: usize,
    /// Calls itself: the body cannot be inlined into its one external
    /// caller.
    recursive_count: usize,
}

/// Nearest-rank percentiles over one metric of the single-caller
/// population. What `--max-loc` / `--max-cyclomatic` should be for a
/// given appetite is read off this, not guessed.
#[derive(Debug, Serialize)]
struct MetricDistribution {
    p50: u64,
    p75: u64,
    p90: u64,
    max: u64,
}

impl MetricDistribution {
    /// `None` when no value was observed (empty population, or a metric
    /// the adapter did not extract).
    fn build(mut values: Vec<u64>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        values.sort_unstable();
        let max = *values.last()?;
        let pct = |p: usize| values[((p * values.len()).div_ceil(100)).saturating_sub(1)];
        Some(Self {
            p50: pct(50),
            p75: pct(75),
            p90: pct(90),
            max,
        })
    }
}

/// The calibration layer: what the thresholds are cutting, measured on
/// this run's own single-caller population.
#[derive(Debug, Serialize)]
struct Calibration {
    /// Functions with exactly one resolved production caller, before
    /// thresholds.
    single_caller_count: usize,
    /// Of those, how many the current thresholds keep as candidates.
    within_thresholds_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    loc: Option<MetricDistribution>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cyclomatic: Option<MetricDistribution>,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    note: &'static str,
    /// All graph nodes, including test functions.
    node_count: usize,
    /// Nodes eligible for the single-caller question (non-test unless
    /// `--only-tests`, and not excluded below).
    candidate_pool_count: usize,
    thresholds: Thresholds,
    excluded: Excluded,
    calibration: Calibration,
    /// Single-caller functions within the thresholds, cleanest and
    /// smallest first.
    candidates: Vec<CandidateEntry>,
    /// Per-module call-site resolution counts. A module whose call
    /// sites mostly failed to resolve can hide callers, which weakens
    /// every single-caller claim inside it.
    modules: Vec<ModuleResolutionSummary>,
}

/// The single-caller population before thresholds, plus what the
/// raw-reference scan needs to check each member.
struct SingleCallers {
    pool_count: usize,
    excluded: Excluded,
    entries: Vec<CandidateEntry>,
    /// Parallel to `entries`.
    targets: Vec<ScanTarget>,
}

/// What the raw-reference scan may ignore for one candidate: mentions
/// of the name inside the definition itself and inside every caller the
/// graph already accounts for (the one production caller, and test
/// callers — their call sites are the test-seam caveat's evidence, not
/// a hidden caller).
struct ScanTarget {
    name: String,
    /// `(file, start_line, end_line)` spans whose mentions are
    /// accounted for.
    allowed: Vec<(String, usize, usize)>,
}

impl SingleCallers {
    fn collect(graph: &CallGraph, only_tests: bool) -> Self {
        let interfaces = InterfaceIndex::new(&graph.interfaces);
        let mut excluded = Excluded::default();
        let inbound = InboundCalls::collect(graph, only_tests);

        let mut entries: Vec<CandidateEntry> = Vec::new();
        let mut targets: Vec<ScanTarget> = Vec::new();
        let mut pool_count = 0usize;
        for (idx, node) in graph.nodes.iter().enumerate() {
            if node.is_test && !only_tests {
                continue;
            }
            if let Some(reason) = exclusion_of(node, idx, &interfaces, &inbound) {
                *reason.count_slot(&mut excluded) += 1;
                continue;
            }
            pool_count += 1;
            let acc = &inbound.per_node[idx];
            if acc.callers.len() != 1 {
                continue;
            }
            let Some((&caller_idx, calls)) = acc.callers.iter().next() else {
                continue;
            };
            let caller_node = &graph.nodes[caller_idx];
            entries.push(candidate_entry(node, caller_node, calls, acc));

            let span_of = |n: &CallGraphNode| (n.file.clone(), n.start_line, n.end_line);
            let mut allowed = vec![span_of(node), span_of(caller_node)];
            allowed.extend(acc.test_callers.iter().map(|&t| span_of(&graph.nodes[t])));
            targets.push(ScanTarget {
                name: node.name.clone(),
                allowed,
            });
        }

        Self {
            pool_count,
            excluded,
            entries,
            targets,
        }
    }
}

/// Bare-name occurrences per candidate, outside its allowed spans.
struct RawReferences {
    /// Parallel to [`SingleCallers::entries`].
    counts: Vec<usize>,
}

impl RawReferences {
    /// Tokenize every source file the graph scanned and count, per
    /// candidate, the occurrences of its bare name outside the spans
    /// its callers already account for. Only scanned files are
    /// searched, so a name reached from a config file, a template, or
    /// another language is invisible here — the same bound as `analyze
    /// unreachable`'s reference scan.
    fn run(
        builder: &CallGraphBuilder,
        roots: &AnalyzeRoots,
        targets: &[ScanTarget],
    ) -> Result<Self, AnalyzerError> {
        let mut counts = vec![0usize; targets.len()];
        if targets.is_empty() {
            return Ok(Self { counts });
        }
        let mut slots_by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut allowed_by_file: HashMap<&str, Vec<(usize, usize, usize)>> = HashMap::new();
        for (slot, target) in targets.iter().enumerate() {
            slots_by_name
                .entry(target.name.as_str())
                .or_default()
                .push(slot);
            for (file, start, end) in &target.allowed {
                allowed_by_file
                    .entry(file.as_str())
                    .or_default()
                    .push((*start, *end, slot));
            }
        }

        builder.visit_source_texts(roots, |file, source| {
            let allowed = allowed_by_file.get(file).map(Vec::as_slice);
            for (offset, line) in source.lines().enumerate() {
                let line_no = offset + 1;
                for token in identifiers(line) {
                    let Some(slots) = slots_by_name.get(token) else {
                        continue;
                    };
                    for &slot in slots {
                        let accounted = allowed.is_some_and(|spans| {
                            spans.iter().any(|&(start, end, s)| {
                                s == slot && start <= line_no && line_no <= end
                            })
                        });
                        if !accounted {
                            counts[slot] += 1;
                        }
                    }
                }
            }
        })?;
        Ok(Self { counts })
    }
}

impl Report {
    fn build(
        roots: &AnalyzeRoots,
        graph: &CallGraph,
        collected: SingleCallers,
        raw: &RawReferences,
        thresholds: Thresholds,
    ) -> Self {
        let SingleCallers {
            pool_count,
            excluded,
            mut entries,
            targets: _,
        } = collected;
        for (entry, &count) in entries.iter_mut().zip(&raw.counts) {
            if count > 0 {
                entry.raw_reference_count = count;
                entry.caveats.push(Caveat::RawReference);
                entry.caveats.sort_unstable();
            }
        }
        let single_callers = entries;

        let calibration = Calibration {
            single_caller_count: single_callers.len(),
            within_thresholds_count: 0,
            loc: MetricDistribution::build(single_callers.iter().map(|e| e.loc as u64).collect()),
            cyclomatic: MetricDistribution::build(
                single_callers
                    .iter()
                    .filter_map(|e| e.cyclomatic_complexity.map(u64::from))
                    .collect(),
            ),
        };

        let mut candidates: Vec<CandidateEntry> = single_callers
            .into_iter()
            .filter(|e| {
                e.loc <= thresholds.max_loc
                    && e.cyclomatic_complexity
                        .is_none_or(|c| c <= thresholds.max_cyclomatic)
            })
            .collect();
        candidates.sort_by(|a, b| {
            a.caveats
                .len()
                .cmp(&b.caveats.len())
                .then_with(|| a.loc.cmp(&b.loc))
                .then_with(|| a.id.cmp(&b.id))
        });

        Self {
            schema_version: SCHEMA_VERSION,
            root: roots.display(),
            language: graph.language,
            note: NOTE,
            node_count: graph.nodes.len(),
            candidate_pool_count: pool_count,
            thresholds,
            excluded,
            calibration: Calibration {
                within_thresholds_count: candidates.len(),
                ..calibration
            },
            candidates,
            modules: graph.module_summary.clone(),
        }
    }
}

/// Per-callee inbound facts off the resolved (and ambiguous) edge sets.
struct InboundCalls {
    per_node: Vec<InboundAccumulator>,
}

#[derive(Debug, Default, Clone)]
struct InboundAccumulator {
    /// Production callers (or all callers under `--only-tests`), with
    /// the aggregated call sites from each.
    callers: BTreeMap<usize, CallsFromCaller>,
    /// Distinct test callers, kept out of `callers` so a test cannot
    /// turn a single-caller function into a two-caller one. Their spans
    /// are also what lets the raw-reference scan ignore test call
    /// sites it already knows about.
    test_callers: BTreeSet<usize>,
    /// Ambiguous call sites naming this node as a candidate target.
    ambiguous_inbound_count: usize,
    /// This node calls itself on a resolved edge.
    self_recursive: bool,
}

#[derive(Debug, Default, Clone)]
struct CallsFromCaller {
    call_count: usize,
    call_lines: Vec<usize>,
    /// Any contributing edge resolved through the last-segment fallback
    /// family, making the caller attribution itself heuristic.
    fallback_resolved: bool,
}

impl InboundCalls {
    fn collect(graph: &CallGraph, only_tests: bool) -> Self {
        let index_by_id = graph.node_index_by_id();
        let mut per_node = vec![InboundAccumulator::default(); graph.nodes.len()];
        for edge in &graph.edges {
            if edge.resolution == Resolution::Ambiguous {
                for candidate in &edge.candidates {
                    if let Some(&to) = index_by_id.get(candidate.as_str()) {
                        per_node[to].ambiguous_inbound_count += edge.call_count;
                    }
                }
                continue;
            }
            if edge.resolution != Resolution::Resolved {
                continue;
            }
            let (Some(from), Some(to)) = (edge.from.as_deref(), edge.to.as_deref()) else {
                continue;
            };
            let (Some(&from), Some(&to)) = (index_by_id.get(from), index_by_id.get(to)) else {
                continue;
            };
            if from == to {
                per_node[to].self_recursive = true;
                continue;
            }
            // Mirrors `analyze hubs`: outside `--only-tests`, a test
            // caller is informational and never a production caller.
            if graph.nodes[from].is_test && !only_tests {
                per_node[to].test_callers.insert(from);
                continue;
            }
            let calls = per_node[to].callers.entry(from).or_default();
            calls.call_count += edge.call_count;
            calls.call_lines.extend(&edge.call_lines);
            calls.fallback_resolved |= matches!(
                edge.resolution_method,
                Some(
                    ResolutionMethod::LastSegment
                        | ResolutionMethod::PathSuffix
                        | ResolutionMethod::CrateNarrowed
                )
            );
        }
        for acc in &mut per_node {
            for calls in acc.callers.values_mut() {
                calls.call_lines.sort_unstable();
                calls.call_lines.dedup();
            }
        }
        Self { per_node }
    }
}

/// Why a node is never a candidate, however many callers it has.
#[derive(Debug, Clone, Copy)]
enum Exclusion {
    TraitMethod,
    InterfaceMethod,
    Annotated,
    Recursive,
}

impl Exclusion {
    fn count_slot(self, excluded: &mut Excluded) -> &mut usize {
        match self {
            Self::TraitMethod => &mut excluded.trait_method_count,
            Self::InterfaceMethod => &mut excluded.interface_method_count,
            Self::Annotated => &mut excluded.annotated_count,
            Self::Recursive => &mut excluded.recursive_count,
        }
    }
}

fn exclusion_of(
    node: &CallGraphNode,
    idx: usize,
    interfaces: &InterfaceIndex,
    inbound: &InboundCalls,
) -> Option<Exclusion> {
    if matches!(
        node.owner_kind,
        Some(lens_domain::OwnerKind::TraitImpl | lens_domain::OwnerKind::Trait)
    ) {
        return Some(Exclusion::TraitMethod);
    }
    let lang = ExportLang::of(node);
    if lang == Some(ExportLang::Go) && !interfaces.matching(node, ExportLang::Go).is_empty() {
        return Some(Exclusion::InterfaceMethod);
    }
    // A known non-inert annotation may itself be a caller. `None`
    // (TypeScript, Python: no attribute extraction) is *not* excluded —
    // those rows already carry the unknown-visibility caveat, and
    // excluding on "could not tell" would empty both languages.
    if node.attributes.as_ref().is_some_and(|attributes| {
        let inert = node
            .graph_language()
            .unwrap_or(super::call_graph::model::GraphLanguage::TypeScript)
            .inert_attribute_names();
        attributes
            .iter()
            .any(|attribute| !inert.contains(attribute))
    }) {
        return Some(Exclusion::Annotated);
    }
    if inbound.per_node[idx].self_recursive {
        return Some(Exclusion::Recursive);
    }
    None
}

fn candidate_entry(
    node: &CallGraphNode,
    caller: &CallGraphNode,
    calls: &CallsFromCaller,
    acc: &InboundAccumulator,
) -> CandidateEntry {
    let mut caveats = Vec::new();
    match ExportLang::of(node) {
        Some(lang) if node.visibility != lang.private() => {
            caveats.push(Caveat::WiderThanPrivate);
        }
        Some(_) => {}
        None => caveats.push(Caveat::UnknownVisibility),
    }
    if !acc.test_callers.is_empty() {
        caveats.push(Caveat::TestCallers);
    }
    if calls.call_count > 1 {
        caveats.push(Caveat::MultipleCallSites);
    }
    if caller.module != node.module {
        caveats.push(Caveat::CrossModuleCaller);
    }
    if acc.ambiguous_inbound_count > 0 {
        caveats.push(Caveat::AmbiguousInbound);
    }
    if calls.fallback_resolved {
        caveats.push(Caveat::FallbackResolvedCall);
    }
    caveats.sort_unstable();
    CandidateEntry {
        id: node.id.clone(),
        qualified_name: node.qualified_name.clone(),
        file: node.file.clone(),
        start_line: node.start_line,
        end_line: node.end_line,
        module: node.module.clone(),
        loc: node.weights.loc,
        cyclomatic_complexity: node.weights.cyclomatic_complexity,
        visibility: node.visibility,
        caller: Caller {
            qualified_name: caller.qualified_name.clone(),
            file: caller.file.clone(),
            start_line: caller.start_line,
            module: caller.module.clone(),
        },
        call_count: calls.call_count,
        call_lines: calls.call_lines.clone(),
        test_fan_in: acc.test_callers.len(),
        raw_reference_count: 0,
        caveats,
    }
}

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    let mut out = format!(
        "# Single-use report: {} ({} function(s), {} single-caller, {} candidate(s))\n",
        report.root,
        report.candidate_pool_count,
        report.calibration.single_caller_count,
        report.candidates.len(),
    );
    let _ = writeln!(out, "\n{NOTE}\n");
    let _ = writeln!(
        out,
        "Thresholds: loc <= {}, cyclomatic <= {}. Excluded outright: {} trait method(s), \
         {} interface-matching method(s), {} annotated, {} self-recursive.",
        report.thresholds.max_loc,
        report.thresholds.max_cyclomatic,
        report.excluded.trait_method_count,
        report.excluded.interface_method_count,
        report.excluded.annotated_count,
        report.excluded.recursive_count,
    );
    if report.candidate_pool_count == 0 {
        out.push_str("\n_No functions to analyze._\n");
        return out;
    }

    let (clean, caveated): (Vec<_>, Vec<_>) =
        report.candidates.iter().partition(|e| e.caveats.is_empty());
    let _ = writeln!(out, "\n## Inline candidates (top {limit})");
    out.push_str(
        "\nPrivate, one resolved caller, one clean claim. Verify with a bare-name search, \
         then inline into the caller or say why the name earns its keep.\n",
    );
    render_entries(&mut out, &clean, limit);

    let _ = writeln!(out, "\n## Candidates with caveats (top {limit})");
    out.push_str(
        "\nSame single-caller shape, but the claim or the edit is weaker — each row says how.\n",
    );
    render_entries(&mut out, &caveated, limit);

    let _ = writeln!(out, "\n## Threshold calibration");
    let _ = writeln!(
        out,
        "\nOver all {} single-caller function(s), the current thresholds keep {}.",
        report.calibration.single_caller_count, report.calibration.within_thresholds_count,
    );
    render_distribution(&mut out, "loc", report.calibration.loc.as_ref());
    render_distribution(
        &mut out,
        "cyclomatic",
        report.calibration.cyclomatic.as_ref(),
    );
    out.push_str(
        "\nSet `--max-loc` / `--max-cyclomatic` (or `[profile.<name>.single-use]`) off these \
         percentiles for this repository's own appetite; p75 keeps the typical extraction, p90 \
         sweeps in most of the tail.\n",
    );

    render_module_confidence(
        &mut out,
        &report.modules,
        "Call sites in these modules often failed to resolve; a hidden caller is likeliest \
         there, so treat their rows with extra suspicion.",
    );
    out
}

fn render_entries(out: &mut String, entries: &[&CandidateEntry], limit: usize) {
    if entries.is_empty() {
        out.push_str("\n_None._\n");
        return;
    }
    out.push('\n');
    for entry in entries.iter().take(limit) {
        let cyclomatic = entry
            .cyclomatic_complexity
            .map_or_else(|| "-".to_owned(), |c| c.to_string());
        let _ = write!(
            out,
            "- `{}` ({}:{}): loc={}, cyclomatic={}, called by `{}` ({}:{})",
            entry.qualified_name,
            entry.file,
            entry.start_line,
            entry.loc,
            cyclomatic,
            entry.caller.qualified_name,
            entry.caller.file,
            entry
                .call_lines
                .first()
                .copied()
                .unwrap_or(entry.caller.start_line),
        );
        if entry.call_count > 1 {
            let _ = write!(out, " at {} sites", entry.call_count);
        }
        if entry.test_fan_in > 0 {
            let _ = write!(out, ", +{} test caller(s)", entry.test_fan_in);
        }
        if entry.raw_reference_count > 0 {
            let _ = write!(out, ", raw refs={}", entry.raw_reference_count);
        }
        if !entry.caveats.is_empty() {
            let caveats: Vec<&str> = entry.caveats.iter().map(|c| c.as_str()).collect();
            let _ = write!(out, " — {}", caveats.join("; "));
        }
        out.push('\n');
    }
}

fn render_distribution(out: &mut String, label: &str, distribution: Option<&MetricDistribution>) {
    match distribution {
        Some(d) => {
            let _ = writeln!(
                out,
                "- {label}: p50={}, p75={}, p90={}, max={}",
                d.p50, d.p75, d.p90, d.max,
            );
        }
        None => {
            let _ = writeln!(out, "- {label}: no values observed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use rstest::rstest;
    use serde_json::Value;
    use std::path::Path;

    fn analyze_json(path: &Path) -> Value {
        analyze_json_with(path, SingleUseAnalyzer::new())
    }

    fn analyze_json_with(path: &Path, analyzer: SingleUseAnalyzer) -> Value {
        let json = analyzer.analyze(path, OutputFormat::Json).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn candidate<'a>(report: &'a Value, name_suffix: &str) -> Option<&'a Value> {
        report["candidates"].as_array().unwrap().iter().find(|e| {
            e["qualified_name"]
                .as_str()
                .is_some_and(|q| q.ends_with(name_suffix))
        })
    }

    #[test]
    fn a_private_single_caller_helper_is_a_clean_candidate() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn helper() { let _ = 1; }\n\
             pub fn caller() { helper(); }\n\
             pub fn other() { caller(); }\n",
        );

        let report = analyze_json(dir.path());
        let entry = candidate(&report, "::helper").expect("helper listed");
        assert_eq!(entry["caveats"], serde_json::json!([]));
        assert_eq!(entry["caller"]["qualified_name"], "crate::caller");
        assert_eq!(entry["call_count"], 1);
        assert_eq!(entry["call_lines"], serde_json::json!([2]));
        // `caller` has one caller too, but it is `pub`.
        let caller_entry = candidate(&report, "::caller").expect("caller listed");
        assert!(
            caller_entry["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "wider_than_private"),
            "got {caller_entry:?}",
        );
    }

    #[test]
    fn multi_caller_and_uncalled_functions_are_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn shared() {}\n\
             fn a() { shared(); }\n\
             fn b() { shared(); }\n\
             fn orphan() {}\n",
        );

        let report = analyze_json(dir.path());
        assert!(candidate(&report, "::shared").is_none());
        assert!(candidate(&report, "::orphan").is_none());
        // `a` and `b` themselves have no caller, so nothing qualifies.
        assert_eq!(report["candidates"].as_array().unwrap().len(), 0);
        assert_eq!(report["calibration"]["single_caller_count"], 0);
    }

    #[rstest]
    #[case::loc(SingleUseAnalyzer::new().with_max_loc(Some(3)), "max_loc")]
    #[case::cyclomatic(
        SingleUseAnalyzer::new().with_max_cyclomatic(Some(1)),
        "max_cyclomatic"
    )]
    fn a_threshold_cuts_the_candidate_but_not_the_calibration(
        #[case] analyzer: SingleUseAnalyzer,
        #[case] which: &str,
    ) {
        let dir = tempfile::tempdir().unwrap();
        // 5 loc, cyclomatic 3 (two ifs): over a max_loc of 3 and over a
        // max_cyclomatic of 1.
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn big(x: bool, y: bool) -> usize {\n\
                 let mut n = 0;\n\
                 if x { n += 1; }\n\
                 if y { n += 1; }\n\
                 n\n\
             }\n\
             pub fn caller() { big(true, false); }\n",
        );

        let report = analyze_json_with(dir.path(), analyzer);
        assert!(
            candidate(&report, "::big").is_none(),
            "{which} must cut the row: {report:?}",
        );
        assert_eq!(report["calibration"]["single_caller_count"], 1);
        assert_eq!(report["calibration"]["within_thresholds_count"], 0);
    }

    #[test]
    fn default_thresholds_keep_a_small_helper() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn small() { let _ = 1; }\npub fn caller() { small(); }\n",
        );

        let report = analyze_json(dir.path());
        assert!(candidate(&report, "::small").is_some());
        assert_eq!(report["thresholds"]["max_loc"], DEFAULT_MAX_LOC);
        assert_eq!(
            report["thresholds"]["max_cyclomatic"],
            DEFAULT_MAX_CYCLOMATIC
        );
    }

    #[test]
    fn trait_impl_methods_and_defaults_are_excluded() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "trait Greet { fn hi(&self) -> usize { 1 } }\n\
             struct S;\n\
             impl Greet for S { fn hi(&self) -> usize { 2 } }\n\
             pub fn caller(s: S) { Greet::hi(&s); }\n",
        );

        let report = analyze_json(dir.path());
        assert!(candidate(&report, "::hi").is_none());
        assert_eq!(report["excluded"]["trait_method_count"], 2);
    }

    #[test]
    fn a_live_annotation_excludes_the_function() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "#[no_mangle]\nfn hooked() {}\n\
             pub fn caller() { hooked(); }\n",
        );

        let report = analyze_json(dir.path());
        assert!(candidate(&report, "::hooked").is_none());
        assert_eq!(report["excluded"]["annotated_count"], 1);
    }

    #[test]
    fn an_inert_annotation_does_not_exclude() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "#[inline]\nfn hinted() {}\n\
             pub fn caller() { hinted(); }\n",
        );

        let report = analyze_json(dir.path());
        assert!(candidate(&report, "::hinted").is_some());
        assert_eq!(report["excluded"]["annotated_count"], 0);
    }

    #[test]
    fn a_self_recursive_function_is_excluded() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn walker(n: usize) { if n > 0 { walker(n - 1); } }\n\
             pub fn caller() { walker(3); }\n",
        );

        let report = analyze_json(dir.path());
        assert!(candidate(&report, "::walker").is_none());
        assert_eq!(report["excluded"]["recursive_count"], 1);
    }

    #[test]
    fn test_callers_do_not_add_to_fan_in_but_flag_the_seam() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn seam() {}\n\
             pub fn caller() { seam(); }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 fn t1() { crate::seam(); }\n\
                 fn t2() { crate::seam(); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let entry = candidate(&report, "::seam").expect("still single-caller");
        assert_eq!(entry["test_fan_in"], 2);
        assert!(
            entry["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "test_callers"),
            "got {entry:?}",
        );
        // The test callers' spans are known, so their call sites are
        // not raw references on top of the test-seam caveat.
        assert_eq!(entry["raw_reference_count"], 0);
    }

    #[test]
    fn a_caller_hidden_in_a_macro_body_demotes_with_raw_reference() {
        // `format!` arguments produce no call edge, so `shorten` looks
        // single-caller to the graph; the raw-name scan is what says
        // otherwise.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn shorten(s: &str) -> &str { s }\n\
             pub fn caller(s: &str) { let _ = shorten(s); }\n\
             pub fn formats(s: &str) -> String { format!(\"x {}\", shorten(s)) }\n",
        );

        let report = analyze_json(dir.path());
        let entry = candidate(&report, "::shorten").expect("graph sees one caller");
        assert_eq!(entry["raw_reference_count"], 1);
        assert_eq!(entry["caveats"], serde_json::json!(["raw_reference"]));
    }

    #[test]
    fn mentions_inside_the_definition_and_its_caller_do_not_demote() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn tidy() {\n\
                 let _ = 1;\n\
             }\n\
             pub fn caller() {\n\
                 // tidy folds into here\n\
                 tidy();\n\
             }\n\
             pub fn other() { caller(); }\n",
        );

        let report = analyze_json(dir.path());
        let entry = candidate(&report, "::tidy").expect("listed");
        assert_eq!(entry["raw_reference_count"], 0);
        assert_eq!(entry["caveats"], serde_json::json!([]));
    }

    #[test]
    fn candidates_sharing_a_name_caveat_each_other() {
        // The scan is textual: each `helper` sees the other module's
        // definition and call site as raw references. A shared name
        // cannot be verified textually, so both rows demote.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { fn helper() {} pub fn go() { helper(); } }\n\
             mod b { fn helper() {} pub fn go() { helper(); } }\n\
             pub fn root() { a::go(); b::go(); }\n",
        );

        let report = analyze_json(dir.path());
        for name in ["a::helper", "b::helper"] {
            let entry = candidate(&report, name).expect("listed");
            assert_eq!(entry["raw_reference_count"], 2, "for {name}: {entry:?}");
            assert!(
                entry["caveats"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|c| c == "raw_reference"),
                "for {name}: {entry:?}",
            );
        }
    }

    #[test]
    fn markdown_carries_the_raw_reference_count() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn shorten(s: &str) -> &str { s }\n\
             pub fn caller(s: &str) { let _ = shorten(s); }\n\
             pub fn formats(s: &str) -> String { format!(\"x {}\", shorten(s)) }\n",
        );

        let md = SingleUseAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("raw refs=1"), "got: {md}");
        assert!(
            md.contains("its bare name is written elsewhere"),
            "got: {md}"
        );
    }

    #[test]
    fn several_call_sites_in_the_one_caller_are_a_caveat() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn twice() {}\n\
             pub fn caller() { twice(); twice(); }\n",
        );

        let report = analyze_json(dir.path());
        let entry = candidate(&report, "::twice").expect("listed");
        assert_eq!(entry["call_count"], 2);
        assert!(
            entry["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "multiple_call_sites"),
            "got {entry:?}",
        );
    }

    #[test]
    fn a_cross_module_caller_is_a_caveat() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { pub(crate) fn helper() {} }\n\
             mod b { pub fn caller() { crate::a::helper(); } }\n",
        );

        let report = analyze_json(dir.path());
        let entry = candidate(&report, "a::helper").expect("listed");
        let caveats = entry["caveats"].as_array().unwrap();
        assert!(
            caveats.iter().any(|c| c == "cross_module_caller"),
            "got {entry:?}",
        );
        // pub(crate) is wider than private, so that caveat rides along.
        assert!(
            caveats.iter().any(|c| c == "wider_than_private"),
            "got {entry:?}",
        );
    }

    #[test]
    fn clean_candidates_rank_before_caveated_ones() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn wide() {}\n\
             fn narrow() { let _ = 1; }\n\
             pub fn caller() { wide(); narrow(); }\n\
             pub fn top() { caller(); }\n",
        );

        let report = analyze_json(dir.path());
        let names: Vec<&str> = report["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["qualified_name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names.first().copied(),
            Some("crate::narrow"),
            "caveat-free first: {names:?}",
        );
    }

    #[test]
    fn calibration_reports_the_single_caller_distribution() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn tiny() {}\n\
             fn medium() { let _ = 1; let _ = 2; let _ = 3; }\n\
             pub fn caller() { tiny(); medium(); }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["calibration"]["single_caller_count"], 2);
        let loc = &report["calibration"]["loc"];
        assert!(loc["p50"].as_u64().unwrap() >= 1, "got {loc:?}");
        assert!(
            loc["max"].as_u64().unwrap() >= loc["p50"].as_u64().unwrap(),
            "got {loc:?}",
        );
    }

    #[test]
    fn analysis_is_deterministic_across_runs() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn h1() {}\nfn h2() {}\npub fn caller() { h1(); h2(); }\n",
        );

        let analyzer = SingleUseAnalyzer::new();
        let a = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        let b = analyzer.analyze(dir.path(), OutputFormat::Json).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn only_tests_mode_reports_test_helpers() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn prod() {}\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 fn helper() {}\n\
                 fn case() { helper(); }\n\
             }\n",
        );

        let report = analyze_json_with(dir.path(), SingleUseAnalyzer::new().with_only_tests(true));
        let entry = candidate(&report, "::helper").expect("test helper listed");
        assert_eq!(entry["caller"]["qualified_name"], "crate::tests::case");
    }

    #[test]
    fn markdown_states_direction_and_calibration() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn helper() {}\npub fn caller() { helper(); }\n",
        );

        let md = SingleUseAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Single-use report:"), "got: {md}");
        // Agents act on wording literally: the row must read as a
        // candidate edit to verify, never as a verdict.
        assert!(md.contains("not a verdict"), "got: {md}");
        assert!(md.contains("bare name"), "got: {md}");
        assert!(md.contains("## Threshold calibration"), "got: {md}");
        // A clean row must not render a zero raw-reference count.
        assert!(!md.contains("raw refs"), "got: {md}");
        assert!(md.contains("`crate::helper`"), "got: {md}");
    }

    #[test]
    fn markdown_reports_empty_input_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "README.md", "no source here\n");

        let md = SingleUseAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("_No functions to analyze._"), "got: {md}");
    }

    #[rstest]
    #[case::empty(vec![], None)]
    #[case::single(vec![7], Some((7, 7, 7, 7)))]
    #[case::spread(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10], Some((5, 8, 9, 10)))]
    fn metric_distribution_uses_nearest_rank(
        #[case] values: Vec<u64>,
        #[case] expected: Option<(u64, u64, u64, u64)>,
    ) {
        let d = MetricDistribution::build(values);
        assert_eq!(d.map(|d| (d.p50, d.p75, d.p90, d.max)), expected,);
    }
}
