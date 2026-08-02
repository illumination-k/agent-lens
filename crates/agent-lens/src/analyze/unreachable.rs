//! `analyze unreachable` — functions no call path from an entry point
//! reaches, reported in confidence tiers.
//!
//! Dead code is expensive for an agent twice over: it reads it, and then
//! it preserves it, because nothing in the tree gives it permission to
//! delete. This analyzer produces that permission where it can be
//! defended, and says so plainly where it cannot.
//!
//! The pipeline is four passes over the shared call graph:
//!
//! 1. **Entry set.** Per language: `main`, Go `init`, test functions,
//!    `pub` / exported declarations, and anything carrying an annotation
//!    that could register it with machinery no call site names. Every
//!    verdict below is relative to this set, so the report emits it.
//! 2. **Multi-source BFS** forward over *resolved* edges. The complement
//!    is the candidate set.
//! 3. **Raw identifier-reference scan.** Every source file the graph
//!    scanned is tokenized, and a candidate whose bare name appears
//!    anywhere outside the candidate spans — in an expression the parser
//!    did not attribute, a macro body, a string, a doc comment — is
//!    demoted. This is the load-bearing pass: it is what covers the
//!    calls a syntax-only graph cannot see.
//! 4. **Tiers.** `confirmed` (private, unreachable, unreferenced, no
//!    caveat), `likely` (nothing in the tree uses it, but the
//!    declaration reaches outside the tree), `unknown` (unreachable, but
//!    something could reach it in a way this analyzer does not model).
//!
//! The design rule is the asymmetry `go vet`'s `deadcode` uses:
//! **sound in the "reported as confirmed ⇒ really dead" direction**,
//! at the cost of missing dead code. A single false "safe to delete"
//! costs more than every finding it would have bought, so each of these
//! demotes rather than decides:
//!
//! - A Rust trait `impl` method, or a trait's own default body: the call
//!   site names the trait, and receiver-typed dispatch is invisible to a
//!   syntax-only resolver.
//! - A Go method matching an interface declared in the analyzed tree by
//!   name and parameter count — the same annotation `analyze visibility`
//!   makes, for the same reason.
//! - Any annotation not on the language's inert list (`#[no_mangle]`,
//!   `#[tokio::main]`, `//go:linkname`, a cgo `//export`): the
//!   annotation itself may be the caller.
//! - An ambiguous call site in reachable code whose candidate set names
//!   the function.
//! - A raw reference from code this analyzer has not itself confirmed
//!   dead, which propagates: if what mentions you may be alive, so may
//!   you.
//!
//! Scope is Rust and Go, the two adapters that extract export status —
//! and for the `confirmed` tier specifically, Rust `private` and Go
//! unexported declarations, since anything wider can be reached from
//! outside the analyzed path. TypeScript and Python functions are
//! counted, treated as live, and never judged.
//!
//! Islands — clusters of unreachable functions that only call each other
//! — are reported separately with their total LOC and a deletion order,
//! because a cluster is both the strongest signal that a feature was
//! abandoned and the cheapest thing to remove in one go.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use super::call_graph::algo::{self, bfs};
use super::call_graph::model::{
    CallGraphNode, GraphLanguage, ModuleResolutionSummary, NodeVisibility, Resolution,
};
use super::call_graph::{CallGraph, CallGraphBuilder, delegate_call_graph_builders};
use super::export_lang::{ExportLang, InterfaceIndex};
use super::format::{ModuleSection, render_module_confidence, render_module_sections};
use super::options::analyzer_options;
use super::runner::render_report;
use super::{AnalyzerError, OutputFormat};

const SCHEMA_VERSION: u32 = 1;

/// Module sections rendered in markdown when `--top` is not given. JSON
/// always carries every module.
const DEFAULT_TOP: usize = 20;

/// Functions listed per module section in markdown. JSON carries all of
/// them.
const FUNCTIONS_PER_MODULE: usize = 10;

/// Islands listed in markdown. JSON carries every one.
const ISLANDS_SHOWN: usize = 5;

/// Members named inline in an island's deletion order before the rest
/// are rolled into a count.
const ISLAND_MEMBERS_PER_ROW: usize = 6;

/// Interfaces named inline on an annotated row before the rest are
/// rolled into a count.
const INTERFACES_PER_ROW: usize = 2;

/// What the verdict means, stated in the output itself: an agent acting
/// on a row is about to delete code, so the bound on each tier travels
/// with it.
const NOTE: &str = "Relative to the entry set below, and sound in one direction only: a `confirmed` \
     row is a private/unexported function with no resolved call path from any entry, whose bare \
     name appears nowhere else in the scanned sources. Dead code this misses is expected; a \
     `confirmed` row that is actually live is a bug. `likely` means nothing in the analyzed path \
     uses it while the declaration still reaches outside that path — deleting one is a question \
     about consumers this analyzer cannot see. `unknown` means something could reach it in a way \
     a syntax-only graph does not model (trait or interface dispatch, an annotation, an ambiguous \
     call site, a raw name reference), so the row is a lead to check, never a verdict. Only files \
     the graph scanned are searched for references, so a name reached from a config file, a \
     template, or another language is invisible here.";

analyzer_options! {
    /// `analyze unreachable` flags, and the `[profile.<name>.unreachable]`
    /// table.
    pub struct UnreachableOptions {
        @shared(ranking);
        /// Lowest confidence tier to render in markdown: `confirmed`
        /// (default) leads with the deletable rows, `likely` adds the
        /// unused public surface, `unknown` adds every lead. JSON output
        /// always carries every tier.
        #[arg(long, value_enum)]
        pub tier: Option<Tier>,
    }
}

/// How much a row can be trusted. Declaration order is the ranking, and
/// doubles as the `--tier` cut: rendering `likely` renders `confirmed`
/// too.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    clap::ValueEnum,
    serde::Deserialize,
    Serialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Private, unreachable, unreferenced, no caveat: deletable on this
    /// evidence alone.
    Confirmed,
    /// Nothing in the analyzed path uses it, but the declaration is
    /// visible outside that path.
    Likely,
    /// Unreachable, with a reason to believe the graph cannot see how it
    /// is reached.
    Unknown,
}

impl Tier {
    fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Likely => "likely",
            Self::Unknown => "unknown",
        }
    }
}

/// Analyzer entry point for `analyze unreachable`.
#[derive(Debug, Default, Clone)]
pub struct UnreachableAnalyzer {
    builder: CallGraphBuilder,
    top: Option<usize>,
    tier: Option<Tier>,
    /// Mirrored from the path filter: dropping test files removes both
    /// the test entry points and the references their bodies hold, so
    /// the report has to say it happened.
    exclude_tests: bool,
}

impl UnreachableAnalyzer {
    /// Apply a whole [`UnreachableOptions`] group. The CLI flags and the
    /// `[profile.<name>.unreachable]` table are the same type, so this is
    /// the only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: UnreachableOptions) -> Self {
        self.with_top(opts.top).with_tier(opts.tier)
    }

    pub fn new() -> Self {
        Self::default()
    }

    delegate_call_graph_builders! {
        builder,
        /// Accepted for CLI uniformity. Test functions are entry points,
        /// so keeping only them makes every function in the graph an
        /// entry and leaves nothing to report.
        only_tests,
        /// Drops test files, and with them both the entry points tests
        /// provide and the references their bodies hold. A function used
        /// only by tests then looks unreachable, so the report says the
        /// entry set was cut rather than presenting the result as
        /// evidence.
        exclude_tests => exclude_tests,
    }

    /// Cap the markdown module sections to the top-N entries. JSON
    /// output always carries every row.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    /// Lowest tier markdown renders. JSON is unaffected.
    pub fn with_tier(mut self, tier: Option<Tier>) -> Self {
        self.tier = tier;
        self
    }

    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        // Interface method sets are what keeps a Go method whose calls
        // dispatch through an interface out of the confirmed tier, so
        // this analyzer always pays for their extraction.
        let graph = self
            .builder
            .clone()
            .with_interface_facts(true)
            .build(path)?;
        let reach = Reachability::compute(&graph);
        let scan = ReferenceScan::run(&self.builder, path, &graph, &reach.candidates)?;
        let report = Report::build(path, &graph, &reach, &scan, self.exclude_tests);
        render_report(&report, format, || {
            format_markdown(&report, self.top, self.tier)
        })
    }
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    /// What every verdict on this report is relative to.
    note: &'static str,
    entries: EntrySet,
    audit: Audit,
    /// Modules holding at least one finding, most confirmed LOC first.
    modules: Vec<ModuleGroup>,
    /// Clusters of unreachable functions that only call each other,
    /// largest first.
    islands: Vec<Island>,
    bounds: Bounds,
    /// Per-module call-site resolution counts — the calibration layer: a
    /// module whose call sites mostly failed to resolve contributes call
    /// paths this analyzer never walked.
    resolution: Vec<ModuleResolutionSummary>,
    summary: Summary,
}

/// The traversal's starting set. Every "unreachable" verdict is relative
/// to it, so it is emitted rather than assumed.
#[derive(Debug, Serialize)]
struct EntrySet {
    function_count: usize,
    /// Entry counts by why the function is one, largest first.
    kinds: Vec<EntryKindCount>,
    /// No entry point reached the graph at all. Every function is then
    /// unreachable by construction, not by evidence.
    absent: bool,
    /// `--exclude-tests` was in force: test entry points and the
    /// references test bodies hold are both missing from this run.
    tests_excluded: bool,
}

#[derive(Debug, Serialize)]
struct EntryKindCount {
    kind: EntryKind,
    count: usize,
}

/// Why a function is an entry point. Declaration order is the priority
/// when several apply, most specific first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum EntryKind {
    /// A `main` function: the program starts here.
    Main,
    /// A Go `init` function: the runtime calls it before `main`.
    Init,
    /// A test function. Tests are code that is kept, so they root the
    /// traversal.
    Test,
    /// Carries an annotation that is not on the language's inert list,
    /// so the annotation itself may be what calls it.
    Annotated,
    /// Rust `pub`: reachable from outside the analyzed path.
    Public,
    /// Go exported (initial capital): same.
    Exported,
    /// A language whose adapter extracts no export status (TypeScript,
    /// Python). Not judged, and treated as live so nothing it calls is
    /// reported.
    UnjudgedLanguage,
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Init => "init",
            Self::Test => "test",
            Self::Annotated => "annotated",
            Self::Public => "public",
            Self::Exported => "exported",
            Self::UnjudgedLanguage => "unjudged-language",
        }
    }

    /// Whether a function that is an entry only for this reason is worth
    /// reporting when nothing in the tree calls it. A `main` is supposed
    /// to have no caller; a `pub fn` nothing uses is a finding.
    fn reportable_without_callers(self) -> bool {
        matches!(self, Self::Public | Self::Exported | Self::Annotated)
    }
}

/// What was examined, so the counts below have a denominator.
#[derive(Debug, Serialize)]
struct Audit {
    /// Non-test Rust and Go functions in scope.
    judged_function_count: usize,
    /// Functions skipped because their language carries no export
    /// status: the TypeScript and Python adapters extract none. They are
    /// treated as entry points, so what they call is reachable.
    unjudged_function_count: usize,
    /// Source files tokenized for the raw-reference scan.
    reference_scan_file_count: usize,
}

/// One module's findings.
#[derive(Debug, Serialize)]
struct ModuleGroup {
    module: String,
    finding_count: usize,
    confirmed_count: usize,
    /// Source lines held by this module's confirmed findings — the
    /// ranking key, since that is the part an agent may delete.
    confirmed_loc: usize,
    findings: Vec<Finding>,
}

#[derive(Debug, Serialize)]
struct Finding {
    id: String,
    qualified_name: String,
    file: String,
    start_line: usize,
    end_line: usize,
    loc: usize,
    visibility: NodeVisibility,
    tier: Tier,
    kind: FindingKind,
    /// Why the row is not `confirmed`. Empty on a confirmed row.
    demoted_by: Vec<Demotion>,
    /// Occurrences of the function's bare name outside every candidate
    /// span — in live code, a macro body, a string, a comment.
    raw_reference_count: usize,
    /// Ambiguous call sites in reachable code whose candidate set names
    /// this function.
    ambiguous_inbound_count: usize,
    /// Distinct resolved callers. Non-zero only for an unreachable
    /// function, whose callers are themselves unreachable.
    caller_count: usize,
    /// Index into the report's `islands`, when this function belongs to
    /// a cluster that only calls itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    island: Option<usize>,
    /// Interfaces declared in the analyzed tree whose method set names
    /// this method — same name, same parameter count.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    may_satisfy_interfaces: Vec<String>,
}

/// Which question a row answers. The two are not the same edit: one is
/// unreachable code, the other is a surface nothing in the tree uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum FindingKind {
    /// No call path from any entry point reaches it.
    Unreachable,
    /// It *is* an entry point — a public, exported, or annotated
    /// declaration — and nothing in the analyzed path calls it.
    UncalledEntry,
}

/// A reason the analyzer cannot confirm a function is dead. Each is a
/// way a call can exist that the resolved call graph does not record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Demotion {
    /// The bare name appears in a source file outside every candidate
    /// span, or inside a candidate this analyzer has not confirmed dead.
    RawReference,
    /// An ambiguous call site in reachable code names it as a candidate.
    AmbiguousCall,
    /// A Rust trait `impl` method or trait default body: callers name
    /// the trait.
    TraitMethod,
    /// A Go method matching an interface declared in the analyzed tree.
    InterfaceMethod,
    /// It carries an annotation that is not on the language's inert
    /// list.
    Annotation,
}

impl Demotion {
    fn as_str(self) -> &'static str {
        match self {
            Self::RawReference => "its name is written elsewhere",
            Self::AmbiguousCall => "an ambiguous call site names it",
            Self::TraitMethod => "trait method: callers name the trait",
            Self::InterfaceMethod => "may satisfy an in-scope interface",
            Self::Annotation => "annotated: the annotation may call it",
        }
    }
}

/// A cluster of unreachable functions that only call each other.
#[derive(Debug, Serialize)]
struct Island {
    id: usize,
    function_count: usize,
    confirmed_count: usize,
    loc: usize,
    /// Modules the members live in, sorted.
    modules: Vec<String>,
    /// Node ids in an order where each member precedes everything it
    /// calls, so removing them in this order never leaves a call to a
    /// member that is already gone. Members of one mutually recursive
    /// group are adjacent and have to go together.
    deletion_order: Vec<String>,
}

/// The directions in which this listing is wrong, quantified.
#[derive(Debug, Serialize)]
struct Bounds {
    /// Call sites in reachable code whose callee did not resolve to any
    /// function. Each could reach a listed function.
    unresolved_call_count_in_reached: usize,
    /// Call sites in reachable code that resolved to several candidates
    /// and were therefore not traversed.
    ambiguous_call_count_in_reached: usize,
    /// Call sites the graph could not attribute to an enclosing function
    /// (top-level and module-initialisation code). They are invisible to
    /// the traversal in both directions.
    caller_unattributed_call_count: usize,
    /// Findings demoted out of the confirmed tier — the measure of how
    /// much this run could not decide.
    demoted_function_count: usize,
    /// Those demotions by reason, most common first. A run with no
    /// confirmed rows is explained here: which kind of call the graph
    /// could not account for is the thing to fix or to check by hand.
    demotions: Vec<DemotionCount>,
}

#[derive(Debug, Serialize)]
struct DemotionCount {
    reason: Demotion,
    count: usize,
}

#[derive(Debug, Serialize)]
struct Summary {
    /// Of the judged functions, how many the traversal reached. An
    /// entry point counts as reaching itself.
    reached_function_count: usize,
    confirmed_count: usize,
    likely_count: usize,
    unknown_count: usize,
    /// Source lines held by the confirmed findings.
    confirmed_loc: usize,
    /// `confirmed_count` over the judged functions, 0.0 when nothing was
    /// judged.
    confirmed_share: f64,
    island_count: usize,
    /// Modules holding at least one finding.
    module_count: usize,
}

/// Entry points, what they reach, and the two candidate sets the tiers
/// are drawn from.
struct Reachability {
    /// Why each node is an entry point, `None` when it is not one.
    entry_kind: Vec<Option<EntryKind>>,
    reached: Vec<bool>,
    /// Every node that may end up on the report, in node order: the
    /// unreachable ones plus the entry points nothing calls.
    candidates: Vec<Candidate>,
}

#[derive(Debug, Clone, Copy)]
struct Candidate {
    node: usize,
    lang: ExportLang,
    kind: FindingKind,
}

impl Reachability {
    fn compute(graph: &CallGraph) -> Self {
        let entry_kind: Vec<Option<EntryKind>> =
            graph.nodes.iter().map(entry_kind_of).collect::<Vec<_>>();
        let roots: Vec<usize> = entry_kind
            .iter()
            .enumerate()
            .filter_map(|(idx, kind)| kind.map(|_| idx))
            .collect();

        let adjacency = graph.resolved_adjacency();
        let mut reached = vec![false; graph.nodes.len()];
        for visit in bfs(&adjacency, &roots) {
            reached[visit.node] = true;
        }

        let callers = graph.resolved_callers();
        let candidates = graph
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(idx, node)| {
                let lang = ExportLang::of(node)?;
                if node.is_test {
                    return None;
                }
                let kind = match entry_kind[idx] {
                    None if !reached[idx] => FindingKind::Unreachable,
                    Some(kind)
                        if kind.reportable_without_callers()
                            && callers.get(&idx).is_none_or(BTreeSet::is_empty) =>
                    {
                        FindingKind::UncalledEntry
                    }
                    _ => return None,
                };
                Some(Candidate {
                    node: idx,
                    lang,
                    kind,
                })
            })
            .collect();

        Self {
            entry_kind,
            reached,
            candidates,
        }
    }
}

/// Why this node roots the traversal, if it does. The first reason that
/// applies wins, so the emitted entry set reads as "what kind of entry
/// point is this" rather than "which rule fired last".
fn entry_kind_of(node: &CallGraphNode) -> Option<EntryKind> {
    let Some(lang) = ExportLang::of(node) else {
        // No export status extracted for this language, so it cannot be
        // judged — and treating it as live is what keeps everything it
        // calls off the report.
        return Some(EntryKind::UnjudgedLanguage);
    };
    if node.is_test {
        return Some(EntryKind::Test);
    }
    if node.impl_owner.is_none() {
        match (lang, node.name.as_str()) {
            (_, "main") => return Some(EntryKind::Main),
            (ExportLang::Go, "init") => return Some(EntryKind::Init),
            _ => {}
        }
    }
    if has_live_annotation(node) {
        return Some(EntryKind::Annotated);
    }
    let exported = match lang {
        ExportLang::Rust => EntryKind::Public,
        ExportLang::Go => EntryKind::Exported,
    };
    (node.visibility == lang.public()).then_some(exported)
}

/// Whether the node carries an annotation that could register it with
/// machinery no call site names. An adapter that extracts no annotations
/// answers `true`: "cannot tell" has to read as "there may be one", or
/// the tier would rest on a fact nobody established.
fn has_live_annotation(node: &CallGraphNode) -> bool {
    let inert = graph_language_of(node).inert_attribute_names();
    node.attributes.as_ref().is_none_or(|attributes| {
        attributes
            .iter()
            .any(|attribute| !inert.contains(attribute))
    })
}

/// The graph language of a node, from its file extension. Falls back to
/// TypeScript's (empty) tables for a path no language claims, which
/// cannot happen for a node the graph built.
fn graph_language_of(node: &CallGraphNode) -> GraphLanguage {
    super::SourceLang::from_path(Path::new(&node.file))
        .map(super::SourceLang::graph_language)
        .unwrap_or(GraphLanguage::TypeScript)
}

/// Where each candidate's bare name is written, across every source file
/// the graph scanned.
///
/// The distinction that matters is *who* wrote the name: a reference
/// from outside every candidate span is code this analyzer never
/// suspected, and it settles the question. A reference from inside
/// another candidate's body is only as trustworthy as that candidate,
/// which is why those are kept separately and resolved by propagation in
/// [`assign_tiers`] instead of counted here.
struct ReferenceScan {
    file_count: usize,
    /// Occurrences of a candidate name outside every candidate span,
    /// keyed by name (candidates sharing a name share the count — the
    /// scan is textual and cannot tell them apart).
    external: BTreeMap<String, usize>,
    /// `(candidate slot, name)` for every candidate name written inside
    /// that candidate's own span, minus its own name.
    internal: BTreeSet<(usize, String)>,
}

impl ReferenceScan {
    fn run(
        builder: &CallGraphBuilder,
        path: &Path,
        graph: &CallGraph,
        candidates: &[Candidate],
    ) -> Result<Self, AnalyzerError> {
        let mut names: HashSet<&str> = HashSet::new();
        let mut spans_by_file: HashMap<&str, Vec<(usize, usize, usize)>> = HashMap::new();
        for (slot, candidate) in candidates.iter().enumerate() {
            let node = &graph.nodes[candidate.node];
            names.insert(node.name.as_str());
            spans_by_file.entry(node.file.as_str()).or_default().push((
                node.start_line,
                node.end_line,
                slot,
            ));
        }

        let mut scan = Self {
            file_count: 0,
            external: BTreeMap::new(),
            internal: BTreeSet::new(),
        };
        if names.is_empty() {
            return Ok(scan);
        }
        let owner_names: Vec<&str> = candidates
            .iter()
            .map(|candidate| graph.nodes[candidate.node].name.as_str())
            .collect();

        scan.file_count = builder.visit_source_texts(path, |file, source| {
            let owner_of_line = line_owners(spans_by_file.get(file).map(Vec::as_slice));
            for (offset, line) in source.lines().enumerate() {
                let owner = owner_of_line.get(offset + 1).copied().flatten();
                for token in identifiers(line) {
                    let Some(&name) = names.get(token) else {
                        continue;
                    };
                    match owner {
                        // A candidate naming itself is its own
                        // declaration, or recursion: neither is evidence
                        // that anything else needs it.
                        Some(slot) if owner_names[slot] == name => {}
                        Some(slot) => {
                            scan.internal.insert((slot, name.to_owned()));
                        }
                        None => *scan.external.entry(name.to_owned()).or_default() += 1,
                    }
                }
            }
        })?;
        Ok(scan)
    }
}

/// Map 1-based source lines to the candidate whose span covers them.
/// Index 0 is unused so a line number indexes directly; lines past the
/// last candidate are absent, which reads the same as uncovered.
fn line_owners(spans: Option<&[(usize, usize, usize)]>) -> Vec<Option<usize>> {
    let spans = spans.unwrap_or_default();
    let last_line = spans.iter().map(|&(_, end, _)| end).max().unwrap_or(0);
    let mut owners = vec![None; last_line + 1];
    for &(start, end, slot) in spans {
        for owner in owners.iter_mut().take(end.min(last_line) + 1).skip(start) {
            *owner = Some(slot);
        }
    }
    owners
}

/// The identifier-shaped runs in one line of source: maximal runs of
/// letters, digits, and `_`.
///
/// Deliberately blind to syntax. A name inside a string, a comment, a
/// macro body, or an attribute argument is exactly what this scan exists
/// to find, and requiring the *whole* run to match keeps `foo` from
/// matching `foobar` — the one distinction it does have to make. The run
/// is Unicode-aware because the languages are: splitting `café` into
/// `caf` would hide every call to it, which is the one error direction
/// this analyzer cannot afford.
fn identifiers(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|token| !token.is_empty())
}

impl Report {
    fn build(
        root: &Path,
        graph: &CallGraph,
        reach: &Reachability,
        scan: &ReferenceScan,
        exclude_tests: bool,
    ) -> Self {
        let verdicts = assign_tiers(graph, reach, scan);
        let islands = islands(graph, reach, &verdicts);
        let island_of = island_membership(&islands);
        let findings = findings(graph, reach, &verdicts, &island_of);
        let modules = module_groups(graph, reach, findings);
        let edges = EdgeScan::run(graph, &reach.reached);

        let judged_function_count = graph
            .nodes
            .iter()
            .filter(|node| !node.is_test && ExportLang::of(node).is_some())
            .count();
        let reached_function_count = graph
            .nodes
            .iter()
            .enumerate()
            .filter(|(idx, node)| {
                reach.reached[*idx] && !node.is_test && ExportLang::of(node).is_some()
            })
            .count();
        let summary = summarize(
            &modules,
            &islands,
            judged_function_count,
            reached_function_count,
        );

        Self {
            schema_version: SCHEMA_VERSION,
            root: root.display().to_string(),
            language: graph.language,
            note: NOTE,
            entries: entry_set(reach, exclude_tests),
            audit: Audit {
                judged_function_count,
                unjudged_function_count: graph
                    .nodes
                    .iter()
                    .filter(|node| !node.is_test && ExportLang::of(node).is_none())
                    .count(),
                reference_scan_file_count: scan.file_count,
            },
            bounds: Bounds {
                unresolved_call_count_in_reached: edges.unresolved_in_reached,
                ambiguous_call_count_in_reached: edges.ambiguous_in_reached,
                caller_unattributed_call_count: edges.caller_unattributed,
                demoted_function_count: verdicts
                    .iter()
                    .filter(|verdict| !verdict.demoted_by.is_empty())
                    .count(),
                demotions: demotion_counts(&verdicts),
            },
            modules,
            islands,
            resolution: graph.module_summary.clone(),
            summary,
        }
    }
}

/// How often each demotion reason fired, most common first.
fn demotion_counts(verdicts: &[Verdict]) -> Vec<DemotionCount> {
    let mut counts: BTreeMap<Demotion, usize> = BTreeMap::new();
    for demotion in verdicts.iter().flat_map(|verdict| &verdict.demoted_by) {
        *counts.entry(*demotion).or_default() += 1;
    }
    let mut counts: Vec<DemotionCount> = counts
        .into_iter()
        .map(|(reason, count)| DemotionCount { reason, count })
        .collect();
    counts.sort_by_key(|entry| (Reverse(entry.count), entry.reason));
    counts
}

fn entry_set(reach: &Reachability, exclude_tests: bool) -> EntrySet {
    let mut counts: BTreeMap<EntryKind, usize> = BTreeMap::new();
    for kind in reach.entry_kind.iter().flatten() {
        *counts.entry(*kind).or_default() += 1;
    }
    let function_count = counts.values().sum();
    let mut kinds: Vec<EntryKindCount> = counts
        .into_iter()
        .map(|(kind, count)| EntryKindCount { kind, count })
        .collect();
    kinds.sort_by_key(|entry| (Reverse(entry.count), entry.kind));
    EntrySet {
        function_count,
        kinds,
        absent: function_count == 0,
        tests_excluded: exclude_tests,
    }
}

/// One candidate's tier and the evidence behind it.
struct Verdict {
    tier: Tier,
    demoted_by: BTreeSet<Demotion>,
    raw_reference_count: usize,
    ambiguous_inbound_count: usize,
    caller_count: usize,
    may_satisfy_interfaces: Vec<String>,
}

/// Tier every candidate, then propagate demotion along raw references.
///
/// The propagation is the part that cannot be done in one pass: a
/// reference from a candidate's body is evidence exactly when that
/// candidate might be alive, and whether it might be is what is being
/// computed. Starting from every candidate that is *not* confirmed and
/// following its references transitively reaches the fixpoint — a
/// confirmed row is then one that only code proven dead ever mentions.
fn assign_tiers(graph: &CallGraph, reach: &Reachability, scan: &ReferenceScan) -> Vec<Verdict> {
    let interfaces = InterfaceIndex::new(&graph.interfaces);
    let ambiguous_inbound = ambiguous_inbound_counts(graph, reach);
    let callers = graph.resolved_callers();

    let mut verdicts: Vec<Verdict> = reach
        .candidates
        .iter()
        .map(|candidate| {
            let node = &graph.nodes[candidate.node];
            let mut demoted_by = BTreeSet::new();
            let raw_reference_count = scan.external.get(&node.name).copied().unwrap_or_default();
            if raw_reference_count > 0 {
                demoted_by.insert(Demotion::RawReference);
            }
            let ambiguous_inbound_count = ambiguous_inbound
                .get(&candidate.node)
                .copied()
                .unwrap_or_default();
            if ambiguous_inbound_count > 0 {
                demoted_by.insert(Demotion::AmbiguousCall);
            }
            if is_trait_method(node) {
                demoted_by.insert(Demotion::TraitMethod);
            }
            let may_satisfy_interfaces = interfaces.matching(node, candidate.lang);
            if !may_satisfy_interfaces.is_empty() {
                demoted_by.insert(Demotion::InterfaceMethod);
            }
            // An annotated entry point is on the report because nothing
            // calls it; the annotation is why that is not conclusive.
            if has_live_annotation(node) {
                demoted_by.insert(Demotion::Annotation);
            }
            Verdict {
                tier: tier_for(candidate, node, &demoted_by),
                demoted_by,
                raw_reference_count,
                ambiguous_inbound_count,
                caller_count: callers.get(&candidate.node).map_or(0, BTreeSet::len),
                may_satisfy_interfaces,
            }
        })
        .collect();

    propagate_references(graph, reach, scan, &mut verdicts);
    verdicts
}

/// Carry doubt along raw references, transitively.
///
/// A name written inside another candidate's body is evidence exactly as
/// strong as that candidate is alive, so the reference hands over the
/// referrer's own tier: what an `unknown` row mentions becomes
/// `unknown`, what an unreachable `likely` row mentions becomes at most
/// `likely` — which is what keeps a cluster hanging off one
/// `pub(crate)` function together instead of scattering it into leads.
///
/// An entry point that is on the report merely for having no caller is
/// the exception: being callable from outside the analyzed path is what
/// put it there, so anything it names may be reached for a reason this
/// analyzer cannot see, and it hands over `unknown` regardless of its
/// own tier. A tier only ever moves down, so the queue terminates.
fn propagate_references(
    graph: &CallGraph,
    reach: &Reachability,
    scan: &ReferenceScan,
    verdicts: &mut [Verdict],
) {
    let mut slots_by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (slot, candidate) in reach.candidates.iter().enumerate() {
        slots_by_name
            .entry(graph.nodes[candidate.node].name.as_str())
            .or_default()
            .push(slot);
    }
    let mut references_from: BTreeMap<usize, Vec<&str>> = BTreeMap::new();
    for (slot, name) in &scan.internal {
        references_from.entry(*slot).or_default().push(name);
    }

    let mut queue: Vec<usize> = (0..verdicts.len())
        .filter(|&slot| verdicts[slot].tier != Tier::Confirmed)
        .collect();
    while let Some(slot) = queue.pop() {
        let carried = match reach.candidates[slot].kind {
            FindingKind::UncalledEntry => Tier::Unknown,
            FindingKind::Unreachable => verdicts[slot].tier,
        };
        let Some(names) = references_from.get(&slot) else {
            continue;
        };
        for name in names {
            for &referenced in slots_by_name.get(name).into_iter().flatten() {
                if verdicts[referenced].tier >= carried {
                    continue;
                }
                verdicts[referenced].tier = carried;
                verdicts[referenced]
                    .demoted_by
                    .insert(Demotion::RawReference);
                queue.push(referenced);
            }
        }
    }
}

/// A candidate with any caveat is `unknown`. Without one, the tier is
/// the reach of the declaration: only a private (Rust) or unexported
/// (Go) function that no entry path reaches can be confirmed, because
/// anything wider — and any entry point reported for having no caller —
/// can still be called from outside the analyzed path.
fn tier_for(candidate: &Candidate, node: &CallGraphNode, demoted_by: &BTreeSet<Demotion>) -> Tier {
    if !demoted_by.is_empty() {
        return Tier::Unknown;
    }
    match candidate.kind {
        FindingKind::UncalledEntry => Tier::Likely,
        FindingKind::Unreachable if node.visibility == candidate.lang.private() => Tier::Confirmed,
        FindingKind::Unreachable => Tier::Likely,
    }
}

/// Whether calls to this Rust method can be written against a trait
/// rather than the definition: a trait `impl` method, or a trait's own
/// default body. Both look private (a trait `impl` method carries no
/// visibility of its own) and both are routinely called without any site
/// naming them.
fn is_trait_method(node: &CallGraphNode) -> bool {
    matches!(
        node.owner_kind,
        Some(lens_domain::OwnerKind::TraitImpl | lens_domain::OwnerKind::Trait)
    )
}

/// Ambiguous call sites in reachable code, per candidate node they name.
fn ambiguous_inbound_counts(graph: &CallGraph, reach: &Reachability) -> BTreeMap<usize, usize> {
    let index_by_id = graph.node_index_by_id();
    let mut counts: BTreeMap<usize, usize> = BTreeMap::new();
    for edge in &graph.edges {
        if edge.resolution != Resolution::Ambiguous {
            continue;
        }
        // A call site with no enclosing function is attributed to
        // nothing, so it counts as coming from reachable code.
        let from_reached = edge
            .from
            .as_deref()
            .and_then(|from| index_by_id.get(from))
            .is_none_or(|&from| reach.reached[from]);
        if !from_reached {
            continue;
        }
        for candidate in &edge.candidates {
            if let Some(&idx) = index_by_id.get(candidate.as_str()) {
                *counts.entry(idx).or_default() += edge.call_count;
            }
        }
    }
    counts
}

/// One pass over the edge list for the global bounds: how much of
/// reachable code's outbound calling the traversal could not follow.
struct EdgeScan {
    unresolved_in_reached: usize,
    ambiguous_in_reached: usize,
    caller_unattributed: usize,
}

impl EdgeScan {
    fn run(graph: &CallGraph, reached: &[bool]) -> Self {
        let index_by_id = graph.node_index_by_id();
        let mut scan = Self {
            unresolved_in_reached: 0,
            ambiguous_in_reached: 0,
            caller_unattributed: 0,
        };
        for edge in &graph.edges {
            let Some(from) = edge.from.as_deref() else {
                // An anonymous callee is not a named function, so it is
                // not a call path anyone could have missed.
                if edge.resolution != Resolution::Anonymous {
                    scan.caller_unattributed += edge.call_count;
                }
                continue;
            };
            let Some(&from_idx) = index_by_id.get(from) else {
                continue;
            };
            if !reached[from_idx] {
                continue;
            }
            match edge.resolution {
                Resolution::Unresolved => scan.unresolved_in_reached += edge.call_count,
                Resolution::Ambiguous => scan.ambiguous_in_reached += edge.call_count,
                Resolution::Resolved | Resolution::Anonymous => {}
            }
        }
        scan
    }
}

/// Clusters of unreachable functions that only call each other.
///
/// Connectivity is taken as undirected over resolved edges inside the
/// unreachable set. Nothing reachable can call into that set — that is
/// what made it unreachable — so a connected component is a closed
/// cluster by construction, and the interesting ones have more than one
/// member: a single abandoned helper is a row, a cluster is a feature.
///
/// Members are drawn from the confirmed and likely tiers only. An
/// `unknown` row is one whose reachability the analyzer does not
/// believe it has established, and a cluster of those is not an
/// abandoned feature — it is a region the resolver lost track of, which
/// is the opposite of something to delete in one edit.
fn islands(graph: &CallGraph, reach: &Reachability, verdicts: &[Verdict]) -> Vec<Island> {
    let members: Vec<usize> = reach
        .candidates
        .iter()
        .zip(verdicts)
        .filter(|(candidate, verdict)| {
            candidate.kind == FindingKind::Unreachable && verdict.tier != Tier::Unknown
        })
        .map(|(candidate, _)| candidate.node)
        .collect();
    let slot_of: HashMap<usize, usize> = members
        .iter()
        .enumerate()
        .map(|(slot, &node)| (node, slot))
        .collect();

    let adjacency = graph.resolved_adjacency();
    let mut directed: Vec<Vec<usize>> = vec![Vec::new(); members.len()];
    let mut undirected: Vec<Vec<usize>> = vec![Vec::new(); members.len()];
    for (slot, &node) in members.iter().enumerate() {
        for &callee in &adjacency[node] {
            let Some(&callee_slot) = slot_of.get(&callee) else {
                continue;
            };
            if callee_slot == slot {
                continue;
            }
            directed[slot].push(callee_slot);
            undirected[slot].push(callee_slot);
            undirected[callee_slot].push(slot);
        }
    }

    let mut islands = Vec::new();
    let mut seen = vec![false; members.len()];
    for slot in 0..members.len() {
        if seen[slot] {
            continue;
        }
        let component: Vec<usize> = bfs(&undirected, &[slot])
            .into_iter()
            .map(|visit| visit.node)
            .collect();
        for &member in &component {
            seen[member] = true;
        }
        if component.len() < 2 {
            continue;
        }
        islands.push(build_island(
            graph,
            &members,
            &directed,
            &component,
            islands.len(),
        ));
    }
    islands.sort_by_key(|island| (Reverse(island.loc), Reverse(island.function_count)));
    for (id, island) in islands.iter_mut().enumerate() {
        island.id = id;
    }
    islands
}

fn build_island(
    graph: &CallGraph,
    members: &[usize],
    directed: &[Vec<usize>],
    component: &[usize],
    id: usize,
) -> Island {
    let confirmed_private = |slot: usize| {
        let node = &graph.nodes[members[slot]];
        ExportLang::of(node).is_some_and(|lang| node.visibility == lang.private())
    };
    Island {
        id,
        function_count: component.len(),
        // A cluster's own tiers are settled per row; what an island
        // states is how much of it is private, i.e. how much cannot have
        // a caller outside the analyzed path either.
        confirmed_count: component
            .iter()
            .copied()
            .filter(|&s| confirmed_private(s))
            .count(),
        loc: component
            .iter()
            .map(|&slot| graph.nodes[members[slot]].weights.loc)
            .sum(),
        modules: component
            .iter()
            .map(|&slot| graph.nodes[members[slot]].module.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        deletion_order: deletion_order(directed, component)
            .into_iter()
            .map(|slot| graph.nodes[members[slot]].id.clone())
            .collect(),
    }
}

/// Order an island's members so each precedes everything it calls.
///
/// [`algo::condense`] returns the strongly connected components in
/// reverse topological order — callees first — so reversing it puts a
/// caller before its callees, which is the order in which removing them
/// never breaks a member that is still there. Mutually recursive members
/// share a component and stay adjacent: they have to be removed
/// together.
fn deletion_order(directed: &[Vec<usize>], component: &[usize]) -> Vec<usize> {
    let local_of: HashMap<usize, usize> = component
        .iter()
        .enumerate()
        .map(|(local, &slot)| (slot, local))
        .collect();
    let mut local_adjacency: Vec<Vec<usize>> = vec![Vec::new(); component.len()];
    for (local, &slot) in component.iter().enumerate() {
        for callee in &directed[slot] {
            if let Some(&callee_local) = local_of.get(callee) {
                local_adjacency[local].push(callee_local);
            }
        }
        local_adjacency[local].sort_unstable();
        local_adjacency[local].dedup();
    }
    algo::condense(&local_adjacency)
        .components
        .into_iter()
        .rev()
        .flatten()
        .map(|local| component[local])
        .collect()
}

/// Island index per node id, for the row-level cross-reference.
fn island_membership(islands: &[Island]) -> HashMap<&str, usize> {
    islands
        .iter()
        .flat_map(|island| {
            island
                .deletion_order
                .iter()
                .map(|id| (id.as_str(), island.id))
        })
        .collect()
}

fn findings(
    graph: &CallGraph,
    reach: &Reachability,
    verdicts: &[Verdict],
    island_of: &HashMap<&str, usize>,
) -> Vec<Finding> {
    reach
        .candidates
        .iter()
        .zip(verdicts)
        .map(|(candidate, verdict)| {
            let node = &graph.nodes[candidate.node];
            Finding {
                id: node.id.clone(),
                qualified_name: node.qualified_name.clone(),
                file: node.file.clone(),
                start_line: node.start_line,
                end_line: node.end_line,
                loc: node.weights.loc,
                visibility: node.visibility,
                tier: verdict.tier,
                kind: candidate.kind,
                demoted_by: verdict.demoted_by.iter().copied().collect(),
                raw_reference_count: verdict.raw_reference_count,
                ambiguous_inbound_count: verdict.ambiguous_inbound_count,
                caller_count: verdict.caller_count,
                island: island_of.get(node.id.as_str()).copied(),
                may_satisfy_interfaces: verdict.may_satisfy_interfaces.clone(),
            }
        })
        .collect()
}

/// Group findings by defining module, most confirmed LOC first. Rows
/// inside a module follow the tier order and then the largest body, so
/// the first line of the first section is the biggest thing an agent may
/// delete outright.
fn module_groups(
    graph: &CallGraph,
    reach: &Reachability,
    findings: Vec<Finding>,
) -> Vec<ModuleGroup> {
    let mut by_module: BTreeMap<&str, Vec<Finding>> = BTreeMap::new();
    for (candidate, finding) in reach.candidates.iter().zip(findings) {
        by_module
            .entry(graph.nodes[candidate.node].module.as_str())
            .or_default()
            .push(finding);
    }
    let mut groups: Vec<ModuleGroup> = by_module
        .into_iter()
        .map(|(module, mut findings)| {
            findings.sort_by(|a, b| {
                (a.tier, Reverse(a.loc), &a.file, a.start_line, &a.id).cmp(&(
                    b.tier,
                    Reverse(b.loc),
                    &b.file,
                    b.start_line,
                    &b.id,
                ))
            });
            ModuleGroup {
                module: module.to_owned(),
                finding_count: findings.len(),
                confirmed_count: findings
                    .iter()
                    .filter(|f| f.tier == Tier::Confirmed)
                    .count(),
                confirmed_loc: findings
                    .iter()
                    .filter(|f| f.tier == Tier::Confirmed)
                    .map(|f| f.loc)
                    .sum(),
                findings,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        (
            Reverse(a.confirmed_loc),
            Reverse(a.finding_count),
            &a.module,
        )
            .cmp(&(
                Reverse(b.confirmed_loc),
                Reverse(b.finding_count),
                &b.module,
            ))
    });
    groups
}

fn summarize(
    modules: &[ModuleGroup],
    islands: &[Island],
    judged_function_count: usize,
    reached_function_count: usize,
) -> Summary {
    let findings = || modules.iter().flat_map(|group| &group.findings);
    let count = |tier: Tier| findings().filter(|f| f.tier == tier).count();
    let confirmed_count = count(Tier::Confirmed);
    Summary {
        reached_function_count,
        confirmed_count,
        likely_count: count(Tier::Likely),
        unknown_count: count(Tier::Unknown),
        confirmed_loc: modules.iter().map(|group| group.confirmed_loc).sum(),
        confirmed_share: if judged_function_count == 0 {
            0.0
        } else {
            confirmed_count as f64 / judged_function_count as f64
        },
        island_count: islands.len(),
        module_count: modules.len(),
    }
}

/// One module's findings at or above the rendered tier. Markdown leads
/// with the deletable rows and expands on `--tier`, so the group the
/// listing renders is a filtered view of the reported one.
struct ModuleView<'a> {
    module: &'a str,
    findings: Vec<&'a Finding>,
}

impl ModuleSection for ModuleView<'_> {
    fn module(&self) -> &str {
        self.module
    }

    fn item_count(&self) -> usize {
        self.findings.len()
    }

    fn heading_detail(&self) -> String {
        let loc: usize = self.findings.iter().map(|f| f.loc).sum();
        format!("{} function(s), {loc} LOC", self.findings.len())
    }

    fn render_items(&self, out: &mut String, limit: usize) {
        for finding in self.findings.iter().take(limit) {
            let _ = writeln!(out, "- {}", render_finding(finding));
        }
    }
}

fn tier_views(report: &Report, tier: Tier) -> Vec<ModuleView<'_>> {
    report
        .modules
        .iter()
        .filter_map(|group| {
            let findings: Vec<&Finding> =
                group.findings.iter().filter(|f| f.tier <= tier).collect();
            (!findings.is_empty()).then_some(ModuleView {
                module: &group.module,
                findings,
            })
        })
        .collect()
}

fn format_markdown(report: &Report, top: Option<usize>, tier: Option<Tier>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    let tier = tier.unwrap_or(Tier::Confirmed);
    let summary = &report.summary;
    let mut out = format!(
        "# Unreachable functions: {} ({} confirmed, {} likely, {} unknown of {} judged \
         function(s))\n",
        report.root,
        summary.confirmed_count,
        summary.likely_count,
        summary.unknown_count,
        report.audit.judged_function_count,
    );
    let _ = writeln!(out, "\n{}", report.note);
    render_entries(&mut out, &report.entries, &report.audit);

    if report.audit.judged_function_count == 0 {
        out.push_str(
            "\n_No Rust or Go function was left to judge._ Export status is only extracted for \
             those two languages, so TypeScript and Python functions are counted as entry points \
             and never reported.\n",
        );
        return out;
    }
    if report.modules.is_empty() {
        out.push_str("\n_Every function is reachable from an entry point._\n");
        return out;
    }

    render_counts(&mut out, summary, tier);
    render_demotions(&mut out, &report.bounds);
    let views = tier_views(report, tier);
    if views.is_empty() {
        let _ = writeln!(
            out,
            "\n_No `{}` finding._ Raise the tier (`--tier unknown`) to list the leads.",
            tier.as_str(),
        );
    } else {
        render_module_sections(
            &mut out,
            &format!(
                "Unreachable by module (`{}` and above, most confirmed LOC first",
                tier.as_str()
            ),
            &views,
            limit,
            FUNCTIONS_PER_MODULE,
        );
    }
    render_islands(&mut out, &report.islands);
    render_module_confidence(
        &mut out,
        &report.resolution,
        "Call sites in these modules resolved worst, so a call path into the functions above is \
         the most likely to have been missed — their functions are the least certain rows.",
    );
    out
}

fn render_entries(out: &mut String, entries: &EntrySet, audit: &Audit) {
    let kinds = entries
        .kinds
        .iter()
        .map(|entry| format!("{} {}", entry.count, entry.kind.as_str()))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = writeln!(
        out,
        "\nEntry set: {} function(s){}. {} source file(s) were scanned for raw name references; \
         {} function(s) in languages with no extracted export status are treated as entry points \
         rather than judged.",
        entries.function_count,
        if kinds.is_empty() {
            String::new()
        } else {
            format!(" ({kinds})")
        },
        audit.reference_scan_file_count,
        audit.unjudged_function_count,
    );
    if entries.absent {
        out.push_str(
            "No entry point was found at all, so every function below is unreachable by \
             construction rather than by evidence.\n",
        );
    }
    if entries.tests_excluded {
        out.push_str(
            "`--exclude-tests` dropped the test files: their entry points are missing and so are \
             the references their bodies hold, which makes code used only by tests look dead. \
             Re-run without it before deleting anything.\n",
        );
    }
}

fn render_counts(out: &mut String, summary: &Summary, tier: Tier) {
    let _ = writeln!(
        out,
        "\n{} confirmed function(s) hold {} LOC ({:.1}% of judged functions), across {} module(s) \
         and {} island(s). Showing `{}` and above; JSON carries every tier.",
        summary.confirmed_count,
        summary.confirmed_loc,
        summary.confirmed_share * 100.0,
        summary.module_count,
        summary.island_count,
        tier.as_str(),
    );
    if summary.likely_count > 0 {
        let _ = writeln!(
            out,
            "{} `likely` row(s) are declarations nothing in the analyzed path uses or names. \
             Pointed at a library that is its published API, and the consumers live elsewhere; \
             pointed at a whole workspace it is surface with no remaining user.",
            summary.likely_count,
        );
    }
}

/// Name the demotions, so an empty confirmed tier reads as a diagnosis
/// rather than as "there is no dead code here".
fn render_demotions(out: &mut String, bounds: &Bounds) {
    if bounds.demotions.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "{} candidate(s) were demoted below `confirmed`: {}.",
        bounds.demoted_function_count,
        bounds
            .demotions
            .iter()
            .map(|entry| format!("{} — {}", entry.count, entry.reason.as_str()))
            .collect::<Vec<_>>()
            .join(", "),
    );
}

fn render_finding(finding: &Finding) -> String {
    let mut row = format!(
        "`{}` ({}:{}, {} LOC, {})",
        finding.qualified_name,
        finding.file,
        finding.start_line,
        finding.loc,
        match finding.kind {
            FindingKind::Unreachable => "no entry path",
            FindingKind::UncalledEntry => "entry point, no caller in the tree",
        },
    );
    if let Some(island) = finding.island {
        let _ = write!(row, ", island {island}");
    }
    if finding.caller_count > 0 {
        let _ = write!(row, ", {} unreachable caller(s)", finding.caller_count,);
    }
    if !finding.demoted_by.is_empty() {
        let _ = write!(
            row,
            " — {}",
            finding
                .demoted_by
                .iter()
                .map(|demotion| demotion.as_str().to_owned())
                .collect::<Vec<_>>()
                .join("; "),
        );
    }
    if finding.raw_reference_count > 0 {
        let _ = write!(row, " ({} occurrence(s))", finding.raw_reference_count);
    }
    if !finding.may_satisfy_interfaces.is_empty() {
        let _ = write!(
            row,
            ": {}",
            render_backticked_list(&finding.may_satisfy_interfaces, INTERFACES_PER_ROW),
        );
    }
    row
}

fn render_islands(out: &mut String, islands: &[Island]) {
    if islands.is_empty() {
        return;
    }
    let _ = writeln!(
        out,
        "\n## Islands ({} cluster(s) that only call each other, largest first)\n\nEach cluster is \
         reachable from nothing outside itself, so it comes out in one edit. The order given \
         removes callers before their callees; adjacent members of a mutually recursive group \
         have to go together.\n",
        islands.len(),
    );
    for island in islands.iter().take(ISLANDS_SHOWN) {
        let _ = writeln!(
            out,
            "- island {}: {} function(s), {} LOC, {} private, in {} — delete in order: {}",
            island.id,
            island.function_count,
            island.loc,
            island.confirmed_count,
            render_backticked_list(&island.modules, ISLAND_MEMBERS_PER_ROW),
            render_backticked_list(&island.deletion_order, ISLAND_MEMBERS_PER_ROW),
        );
    }
    let overflow = islands.len().saturating_sub(ISLANDS_SHOWN);
    if overflow > 0 {
        let _ = writeln!(out, "- +{overflow} more (JSON output carries every island)");
    }
}

/// Backticked items up to `cap`, the remainder rolled into a count.
fn render_backticked_list(items: &[String], cap: usize) -> String {
    let listed = items
        .iter()
        .take(cap)
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let overflow = items.len().saturating_sub(cap);
    if overflow > 0 {
        format!("{listed} +{overflow} more")
    } else {
        listed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use proptest::prelude::*;
    use rstest::rstest;
    use serde_json::Value;

    fn analyze_json(path: &Path) -> Value {
        let json = UnreachableAnalyzer::new()
            .analyze(path, OutputFormat::Json)
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn analyze_md(path: &Path) -> String {
        UnreachableAnalyzer::new()
            .analyze(path, OutputFormat::Md)
            .unwrap()
    }

    /// Every reported row as `(qualified name, tier)`, in report order.
    fn tiers(report: &Value) -> Vec<(String, String)> {
        rows(report)
            .into_iter()
            .map(|row| {
                (
                    row["qualified_name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    row["tier"].as_str().unwrap_or_default().to_owned(),
                )
            })
            .collect()
    }

    fn rows(report: &Value) -> Vec<&Value> {
        report["modules"]
            .as_array()
            .map(|modules| {
                modules
                    .iter()
                    .filter_map(|module| module["findings"].as_array())
                    .flatten()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The row whose qualified name ends with `suffix`, or `None` when
    /// the function was not reported at all.
    fn row_for<'a>(report: &'a Value, suffix: &str) -> Option<&'a Value> {
        rows(report).into_iter().find(|row| {
            row["qualified_name"]
                .as_str()
                .is_some_and(|name| name.ends_with(suffix))
        })
    }

    fn tier_of(report: &Value, suffix: &str) -> Option<String> {
        row_for(report, suffix)
            .and_then(|row| row["tier"].as_str())
            .map(ToOwned::to_owned)
    }

    fn demotions(report: &Value, suffix: &str) -> Vec<String> {
        row_for(report, suffix)
            .and_then(|row| row["demoted_by"].as_array())
            .map(|reasons| {
                reasons
                    .iter()
                    .filter_map(|reason| reason.as_str())
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A Rust binary root with one entry point, one function it calls,
    /// and one nothing calls at all. The entry point is `main` rather
    /// than a `pub fn` so the fixture has no public surface — an
    /// uncalled `pub fn` is a finding of its own, which these cases are
    /// not about.
    const RUST_ORPHAN: &str = "fn main() { live(); }\n\
                               fn live() -> usize { 1 }\n\
                               fn orphan() -> usize { 2 }\n";

    #[test]
    fn a_private_function_no_entry_path_reaches_is_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/main.rs", RUST_ORPHAN);

        let report = analyze_json(dir.path());
        assert_eq!(report["schema_version"], 1);
        assert_eq!(
            tiers(&report),
            [("crate::orphan".to_owned(), "confirmed".to_owned())],
            "only the orphan is reported: {report}",
        );
        assert_eq!(report["summary"]["confirmed_count"], 1);
        assert_eq!(report["summary"]["reached_function_count"], 2);
        assert_eq!(report["audit"]["reference_scan_file_count"], 1);

        let md = analyze_md(dir.path());
        assert!(
            md.contains("1 confirmed function(s) hold 1 LOC"),
            "got {md}",
        );
        assert!(
            md.contains("- `crate::orphan` (src/main.rs:3, 1 LOC, no entry path)"),
            "the row carries where it is, how big it is, and why it is here: {md}",
        );
    }

    /// The reference scan covers the whole corpus, not the defining
    /// file, and reaches code no function encloses — a name-keyed
    /// registry in a sibling file is exactly the call a syntax-only
    /// resolver misses. The scanned-file count is what says how much
    /// ground the search actually covered.
    #[test]
    fn a_reference_from_another_file_demotes_and_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/main.rs", RUST_ORPHAN);
        write_file(
            dir.path(),
            "src/other.rs",
            "pub const HANDLERS: [&str; 1] = [\"orphan\"];\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["audit"]["reference_scan_file_count"], 2);
        assert_eq!(tier_of(&report, "::orphan").as_deref(), Some("unknown"));
        assert_eq!(
            row_for(&report, "::orphan").map(|row| row["raw_reference_count"].clone()),
            Some(Value::from(1)),
            "the sibling file's mention is the evidence: {report}",
        );
    }

    /// The load-bearing pass: the graph resolves nothing here, and the
    /// textual scan is what keeps each shape out of the confirmed tier.
    #[rstest]
    #[case::string_literal("pub fn entry() -> &'static str { \"orphan\" }\n")]
    #[case::macro_body("macro_rules! call { () => { orphan() } }\n")]
    #[case::comment("// orphan() is called by the runtime\npub fn entry() {}\n")]
    #[case::attribute_argument("#[allow(orphan)]\npub fn entry() {}\n")]
    fn a_name_written_anywhere_in_the_sources_demotes_it(#[case] preamble: &str) {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            &format!("{preamble}fn orphan() -> usize {{ 2 }}\n"),
        );

        let report = analyze_json(dir.path());
        assert_eq!(tier_of(&report, "::orphan").as_deref(), Some("unknown"));
        assert!(
            demotions(&report, "::orphan").contains(&"raw_reference".to_owned()),
            "got {:?}",
            demotions(&report, "::orphan"),
        );

        // A row an agent is told to check has to say what to check.
        let md = UnreachableAnalyzer::new()
            .with_tier(Some(Tier::Unknown))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("its name is written elsewhere"), "got {md}");
    }

    /// A mention that only dead code makes is no evidence: the whole
    /// point of an island is that its members call each other.
    #[test]
    fn a_reference_from_confirmed_dead_code_does_not_demote() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn entry() -> usize { 1 }\n\
             fn dead_caller() -> usize { dead_callee() }\n\
             fn dead_callee() -> usize { 2 }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(
            tier_of(&report, "::dead_callee").as_deref(),
            Some("confirmed"),
            "referenced only by dead code: {report}",
        );
        assert_eq!(
            tier_of(&report, "::dead_caller").as_deref(),
            Some("confirmed"),
        );
    }

    /// The propagation direction that matters: code the analyzer could
    /// not confirm dead is code that may run, so what it names may run
    /// too.
    #[test]
    fn a_reference_from_unconfirmed_code_demotes_transitively() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            // `middle` is demoted by the string in the entry point, and
            // has to carry that uncertainty to what it calls.
            "pub fn entry() -> &'static str { \"middle\" }\n\
             fn middle() -> usize { tail() }\n\
             fn tail() -> usize { 2 }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(tier_of(&report, "::middle").as_deref(), Some("unknown"));
        assert_eq!(
            tier_of(&report, "::tail").as_deref(),
            Some("unknown"),
            "the demotion propagates along the reference: {report}",
        );
    }

    /// The Rust soundness hazard the tier exists for: a trait `impl`
    /// method carries no visibility of its own and its callers name the
    /// trait, so "private and uncalled" says nothing about it.
    #[test]
    fn a_trait_impl_method_is_never_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub trait Greet { fn greeting(&self) -> usize; }\n\
             pub struct W;\n\
             impl Greet for W { fn greeting(&self) -> usize { 1 } }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(tier_of(&report, "W::greeting").as_deref(), Some("unknown"));
        assert!(
            demotions(&report, "W::greeting").contains(&"trait_method".to_owned()),
            "got {:?}",
            demotions(&report, "W::greeting"),
        );
    }

    /// An inherent method of the same shape is judged normally — the
    /// exemption above is about trait dispatch, not about methods.
    #[test]
    fn an_inherent_method_nothing_calls_is_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub struct W;\n\
             impl W { fn greeting(&self) -> usize { 1 } }\n",
        );

        assert_eq!(
            tier_of(&analyze_json(dir.path()), "W::greeting").as_deref(),
            Some("confirmed"),
        );
    }

    #[rstest]
    // Lints and codegen hints cannot call anything, so they leave the
    // verdict alone.
    #[case::inert_attribute("#[inline]", "confirmed", "unreachable")]
    #[case::lint_attribute("#[cold]", "confirmed", "unreachable")]
    // A linker symbol or an attribute macro can be the caller.
    #[case::linker_symbol("#[no_mangle]", "unknown", "uncalled_entry")]
    #[case::unsafe_wrapped_linker_symbol("#[unsafe(no_mangle)]", "unknown", "uncalled_entry")]
    #[case::attribute_macro("#[ctor::ctor]", "unknown", "uncalled_entry")]
    fn an_annotation_decides_whether_the_function_is_judged_or_trusted(
        #[case] attribute: &str,
        #[case] tier: &str,
        #[case] kind: &str,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            &format!("{attribute}\nfn annotated() -> usize {{ 1 }}\n"),
        );

        let report = analyze_json(dir.path());
        let row = row_for(&report, "::annotated").expect("the function is reported");
        assert_eq!(row["tier"], tier, "got {row}");
        assert_eq!(row["kind"], kind, "got {row}");
    }

    /// A `pub` function is an entry point, so it is never "unreachable"
    /// — but one nothing in the tree calls or names is still a finding,
    /// at the tier that says the answer lies outside the analyzed path.
    #[test]
    fn an_uncalled_public_function_is_likely_rather_than_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn used() -> usize { 1 }\n\
             pub fn unused() -> usize { 2 }\n\
             pub fn entry() -> usize { used() }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(tier_of(&report, "::unused").as_deref(), Some("likely"));
        assert_eq!(
            row_for(&report, "::unused").map(|row| row["kind"].clone()),
            Some(Value::from("uncalled_entry")),
        );
        assert_eq!(
            tier_of(&report, "::used"),
            None,
            "a public function with a caller is not a finding: {report}",
        );

        let md = UnreachableAnalyzer::new()
            .with_tier(Some(Tier::Likely))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains(
                "`crate::unused` (src/lib.rs:2, 1 LOC, entry point, no caller in the tree)"
            ),
            "an uncalled entry reads differently from an unreachable function: {md}",
        );
    }

    /// One Go package with an entry point, a helper it calls, and an
    /// unexported function nothing reaches.
    const GO_ORPHAN: &str = "package app\n\
                             \n\
                             func main() { live() }\n\
                             \n\
                             func live() {}\n\
                             \n\
                             func orphan() {}\n";

    #[test]
    fn go_judges_unexported_functions_and_roots_main() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "app/main.go", GO_ORPHAN);

        let report = analyze_json(dir.path());
        assert_eq!(
            tiers(&report),
            [("app::orphan".to_owned(), "confirmed".to_owned())],
            "got {report}",
        );
    }

    #[rstest]
    // The Go runtime calls `init` before `main`, with no call site.
    #[case::init("func init() { helper() }", None)]
    // An exported function can be called from another package.
    #[case::exported("func Exported() { helper() }", None)]
    // …while an unexported one that nothing calls is judged.
    #[case::unexported("func caller() { helper() }", Some("confirmed"))]
    fn go_entry_points_root_the_traversal(#[case] root: &str, #[case] helper_tier: Option<&str>) {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "app/lib.go",
            &format!("package app\n\n{root}\n\nfunc helper() {{}}\n"),
        );

        assert_eq!(
            tier_of(&analyze_json(dir.path()), "::helper").as_deref(),
            helper_tier,
        );
    }

    /// The Go counterpart of the trait-method hazard, and the fact
    /// `analyze visibility` already extracts: a method matching an
    /// in-scope interface can be called through it.
    #[test]
    fn a_go_method_matching_an_in_scope_interface_is_never_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "app/lib.go",
            "package app\n\
             \n\
             type greeter interface {\n\
             \x20   greeting() string\n\
             }\n\
             \n\
             type svc struct{}\n\
             \n\
             func (s svc) greeting() string { return \"\" }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(
            tier_of(&report, "svc::greeting").as_deref(),
            Some("unknown"),
            "got {report}",
        );
        assert!(
            demotions(&report, "svc::greeting").contains(&"interface_method".to_owned()),
            "got {:?}",
            demotions(&report, "svc::greeting"),
        );
    }

    /// A Go compiler directive is the language's only annotation, and
    /// the cgo `//export` is one that publishes the function to a
    /// caller written in C.
    #[test]
    fn a_go_export_directive_makes_the_function_an_entry_point() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "app/lib.go",
            "package app\n\
             \n\
             //export ffi\n\
             func ffi() {}\n",
        );

        let report = analyze_json(dir.path());
        let row = row_for(&report, "::ffi").expect("the function is reported");
        assert_eq!(row["kind"], "uncalled_entry", "got {row}");
        assert_eq!(row["tier"], "unknown");
        assert!(
            row["demoted_by"]
                .as_array()
                .is_some_and(|reasons| reasons.contains(&Value::from("annotation"))),
            "got {row}",
        );
    }

    /// An inert directive leaves the judgement alone.
    #[test]
    fn a_go_codegen_directive_leaves_the_verdict_alone() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "app/lib.go",
            "package app\n\
             \n\
             //go:noinline\n\
             func orphan() {}\n",
        );

        assert_eq!(
            tier_of(&analyze_json(dir.path()), "::orphan").as_deref(),
            Some("confirmed"),
        );
    }

    /// Three dead functions in a chain: one cluster, ordered so that
    /// removing them in sequence never leaves a call to something
    /// already gone.
    #[test]
    fn an_island_reports_its_cluster_with_a_caller_first_order() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn entry() -> usize { 1 }\n\
             fn head() -> usize { waist() }\n\
             fn waist() -> usize { foot() }\n\
             fn foot() -> usize { 2 }\n",
        );

        let report = analyze_json(dir.path());
        let islands = report["islands"].as_array().expect("islands array");
        assert_eq!(islands.len(), 1, "got {report}");
        let island = &islands[0];
        assert_eq!(island["function_count"], 3);
        assert_eq!(island["confirmed_count"], 3);
        let order: Vec<&str> = island["deletion_order"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|id| id.as_str())
            .collect();
        assert_eq!(
            order,
            [
                "src/lib.rs:head:2",
                "src/lib.rs:waist:3",
                "src/lib.rs:foot:4"
            ],
            "callers come before what they call",
        );
        assert!(
            rows(&report)
                .iter()
                .filter(|row| row["island"] == 0)
                .count()
                == 3,
            "each member cross-references its island: {report}",
        );

        let md = analyze_md(dir.path());
        assert!(
            md.contains("island 0: 3 function(s), 3 LOC, 3 private, in `crate`"),
            "got {md}",
        );
        assert!(
            md.contains("delete in order: `src/lib.rs:head:2`"),
            "the order is the edit list, so it is rendered, not just stored: {md}",
        );
    }

    /// A cluster whose members the analyzer could not confirm dead is
    /// not an abandoned feature; it is a region the resolver lost track
    /// of, and offering it as one edit would be the worst kind of false
    /// positive.
    #[test]
    fn an_unknown_tier_cluster_is_not_reported_as_an_island() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn entry() -> &'static str { \"head\" }\n\
             fn head() -> usize { waist() }\n\
             fn waist() -> usize { 2 }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(tier_of(&report, "::head").as_deref(), Some("unknown"));
        assert_eq!(
            report["islands"].as_array().map(Vec::len),
            Some(0),
            "got {report}",
        );
    }

    /// Every verdict is relative to the entry set, so the entry set is
    /// part of the report rather than an assumption behind it.
    #[test]
    fn the_entry_set_is_emitted_with_its_kinds() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/main.rs",
            "fn main() { run(); }\n\
             pub fn run() {}\n\
             #[test]\n\
             fn covered() { run(); }\n",
        );

        let report = analyze_json(dir.path());
        let kinds: Vec<(String, u64)> = report["entries"]["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["kind"].as_str().unwrap_or_default().to_owned(),
                    entry["count"].as_u64().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            [
                ("main".to_owned(), 1),
                ("test".to_owned(), 1),
                ("public".to_owned(), 1),
            ],
            "got {report}",
        );
        assert_eq!(report["entries"]["function_count"], 3);
        assert_eq!(report["entries"]["absent"], false);
        // The markdown carries the same set: a reader who only sees the
        // rendered report still knows what the verdicts are relative to.
        assert!(
            analyze_md(dir.path()).contains("Entry set: 3 function(s) (1 main, 1 test, 1 public)"),
            "got {}",
            analyze_md(dir.path()),
        );
    }

    /// Languages with no extracted export status are treated as live so
    /// nothing they call is reported, and counted so the omission is
    /// visible.
    #[test]
    fn typescript_and_python_functions_are_counted_and_never_judged() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.ts",
            "function orphanTs() { return 1; }\n",
        );
        write_file(dir.path(), "src/lib.py", "def orphan_py():\n    return 1\n");

        let report = analyze_json(dir.path());
        assert_eq!(report["audit"]["unjudged_function_count"], 2);
        assert_eq!(report["audit"]["judged_function_count"], 0);
        assert!(rows(&report).is_empty(), "got {report}");
        assert!(
            analyze_md(dir.path()).contains("No Rust or Go function was left to judge"),
            "got {}",
            analyze_md(dir.path()),
        );
    }

    /// The two denominators every share on the report divides by, kept
    /// apart on a corpus that has one of each: a judged Rust function, a
    /// test that is neither, and two functions in languages that carry
    /// no export status.
    #[test]
    fn the_judged_and_unjudged_counts_split_the_corpus() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.ts",
            "function orphanTs() { return 1; }\n",
        );
        write_file(dir.path(), "src/lib.py", "def orphan_py():\n    return 1\n");
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn judged() -> usize { 1 }\n\
             #[test]\n\
             fn covered() { judged(); }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(
            report["audit"]["judged_function_count"], 1,
            "only the non-test Rust function is judged: {report}",
        );
        assert_eq!(
            report["audit"]["unjudged_function_count"], 2,
            "the test function is not unjudged — it is an entry point: {report}",
        );
    }

    /// Dropping the test files takes the entry points tests provide with
    /// it, which is the one flag that can turn live code into a
    /// confirmed row. Saying so is the whole mitigation.
    #[test]
    fn excluding_tests_is_reported_as_a_cut_entry_set() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/main.rs", RUST_ORPHAN);

        let md = UnreachableAnalyzer::new()
            .with_exclude_tests(true)
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("`--exclude-tests` dropped the test files"),
            "got {md}"
        );
    }

    /// Markdown leads with the tier an agent may act on and expands on
    /// request; JSON is unfiltered either way.
    #[test]
    fn the_tier_flag_widens_the_markdown_listing_only() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/main.rs",
            "fn main() -> &'static str { \"demoted\" }\n\
             fn demoted() -> usize { 1 }\n\
             fn orphan() -> usize { 2 }\n",
        );

        let default = analyze_md(dir.path());
        assert!(default.contains("`crate::orphan`"), "got {default}");
        assert!(!default.contains("`crate::demoted`"), "got {default}");
        // Which tier is being shown is half the meaning of the listing,
        // so the cut is named in the heading rather than implied by
        // which rows are present.
        assert!(default.contains("(`confirmed` and above"), "got {default}",);

        let widened = UnreachableAnalyzer::new()
            .with_tier(Some(Tier::Unknown))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(widened.contains("`crate::demoted`"), "got {widened}");
        assert!(widened.contains("(`unknown` and above"), "got {widened}");

        let report = analyze_json(dir.path());
        assert_eq!(tiers(&report).len(), 2, "JSON carries every tier: {report}");
    }

    /// `--top` caps how many module sections markdown renders, and says
    /// what it hid rather than dropping it silently. JSON is unaffected.
    #[test]
    fn top_caps_the_markdown_module_listing() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/main.rs",
            "fn main() {}\nfn orphan_here() -> usize { 1 }\n",
        );
        write_file(
            dir.path(),
            "src/other.rs",
            "fn orphan_there() -> usize { 2 }\n",
        );

        let full = analyze_md(dir.path());
        assert!(full.contains("; 2 of 2 module(s))"), "got {full}");

        let capped = UnreachableAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(capped.contains("; 1 of 2 module(s))"), "got {capped}");
        assert!(
            capped.contains("+1 more module(s) not shown"),
            "got {capped}",
        );
        assert_eq!(
            tiers(&analyze_json(dir.path())).len(),
            2,
            "JSON carries every module",
        );
    }

    /// With nothing to report the markdown says which of the two empty
    /// cases it is, rather than rendering an empty section.
    #[test]
    fn a_fully_reachable_tree_says_so() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/main.rs", "fn main() { }\n");

        let md = analyze_md(dir.path());
        assert!(
            md.contains("Every function is reachable from an entry point"),
            "got {md}",
        );
    }

    /// The bounds block is what an agent reads before trusting an empty
    /// confirmed tier: it says which kind of call the graph could not
    /// account for.
    #[test]
    fn demotions_are_counted_by_reason() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn entry() -> &'static str { \"first\" }\n\
             fn first() -> usize { 1 }\n\
             fn second() -> usize { 2 }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["bounds"]["demoted_function_count"], 1);
        assert_eq!(report["bounds"]["demotions"][0]["reason"], "raw_reference");
        assert_eq!(report["bounds"]["demotions"][0]["count"], 1);
        assert!(
            analyze_md(dir.path()).contains("1 candidate(s) were demoted below `confirmed`"),
            "got {}",
            analyze_md(dir.path()),
        );
    }

    /// What a call site, a string, or a comment puts around a name.
    /// Unicode letters and digits are deliberately absent: those *are*
    /// identifier characters, so a name glued to one is a different
    /// name — the property below would be asserting the opposite of
    /// what the scan promises.
    const SEPARATORS: &[&str] = &[" ", ".", "(", ")", ",", ";", "\t", "\"", "+", "::"];

    #[rstest]
    #[case::bare("orphan", vec!["orphan"])]
    #[case::call_expression("    orphan();", vec!["orphan"])]
    #[case::method_call("self.orphan()", vec!["self", "orphan"])]
    #[case::inside_a_string("let s = \"orphan\";", vec!["let", "s", "orphan"])]
    #[case::longer_identifier("orphaned()", vec!["orphaned"])]
    #[case::prefixed("my_orphan", vec!["my_orphan"])]
    #[case::non_ascii_name("café()", vec!["café"])]
    // A Unicode digit is an identifier character like any other, so it
    // glues rather than separates — the same rule as `orphaned`.
    #[case::unicode_digit_glues("¹a", vec!["¹a"])]
    #[case::empty("", Vec::new())]
    fn identifiers_yields_whole_runs_only(#[case] line: &str, #[case] expected: Vec<&str>) {
        assert_eq!(identifiers(line).collect::<Vec<_>>(), expected);
    }

    // The one distinction the scan has to make: a name surrounded by
    // anything that is not an identifier character is found, and a name
    // that is only part of a longer run is not.
    proptest! {
        #[test]
        fn a_name_is_found_only_as_a_whole_run(
            name in "[a-z][a-z_]{0,8}",
            left in prop::sample::select(SEPARATORS),
            right in prop::sample::select(SEPARATORS),
            suffix in "[a-z]{1,4}",
        ) {
            let line = format!("{left}{name}{right}");
            prop_assert!(identifiers(&line).any(|token| token == name), "{line}");

            let glued = format!("{left}{name}{suffix}{right}");
            prop_assert!(
                !identifiers(&glued).any(|token| token == name),
                "{glued} must not match {name}",
            );
        }
    }

    // The ordering contract of a deletion order, on arbitrary shapes: a
    // member never precedes one of its callers unless the two are in a
    // cycle, so removing them in order never breaks a member that is
    // still there.
    proptest! {
        #[test]
        fn a_deletion_order_puts_callers_before_what_they_call(
            edges in prop::collection::vec((0usize..6, 0usize..6), 0..12),
        ) {
            let size = 6;
            let mut directed: Vec<Vec<usize>> = vec![Vec::new(); size];
            for (from, to) in &edges {
                if from != to {
                    directed[*from].push(*to);
                }
            }
            let component: Vec<usize> = (0..size).collect();
            let order = deletion_order(&directed, &component);
            prop_assert_eq!(order.len(), size);

            let position: BTreeMap<usize, usize> =
                order.iter().enumerate().map(|(at, &node)| (node, at)).collect();
            for (from, to) in &edges {
                if from == to {
                    continue;
                }
                // Either the caller comes first, or the two are in one
                // strongly connected component and have to go together.
                let reaches_back = algo::shortest_path(&directed, *to, *from, None).is_some();
                prop_assert!(
                    position[from] < position[to] || reaches_back,
                    "{from} -> {to} in {order:?}",
                );
            }
        }
    }
}
