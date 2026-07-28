//! `analyze delegation` — chains of functions that only forward, and the
//! modules built out of them.
//!
//! `analyze wrapper` reports the one-hop case: a function whose body is
//! a forwarding call. This analyzer reports what that becomes when it
//! stacks. A chain `f1 → f2 → … → fk → terminus` where every `fi` only
//! forwards costs an agent `k` files to open before it reaches the one
//! function doing the work, so the terminus — not the head — is the
//! headline of every row.
//!
//! Classification is deliberately biased toward under-reporting: a
//! forwarder that also logs, locks, or validates is doing work, and
//! calling it a middle man would be wrong. A node is a delegator only
//! when all of this holds:
//!
//! - exactly one resolved outgoing target, and it is not itself;
//! - every other call site it makes is a trivial adapter — a name the
//!   language's own tables mark as ubiquitous (`.clone()`, `.into()`)
//!   or built in (`len`, `append`). One unrecognised extra call, one
//!   anonymous call site, and the function is not a delegator;
//! - a body of at most [`MAX_DELEGATOR_STATEMENTS`] statements with
//!   `cyclomatic == 1`. A function whose body facts were not extracted
//!   is counted as unclassified rather than assumed thin.
//!
//! Three exemptions sit on top, all of them removing a node from the
//! delegator set entirely (so a chain running through one is cut):
//! test functions, a module's sole public surface (a facade, Rust and
//! Go only — TypeScript and Python carry no extracted export status),
//! and a doc comment that says the function is deprecated.
//!
//! What the report cannot see:
//!
//! - Chains follow **resolved** edges. A delegator whose forwarding call
//!   the resolver could not attribute ends its chain there, so reported
//!   depths are lower bounds and some chains are missing outright.
//! - Per-language idioms differ. Rust is the strongest: its adapter
//!   extracts visibility, its wrapper detector understands the receiver
//!   forms, and its name tables are the most complete. Python
//!   properties and Go embedded-struct promotion are not modelled, so
//!   those languages under-report further.
//! - A delegator cycle has no head to walk from, so its members are
//!   counted and not listed.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use serde::Serialize;

use super::call_graph::model::{
    CallGraphNode, GraphLanguage, ModuleResolutionSummary, NodeVisibility, Resolution,
};
use super::call_graph::{CallGraph, CallGraphBuilder};
use super::format::render_module_confidence;
use super::{AnalyzerError, LineRange, OutputFormat, SourceLang, overlaps_any};

const SCHEMA_VERSION: u32 = 1;

/// Chains (and module rows) rendered in markdown when `--top` is not
/// given. JSON always carries every row.
const DEFAULT_TOP: usize = 20;

/// Body size a delegator may have. Above this the function is doing
/// something the chain report would be lying about.
const MAX_DELEGATOR_STATEMENTS: usize = 3;

/// Forwarding hops a chain needs to be reported. One hop is a wrapper,
/// which `analyze wrapper` already covers with argument-level evidence.
const MIN_CHAIN_HOPS: usize = 2;

/// Delegators a module needs before its shape is called a layer rather
/// than a coincidence.
const LASAGNA_MIN_DELEGATORS: usize = 3;

/// Share of a module's functions that must be delegators, and share of
/// those delegators that must point at the same other module, for the
/// module to be flagged as a delegation layer.
const LASAGNA_MIN_DELEGATOR_RATIO: f64 = 0.5;
const LASAGNA_MIN_CONCENTRATION: f64 = 0.5;

/// Hops listed per chain in markdown before the rest fold into a count.
const HOPS_PER_CHAIN: usize = 6;

/// What every row is relative to, stated in the output itself.
const NOTE: &str = "Candidates, not verdicts: a row says every hop between the head and the \
     terminus adds no logic the resolver could see, so the chain can collapse to a direct call. \
     Classification under-reports on purpose — a forwarder that also logs, locks, or validates is \
     not counted, and neither is one whose body facts were unavailable. Chains follow resolved \
     edges only, so a forwarding call the resolver could not attribute ends its chain early and \
     depths are lower bounds. A hop marked as forwarding its arguments verbatim carries the \
     language's own wrapper evidence; a hop without that mark was classified from body shape \
     alone and can still be composing rather than forwarding (a constructor calling a \
     constructor is the common case), so those are the rows to read first. Confidence is highest \
     on Rust; TypeScript and Python carry no extracted export status (no facade exemption) and \
     their idioms are not modelled.";

/// Analyzer entry point for `analyze delegation`.
#[derive(Debug, Clone)]
pub struct DelegationAnalyzer {
    builder: CallGraphBuilder,
    top: Option<usize>,
    diff_only: bool,
}

impl Default for DelegationAnalyzer {
    fn default() -> Self {
        Self {
            // The body facts are the classifier's whole basis, so this
            // analyzer is the one that pays for extracting them.
            builder: CallGraphBuilder::new().with_delegation_facts(true),
            top: None,
            diff_only: false,
        }
    }
}

impl DelegationAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Accepted for CLI uniformity. Test functions are never
    /// delegators — forwarding is what a test helper is for — so this
    /// leaves a report with nothing in it.
    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.builder = self.builder.with_only_tests(only_tests);
        self
    }

    /// Drops test files from the graph. Chains are unaffected in shape:
    /// test callers are never hops, they only call into one.
    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.builder = self.builder.with_exclude_tests(exclude_tests);
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.builder = self.builder.with_exclude_patterns(exclude);
        self
    }

    /// Cap the markdown listings to the top-N entries. JSON output
    /// always carries every row.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    /// Keep only chains with a hop (or a terminus) on an unstaged
    /// changed line. The module roll-up narrows to the modules those
    /// chains run through; the ratios inside it stay whole-module.
    pub fn with_diff_only(mut self, diff_only: bool) -> Self {
        self.diff_only = diff_only;
        self
    }

    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let graph = self.builder.build(path)?;
        let changed = self
            .diff_only
            .then(|| self.builder.changed_line_ranges_by_display_path(path))
            .transpose()?;
        let report = Report::build(path, &graph, changed.as_ref());
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).map_err(AnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&report, self.top)),
        }
    }
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    language: &'static str,
    /// What every row on this report is relative to.
    note: &'static str,
    audit: Audit,
    /// Forwarding chains of [`MIN_CHAIN_HOPS`] hops or more, deepest
    /// first.
    chains: Vec<Chain>,
    /// Per-module delegation density and where that delegation points.
    modules: Vec<ModuleRollup>,
    /// Per-module call-site resolution counts — the calibration layer:
    /// a module whose call sites mostly failed to resolve hides hops
    /// this analyzer never walked.
    resolution: Vec<ModuleResolutionSummary>,
    summary: Summary,
}

/// What was actually classified. Every chain is relative to it, so it
/// is emitted rather than assumed.
#[derive(Debug, Serialize)]
struct Audit {
    /// Non-test functions in scope — the denominator.
    function_count: usize,
    /// Functions classified as pure forwarders.
    delegator_count: usize,
    /// Delegators whose chain is a single hop. Those are wrappers;
    /// `analyze wrapper` reports them with argument-level evidence.
    single_hop_count: usize,
    /// Delegators inside a forwarding cycle. A cycle has no head to
    /// walk from, so its members are counted here instead of listed.
    cyclic_delegator_count: usize,
    /// Functions that forward but were left unclassified because a body
    /// fact (statement count, cyclomatic complexity) was missing.
    unclassified_forwarder_count: usize,
    /// Forwarders exempted as a module's sole public surface.
    facade_exempt_count: usize,
    /// Forwarders exempted by a doc comment saying they are deprecated.
    deprecated_exempt_count: usize,
    /// Whether the listing was narrowed to unstaged changed lines.
    diff_only: bool,
    /// Chains found before the `--diff-only` filter ran. Equal to
    /// `summary.chain_count` when the filter was off.
    chain_count_before_diff_filter: usize,
}

/// One forwarding chain: the hops that only pass the call on, and the
/// function at the end that does the work.
#[derive(Debug, Serialize)]
struct Chain {
    /// Forwarding hops, head first.
    depth: usize,
    /// Distinct files across the hops and the terminus — how many files
    /// reading this call path costs.
    file_count: usize,
    /// Distinct modules across the hops and the terminus.
    module_count: usize,
    /// Hops whose arguments are the parameters passed straight through,
    /// per the language's own thin-wrapper detector.
    pass_through_hop_count: usize,
    /// The walk stopped because the next hop was already on this chain:
    /// the tail is a forwarding cycle, and the terminus is the hop it
    /// re-entered rather than a function doing work.
    truncated_at_cycle: bool,
    hops: Vec<Hop>,
    terminus: Terminus,
}

#[derive(Debug, Serialize)]
struct Hop {
    id: String,
    qualified_name: String,
    module: String,
    file: String,
    start_line: usize,
    end_line: usize,
    loc: usize,
    /// Top-level statements in the body.
    statement_count: Option<usize>,
    /// Arguments are the parameters, passed straight through.
    pass_through: bool,
    /// Distinct resolved callers of this hop. A hop with callers of its
    /// own cannot simply be deleted — those call sites move to the
    /// terminus.
    caller_count: usize,
}

/// The function a chain ends at: the one doing the work, and the only
/// file in the chain worth opening first.
#[derive(Debug, Serialize)]
struct Terminus {
    id: String,
    qualified_name: String,
    module: String,
    file: String,
    start_line: usize,
    end_line: usize,
    loc: usize,
    cyclomatic_complexity: Option<u32>,
}

/// One module's delegation density and the module it delegates into.
#[derive(Debug, Serialize)]
struct ModuleRollup {
    module: String,
    /// Non-test functions in the module.
    function_count: usize,
    delegator_count: usize,
    /// `delegator_count / function_count`.
    delegator_ratio: f64,
    /// Module most of this module's delegators forward into, if any.
    dominant_target_module: Option<String>,
    /// Share of the module's delegators pointing at
    /// `dominant_target_module`.
    target_concentration: f64,
    /// Most of the module is forwarding, and mostly into one other
    /// module: the module is a layer, and inlining it is a single
    /// mechanical change.
    lasagna_candidate: bool,
    /// Hops this module contributes to the reported chains.
    chain_hop_count: usize,
}

#[derive(Debug, Serialize)]
struct Summary {
    chain_count: usize,
    /// Forwarding hops across every reported chain.
    hop_count: usize,
    max_depth: usize,
    /// Chains whose hops span more than one file — the ones that
    /// actually cost an agent file opens.
    multi_file_chain_count: usize,
    pass_through_hop_count: usize,
    truncated_chain_count: usize,
    module_count: usize,
    lasagna_module_count: usize,
}

impl Report {
    fn build(
        root: &Path,
        graph: &CallGraph,
        changed: Option<&BTreeMap<String, Vec<LineRange>>>,
    ) -> Self {
        let classified = classify_nodes(graph);
        let walk = walk_chains(graph, &classified.next);
        let callers = resolved_caller_counts(graph);

        let all_chains: Vec<Chain> = walk
            .chains
            .iter()
            .filter(|chain| chain.hops.len() >= MIN_CHAIN_HOPS)
            .map(|chain| build_chain(graph, chain, &callers))
            .collect();
        let chain_count_before_diff_filter = all_chains.len();
        let mut chains: Vec<Chain> = match changed {
            Some(changed) => all_chains
                .into_iter()
                .filter(|chain| chain.touches_changed_lines(changed))
                .collect(),
            None => all_chains,
        };
        chains.sort_by(|a, b| a.rank_key().cmp(&b.rank_key()));

        let modules = module_rollups(graph, &classified.next, &chains, changed.is_some());
        let summary = summarize(&chains, &modules);
        Self {
            schema_version: SCHEMA_VERSION,
            root: root.display().to_string(),
            language: graph.language,
            note: NOTE,
            audit: Audit {
                function_count: graph.nodes.iter().filter(|node| !node.is_test).count(),
                delegator_count: classified.next.len(),
                single_hop_count: walk
                    .chains
                    .iter()
                    .filter(|chain| chain.hops.len() < MIN_CHAIN_HOPS)
                    .count(),
                cyclic_delegator_count: walk.cyclic_delegator_count,
                unclassified_forwarder_count: classified.unclassified,
                facade_exempt_count: classified.facade_exempt,
                deprecated_exempt_count: classified.deprecated_exempt,
                diff_only: changed.is_some(),
                chain_count_before_diff_filter,
            },
            chains,
            modules,
            resolution: graph.module_summary.clone(),
            summary,
        }
    }
}

impl Chain {
    /// Deepest first, then the chains with the most argument-level
    /// evidence, then the ones costing the most file opens, then source
    /// order so the listing is stable.
    fn rank_key(&self) -> (Reverse<usize>, Reverse<usize>, Reverse<usize>, &str, usize) {
        let head = &self.hops[0];
        (
            Reverse(self.depth),
            Reverse(self.pass_through_hop_count),
            Reverse(self.file_count),
            head.file.as_str(),
            head.start_line,
        )
    }

    fn touches_changed_lines(&self, changed: &BTreeMap<String, Vec<LineRange>>) -> bool {
        let hops = self
            .hops
            .iter()
            .map(|hop| (&hop.file, hop.start_line, hop.end_line));
        let terminus = std::iter::once((
            &self.terminus.file,
            self.terminus.start_line,
            self.terminus.end_line,
        ));
        hops.chain(terminus).any(|(file, start, end)| {
            changed
                .get(file)
                .is_some_and(|ranges| overlaps_any(start, end, ranges))
        })
    }
}

/// Outcome of classifying every node, plus the counts that say what was
/// left out and why.
struct Classified {
    /// Delegator node index → the one function it forwards to.
    next: BTreeMap<usize, usize>,
    unclassified: usize,
    facade_exempt: usize,
    deprecated_exempt: usize,
}

/// Why a node is not a delegator, or that it is.
enum Verdict {
    /// Forwards to exactly one target, adds nothing else.
    Delegator(usize),
    /// Does work, calls more than one thing, or calls nothing at all.
    Works,
    /// Forwards, but a body fact needed to say "adds nothing" was not
    /// extracted.
    Unclassified,
    /// The sole public surface of its module: a facade is the interface
    /// on purpose.
    ExemptFacade,
    /// The doc says the function is deprecated, so it is already on its
    /// way out.
    ExemptDeprecated,
}

fn classify_nodes(graph: &CallGraph) -> Classified {
    let outgoing = outgoing_calls(graph);
    let facades = facade_nodes(graph);
    let mut classified = Classified {
        next: BTreeMap::new(),
        unclassified: 0,
        facade_exempt: 0,
        deprecated_exempt: 0,
    };
    for (idx, outgoing) in outgoing.iter().enumerate() {
        match classify(graph, idx, outgoing, &facades) {
            Verdict::Delegator(target) => {
                classified.next.insert(idx, target);
            }
            Verdict::Unclassified => classified.unclassified += 1,
            Verdict::ExemptFacade => classified.facade_exempt += 1,
            Verdict::ExemptDeprecated => classified.deprecated_exempt += 1,
            Verdict::Works => {}
        }
    }
    classified
}

fn classify(
    graph: &CallGraph,
    idx: usize,
    outgoing: &OutgoingCalls<'_>,
    facades: &HashSet<usize>,
) -> Verdict {
    let node = &graph.nodes[idx];
    if node.is_test {
        return Verdict::Works;
    }
    let [target] = outgoing.targets.iter().copied().collect::<Vec<_>>()[..] else {
        return Verdict::Works;
    };
    // Recursion is not forwarding, whatever the body size says.
    if target == idx {
        return Verdict::Works;
    }
    let Some(language) = graph_language_of(node) else {
        return Verdict::Works;
    };
    if !outgoing
        .other_callees
        .iter()
        .all(|callee| callee.is_some_and(|name| is_trivial_adapter(language, name)))
    {
        return Verdict::Works;
    }
    let Some(facts) = &node.delegation else {
        return Verdict::Unclassified;
    };
    let (Some(statements), Some(cyclomatic)) =
        (facts.statement_count, node.weights.cyclomatic_complexity)
    else {
        return Verdict::Unclassified;
    };
    if statements > MAX_DELEGATOR_STATEMENTS || cyclomatic > 1 {
        return Verdict::Works;
    }
    if facts.deprecated_doc {
        return Verdict::ExemptDeprecated;
    }
    if facades.contains(&idx) {
        return Verdict::ExemptFacade;
    }
    Verdict::Delegator(target)
}

/// Everything one node calls, split into the workspace functions the
/// resolver attributed and the call sites it did not.
#[derive(Default)]
struct OutgoingCalls<'a> {
    /// Distinct resolved targets, by node index.
    targets: BTreeSet<usize>,
    /// Callee names of every other call site. `None` for an anonymous
    /// site (a closure, a function value) — nothing names what it
    /// reaches, so it can hide any amount of work.
    other_callees: Vec<Option<&'a str>>,
}

fn outgoing_calls(graph: &CallGraph) -> Vec<OutgoingCalls<'_>> {
    let index_by_id = graph.node_index_by_id();
    let mut calls: Vec<OutgoingCalls<'_>> = (0..graph.nodes.len())
        .map(|_| OutgoingCalls::default())
        .collect();
    for edge in &graph.edges {
        let Some(from) = edge.from.as_deref().and_then(|id| index_by_id.get(id)) else {
            continue;
        };
        let target = (edge.resolution == Resolution::Resolved)
            .then(|| edge.to.as_deref().and_then(|id| index_by_id.get(id)))
            .flatten();
        match target {
            Some(&to) => {
                calls[*from].targets.insert(to);
            }
            None => calls[*from].other_callees.push(edge.callee_name.as_deref()),
        }
    }
    calls
}

/// Whether a call site adds nothing a reader has to follow: a name the
/// language's own tables mark as ubiquitous (`.clone()`, `.into()`) or
/// as a builtin (`len`, `append`). Everything else — a log call, a lock
/// acquisition, a second workspace call — is work.
fn is_trivial_adapter(language: GraphLanguage, callee: &str) -> bool {
    language.ubiquitous_method_names().contains(callee)
        || language.builtin_function_names().contains(callee)
}

fn graph_language_of(node: &CallGraphNode) -> Option<GraphLanguage> {
    SourceLang::from_path(Path::new(&node.file)).map(SourceLang::graph_language)
}

/// Functions that are the only public surface of a module that has
/// something behind it.
///
/// A module exposing exactly one function, with other functions it
/// keeps private, is a facade: forwarding through it is the pattern
/// working as intended. A module whose only function is that one
/// forwarder fronts nothing, so it is not exempt — that is a hop like
/// any other.
///
/// Only Rust and Go are covered: the TypeScript and Python adapters
/// extract no export status, so "public" is not a question their nodes
/// can answer.
fn facade_nodes(graph: &CallGraph) -> HashSet<usize> {
    let mut public_by_module: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut function_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for (idx, node) in graph.nodes.iter().enumerate() {
        if node.is_test {
            continue;
        }
        let Some(language) = graph_language_of(node) else {
            continue;
        };
        *function_counts.entry(node.module.as_str()).or_default() += 1;
        let public = match language {
            GraphLanguage::Rust => node.visibility == NodeVisibility::Public,
            GraphLanguage::Go => node.visibility == NodeVisibility::Exported,
            GraphLanguage::TypeScript | GraphLanguage::Python => false,
        };
        if public {
            public_by_module
                .entry(node.module.as_str())
                .or_default()
                .push(idx);
        }
    }
    public_by_module
        .into_iter()
        .filter(|(module, public)| {
            public.len() == 1 && function_counts.get(module).is_some_and(|count| *count > 1)
        })
        .flat_map(|(_, public)| public)
        .collect()
}

/// Distinct resolved callers per node index.
fn resolved_caller_counts(graph: &CallGraph) -> BTreeMap<usize, usize> {
    let index_by_id = graph.node_index_by_id();
    let mut callers: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
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
        if from != to {
            callers.entry(to).or_default().insert(from);
        }
    }
    callers
        .into_iter()
        .map(|(idx, callers)| (idx, callers.len()))
        .collect()
}

/// One maximal path through the delegator subgraph, by node index.
struct RawChain {
    hops: Vec<usize>,
    terminus: usize,
    truncated_at_cycle: bool,
}

struct ChainWalk {
    chains: Vec<RawChain>,
    cyclic_delegator_count: usize,
}

/// Follow unique successors from every chain head. Each delegator has
/// exactly one successor by construction, so the whole induced subgraph
/// is walked once: O(V+E).
///
/// A head is a delegator no other delegator forwards to. Delegators
/// inside a cycle have no head above them and are therefore never
/// walked from; they are counted instead, because "these three
/// functions forward to each other" is a different finding than a chain.
fn walk_chains(graph: &CallGraph, next: &BTreeMap<usize, usize>) -> ChainWalk {
    let entered: HashSet<usize> = next
        .values()
        .copied()
        .filter(|target| next.contains_key(target))
        .collect();
    let mut walked: HashSet<usize> = HashSet::new();
    let mut chains = Vec::new();
    for (&head, _) in next.iter().filter(|(head, _)| !entered.contains(head)) {
        let mut hops = vec![head];
        let mut on_chain: HashSet<usize> = HashSet::from([head]);
        let (terminus, truncated_at_cycle) = loop {
            // Every hop is a delegator, and a delegator has a
            // successor: the index is in range by construction.
            let Some(&target) = hops.last().and_then(|last| next.get(last)) else {
                break (head, false);
            };
            if !next.contains_key(&target) {
                break (target, false);
            }
            if !on_chain.insert(target) {
                break (target, true);
            }
            hops.push(target);
        };
        debug_assert!(graph.nodes.get(terminus).is_some());
        walked.extend(hops.iter().copied());
        chains.push(RawChain {
            hops,
            terminus,
            truncated_at_cycle,
        });
    }
    ChainWalk {
        cyclic_delegator_count: next.len() - walked.len(),
        chains,
    }
}

fn build_chain(graph: &CallGraph, chain: &RawChain, callers: &BTreeMap<usize, usize>) -> Chain {
    let hops: Vec<Hop> = chain
        .hops
        .iter()
        .map(|&idx| {
            let node = &graph.nodes[idx];
            Hop {
                id: node.id.clone(),
                qualified_name: node.qualified_name.clone(),
                module: node.module.clone(),
                file: node.file.clone(),
                start_line: node.start_line,
                end_line: node.end_line,
                loc: node.weights.loc,
                statement_count: node.delegation.as_ref().and_then(|f| f.statement_count),
                pass_through: node.delegation.as_ref().is_some_and(|f| f.pass_through),
                caller_count: callers.get(&idx).copied().unwrap_or(0),
            }
        })
        .collect();
    let terminus = &graph.nodes[chain.terminus];
    let files: BTreeSet<&str> = hops
        .iter()
        .map(|hop| hop.file.as_str())
        .chain(std::iter::once(terminus.file.as_str()))
        .collect();
    let modules: BTreeSet<&str> = hops
        .iter()
        .map(|hop| hop.module.as_str())
        .chain(std::iter::once(terminus.module.as_str()))
        .collect();
    Chain {
        depth: hops.len(),
        file_count: files.len(),
        module_count: modules.len(),
        pass_through_hop_count: hops.iter().filter(|hop| hop.pass_through).count(),
        truncated_at_cycle: chain.truncated_at_cycle,
        hops,
        terminus: Terminus {
            id: terminus.id.clone(),
            qualified_name: terminus.qualified_name.clone(),
            module: terminus.module.clone(),
            file: terminus.file.clone(),
            start_line: terminus.start_line,
            end_line: terminus.end_line,
            loc: terminus.weights.loc,
            cyclomatic_complexity: terminus.weights.cyclomatic_complexity,
        },
    }
}

/// Per-module delegation density plus where that delegation points.
///
/// With `--diff-only` the roll-up is narrowed to the modules the
/// reported chains run through; the ratios inside each row still count
/// the whole module, because "half this module forwards" is not a
/// statement about a diff.
fn module_rollups(
    graph: &CallGraph,
    next: &BTreeMap<usize, usize>,
    chains: &[Chain],
    diff_only: bool,
) -> Vec<ModuleRollup> {
    let mut function_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for node in graph.nodes.iter().filter(|node| !node.is_test) {
        *function_counts.entry(node.module.as_str()).or_default() += 1;
    }
    let mut delegators: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (&idx, &target) in next {
        delegators
            .entry(graph.nodes[idx].module.as_str())
            .or_default()
            .push(graph.nodes[target].module.as_str());
    }
    let mut hop_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for hop in chains.iter().flat_map(|chain| &chain.hops) {
        *hop_counts.entry(hop.module.as_str()).or_default() += 1;
    }

    let mut rollups: Vec<ModuleRollup> = delegators
        .into_iter()
        .filter(|(module, _)| !diff_only || hop_counts.contains_key(module))
        .map(|(module, targets)| {
            let function_count = function_counts
                .get(module)
                .copied()
                .unwrap_or(targets.len());
            let (dominant_target_module, dominant_count) = dominant_target(module, &targets);
            let delegator_ratio = ratio(targets.len(), function_count);
            let target_concentration = ratio(dominant_count, targets.len());
            ModuleRollup {
                module: module.to_owned(),
                function_count,
                delegator_count: targets.len(),
                delegator_ratio,
                lasagna_candidate: dominant_target_module.is_some()
                    && targets.len() >= LASAGNA_MIN_DELEGATORS
                    && delegator_ratio >= LASAGNA_MIN_DELEGATOR_RATIO
                    && target_concentration >= LASAGNA_MIN_CONCENTRATION,
                dominant_target_module: dominant_target_module.map(ToOwned::to_owned),
                target_concentration,
                chain_hop_count: hop_counts.get(module).copied().unwrap_or(0),
            }
        })
        .collect();
    rollups.sort_by(|a, b| a.rank_key().cmp(&b.rank_key()));
    rollups
}

impl ModuleRollup {
    /// Flagged layers first, then the modules with the most forwarding.
    fn rank_key(&self) -> (Reverse<bool>, Reverse<usize>, Reverse<u64>, &str) {
        (
            Reverse(self.lasagna_candidate),
            Reverse(self.delegator_count),
            // Ratios are in [0, 1]; scaling to an integer keeps the key
            // orderable without imposing `Ord` on a float.
            Reverse((self.delegator_ratio * 1e6) as u64),
            self.module.as_str(),
        )
    }
}

/// The other module most of `module`'s delegators forward into, with
/// how many of them do. Targets inside `module` itself are not a layer
/// boundary, so they are excluded from the choice but stay in the
/// denominator: a module forwarding half inward and half outward is not
/// concentrated.
fn dominant_target<'a>(module: &str, targets: &[&'a str]) -> (Option<&'a str>, usize) {
    let mut counts: BTreeMap<&'a str, usize> = BTreeMap::new();
    for target in targets.iter().filter(|target| **target != module) {
        *counts.entry(target).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by_key(|&(module, count)| (count, Reverse(module)))
        .map_or((None, 0), |(module, count)| (Some(module), count))
}

fn ratio(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

fn summarize(chains: &[Chain], modules: &[ModuleRollup]) -> Summary {
    Summary {
        chain_count: chains.len(),
        hop_count: chains.iter().map(|chain| chain.depth).sum(),
        max_depth: chains.iter().map(|chain| chain.depth).max().unwrap_or(0),
        multi_file_chain_count: chains.iter().filter(|chain| chain.file_count > 1).count(),
        pass_through_hop_count: chains
            .iter()
            .map(|chain| chain.pass_through_hop_count)
            .sum(),
        truncated_chain_count: chains
            .iter()
            .filter(|chain| chain.truncated_at_cycle)
            .count(),
        module_count: modules.len(),
        lasagna_module_count: modules.iter().filter(|m| m.lasagna_candidate).count(),
    }
}

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    let summary = &report.summary;
    let mut out = format!(
        "# Delegation chains: {} ({} chain(s) of {}+ forwarding hops, {} module(s) rolled up)\n",
        report.root, summary.chain_count, MIN_CHAIN_HOPS, summary.module_count,
    );
    let _ = writeln!(out, "\n{}", report.note);
    render_audit(&mut out, &report.audit);

    if report.chains.is_empty() {
        out.push_str(if report.audit.chain_count_before_diff_filter > 0 {
            "\n_No forwarding chain touches the unstaged changes._\n"
        } else {
            "\n_No multi-hop forwarding chain found._\n"
        });
    } else {
        render_counts(&mut out, summary);
        render_chains(&mut out, &report.chains, limit);
    }
    render_modules(&mut out, &report.modules, limit);
    render_module_confidence(
        &mut out,
        &report.resolution,
        "Call sites in these modules resolved worst, so a forwarding hop is the most likely to \
         have been missed there — chains through them are the most truncated.",
    );
    out
}

fn render_audit(out: &mut String, audit: &Audit) {
    let _ = writeln!(
        out,
        "\nClassified {} non-test function(s): {} delegator(s), of which {} forward a single hop \
         (that is `analyze wrapper`'s report) and {} sit inside a forwarding cycle. {} forward but \
         were left unclassified for want of a body fact; {} exempt as a module facade, {} as \
         deprecated.",
        audit.function_count,
        audit.delegator_count,
        audit.single_hop_count,
        audit.cyclic_delegator_count,
        audit.unclassified_forwarder_count,
        audit.facade_exempt_count,
        audit.deprecated_exempt_count,
    );
    if audit.diff_only {
        let _ = writeln!(
            out,
            "`--diff-only`: of {} chain(s) found, only those with a hop or terminus on an unstaged \
             changed line are listed, and the module roll-up is narrowed to the modules they run \
             through.",
            audit.chain_count_before_diff_filter,
        );
    }
}

fn render_counts(out: &mut String, summary: &Summary) {
    let _ = writeln!(
        out,
        "\n{} forwarding hop(s) across {} chain(s), deepest {} hops; {} chain(s) span more than \
         one file, {} hop(s) pass their arguments straight through, and {} chain(s) end in a \
         forwarding cycle rather than at a function doing work.",
        summary.hop_count,
        summary.chain_count,
        summary.max_depth,
        summary.multi_file_chain_count,
        summary.pass_through_hop_count,
        summary.truncated_chain_count,
    );
}

fn render_chains(out: &mut String, chains: &[Chain], limit: usize) {
    let shown = chains.len().min(limit);
    let _ = writeln!(
        out,
        "\n## Chains (deepest first; {shown} of {} chain(s))",
        chains.len(),
    );
    for chain in chains.iter().take(limit) {
        let path = chain
            .hops
            .iter()
            .map(|hop| format!("`{}`", hop.qualified_name))
            .chain(std::iter::once(format!(
                "`{}`",
                chain.terminus.qualified_name
            )))
            .collect::<Vec<_>>()
            .join(" -> ");
        let _ = writeln!(
            out,
            "\n- {path} — {} forwarding hop(s) ({} forwarding arguments verbatim) across {} \
             file(s); {}",
            chain.depth,
            chain.pass_through_hop_count,
            chain.file_count,
            if chain.truncated_at_cycle {
                format!(
                    "the tail re-enters `{}`, so this chain is a forwarding cycle",
                    chain.terminus.qualified_name,
                )
            } else {
                format!(
                    "logic at {}:{} ({} LOC)",
                    chain.terminus.file, chain.terminus.start_line, chain.terminus.loc,
                )
            },
        );
        for (position, hop) in chain.hops.iter().take(HOPS_PER_CHAIN).enumerate() {
            let _ = writeln!(out, "  - hop {}: {}", position + 1, render_hop(hop));
        }
        let overflow = chain.hops.len().saturating_sub(HOPS_PER_CHAIN);
        if overflow > 0 {
            let _ = writeln!(out, "  - +{overflow} more hop(s) (JSON carries every hop)");
        }
    }
    let overflow = chains.len() - shown;
    if overflow > 0 {
        let _ = writeln!(
            out,
            "\n+{overflow} more chain(s) not shown (raise `--top`; JSON carries every row)."
        );
    }
}

fn render_hop(hop: &Hop) -> String {
    let mut row = format!(
        "`{}` ({}:{}, {} LOC",
        hop.qualified_name, hop.file, hop.start_line, hop.loc,
    );
    if let Some(statements) = hop.statement_count {
        let _ = write!(row, ", {statements} stmt");
    }
    row.push(')');
    if hop.pass_through {
        row.push_str("; args forwarded verbatim");
    }
    if hop.caller_count > 0 {
        let _ = write!(row, "; {} other caller(s) to move", hop.caller_count);
    }
    row
}

fn render_modules(out: &mut String, modules: &[ModuleRollup], limit: usize) {
    if modules.is_empty() {
        return;
    }
    let shown = modules.len().min(limit);
    let _ = writeln!(
        out,
        "\n## Delegation by module (layer candidates first; {shown} of {} module(s))",
        modules.len(),
    );
    for module in modules.iter().take(limit) {
        let _ = writeln!(out, "- {}", render_module(module));
    }
    let overflow = modules.len() - shown;
    if overflow > 0 {
        let _ = writeln!(
            out,
            "\n+{overflow} more module(s) not shown (raise `--top`; JSON carries every row)."
        );
    }
}

fn render_module(module: &ModuleRollup) -> String {
    let mut row = format!(
        "`{}` — {}/{} function(s) forward ({:.0}%)",
        module.module,
        module.delegator_count,
        module.function_count,
        module.delegator_ratio * 100.0,
    );
    if let Some(target) = &module.dominant_target_module {
        let _ = write!(
            row,
            ", {:.0}% of them into `{target}`",
            module.target_concentration * 100.0,
        );
    }
    if module.lasagna_candidate {
        row.push_str(" — layer candidate: inlining it is one mechanical change");
    }
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};
    use rstest::rstest;
    use serde_json::Value;

    fn analyze_json(path: &Path) -> Value {
        let json = DelegationAnalyzer::new()
            .analyze(path, OutputFormat::Json)
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn analyze_md(path: &Path) -> String {
        DelegationAnalyzer::new()
            .analyze(path, OutputFormat::Md)
            .unwrap()
    }

    /// `head -> … -> terminus` qualified names for every reported chain.
    fn chain_paths(report: &Value) -> Vec<Vec<String>> {
        report["chains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|chain| {
                chain["hops"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|hop| hop["qualified_name"].as_str().unwrap().to_owned())
                    .chain(std::iter::once(
                        chain["terminus"]["qualified_name"]
                            .as_str()
                            .unwrap()
                            .to_owned(),
                    ))
                    .collect()
            })
            .collect()
    }

    /// A four-layer forwarding stack, one module per layer, ending in
    /// the one function that does work.
    const LAYERED_STACK: &str = "\
pub mod api {
    pub fn save(id: usize) -> usize { crate::service::save(id) }
}
pub mod service {
    pub fn save(id: usize) -> usize { crate::repo::save(id) }
}
pub mod repo {
    pub fn save(id: usize) -> usize { crate::db::insert(id) }
}
pub mod db {
    pub fn insert(id: usize) -> usize {
        let mut total = 0;
        for i in 0..id { if i % 2 == 0 { total += i; } }
        total
    }
}
";

    #[test]
    fn a_forwarding_stack_is_reported_as_one_chain_ending_at_the_work() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", LAYERED_STACK);

        let report = analyze_json(dir.path());
        assert_eq!(report["schema_version"], 1);
        assert_eq!(
            chain_paths(&report),
            vec![vec![
                "crate::api::save".to_owned(),
                "crate::service::save".to_owned(),
                "crate::repo::save".to_owned(),
                "crate::db::insert".to_owned(),
            ]],
            "report: {report}",
        );
        let chain = &report["chains"][0];
        assert_eq!(chain["depth"], 3);
        assert_eq!(chain["truncated_at_cycle"], false);
        assert_eq!(
            chain["pass_through_hop_count"], 3,
            "every hop forwards its parameter untouched: {chain}",
        );
        assert_eq!(report["summary"]["max_depth"], 3);
        assert_eq!(report["audit"]["delegator_count"], 3);
        assert_eq!(
            report["audit"]["single_hop_count"], 0,
            "the stack is one chain, not three wrappers: {report}",
        );
    }

    #[test]
    fn the_markdown_headline_names_the_terminus_and_its_file_line() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", LAYERED_STACK);

        let md = analyze_md(dir.path());
        assert!(
            md.contains(
                "`crate::api::save` -> `crate::service::save` -> `crate::repo::save` -> \
                 `crate::db::insert`"
            ),
            "chain path missing: {md}",
        );
        assert!(
            md.contains("logic at src/lib.rs:"),
            "terminus missing: {md}"
        );
        assert!(md.contains("3 forwarding hop(s)"), "depth missing: {md}");
        assert!(md.contains("args forwarded verbatim"), "hop facts: {md}");
    }

    /// One hop that does something of its own is enough to cut the
    /// chain: the report is about hops that add nothing.
    #[rstest]
    #[case::logs("crate::log_it(id); crate::repo::save(id)", "an extra call is work")]
    #[case::branches(
        "if id == 0 { return 0; } crate::repo::save(id)",
        "a branch lifts cyclomatic above 1"
    )]
    #[case::four_statements(
        "let a = id; let b = a; let c = b; crate::repo::save(c)",
        "four statements is more body than a forward"
    )]
    fn a_hop_that_does_work_is_not_a_delegator(#[case] body: &str, #[case] why: &str) {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            &format!(
                "pub fn log_it(id: usize) {{ let _ = id; }}
pub mod api {{
    pub fn save(id: usize) -> usize {{ crate::service::save(id) }}
}}
pub mod service {{
    pub fn save(id: usize) -> usize {{ {body} }}
}}
pub mod repo {{
    pub fn save(id: usize) -> usize {{ crate::db::insert(id) }}
}}
pub mod db {{
    pub fn insert(id: usize) -> usize {{ id + 1 }}
}}
",
            ),
        );

        let report = analyze_json(dir.path());
        assert!(
            chain_paths(&report).is_empty(),
            "{why}, so neither side of it is a chain of 2+ hops: {report}",
        );
    }

    /// A trivial adapter on the forwarded result is still forwarding —
    /// `.into()` is a coercion, not logic.
    #[test]
    fn a_trivial_adapter_on_the_forwarded_call_keeps_the_hop() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub mod api {
    pub fn save(id: usize) -> u64 { crate::service::save(id).into() }
}
pub mod service {
    pub fn save(id: usize) -> u32 { crate::db::insert(id) }
}
pub mod db {
    pub fn insert(id: usize) -> u32 { id as u32 }
}
",
        );

        assert_eq!(
            chain_paths(&analyze_json(dir.path())),
            vec![vec![
                "crate::api::save".to_owned(),
                "crate::service::save".to_owned(),
                "crate::db::insert".to_owned(),
            ]],
        );
    }

    /// A single forwarding hop belongs to `analyze wrapper`, which can
    /// say more about it. It is counted here, not listed.
    #[test]
    fn a_single_hop_forward_is_counted_but_not_listed() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub mod api {
    pub fn save(id: usize) -> usize { crate::db::insert(id) }
    pub fn other(id: usize) -> usize { id }
}
pub mod db {
    pub fn insert(id: usize) -> usize { id + 1 }
    pub fn other(id: usize) -> usize { id }
}
",
        );

        let report = analyze_json(dir.path());
        assert!(chain_paths(&report).is_empty(), "report: {report}");
        assert_eq!(report["audit"]["single_hop_count"], 1);
        assert!(
            analyze_md(dir.path()).contains("No multi-hop forwarding chain found"),
            "the empty report says so",
        );
    }

    /// Exemptions take a function out of the delegator set, which cuts
    /// any chain running through it.
    #[rstest]
    #[case::facade("", "facade_exempt_count")]
    #[case::deprecated("/// Deprecated: call the repo directly.\n", "deprecated_exempt_count")]
    fn an_exempt_hop_cuts_the_chain(#[case] doc: &str, #[case] counter: &str) {
        // Either way the middle module keeps a private helper, so the
        // forwarder fronts something. With no doc it is the module's
        // sole `pub` item and exempt as a facade; with the doc a second
        // `pub` item takes it off the facade rule and the deprecation
        // note is what exempts it.
        let sibling = if doc.is_empty() {
            "fn helper(id: usize) -> usize { id }\n"
        } else {
            "pub fn helper(id: usize) -> usize { id }\n"
        };
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            &format!(
                "pub mod api {{
    pub fn save(id: usize) -> usize {{ crate::service::save(id) }}
    pub fn extra(id: usize) -> usize {{ id }}
}}
pub mod service {{
    {sibling}{doc}    pub fn save(id: usize) -> usize {{ crate::repo::save(id) }}
}}
pub mod repo {{
    pub fn save(id: usize) -> usize {{ crate::db::insert(id) }}
    pub fn extra(id: usize) -> usize {{ id }}
}}
pub mod db {{
    pub fn insert(id: usize) -> usize {{ id + 1 }}
    pub fn extra(id: usize) -> usize {{ id }}
}}
",
            ),
        );

        let report = analyze_json(dir.path());
        assert!(
            chain_paths(&report).is_empty(),
            "the exempt middle hop cuts the stack into single hops: {report}",
        );
        assert_eq!(report["audit"][counter], 1, "report: {report}");
    }

    #[test]
    fn a_forwarding_cycle_is_counted_not_walked() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub mod a {
    pub fn go(id: usize) -> usize { crate::b::go(id) }
    pub fn extra(id: usize) -> usize { id }
}
pub mod b {
    pub fn go(id: usize) -> usize { crate::a::go(id) }
    pub fn extra(id: usize) -> usize { id }
}
",
        );

        let report = analyze_json(dir.path());
        assert!(chain_paths(&report).is_empty(), "report: {report}");
        assert_eq!(report["audit"]["cyclic_delegator_count"], 2);
    }

    /// Test functions forward constantly and are not middle men.
    #[test]
    fn test_functions_are_never_delegators() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub mod api {
    pub fn save(id: usize) -> usize { crate::db::insert(id) }
    pub fn extra(id: usize) -> usize { id }
}
pub mod db {
    pub fn insert(id: usize) -> usize { id + 1 }
    pub fn extra(id: usize) -> usize { id }
}
#[cfg(test)]
mod tests {
    #[test]
    fn forwards() { let _ = crate::api::save(1); }
}
",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["audit"]["delegator_count"], 1, "report: {report}");
    }

    /// The module roll-up is the "lasagna layer" half: a module that is
    /// mostly forwarding, mostly into one other module.
    #[test]
    fn a_module_of_forwarders_pointing_one_way_is_a_layer_candidate() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub mod api {
    pub fn a(id: usize) -> usize { crate::service::a(id) }
    pub fn b(id: usize) -> usize { crate::service::b(id) }
    pub fn c(id: usize) -> usize { crate::service::c(id) }
}
pub mod service {
    pub fn a(id: usize) -> usize { crate::db::insert(id) }
    pub fn b(id: usize) -> usize { crate::db::insert(id) }
    pub fn c(id: usize) -> usize { crate::db::insert(id) }
}
pub mod db {
    pub fn insert(id: usize) -> usize { id + 1 }
    pub fn extra(id: usize) -> usize { id }
}
",
        );

        let report = analyze_json(dir.path());
        let api = report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["module"] == "crate::api")
            .unwrap_or_else(|| panic!("report: {report}"));
        assert_eq!(api["delegator_count"], 3);
        assert_eq!(api["function_count"], 3);
        assert_eq!(api["delegator_ratio"], 1.0);
        assert_eq!(api["dominant_target_module"], "crate::service");
        assert_eq!(api["target_concentration"], 1.0);
        assert_eq!(api["lasagna_candidate"], true);
        assert_eq!(report["summary"]["lasagna_module_count"], 2);
        assert!(
            analyze_md(dir.path()).contains("layer candidate"),
            "the markdown says which modules are layers",
        );
    }

    /// Chains are ranked deepest first so the worst context tax is the
    /// first row.
    #[test]
    fn chains_are_ranked_deepest_first() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub mod deep {
    pub fn one(id: usize) -> usize { two(id) }
    pub fn two(id: usize) -> usize { three(id) }
    pub fn three(id: usize) -> usize { crate::work::run(id) }
}
pub mod shallow {
    pub fn one(id: usize) -> usize { two(id) }
    pub fn two(id: usize) -> usize { crate::work::run(id) }
}
pub mod work {
    pub fn run(id: usize) -> usize { id + 1 }
    pub fn extra(id: usize) -> usize { id }
}
",
        );

        let depths: Vec<usize> = analyze_json(dir.path())["chains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|chain| chain["depth"].as_u64().unwrap() as usize)
            .collect();
        assert_eq!(depths, vec![3, 2]);
    }

    #[test]
    fn diff_only_keeps_the_chains_touching_unstaged_changes() {
        let dir = tempfile::tempdir().unwrap();
        // Two chains, one per file. Only the first file is edited, and
        // the edit lands on a hop.
        let chain_source = |param: &str| {
            format!(
                "pub mod api {{
    pub fn save({param}: usize) -> usize {{ crate::service::save({param}) }}
}}
pub mod service {{
    pub fn save(id: usize) -> usize {{ crate::work::run(id) }}
}}
pub mod work {{
    pub fn run(id: usize) -> usize {{ id + 1 }}
    pub fn extra(id: usize) -> usize {{ id }}
}}
"
            )
        };
        write_file(dir.path(), "src/lib.rs", &chain_source("id"));
        write_file(
            dir.path(),
            "src/other.rs",
            "pub fn one(id: usize) -> usize { two(id) }
pub fn two(id: usize) -> usize { run(id) }
pub fn run(id: usize) -> usize { id + 1 }
pub fn extra(id: usize) -> usize { id }
",
        );
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);
        write_file(dir.path(), "src/lib.rs", &chain_source("ident"));

        let json = DelegationAnalyzer::new()
            .with_diff_only(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let report: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(report["audit"]["diff_only"], true);
        assert_eq!(report["audit"]["chain_count_before_diff_filter"], 2);
        assert_eq!(
            chain_paths(&report),
            vec![vec![
                "crate::api::save".to_owned(),
                "crate::service::save".to_owned(),
                "crate::work::run".to_owned(),
            ]],
            "only the chain whose hops moved is listed: {report}",
        );
        let modules: Vec<&str> = report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["module"].as_str().unwrap())
            .collect();
        assert_eq!(
            modules,
            ["crate::api", "crate::service"],
            "the roll-up narrows to the modules the kept chain runs through",
        );
    }

    /// The chain shape is language-neutral even though the confidence
    /// is not: TypeScript, Python, and Go stacks are walked the same
    /// way Rust's is.
    #[rstest]
    #[case::typescript(
        "src/lib.ts",
        "export function apiSave(id: number): number { return serviceSave(id); }\n\
         export function serviceSave(id: number): number { return repoSave(id); }\n\
         export function repoSave(id: number): number { return dbInsert(id); }\n\
         export function dbInsert(id: number): number { let total = 0; for (const x of [id]) { total += x; } return total; }\n",
        ["apiSave", "serviceSave", "repoSave", "dbInsert"]
    )]
    #[case::python(
        "src/lib.py",
        "def api_save(id):\n    return service_save(id)\n\n\
         def service_save(id):\n    return repo_save(id)\n\n\
         def repo_save(id):\n    return db_insert(id)\n\n\
         def db_insert(id):\n    total = 0\n    for x in [id]:\n        total += x\n    return total\n",
        ["api_save", "service_save", "repo_save", "db_insert"]
    )]
    // The Go package exports a second function so the head is not the
    // package's sole export, which would exempt it as a facade.
    #[case::go(
        "src/lib.go",
        "package lib\n\n\
         func APISave(id int) int { return serviceSave(id) }\n\n\
         func Unrelated(id int) int { return id }\n\n\
         func serviceSave(id int) int { return repoSave(id) }\n\n\
         func repoSave(id int) int { return dbInsert(id) }\n\n\
         func dbInsert(id int) int { total := 0; for i := 0; i < id; i++ { total += i }; return total }\n",
        ["APISave", "serviceSave", "repoSave", "dbInsert"]
    )]
    fn forwarding_stacks_are_walked_in_every_language(
        #[case] file: &str,
        #[case] source: &str,
        #[case] expected: [&str; 4],
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), file, source);

        let report = analyze_json(dir.path());
        let paths = chain_paths(&report);
        assert_eq!(paths.len(), 1, "report: {report}");
        let names: Vec<&str> = paths[0]
            .iter()
            .map(|name| name.rsplit("::").next().unwrap_or(name))
            .collect();
        assert_eq!(names, expected, "report: {report}");
    }

    #[test]
    fn the_dominant_target_ignores_targets_inside_the_module_itself() {
        assert_eq!(
            dominant_target("crate::a", &["crate::a", "crate::b", "crate::b"]),
            (Some("crate::b"), 2),
        );
        assert_eq!(
            dominant_target("crate::a", &["crate::a", "crate::a"]),
            (None, 0),
            "a module forwarding only inside itself crosses no boundary",
        );
    }

    #[rstest]
    #[case::empty_whole(1, 0, 0.0)]
    #[case::half(1, 2, 0.5)]
    #[case::whole(3, 3, 1.0)]
    fn ratio_is_zero_when_nothing_was_counted(
        #[case] part: usize,
        #[case] whole: usize,
        #[case] expected: f64,
    ) {
        assert!((ratio(part, whole) - expected).abs() < f64::EPSILON);
    }

    #[test]
    fn missing_path_surfaces_a_path_error() {
        let err = DelegationAnalyzer::new()
            .analyze(Path::new("/definitely/does/not/exist"), OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::PathNotFound { .. }), "{err:?}");
    }
}
