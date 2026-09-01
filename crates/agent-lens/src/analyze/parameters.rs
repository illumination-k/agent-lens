//! `analyze parameters` — parameters that only ever receive one thing.
//!
//! The third member of the `single-use` family, one level down: where
//! `single-use` asks "how many callers does this function have?", this
//! asks, per parameter, "how many distinct values does it ever see?".
//! Two finding kinds come out of that question:
//!
//! * **Constant argument** — every resolved production call site passes
//!   the same literal (or the same constant-looking path). The candidate
//!   edit is to inline that value into the body and drop the parameter.
//!   Linters cannot see this: it needs the call graph's cross-file view
//!   of every call site.
//! * **Dead parameter** — the body never reads the parameter. Rust's
//!   compiler warns about the easy cases, but TypeScript, Python, and Go
//!   commonly do not, and the edit (drop the parameter, update the call
//!   sites) needs the caller list anyway.
//!
//! Soundness follows `single-use`: "every call site" is a claim about
//! what the graph could see. Call sites the adapter could not line up
//! against the parameter list — spreads, unmatched keywords, arity
//! mismatches — demote the row with a caveat instead of being silently
//! ignored, an ambiguous inbound edge or a raw name reference means a
//! hidden call site can pass a different value, and a public function
//! can be called from outside the analyzed tree entirely. Trait and
//! interface methods are excluded outright: their signatures are fixed
//! by the abstraction, so a constant argument there is a fact about the
//! callers, not an edit on the method.
//!
//! The dead-parameter check is textual on purpose: a parameter whose
//! name never appears in the function's span beyond its declaration is
//! never read, whatever the language. Mentions in strings, comments, or
//! shadowing bindings count as reads, so the check under-reports rather
//! than over-reports — the same direction every caveat here leans.
//!
//! A parameter with one call site is trivially "always the same value";
//! that is `single-use`'s finding, not this one's, which is why
//! `--min-call-sites` defaults to 2 and the calibration section reports
//! how many parameters have 1 / 2 / 3+ distinct values so the threshold
//! can be set per repository.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use serde::Serialize;

use super::call_graph::model::{
    CallGraphNode, CallSiteFacts, GraphLanguage, ModuleResolutionSummary, NodeVisibility,
    Resolution, ResolutionMethod,
};
use super::call_graph::{CallGraph, CallGraphBuilder, delegate_call_graph_builders};
use super::export_lang::{ExportLang, InterfaceIndex};
use super::format::render_module_confidence;
use super::options::analyzer_options;
use super::runner::render_report;
use super::unreachable::identifiers;
use super::{AnalyzeRoots, AnalyzerError, OutputFormat};
use lens_domain::ArgumentShape;

const SCHEMA_VERSION: u32 = 1;

/// Markdown ranking cap when `--top` is not given. JSON always carries
/// every finding.
const DEFAULT_TOP: usize = 20;

/// Minimum resolved production call sites before "always the same
/// value" means anything. One call site is trivially constant — and is
/// `single-use`'s finding, not this analyzer's.
pub const DEFAULT_MIN_CALL_SITES: usize = 2;

const NOTE: &str = "Two finding kinds, both candidates rather than verdicts. A constant-argument \
     row is a parameter that receives the same literal (or the same constant-looking path) at \
     every resolved production call site the graph could line up against the parameter list; \
     the candidate edit is inlining the value and dropping the parameter. A default-only row \
     is a parameter no production call site ever passes, so the declared default is the only \
     value. A dead-parameter row is a parameter the body never reads (textual check: mentions \
     in strings, comments, or shadowing bindings count as reads, so this under-reports). \
     \"Every call site\" is what the graph saw: an ambiguous inbound edge, a raw name \
     reference, a call site with a spread or an unmatched keyword, or a public/exported \
     function all mean a hidden call site can pass something else — each demotes the row \
     with a caveat rather than removing it. Trait/interface methods and annotated functions \
     are excluded outright because their signatures are not theirs to change.";

analyzer_options! {
    /// `analyze parameters` flags, and the `[profile.<name>.parameters]`
    /// table.
    pub struct ParametersOptions {
        @shared(ranking);
        /// Minimum resolved production call sites a parameter needs
        /// before "always the same value" is reported. Defaults to 2:
        /// a single-caller function's arguments are `single-use`'s
        /// finding, not this one's.
        #[arg(long, value_name = "N")]
        pub min_call_sites: Option<usize>,
    }
}

/// Analyzer entry point for `analyze parameters`.
#[derive(Debug, Default, Clone)]
pub struct ParametersAnalyzer {
    builder: CallGraphBuilder,
    only_tests: bool,
    top: Option<usize>,
    min_call_sites: Option<usize>,
}

impl ParametersAnalyzer {
    /// Apply a whole [`ParametersOptions`] group. The CLI flags and the
    /// `[profile.<name>.parameters]` table are the same type, so this is
    /// the only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: ParametersOptions) -> Self {
        self.with_top(opts.top)
            .with_min_call_sites(opts.min_call_sites)
    }

    pub fn new() -> Self {
        Self::default()
    }

    delegate_call_graph_builders! {
        builder,
        /// Analyze the test corpus instead: test helpers' parameters
        /// earn their keep the same way, and in this mode test callers
        /// are the production callers.
        only_tests => only_tests,
        /// Drops test files. Test call sites then stop being visible,
        /// so the tests-vary-value caveat cannot fire.
        exclude_tests,
    }

    /// Cap the markdown finding lists to the top-N entries. JSON output
    /// always carries every finding.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    /// Call-site floor; `None` applies [`DEFAULT_MIN_CALL_SITES`].
    pub fn with_min_call_sites(mut self, min_call_sites: Option<usize>) -> Self {
        self.min_call_sites = min_call_sites;
        self
    }

    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, AnalyzerError> {
        let roots = roots.into();
        // Interface facts keep Go methods matching an in-scope
        // interface's method set out of the pool; argument facts are
        // this analyzer's whole substrate.
        let graph = self
            .builder
            .clone()
            .with_interface_facts(true)
            .with_argument_facts(true)
            .build(&roots)?;
        let min_call_sites = self.min_call_sites.unwrap_or(DEFAULT_MIN_CALL_SITES);
        let collected = Collected::collect(&graph, self.only_tests, min_call_sites);
        let scan = SourceScan::run(
            &self.builder,
            &roots,
            &collected.dead_targets,
            &collected.raw_targets,
        )?;
        let report = Report::build(&roots, &graph, collected, &scan, min_call_sites);
        render_report(&report, format, || format_markdown(&report, self.top))
    }
}

/// A reason a row's claim, or the edit it suggests, is weaker than the
/// row's presence suggests. Caveats demote; they never remove a row
/// from the JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum Caveat {
    /// Visible beyond its own module (or compilation unit): a call site
    /// outside the analyzed path can pass anything.
    WiderThanPrivate,
    /// The adapter extracts no export status for this language
    /// (TypeScript, Python), so outside call sites cannot be ruled out.
    UnknownVisibility,
    /// Some call sites could not be lined up against the parameter list
    /// (a spread, an unmatched keyword, an arity mismatch, a call shape
    /// with no extracted arguments) — any of them can pass a different
    /// value.
    UnanalyzedCallSites,
    /// An ambiguous call site names this function as a candidate — a
    /// hidden call site may exist.
    AmbiguousInbound,
    /// A contributing edge resolved through a name-fallback heuristic,
    /// so even the counted call sites are less certain.
    FallbackResolvedCall,
    /// A test call site passes a different value: the parameter is a
    /// seam the tests need, and inlining the constant moves those tests
    /// onto the callers.
    TestsVaryValue,
    /// The bare name is written somewhere outside the definition and
    /// its known callers — a macro body, a string, an import. Any of
    /// those can be, or become, a call site passing a different value.
    RawReference,
}

impl Caveat {
    fn as_str(self) -> &'static str {
        match self {
            Self::WiderThanPrivate => "visible outside its module",
            Self::UnknownVisibility => "export status unknown for this language",
            Self::UnanalyzedCallSites => "call site(s) could not be lined up",
            Self::AmbiguousInbound => "an ambiguous call site names it",
            Self::FallbackResolvedCall => "call sites attributed by name fallback",
            Self::TestsVaryValue => "tests pass a different value",
            Self::RawReference => "its bare name is written elsewhere",
        }
    }
}

/// Which claim a constant-argument row makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ConstantKind {
    /// Every analyzable production call site passes the same value.
    Constant,
    /// No production call site passes the position at all, so the
    /// declared default is the only value (Python / TypeScript).
    DefaultOnly,
}

/// The parameter a finding is about, spelled positionally so the row is
/// actionable even for a nameless slot.
#[derive(Debug, Clone, Serialize)]
struct ParameterRef {
    /// 0-based slot position in the declared parameter list (the
    /// receiver is not a slot).
    position: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

/// One constant-argument (or default-only) finding.
#[derive(Debug, Clone, Serialize)]
struct ConstantArgumentEntry {
    id: String,
    qualified_name: String,
    file: String,
    start_line: usize,
    module: String,
    visibility: NodeVisibility,
    parameter: ParameterRef,
    kind: ConstantKind,
    /// The one value, verbatim from the call sites. Absent for
    /// default-only rows — the value lives in the signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,
    /// Production call sites the claim is built on (the analyzable
    /// ones).
    call_site_count: usize,
    /// Distinct production callers behind those sites.
    caller_count: usize,
    /// Source lines of the analyzable call sites, per caller file
    /// ordering of the graph.
    call_lines: Vec<usize>,
    /// Call sites that could not be lined up against the parameter
    /// list. Non-zero carries the unanalyzed-call-sites caveat.
    unanalyzed_call_site_count: usize,
    /// Analyzable test call sites (informational; a differing value
    /// carries the tests-vary caveat).
    test_site_count: usize,
    /// Occurrences of the bare name outside the definition and its
    /// known callers. Non-zero carries the raw-reference caveat.
    raw_reference_count: usize,
    /// Why this row is weaker than its presence suggests, sorted.
    caveats: Vec<Caveat>,
}

/// One dead-parameter finding.
#[derive(Debug, Clone, Serialize)]
struct DeadParameterEntry {
    id: String,
    qualified_name: String,
    file: String,
    start_line: usize,
    module: String,
    visibility: NodeVisibility,
    parameter: ParameterRef,
    caveats: Vec<Caveat>,
}

/// Functions never considered, by reason — emitted so "why is X not
/// listed?" has an answer in the report itself.
#[derive(Debug, Default, Serialize)]
struct Excluded {
    /// Rust trait `impl` methods and trait default bodies: the trait
    /// owns the signature.
    trait_method_count: usize,
    /// Go methods matching an in-scope interface's method set by name
    /// and arity: the interface owns the signature.
    interface_method_count: usize,
    /// Carries an annotation not on the language's inert list: a
    /// framework may call it with anything, and often owns the
    /// signature too.
    annotated_count: usize,
    /// Synthetic units (TS closures, harness callbacks): called through
    /// a binding no call-site name reaches.
    synthetic_count: usize,
}

/// What the analyzer had to skip inside the pool, so absence of a row
/// is auditable.
#[derive(Debug, Default, Serialize)]
struct Audit {
    /// Pool functions whose adapter extracted no parameter list.
    missing_signature_count: usize,
    /// Parameter slots with no single binding name (destructuring
    /// patterns, unnamed Go slots) — skipped by the dead check, still
    /// eligible for constant-argument findings.
    unnamed_parameter_count: usize,
    /// Call sites across the pool that could not be lined up against a
    /// parameter list.
    unanalyzed_call_site_count: usize,
}

/// How many parameters, across the whole tree, have 1 / 2 / 3+ distinct
/// values over their analyzable production call sites — what
/// `--min-call-sites` should be is read off this, not guessed.
#[derive(Debug, Default, Serialize)]
struct Calibration {
    /// Parameters with at least `min_call_sites` analyzable production
    /// call sites, every one providing a value.
    measured_parameter_count: usize,
    /// Of those, parameters whose every value is the same constant.
    one_value_count: usize,
    /// All values constant, but more than one distinct value.
    several_values_count: usize,
    /// At least one non-constant argument (an identifier, a computed
    /// expression) — the parameter genuinely varies.
    varying_count: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct Thresholds {
    min_call_sites: usize,
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    note: &'static str,
    /// All graph nodes, including test functions.
    node_count: usize,
    /// Nodes eligible for either question (non-test unless
    /// `--only-tests`, and not excluded below).
    candidate_pool_count: usize,
    thresholds: Thresholds,
    excluded: Excluded,
    audit: Audit,
    calibration: Calibration,
    /// Parameters that only ever receive one value, strongest claim
    /// first.
    constant_arguments: Vec<ConstantArgumentEntry>,
    /// Parameters the body never reads.
    dead_parameters: Vec<DeadParameterEntry>,
    /// Per-module call-site resolution counts. A module whose call
    /// sites mostly failed to resolve can hide call sites, which
    /// weakens every constant-argument claim inside it.
    modules: Vec<ModuleResolutionSummary>,
}

/// Why a node is never a candidate, however its parameters are used.
#[derive(Debug, Clone, Copy)]
enum Exclusion {
    TraitMethod,
    InterfaceMethod,
    Annotated,
    Synthetic,
}

impl Exclusion {
    fn count_slot(self, excluded: &mut Excluded) -> &mut usize {
        match self {
            Self::TraitMethod => &mut excluded.trait_method_count,
            Self::InterfaceMethod => &mut excluded.interface_method_count,
            Self::Annotated => &mut excluded.annotated_count,
            Self::Synthetic => &mut excluded.synthetic_count,
        }
    }
}

fn exclusion_of(node: &CallGraphNode, interfaces: &InterfaceIndex) -> Option<Exclusion> {
    if matches!(
        node.owner_kind,
        Some(lens_domain::OwnerKind::TraitImpl | lens_domain::OwnerKind::Trait)
    ) {
        return Some(Exclusion::TraitMethod);
    }
    if ExportLang::of(node) == Some(ExportLang::Go)
        && !interfaces.matching(node, ExportLang::Go).is_empty()
    {
        return Some(Exclusion::InterfaceMethod);
    }
    // A known non-inert annotation may mean a framework owns both the
    // signature and the call sites. `None` (TypeScript, Python: no
    // attribute extraction) is *not* excluded — excluding on "could not
    // tell" would empty both languages.
    if node.attributes.as_ref().is_some_and(|attributes| {
        let inert = node
            .graph_language()
            .unwrap_or(GraphLanguage::TypeScript)
            .inert_attribute_names();
        attributes
            .iter()
            .any(|attribute| !inert.contains(attribute))
    }) {
        return Some(Exclusion::Annotated);
    }
    // The walker mints `parent::closure#N` / `it#1("…")` names for
    // units no call-site name can reach; their parameters are bound at
    // the one place the closure is written.
    if node.name.contains('#') {
        return Some(Exclusion::Synthetic);
    }
    None
}

/// One analyzable call site's per-slot outcome: a value for the slot,
/// or a known omission (the caller relies on the default).
#[derive(Debug, Clone, PartialEq, Eq)]
enum SlotValue {
    Constant(String),
    NonConstant,
    Omitted,
}

/// Line one call site's arguments up against the parameter slots.
/// `Err(())` marks the site unanalyzable: a spread, an unmatched or
/// duplicate keyword, more arguments than slots, or an omission in a
/// language without argument defaults.
fn assign_site(
    site: &CallSiteFacts,
    has_receiver: bool,
    slot_names: &[Option<String>],
    allows_omission: bool,
) -> Result<Vec<SlotValue>, ()> {
    let mut args = site.arguments.as_slice();
    if has_receiver && !site.has_receiver_expression {
        // `Owner::method(receiver, …)` — the first argument fills the
        // receiver, not slot 0.
        let Some(rest) = args.split_first().map(|(_, rest)| rest) else {
            return Err(());
        };
        args = rest;
    }
    let mut slots: Vec<Option<SlotValue>> = vec![None; slot_names.len()];
    let mut positional = 0usize;
    let mut seen_keyword = false;
    for arg in args {
        match arg {
            ArgumentShape::Spread => return Err(()),
            ArgumentShape::Keyword { name, value } => {
                seen_keyword = true;
                let Some(index) = slot_names
                    .iter()
                    .position(|slot| slot.as_deref() == Some(name.as_str()))
                else {
                    return Err(());
                };
                if slots[index].replace(value_of(value)).is_some() {
                    return Err(());
                }
            }
            shape => {
                if seen_keyword || positional >= slots.len() {
                    return Err(());
                }
                slots[positional] = Some(value_of(shape));
                positional += 1;
            }
        }
    }
    if !allows_omission && slots.iter().any(Option::is_none) {
        return Err(());
    }
    Ok(slots
        .into_iter()
        .map(|slot| slot.unwrap_or(SlotValue::Omitted))
        .collect())
}

fn value_of(shape: &ArgumentShape) -> SlotValue {
    match shape.constant_text() {
        Some(text) => SlotValue::Constant(text.to_owned()),
        None => SlotValue::NonConstant,
    }
}

/// Per-callee inbound call-site facts off the edge sets.
#[derive(Debug, Default, Clone)]
struct InboundSites {
    /// Analyzable-candidate production sites (self-recursive sites
    /// included: a recursive call passes arguments like any other).
    production: Vec<CallSiteFacts>,
    production_callers: BTreeSet<usize>,
    /// Production call sites the edges counted, whether or not argument
    /// facts exist for them.
    production_call_count: usize,
    test: Vec<CallSiteFacts>,
    ambiguous_inbound_count: usize,
    fallback_resolved: bool,
    test_callers: BTreeSet<usize>,
}

impl InboundSites {
    fn collect(graph: &CallGraph, only_tests: bool) -> Vec<Self> {
        let index_by_id = graph.node_index_by_id();
        let mut per_node = vec![Self::default(); graph.nodes.len()];
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
            let acc = &mut per_node[to];
            // Mirrors `analyze single-use`: outside `--only-tests`, a
            // test caller is informational, never part of the claim.
            if graph.nodes[from].is_test && !only_tests {
                acc.test_callers.insert(from);
                acc.test.extend(edge.call_sites.iter().cloned());
                continue;
            }
            acc.production_callers.insert(from);
            acc.production_call_count += edge.call_count;
            acc.production.extend(edge.call_sites.iter().cloned());
            acc.fallback_resolved |= matches!(
                edge.resolution_method,
                Some(
                    ResolutionMethod::LastSegment
                        | ResolutionMethod::PathSuffix
                        | ResolutionMethod::CrateNarrowed
                )
            );
        }
        per_node
    }
}

/// One parameter's aggregate over the analyzable production sites.
/// Provided-site counts are implicit: every analyzable site records
/// exactly one outcome per slot, so `analyzable - omitted` sites
/// provided a value.
#[derive(Debug, Default)]
struct SlotAggregate {
    omitted: usize,
    non_constant: usize,
    values: BTreeSet<String>,
    lines: Vec<usize>,
}

/// A dead-check target: one named parameter and the span its mentions
/// are counted in.
struct DeadTarget {
    file: String,
    start_line: usize,
    end_line: usize,
    name: String,
    /// Index into [`Collected::dead_candidates`].
    slot: usize,
}

/// A raw-reference target: one function with a constant-argument
/// finding, and the spans whose mentions of its name are accounted for.
struct RawTarget {
    name: String,
    /// `(file, start_line, end_line)` spans of the definition and its
    /// known callers.
    allowed: Vec<(String, usize, usize)>,
}

/// Everything the source scan cannot compute: candidate rows waiting on
/// their scan-derived caveats, plus the targets.
struct Collected {
    pool_count: usize,
    excluded: Excluded,
    audit: Audit,
    calibration: Calibration,
    constant_entries: Vec<ConstantArgumentEntry>,
    /// Parallel to `constant_entries`: index into `raw_targets` whose
    /// count feeds each row.
    constant_raw_slots: Vec<usize>,
    raw_targets: Vec<RawTarget>,
    dead_candidates: Vec<DeadParameterEntry>,
    dead_targets: Vec<DeadTarget>,
}

impl Collected {
    fn collect(graph: &CallGraph, only_tests: bool, min_call_sites: usize) -> Self {
        let interfaces = InterfaceIndex::new(&graph.interfaces);
        let inbound = InboundSites::collect(graph, only_tests);
        let mut out = Self {
            pool_count: 0,
            excluded: Excluded::default(),
            audit: Audit::default(),
            calibration: Calibration::default(),
            constant_entries: Vec::new(),
            constant_raw_slots: Vec::new(),
            raw_targets: Vec::new(),
            dead_candidates: Vec::new(),
            dead_targets: Vec::new(),
        };
        let mut raw_slot_by_node: HashMap<usize, usize> = HashMap::new();

        for (idx, node) in graph.nodes.iter().enumerate() {
            if node.is_test && !only_tests {
                continue;
            }
            if let Some(reason) = exclusion_of(node, &interfaces) {
                *reason.count_slot(&mut out.excluded) += 1;
                continue;
            }
            out.pool_count += 1;
            let Some(slot_names) = node.param_names.as_ref() else {
                out.audit.missing_signature_count += 1;
                continue;
            };
            if slot_names.is_empty() {
                continue;
            }
            out.audit.unnamed_parameter_count +=
                slot_names.iter().filter(|name| name.is_none()).count();

            out.collect_dead_candidates(node, slot_names);
            out.collect_constant_candidates(
                graph,
                idx,
                slot_names,
                &inbound[idx],
                min_call_sites,
                &mut raw_slot_by_node,
            );
        }
        out
    }

    fn collect_dead_candidates(&mut self, node: &CallGraphNode, slot_names: &[Option<String>]) {
        for (position, name) in slot_names.iter().enumerate() {
            let Some(name) = name.as_deref() else {
                continue;
            };
            // `_`-prefixed names are the language-level "deliberately
            // unused" spelling; reporting them would nag about a choice
            // already made.
            if name.starts_with('_') || name == "self" || name == "cls" || name == "this" {
                continue;
            }
            self.dead_targets.push(DeadTarget {
                file: node.file.clone(),
                start_line: node.start_line,
                end_line: node.end_line,
                name: name.to_owned(),
                slot: self.dead_candidates.len(),
            });
            self.dead_candidates.push(DeadParameterEntry {
                id: node.id.clone(),
                qualified_name: node.qualified_name.clone(),
                file: node.file.clone(),
                start_line: node.start_line,
                module: node.module.clone(),
                visibility: node.visibility,
                parameter: ParameterRef {
                    position,
                    name: Some(name.to_owned()),
                },
                caveats: visibility_caveats(node),
            });
        }
    }

    fn collect_constant_candidates(
        &mut self,
        graph: &CallGraph,
        node_idx: usize,
        slot_names: &[Option<String>],
        inbound: &InboundSites,
        min_call_sites: usize,
        raw_slot_by_node: &mut HashMap<usize, usize>,
    ) {
        let node = &graph.nodes[node_idx];
        let has_receiver = node.has_receiver.unwrap_or(false);
        let allows_omission = matches!(
            node.graph_language(),
            Some(GraphLanguage::Python | GraphLanguage::TypeScript)
        );

        let mut aggregates: Vec<SlotAggregate> = (0..slot_names.len())
            .map(|_| SlotAggregate::default())
            .collect();
        let mut analyzable = 0usize;
        let mut unanalyzed = inbound
            .production_call_count
            .saturating_sub(inbound.production.len());
        for site in &inbound.production {
            match assign_site(site, has_receiver, slot_names, allows_omission) {
                Ok(slots) => {
                    analyzable += 1;
                    for (aggregate, value) in aggregates.iter_mut().zip(slots) {
                        aggregate.lines.push(site.line);
                        match value {
                            SlotValue::Constant(text) => {
                                aggregate.values.insert(text);
                            }
                            SlotValue::NonConstant => aggregate.non_constant += 1,
                            SlotValue::Omitted => aggregate.omitted += 1,
                        }
                    }
                }
                Err(()) => unanalyzed += 1,
            }
        }
        self.audit.unanalyzed_call_site_count += unanalyzed;

        // Test sites are analyzed the same way but never join the
        // claim; they only witness against it.
        let mut test_slots: Vec<Vec<SlotValue>> = Vec::new();
        for site in &inbound.test {
            if let Ok(slots) = assign_site(site, has_receiver, slot_names, allows_omission) {
                test_slots.push(slots);
            }
        }

        for (position, aggregate) in aggregates.iter().enumerate() {
            if analyzable < min_call_sites {
                continue;
            }
            // Every analyzable site records exactly one of
            // provided/omitted per slot, so zero omissions means every
            // site provided a value.
            let fully_provided = aggregate.omitted == 0;
            if fully_provided {
                self.calibration.measured_parameter_count += 1;
                if aggregate.non_constant > 0 {
                    self.calibration.varying_count += 1;
                } else if aggregate.values.len() == 1 {
                    self.calibration.one_value_count += 1;
                } else {
                    self.calibration.several_values_count += 1;
                }
            }

            let (kind, value) = if fully_provided
                && aggregate.non_constant == 0
                && aggregate.values.len() == 1
            {
                (
                    ConstantKind::Constant,
                    aggregate.values.iter().next().cloned(),
                )
            } else if aggregate.omitted == analyzable && aggregate.omitted > 0 && allows_omission {
                (ConstantKind::DefaultOnly, None)
            } else {
                continue;
            };

            let tests_vary = test_slots.iter().any(|slots| {
                slots.get(position).is_some_and(|slot| match kind {
                    ConstantKind::Constant => {
                        !matches!(slot, SlotValue::Constant(text) if Some(text) == value.as_ref())
                    }
                    ConstantKind::DefaultOnly => !matches!(slot, SlotValue::Omitted),
                })
            });

            let mut caveats = visibility_caveats(node);
            if unanalyzed > 0 {
                caveats.push(Caveat::UnanalyzedCallSites);
            }
            if inbound.ambiguous_inbound_count > 0 {
                caveats.push(Caveat::AmbiguousInbound);
            }
            if inbound.fallback_resolved {
                caveats.push(Caveat::FallbackResolvedCall);
            }
            if tests_vary {
                caveats.push(Caveat::TestsVaryValue);
            }
            caveats.sort_unstable();

            let mut call_lines = aggregate.lines.clone();
            call_lines.sort_unstable();
            call_lines.dedup();

            let raw_slot = *raw_slot_by_node.entry(node_idx).or_insert_with(|| {
                let span_of = |n: &CallGraphNode| (n.file.clone(), n.start_line, n.end_line);
                let mut allowed = vec![span_of(node)];
                allowed.extend(
                    inbound
                        .production_callers
                        .iter()
                        .chain(&inbound.test_callers)
                        .map(|&caller| span_of(&graph.nodes[caller])),
                );
                self.raw_targets.push(RawTarget {
                    name: node.name.clone(),
                    allowed,
                });
                self.raw_targets.len() - 1
            });
            self.constant_raw_slots.push(raw_slot);
            self.constant_entries.push(ConstantArgumentEntry {
                id: node.id.clone(),
                qualified_name: node.qualified_name.clone(),
                file: node.file.clone(),
                start_line: node.start_line,
                module: node.module.clone(),
                visibility: node.visibility,
                parameter: ParameterRef {
                    position,
                    name: slot_names[position].clone(),
                },
                kind,
                value,
                call_site_count: analyzable,
                caller_count: inbound.production_callers.len(),
                call_lines,
                unanalyzed_call_site_count: unanalyzed,
                test_site_count: test_slots.len(),
                raw_reference_count: 0,
                caveats,
            });
        }
    }
}

fn visibility_caveats(node: &CallGraphNode) -> Vec<Caveat> {
    match ExportLang::of(node) {
        Some(lang) if node.visibility != lang.private() => vec![Caveat::WiderThanPrivate],
        Some(_) => Vec::new(),
        None => vec![Caveat::UnknownVisibility],
    }
}

/// One tokenizing pass over every scanned source file, answering both
/// textual questions at once: how often each dead-check parameter name
/// appears inside its function's span, and how often each
/// constant-finding function's bare name appears outside the spans its
/// callers already account for.
struct SourceScan {
    /// Parallel to [`Collected::dead_candidates`]: identifier
    /// occurrences of the parameter name inside the function span. 1 is
    /// the declaration alone — never read.
    dead_counts: Vec<usize>,
    /// Parallel to [`Collected::raw_targets`].
    raw_counts: Vec<usize>,
}

impl SourceScan {
    fn run(
        builder: &CallGraphBuilder,
        roots: &AnalyzeRoots,
        dead_targets: &[DeadTarget],
        raw_targets: &[RawTarget],
    ) -> Result<Self, AnalyzerError> {
        let mut dead_counts = vec![0usize; dead_targets.len()];
        let mut raw_counts = vec![0usize; raw_targets.len()];
        if dead_targets.is_empty() && raw_targets.is_empty() {
            return Ok(Self {
                dead_counts,
                raw_counts,
            });
        }

        let mut dead_by_file: HashMap<&str, Vec<&DeadTarget>> = HashMap::new();
        for target in dead_targets {
            dead_by_file
                .entry(target.file.as_str())
                .or_default()
                .push(target);
        }
        let mut raw_slots_by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut raw_allowed_by_file: HashMap<&str, Vec<(usize, usize, usize)>> = HashMap::new();
        for (slot, target) in raw_targets.iter().enumerate() {
            raw_slots_by_name
                .entry(target.name.as_str())
                .or_default()
                .push(slot);
            for (file, start, end) in &target.allowed {
                raw_allowed_by_file
                    .entry(file.as_str())
                    .or_default()
                    .push((*start, *end, slot));
            }
        }

        builder.visit_source_texts(roots, |file, source| {
            let dead = dead_by_file.get(file).map(Vec::as_slice);
            let raw_allowed = raw_allowed_by_file.get(file).map(Vec::as_slice);
            for (offset, line) in source.lines().enumerate() {
                let line_no = offset + 1;
                for token in identifiers(line) {
                    if let Some(targets) = dead {
                        for target in targets {
                            if target.name == token
                                && target.start_line <= line_no
                                && line_no <= target.end_line
                            {
                                dead_counts[target.slot] += 1;
                            }
                        }
                    }
                    if let Some(slots) = raw_slots_by_name.get(token) {
                        for &slot in slots {
                            let accounted = raw_allowed.is_some_and(|spans| {
                                spans.iter().any(|&(start, end, s)| {
                                    s == slot && start <= line_no && line_no <= end
                                })
                            });
                            if !accounted {
                                raw_counts[slot] += 1;
                            }
                        }
                    }
                }
            }
        })?;
        Ok(Self {
            dead_counts,
            raw_counts,
        })
    }
}

impl Report {
    fn build(
        roots: &AnalyzeRoots,
        graph: &CallGraph,
        collected: Collected,
        scan: &SourceScan,
        min_call_sites: usize,
    ) -> Self {
        let Collected {
            pool_count,
            excluded,
            audit,
            calibration,
            mut constant_entries,
            constant_raw_slots,
            raw_targets: _,
            dead_candidates,
            dead_targets,
        } = collected;

        for (entry, &raw_slot) in constant_entries.iter_mut().zip(&constant_raw_slots) {
            let count = scan.raw_counts[raw_slot];
            if count > 0 {
                entry.raw_reference_count = count;
                entry.caveats.push(Caveat::RawReference);
                entry.caveats.sort_unstable();
            }
        }
        constant_entries.sort_by(|a, b| {
            a.caveats
                .len()
                .cmp(&b.caveats.len())
                .then_with(|| b.call_site_count.cmp(&a.call_site_count))
                .then_with(|| a.id.cmp(&b.id))
                .then_with(|| a.parameter.position.cmp(&b.parameter.position))
        });

        let mut dead_parameters: Vec<DeadParameterEntry> = dead_targets
            .iter()
            .filter(|target| scan.dead_counts[target.slot] <= 1)
            .map(|target| dead_candidates[target.slot].clone())
            .collect();
        dead_parameters.sort_by(|a, b| {
            a.caveats
                .len()
                .cmp(&b.caveats.len())
                .then_with(|| a.id.cmp(&b.id))
                .then_with(|| a.parameter.position.cmp(&b.parameter.position))
        });

        Self {
            schema_version: SCHEMA_VERSION,
            root: roots.display(),
            language: graph.language,
            note: NOTE,
            node_count: graph.nodes.len(),
            candidate_pool_count: pool_count,
            thresholds: Thresholds { min_call_sites },
            excluded,
            audit,
            calibration,
            constant_arguments: constant_entries,
            dead_parameters,
            modules: graph.module_summary.clone(),
        }
    }
}

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    let mut out = format!(
        "# Parameters report: {} ({} function(s) in pool, {} constant-argument finding(s), {} dead parameter(s))\n",
        report.root,
        report.candidate_pool_count,
        report.constant_arguments.len(),
        report.dead_parameters.len(),
    );
    let _ = writeln!(out, "\n{NOTE}\n");
    let _ = writeln!(
        out,
        "Threshold: min call sites = {}. Excluded outright: {} trait method(s), {} \
         interface-matching method(s), {} annotated, {} synthetic. Skipped inside the pool: {} \
         function(s) without a parameter list, {} nameless slot(s), {} call site(s) that could \
         not be lined up.",
        report.thresholds.min_call_sites,
        report.excluded.trait_method_count,
        report.excluded.interface_method_count,
        report.excluded.annotated_count,
        report.excluded.synthetic_count,
        report.audit.missing_signature_count,
        report.audit.unnamed_parameter_count,
        report.audit.unanalyzed_call_site_count,
    );
    if report.candidate_pool_count == 0 {
        out.push_str("\n_No functions to analyze._\n");
        return out;
    }

    let _ = writeln!(out, "\n## Constant arguments (top {limit})");
    out.push_str(
        "\nEvery analyzable production call site passes the value shown (or, for default-only \
         rows, none passes the position at all). Verify with a bare-name search, then inline \
         the value and drop the parameter — or say why the seam earns its keep.\n",
    );
    render_constant_entries(&mut out, &report.constant_arguments, limit);

    let _ = writeln!(out, "\n## Dead parameters (top {limit})");
    out.push_str(
        "\nThe body never mentions the name past its declaration. Drop the parameter and \
         update the call sites; a caveat means callers outside the analyzed tree may exist.\n",
    );
    render_dead_entries(&mut out, &report.dead_parameters, limit);

    let _ = writeln!(out, "\n## Threshold calibration");
    let _ = writeln!(
        out,
        "\nOver {} parameter(s) with >= {} analyzable call sites (all providing a value): {} \
         always receive one value, {} receive several distinct constants, {} genuinely vary.",
        report.calibration.measured_parameter_count,
        report.thresholds.min_call_sites,
        report.calibration.one_value_count,
        report.calibration.several_values_count,
        report.calibration.varying_count,
    );
    out.push_str(
        "\nSet `--min-call-sites` (or `[profile.<name>.parameters]`) off these counts: raising \
         it trades findings for confidence that \"always\" is not an accident of few callers.\n",
    );

    render_module_confidence(
        &mut out,
        &report.modules,
        "Call sites in these modules often failed to resolve; a hidden call site is likeliest \
         there, so treat their rows with extra suspicion.",
    );
    out
}

fn render_constant_entries(out: &mut String, entries: &[ConstantArgumentEntry], limit: usize) {
    if entries.is_empty() {
        out.push_str("\n_None._\n");
        return;
    }
    out.push('\n');
    for entry in entries.iter().take(limit) {
        let parameter = entry.parameter.name.as_deref().map_or_else(
            || format!("#{}", entry.parameter.position),
            |n| format!("`{n}` (#{})", entry.parameter.position),
        );
        let _ = write!(
            out,
            "- `{}` ({}:{}) parameter {}: ",
            entry.qualified_name, entry.file, entry.start_line, parameter,
        );
        match (&entry.kind, entry.value.as_deref()) {
            (ConstantKind::Constant, Some(value)) => {
                let _ = write!(out, "always `{value}`");
            }
            _ => {
                let _ = write!(out, "never passed — the default is the only value");
            }
        }
        let _ = write!(
            out,
            " across {} site(s) in {} caller(s)",
            entry.call_site_count, entry.caller_count,
        );
        if entry.test_site_count > 0 {
            let _ = write!(out, ", +{} test site(s)", entry.test_site_count);
        }
        if entry.unanalyzed_call_site_count > 0 {
            let _ = write!(out, ", {} unanalyzed", entry.unanalyzed_call_site_count);
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

fn render_dead_entries(out: &mut String, entries: &[DeadParameterEntry], limit: usize) {
    if entries.is_empty() {
        out.push_str("\n_None._\n");
        return;
    }
    out.push('\n');
    for entry in entries.iter().take(limit) {
        let name = entry.parameter.name.as_deref().unwrap_or("?");
        let _ = write!(
            out,
            "- `{}` ({}:{}) parameter `{}` (#{}): never read in the body",
            entry.qualified_name, entry.file, entry.start_line, name, entry.parameter.position,
        );
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
    use rstest::rstest;
    use serde_json::Value;
    use std::path::Path;

    fn analyze_json(path: &Path) -> Value {
        analyze_json_with(path, ParametersAnalyzer::new())
    }

    fn analyze_json_with(path: &Path, analyzer: ParametersAnalyzer) -> Value {
        let json = analyzer.analyze(path, OutputFormat::Json).unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn constant_row<'a>(report: &'a Value, name_suffix: &str, position: u64) -> Option<&'a Value> {
        report["constant_arguments"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e["qualified_name"]
                    .as_str()
                    .is_some_and(|q| q.ends_with(name_suffix))
                    && e["parameter"]["position"] == position
            })
    }

    fn dead_row<'a>(report: &'a Value, name_suffix: &str, param: &str) -> Option<&'a Value> {
        report["dead_parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| {
                e["qualified_name"]
                    .as_str()
                    .is_some_and(|q| q.ends_with(name_suffix))
                    && e["parameter"]["name"] == param
            })
    }

    #[test]
    fn a_parameter_receiving_one_literal_everywhere_is_a_constant_argument() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn emit(level: u32, msg: &str) { let _ = (level, msg); }\n\
             pub fn a(msg: &str) { emit(3, msg); }\n\
             pub fn b(msg: &str) { emit(3, msg); }\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "::emit", 0).expect("level listed");
        assert_eq!(row["kind"], "constant");
        assert_eq!(row["value"], "3");
        assert_eq!(row["call_site_count"], 2);
        assert_eq!(row["caller_count"], 2);
        assert_eq!(row["caveats"], serde_json::json!([]));
        // `msg` is an identifier at every site: it varies by scope, so
        // no row — and the calibration counts it as varying.
        assert!(constant_row(&report, "::emit", 1).is_none());
        assert_eq!(report["calibration"]["measured_parameter_count"], 2);
        assert_eq!(report["calibration"]["one_value_count"], 1);
        assert_eq!(report["calibration"]["varying_count"], 1);
    }

    #[test]
    fn distinct_literals_are_not_a_finding_but_calibrate() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn emit(level: u32) { let _ = level; }\n\
             pub fn a() { emit(3); }\n\
             pub fn b() { emit(4); }\n",
        );

        let report = analyze_json(dir.path());
        assert!(constant_row(&report, "::emit", 0).is_none());
        assert_eq!(report["calibration"]["several_values_count"], 1);
    }

    #[test]
    fn a_single_call_site_is_below_the_default_threshold() {
        // One call site is trivially constant — that is `single-use`'s
        // finding. Lowering the floor to 1 surfaces it here.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn emit(level: u32) { let _ = level; }\n\
             pub fn a() { emit(3); }\n",
        );

        let report = analyze_json(dir.path());
        assert!(constant_row(&report, "::emit", 0).is_none());

        let relaxed = analyze_json_with(
            dir.path(),
            ParametersAnalyzer::new().with_min_call_sites(Some(1)),
        );
        assert!(constant_row(&relaxed, "::emit", 0).is_some());
    }

    #[test]
    fn constant_paths_count_like_literals() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub enum Color { Red }\n\
             fn paint(color: Color) { let _ = color; }\n\
             pub fn a() { paint(Color::Red); }\n\
             pub fn b() { paint(Color::Red); }\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "::paint", 0).expect("color listed");
        assert_eq!(row["value"], "Color::Red");
    }

    #[test]
    fn receiver_and_path_calls_line_up_on_the_same_slots() {
        // `s.m(3)` and `S::m(&s, 3)` both put `3` in slot 0: the path
        // call's first argument fills the receiver, not the slot.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub struct S;\n\
             impl S { fn m(&self, x: u32) { let _ = x; } }\n\
             pub fn a(s: &S) { s.m(3); }\n\
             pub fn b(s: &S) { S::m(s, 3); }\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "S::m", 0).expect("x listed");
        assert_eq!(row["value"], "3");
        assert_eq!(row["call_site_count"], 2);
    }

    #[test]
    fn python_keywords_map_by_name_and_omission_is_default_only() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/app.py",
            "def emit(level, mode=\"w\", flag=True):\n    return (level, mode, flag)\n\
             \n\
             def a():\n    emit(3, mode=\"r\")\n\
             \n\
             def b():\n    emit(3, mode=\"r\")\n",
        );

        let report = analyze_json(dir.path());
        let level = constant_row(&report, "::emit", 0).expect("level listed");
        assert_eq!(level["value"], "3");
        let mode = constant_row(&report, "::emit", 1).expect("mode listed");
        assert_eq!(mode["value"], "\"r\"");
        let flag = constant_row(&report, "::emit", 2).expect("flag listed");
        assert_eq!(flag["kind"], "default_only");
        assert_eq!(flag.get("value").and_then(Value::as_str), None);
        // Python export status is unknown, so every row carries that
        // caveat rather than claiming a closed world.
        assert!(
            level["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "unknown_visibility"),
            "got {level:?}",
        );
    }

    #[test]
    fn an_omitted_argument_in_rust_marks_the_site_unanalyzable() {
        // Rust has no argument defaults: a call with fewer arguments
        // than slots did not resolve to this signature at all, so the
        // site is skipped with a caveat instead of read as an omission.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod x { pub fn emit(level: u32, extra: u32) { let _ = (level, extra); } }\n\
             mod y { pub fn emit(level: u32) { let _ = level; } }\n\
             pub fn a() { x::emit(3, 4); }\n\
             pub fn b() { x::emit(3, 4); }\n\
             pub fn c() { y::emit(3); }\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "x::emit", 0).expect("level listed");
        assert_eq!(row["unanalyzed_call_site_count"], 0);
    }

    #[test]
    fn a_spread_call_site_demotes_with_a_caveat() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/app.py",
            "def emit(level):\n    return level\n\
             \n\
             def a():\n    emit(3)\n\
             \n\
             def b():\n    emit(3)\n\
             \n\
             def c(args):\n    emit(*args)\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "::emit", 0).expect("level listed");
        assert_eq!(row["unanalyzed_call_site_count"], 1);
        assert_eq!(report["audit"]["unanalyzed_call_site_count"], 1);
        assert!(
            row["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "unanalyzed_call_sites"),
            "got {row:?}",
        );
    }

    #[test]
    fn more_arguments_than_slots_mark_the_site_unanalyzable() {
        // A call passing more arguments than the resolved signature
        // declares did not really resolve to it (a variadic or a
        // same-named overload); the site is skipped with a count
        // instead of shifting positions.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/app.py",
            "def emit(level):\n    return level\n\
             \n\
             def a():\n    emit(3)\n\
             \n\
             def b():\n    emit(3)\n\
             \n\
             def c():\n    emit(3, 4)\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "::emit", 0).expect("level listed");
        assert_eq!(row["call_site_count"], 2);
        assert_eq!(row["unanalyzed_call_site_count"], 1);
    }

    #[test]
    fn a_fallback_resolved_caller_is_a_caveat() {
        // `self.inner.helper(3)` resolves through the last-segment
        // fallback, so even the counted call sites are heuristic.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub struct Inner;\n\
             impl Inner { fn helper(&self, level: u32) { let _ = level; } }\n\
             pub struct S { inner: Inner }\n\
             impl S {\n\
                 pub fn a(&self) { self.inner.helper(3); }\n\
                 pub fn b(&self) { self.inner.helper(3); }\n\
             }\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "Inner::helper", 0).expect("level listed");
        assert!(
            row["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "fallback_resolved_call"),
            "got {row:?}",
        );
    }

    #[test]
    fn mixed_omission_and_provision_is_not_default_only() {
        // One caller relies on the default, another passes a value: the
        // default is not the only value, and the provided value cannot
        // claim "every site" either. No finding.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/app.py",
            "def emit(level, flag=True):\n    return (level, flag)\n\
             \n\
             def a():\n    emit(1, flag=False)\n\
             \n\
             def b():\n    emit(2)\n",
        );

        let report = analyze_json(dir.path());
        assert!(constant_row(&report, "::emit", 1).is_none(), "{report:?}");
    }

    #[test]
    fn a_zero_site_floor_does_not_invent_default_only_rows() {
        // `--min-call-sites 0` is a degenerate floor; an uncalled
        // Python function must still not read as "always defaulted".
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/app.py",
            "def emit(level, flag=True):\n    return (level, flag)\n",
        );

        let report = analyze_json_with(
            dir.path(),
            ParametersAnalyzer::new().with_min_call_sites(Some(0)),
        );
        assert_eq!(report["constant_arguments"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_test_omitting_a_default_only_parameter_does_not_caveat() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/app.py",
            "def emit(level, flag=True):\n    return (level, flag)\n\
             \n\
             def a():\n    emit(1)\n\
             \n\
             def b():\n    emit(2)\n\
             \n\
             def test_emit():\n    emit(3)\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "::emit", 1).expect("flag listed");
        assert_eq!(row["kind"], "default_only");
        assert!(
            !row["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "tests_vary_value"),
            "got {row:?}",
        );
    }

    #[test]
    fn a_test_passing_a_default_only_parameter_is_a_seam_caveat() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/app.py",
            "def emit(level, flag=True):\n    return (level, flag)\n\
             \n\
             def a():\n    emit(1)\n\
             \n\
             def b():\n    emit(2)\n\
             \n\
             def test_emit():\n    emit(3, flag=False)\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "::emit", 1).expect("flag listed");
        assert_eq!(row["kind"], "default_only");
        assert!(
            row["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "tests_vary_value"),
            "got {row:?}",
        );
    }

    #[test]
    fn a_test_passing_a_different_value_is_a_seam_caveat() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn emit(level: u32) { let _ = level; }\n\
             pub fn a() { emit(3); }\n\
             pub fn b() { emit(3); }\n\
             #[cfg(test)]\n\
             mod tests { fn t() { crate::emit(9); } }\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "::emit", 0).expect("level listed");
        assert_eq!(row["test_site_count"], 1);
        assert!(
            row["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "tests_vary_value"),
            "got {row:?}",
        );
    }

    #[test]
    fn a_test_passing_the_same_value_does_not_caveat() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn emit(level: u32) { let _ = level; }\n\
             pub fn a() { emit(3); }\n\
             pub fn b() { emit(3); }\n\
             #[cfg(test)]\n\
             mod tests { fn t() { crate::emit(3); } }\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "::emit", 0).expect("level listed");
        assert!(
            !row["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "tests_vary_value"),
            "got {row:?}",
        );
    }

    #[test]
    fn a_raw_name_reference_outside_known_callers_demotes() {
        // The call inside `format!` arguments produces no call edge, so
        // the graph sees two constant sites; the raw-name scan is what
        // says a hidden site can pass something else.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn emit(level: u32) -> u32 { level }\n\
             pub fn a() { emit(3); }\n\
             pub fn b() { emit(3); }\n\
             pub fn c() -> String { format!(\"{}\", emit(9)) }\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "::emit", 0).expect("level listed");
        assert!(row["raw_reference_count"].as_u64().unwrap() > 0);
        assert!(
            row["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "raw_reference"),
            "got {row:?}",
        );
    }

    #[test]
    fn a_parameter_the_body_never_reads_is_dead() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn pick(a: u32, b: u32, _c: u32) -> u32 { a }\n\
             pub fn caller() { pick(1, 2, 3); }\n",
        );

        let report = analyze_json(dir.path());
        let row = dead_row(&report, "::pick", "b").expect("b listed");
        assert_eq!(row["parameter"]["position"], 1);
        // Public: callers outside the tree need the edit too.
        assert!(
            row["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "wider_than_private"),
            "got {row:?}",
        );
        // `a` is read; `_c` is already spelled deliberately unused.
        assert!(dead_row(&report, "::pick", "a").is_none());
        assert!(dead_row(&report, "::pick", "_c").is_none());
    }

    #[rstest]
    #[case::go(
        "src/lib.go",
        "package p\n\nfunc pick(a int, b int) int { return a }\n\nfunc Caller() int { return pick(1, 2) }\n",
        "::pick"
    )]
    #[case::typescript(
        "src/lib.ts",
        "function pick(a: number, b: number): number { return a; }\nexport function caller(): number { return pick(1, 2); }\n",
        "::pick"
    )]
    #[case::python(
        "src/lib.py",
        "def pick(a, b):\n    return a\n\ndef caller():\n    return pick(1, 2)\n",
        "::pick"
    )]
    fn dead_parameters_are_found_in_every_language(
        #[case] file: &str,
        #[case] source: &str,
        #[case] suffix: &str,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), file, source);

        let report = analyze_json(dir.path());
        assert!(dead_row(&report, suffix, "b").is_some(), "got {report:?}",);
        assert!(dead_row(&report, suffix, "a").is_none());
    }

    /// The receiver spellings are skipped per name, not per language:
    /// a Go parameter literally named `self` or `cls` is (rare but)
    /// unread here, and still stays out of the dead list.
    #[test]
    fn receiver_spelled_names_are_never_dead_candidates() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.go",
            "package p\n\nfunc handle(self int, cls int) int { return 0 }\n\nfunc Use() int { return handle(1, 2) }\n",
        );

        let report = analyze_json(dir.path());
        assert!(dead_row(&report, "::handle", "self").is_none());
        assert!(dead_row(&report, "::handle", "cls").is_none());
    }

    #[test]
    fn nameless_slots_are_skipped_with_an_audit_count() {
        // A destructuring pattern is one positional slot with no single
        // binding name: the dead check cannot ask about it, and the
        // audit says so rather than dropping it silently.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.ts",
            "function pick({a}: {a: number}): number { return a; }\n\
             export function caller(): number { return pick({a: 1}); }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["audit"]["unnamed_parameter_count"], 1);
        assert_eq!(report["dead_parameters"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_mention_in_a_string_counts_as_a_read() {
        // The check is textual and leans toward under-reporting: any
        // occurrence of the name past the declaration keeps the row out.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.py",
            "def pick(a, b):\n    return \"b was here\" + a\n",
        );

        let report = analyze_json(dir.path());
        assert!(dead_row(&report, "::pick", "b").is_none());
    }

    #[test]
    fn trait_methods_and_annotated_functions_are_excluded() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "trait Greet { fn hi(&self, unused: u32); }\n\
             struct S;\n\
             impl Greet for S { fn hi(&self, unused: u32) {} }\n\
             #[no_mangle]\n\
             fn hooked(unused: u32) {}\n\
             pub fn caller(s: S) { Greet::hi(&s, 1); hooked(2); }\n",
        );

        let report = analyze_json(dir.path());
        assert!(dead_row(&report, "::hi", "unused").is_none());
        assert!(dead_row(&report, "::hooked", "unused").is_none());
        assert_eq!(report["excluded"]["trait_method_count"], 1);
        assert_eq!(report["excluded"]["annotated_count"], 1);
    }

    #[test]
    fn go_interface_matching_methods_are_excluded() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.go",
            "package p\n\n\
             type Greeter interface {\n\
             \tGreet(x int) int\n\
             }\n\n\
             type T struct{}\n\n\
             func (t T) Greet(x int) int { return 0 }\n\n\
             func Use(t T) int { return t.Greet(1) + t.Greet(1) }\n",
        );

        let report = analyze_json(dir.path());
        assert!(dead_row(&report, "::Greet", "x").is_none());
        assert!(constant_row(&report, "::Greet", 0).is_none());
        assert_eq!(report["excluded"]["interface_method_count"], 1);
    }

    #[test]
    fn go_constant_arguments_resolve_across_receiver_methods() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.go",
            "package p\n\n\
             type S struct{}\n\n\
             func (s S) Emit(level int) int { return level }\n\n\
             func A(s S) int { return s.Emit(3) }\n\n\
             func B(s S) int { return s.Emit(3) }\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "S::Emit", 0).expect("level listed");
        assert_eq!(row["value"], "3");
        assert_eq!(row["call_site_count"], 2);
    }

    #[test]
    fn markdown_renders_both_sections_and_caveat_text() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn emit(level: u32, dead: u32) -> u32 { level }\n\
             pub fn a() { emit(3, 1); }\n\
             pub fn b() { emit(3, 2); }\n",
        );

        let md = ParametersAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("## Constant arguments"), "got: {md}");
        assert!(md.contains("always `3`"), "got: {md}");
        assert!(md.contains("across 2 site(s) in 2 caller(s)"), "got: {md}");
        assert!(md.contains("## Dead parameters"), "got: {md}");
        assert!(
            md.contains("parameter `dead` (#1): never read in the body"),
            "got: {md}",
        );
        assert!(md.contains("## Threshold calibration"), "got: {md}");
        // Suffixes render only where they carry information; a private
        // Rust row with clean claims carries none of them, and no
        // caveat separator either.
        assert!(!md.contains("test site(s)"), "got: {md}");
        assert!(!md.contains("0 unanalyzed"), "got: {md}");
        assert!(!md.contains("raw refs="), "got: {md}");
        for line in md.lines().filter(|l| l.starts_with("- `crate::emit`")) {
            assert!(!line.contains(" — "), "got: {line}");
        }
    }

    #[test]
    fn markdown_renders_row_suffixes_and_caveat_text() {
        // `emit` is public (wider-than-private caveat text must render
        // verbatim), has a test caller, a spread site, and its name in
        // a comment outside any known caller — every markdown suffix at
        // once, on both row kinds.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn emit(level: u32, dead: u32) -> u32 { level }\n\
             pub fn a() { emit(3, 1); }\n\
             pub fn b() { emit(3, 2); }\n\
             // emit is also named here, outside every caller.\n\
             #[cfg(test)]\n\
             mod tests { fn t() { crate::emit(3, 9); } }\n",
        );

        let md = ParametersAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains(", +1 test site(s)"), "got: {md}");
        assert!(md.contains(", raw refs="), "got: {md}");
        assert!(md.contains("visible outside its module"), "got: {md}");
        assert!(
            md.contains("its bare name is written elsewhere"),
            "got: {md}"
        );
        // The dead row carries the caveat separator with the text.
        let dead_line = md
            .lines()
            .find(|l| l.contains("parameter `dead`"))
            .expect("dead row rendered");
        assert!(
            dead_line.contains(" — visible outside its module"),
            "got: {dead_line}"
        );
    }

    #[test]
    fn markdown_renders_the_unanalyzed_suffix() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/app.py",
            "def emit(level):\n    return level\n\
             \n\
             def a():\n    emit(3)\n\
             \n\
             def b():\n    emit(3)\n\
             \n\
             def c(args):\n    emit(*args)\n",
        );

        let md = ParametersAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains(", 1 unanalyzed"), "got: {md}");
    }

    /// The one edge the end-to-end fixtures cannot produce: a call
    /// shape whose adapter extracted no argument facts. The edge still
    /// counts call sites, so the gap must surface as an unanalyzed
    /// count rather than silently shrinking the claim's denominator —
    /// and a pool function without a parameter list must be audited,
    /// not skipped without trace.
    #[test]
    fn missing_argument_facts_and_signatures_are_audited() {
        use crate::analyze::call_graph::FileGraphInput;
        use lens_domain::{
            BodyShape, CallShape, FunctionShape, LexicalResolutionStatus, ParameterShape,
            ReceiverExprKind, ReceiverShape, SignatureShape, SourceSpan, SyntaxFact,
        };
        use std::sync::Arc;

        let signature = SignatureShape {
            name_tokens: SyntaxFact::Unknown,
            params: vec![ParameterShape {
                name: SyntaxFact::Known(Some("level".to_owned())),
                type_annotation: SyntaxFact::Unknown,
                type_paths: Vec::new(),
            }],
            return_type: SyntaxFact::Unknown,
            return_type_paths: Vec::new(),
            receiver: SyntaxFact::Known(ReceiverShape::None),
            generics: SyntaxFact::Unknown,
            bounds: SyntaxFact::Unknown,
        };
        let function =
            |name: &str, line: usize, signature: SyntaxFact<SignatureShape>| FunctionShape {
                display_name: name.to_owned(),
                qualified_name: SyntaxFact::Known(format!("m::{name}")),
                module_path: SyntaxFact::Known("m".to_owned()),
                owner: SyntaxFact::Known(None),
                visibility: SyntaxFact::Unknown,
                signature,
                doc: None,
                attributes: SyntaxFact::Unknown,
                body: BodyShape {
                    tree: lens_domain::TreeNode::leaf("Block"),
                },
                span: SourceSpan {
                    start_line: line,
                    end_line: line,
                },
                is_test: false,
            };
        let call = CallShape {
            caller_qualified_name: SyntaxFact::Known(Some("m::caller".to_owned())),
            caller_module: SyntaxFact::Known("m".to_owned()),
            caller_owner: SyntaxFact::Known(None),
            callee_display_name: SyntaxFact::Known(Some("target".to_owned())),
            callee_path_segments: SyntaxFact::Known(vec!["target".to_owned()]),
            receiver_expr_kind: SyntaxFact::Known(ReceiverExprKind::None),
            // The point of the fixture: the adapter did not extract
            // argument facts for this site.
            arguments: SyntaxFact::Unknown,
            callee_is_locally_bound: SyntaxFact::Known(false),
            lexical_resolution: LexicalResolutionStatus::NotAttempted,
            visible_imports: Vec::new(),
            line: 2,
        };
        let graph = CallGraph::build(
            vec![FileGraphInput {
                file: "src/lib.py".to_owned(),
                language: GraphLanguage::Python,
                module: "m".to_owned(),
                path_is_test: false,
                functions: Arc::new(vec![
                    function("target", 1, SyntaxFact::Known(signature)),
                    function("caller", 2, SyntaxFact::Unknown),
                ]),
                included: vec![true, true],
                calls: Arc::new(vec![call]),
                complexity: Arc::new(Vec::new()),
                wrappers: None,
                interfaces: Arc::new(Vec::new()),
            }],
            true,
        );

        let collected = Collected::collect(&graph, false, DEFAULT_MIN_CALL_SITES);
        assert_eq!(collected.audit.unanalyzed_call_site_count, 1);
        assert_eq!(collected.audit.missing_signature_count, 1);
    }

    #[test]
    fn top_caps_the_markdown_lists() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "fn one(x: u32) { let _ = x; }\n\
             fn two(y: u32) { let _ = y; }\n\
             pub fn a() { one(1); two(2); }\n\
             pub fn b() { one(1); two(2); }\n\
             pub fn c() { one(1); two(2); one(1); }\n",
        );

        let md = ParametersAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("top 1"), "got: {md}");
        // Sorted by call-site count within equal caveats, so `one`
        // (4 sites) survives the cap and `two` is cut from markdown.
        assert!(md.contains("`crate::one`"), "got: {md}");
        assert!(!md.contains("`crate::two`"), "got: {md}");
    }

    #[test]
    fn empty_tree_reports_an_empty_pool() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "README.md", "# not source\n");

        let md = ParametersAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("_No functions to analyze._"), "got: {md}");
    }

    #[test]
    fn only_tests_analyzes_the_test_corpus() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "#[cfg(test)]\n\
             mod tests {\n\
                 fn helper(level: u32) { let _ = level; }\n\
                 fn t1() { helper(3); }\n\
                 fn t2() { helper(3); }\n\
             }\n",
        );

        let report = analyze_json_with(dir.path(), ParametersAnalyzer::new().with_only_tests(true));
        assert!(constant_row(&report, "::helper", 0).is_some());
    }

    #[test]
    fn an_ambiguous_inbound_call_site_is_a_caveat() {
        // The bare `dup(9)` at crate root matches both module-level
        // definitions, so each keeps its resolved sites and gains an
        // ambiguous inbound edge that could pass a different value.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "mod a { fn dup(level: u32) { let _ = level; } pub fn c1() { dup(3); } pub fn c2() { dup(3); } }\n\
             mod b { fn dup(level: u32) { let _ = level; } pub fn c1() { dup(3); } pub fn c2() { dup(3); } }\n\
             pub fn wild() { dup(9); }\n\
             pub fn root() { a::c1(); a::c2(); b::c1(); b::c2(); }\n",
        );

        let report = analyze_json(dir.path());
        let row = constant_row(&report, "a::dup", 0).expect("still constant");
        assert!(
            row["caveats"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "ambiguous_inbound"),
            "got {row:?}",
        );
    }
}
