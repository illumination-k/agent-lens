//! `analyze visibility` — `pub` (Rust) / exported (Go) functions whose
//! resolved callers never leave a narrower scope than the one they are
//! declared with.
//!
//! One pass over the resolved edges of the shared call graph: for every
//! public function, take the modules its callers live in and fold them
//! into the narrowest module that contains all of them. When that module
//! is narrower than what the declaration exposes, the declaration is
//! wider than the code needs and the report names the visibility that
//! would still compile.
//!
//! This is the inverse of a dead-code report and deliberately the safe
//! half of it: a false positive here suggests *narrowing* a visibility,
//! which the compiler rejects on the spot if the analyzer was wrong.
//! Nothing is proposed for deletion, so an over-eager row costs one
//! failed build rather than lost code.
//!
//! What it can and cannot see:
//!
//! - Only **resolved** edges carry a caller module, so a function whose
//!   callers were not attributed looks narrower than it is. The two
//!   kinds of site that can hide one — an ambiguous call naming the
//!   function among its candidates, and a receiver call on a name the
//!   resolver refuses to attribute (`.clone()`, `.get()`) — are counted
//!   per row when they sit outside the proposed scope. A flagged row is
//!   the one to check by hand first.
//! - Callers outside the analyzed path do not exist for this analyzer.
//!   Pointed at a single library crate, its whole intended API surface
//!   looks crate-internal, which is why the crate count is reported and
//!   the markdown says so when only one crate was seen.
//! - Items a crate root re-exports with `pub use` are dropped from the
//!   audit instead of reported: narrowing one breaks the re-export
//!   statement itself, not merely a caller. That scan is textual and
//!   reads only `src/lib.rs`, so a `pub use` chained through an
//!   intermediate module is missed, and `pub mod` — how most Rust APIs
//!   are actually reached — is deliberately not treated as evidence.
//!
//! Scope is Rust and Go, the two adapters that extract export status.
//! TypeScript and Python functions are counted and skipped rather than
//! silently absent. `pub(crate)` / `pub(super)` declarations are out of
//! scope too: the graph keeps only that they are restricted, not to
//! what, so "narrower than today" is not decidable for them.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use lens_domain::UbiquitousMethodNames;
use serde::Serialize;

use super::call_graph::model::{
    CallGraphNode, GraphLanguage, ModuleResolutionSummary, NodeVisibility, Resolution,
    name_last_segment,
};
use super::call_graph::{CallGraph, CallGraphBuilder, delegate_call_graph_builders};
use super::format::{ModuleSection, render_module_confidence, render_module_sections};
use super::runner::render_report;
use super::{AnalyzerError, OutputFormat, SourceLang};

const SCHEMA_VERSION: u32 = 1;

/// Module sections rendered in markdown when `--top` is not given. JSON
/// always carries every module.
const DEFAULT_TOP: usize = 20;

/// Findings listed per module section in markdown. JSON carries all of
/// them.
const FINDINGS_PER_MODULE: usize = 10;

/// Caller modules named inline on a markdown row before the rest are
/// rolled into a count.
const CALLER_MODULES_PER_ROW: usize = 3;

/// What the verdict means, stated in the output itself: every row is a
/// candidate whose worst case is a failed build, and the evidence is
/// bounded by what the resolver could see.
const NOTE: &str = "Candidates, not verdicts: each row is a visibility the resolved callers would \
     still permit, and the compiler rejects it immediately if a caller was missed. Only resolved \
     edges carry a caller module, so callers reached through ambiguous call sites, or through \
     receiver calls on a name the resolver refuses to attribute, are invisible — sites of either \
     kind that name the function from outside the proposed scope are counted per row. Callers \
     outside the analyzed path do not exist here, and only `pub use` re-exports written in a crate \
     root are recognised as intended API, so a surface published another way can still appear.";

/// Analyzer entry point for `analyze visibility`.
#[derive(Debug, Default, Clone)]
pub struct VisibilityAnalyzer {
    builder: CallGraphBuilder,
    top: Option<usize>,
}

impl VisibilityAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    delegate_call_graph_builders! {
        builder,
        /// Accepted for CLI uniformity. Test functions are never audited —
        /// a `pub fn` in a test module is not an API surface — so this
        /// leaves only test code to call the (now absent) findings.
        only_tests,
        /// Drops test files from the graph, and with them the callers that
        /// live there. A function called only from tests then looks
        /// uncalled rather than test-called, so the report says how many
        /// callers were tests when they are in scope.
        exclude_tests,
    }

    /// Cap the markdown module sections to the top-N entries. JSON
    /// output always carries every row.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let graph = self.builder.build(path)?;
        let report = Report::build(path, &graph);
        render_report(&report, format, || format_markdown(&report, self.top))
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
    /// Modules holding at least one over-exposed function, most
    /// findings first.
    modules: Vec<ModuleGroup>,
    /// Per-module call-site resolution counts — the calibration layer:
    /// a module whose call sites mostly failed to resolve contributes
    /// callers this analyzer never saw.
    resolution: Vec<ModuleResolutionSummary>,
    summary: Summary,
}

/// What was actually examined. Every "over-exposed" verdict is relative
/// to it, so it is emitted rather than assumed.
#[derive(Debug, Serialize)]
struct Audit {
    /// Non-test `pub` (Rust) / exported (Go) functions in scope — the
    /// denominator.
    public_function_count: usize,
    /// Rust crate names seen, from each file's enclosing `Cargo.toml`.
    crates: Vec<String>,
    /// Of `public_function_count`, how many were left out because a
    /// crate root re-exports them (or the type owning them) with `pub
    /// use`: narrowing those breaks the re-export itself.
    re_exported_function_count: usize,
    /// A single Rust crate in scope means no cross-crate caller can
    /// exist in the graph, so a library's own API surface is
    /// indistinguishable from over-exposure.
    single_crate: bool,
    /// Cargo packages carrying both a library and a binary root. Those
    /// are two crates sharing one name, and module paths cannot tell
    /// them apart: a `pub(crate)` proposed for an item the binary calls
    /// out of the library would not compile.
    mixed_target_crates: Vec<String>,
    /// Non-test functions skipped because their language carries no
    /// export status: the TypeScript and Python adapters do not extract
    /// one.
    unsupported_language_function_count: usize,
}

impl Audit {
    /// Public functions actually judged: everything the languages
    /// expose, minus the re-exports that are API by construction.
    fn audited_function_count(&self) -> usize {
        self.public_function_count - self.re_exported_function_count
    }
}

/// One module's over-exposed functions.
#[derive(Debug, Serialize)]
struct ModuleGroup {
    module: String,
    finding_count: usize,
    /// Of `finding_count`, how many have at least one resolved caller —
    /// the rows with positive evidence for the proposed visibility.
    called_count: usize,
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
    /// As declared: `public` for Rust `pub`, `exported` for Go.
    visibility: NodeVisibility,
    caller_scope: CallerScope,
    /// Narrowest module containing every resolved caller. Equal to the
    /// defining module when there are none.
    scope_module: String,
    /// The declaration the resolved callers would still permit, written
    /// as the language spells it.
    suggested_visibility: String,
    /// Distinct resolved callers.
    caller_count: usize,
    /// Of `caller_count`, how many are test functions. Narrowing stays
    /// valid for Rust tests (they compile inside the crate); a Go
    /// `_test.go` file declaring an external `package x_test` needs the
    /// export, and the package clause is not in the graph.
    test_caller_count: usize,
    /// Distinct modules the resolved callers live in, sorted.
    caller_modules: Vec<String>,
    /// Ambiguous call sites outside `scope_module` whose candidate set
    /// names this function: the resolver could not decide, so a caller
    /// the narrowing would break may exist.
    ambiguous_calls_outside_scope: usize,
    /// Receiver call sites outside `scope_module` on this function's
    /// name, left unresolved because the name is one the resolver
    /// refuses to attribute (`clone`, `get`, `map`, …).
    ///
    /// These are the only unresolved sites that can still hide a
    /// caller. Every other unresolved site either found no workspace
    /// name to match or was written as a path the resolver checked
    /// against this function and ruled out.
    ubiquitous_name_calls_outside_scope: usize,
}

impl Finding {
    /// Whether any call site outside the proposed scope might reach
    /// this function — the rows to verify before narrowing.
    fn possible_external_caller(&self) -> bool {
        self.ambiguous_calls_outside_scope > 0 || self.ubiquitous_name_calls_outside_scope > 0
    }
}

/// How far the resolved callers of a public function actually reach.
/// Declaration order is the ranking: the narrowest containment (and so
/// the largest reduction in exposure) first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum CallerScope {
    /// Every caller is in the defining module — or, for Rust, below it,
    /// where a private item is still visible.
    SameModule,
    /// Rust only: every caller sits under a proper ancestor module of
    /// the definition, narrower than the crate.
    AncestorModule,
    /// Rust only: every caller is in the defining crate.
    SameCrate,
    /// Nothing in the analyzed tree calls it. Either the surface is
    /// meant for consumers outside the tree, or it is unused; this
    /// analyzer does not decide which.
    NoResolvedCallers,
}

#[derive(Debug, Serialize)]
struct Summary {
    over_exposed_count: usize,
    /// `over_exposed_count` over the audited functions (public minus
    /// re-exported), 0.0 when nothing was audited.
    over_exposed_share: f64,
    same_module_count: usize,
    ancestor_module_count: usize,
    same_crate_count: usize,
    no_resolved_caller_count: usize,
    /// Findings carrying an ambiguous or name-matching unresolved call
    /// site from outside the proposed scope.
    possible_external_caller_count: usize,
    /// Modules holding at least one finding.
    module_count: usize,
}

/// The two languages whose adapters extract export status. Everything
/// else is counted as skipped instead of being judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditLang {
    /// Private items are visible in the defining module *and its
    /// descendants*, and the intermediate `pub(in …)` scopes exist.
    Rust,
    /// The package is the only boundary: a caller one directory down is
    /// as external as one in another repository.
    Go,
}

impl AuditLang {
    fn of(node: &CallGraphNode) -> Option<Self> {
        match SourceLang::from_path(Path::new(&node.file)) {
            Some(SourceLang::Rust) => Some(Self::Rust),
            Some(SourceLang::Go) => Some(Self::Go),
            _ => None,
        }
    }

    /// The visibility that exposes a function beyond its own compilation
    /// unit in this language.
    fn public(self) -> NodeVisibility {
        match self {
            Self::Rust => NodeVisibility::Public,
            Self::Go => NodeVisibility::Exported,
        }
    }

    /// Whether a scope module also covers the modules below it. Rust
    /// visibility is inherited downward; Go packages are flat.
    fn scope_covers_descendants(self) -> bool {
        matches!(self, Self::Rust)
    }

    /// The method names this language's resolver refuses to attribute
    /// from a receiver call alone.
    fn ubiquitous_names(self) -> UbiquitousMethodNames {
        match self {
            Self::Rust => GraphLanguage::Rust.ubiquitous_method_names(),
            Self::Go => GraphLanguage::Go.ubiquitous_method_names(),
        }
    }
}

impl Report {
    fn build(root: &Path, graph: &CallGraph) -> Self {
        let crate_dirs = rust_crate_dirs(graph);
        let re_exports = ReExports::scan(root, &crate_dirs);
        let public = public_nodes(graph);
        let audited: Vec<(usize, AuditLang)> = public
            .iter()
            .copied()
            .filter(|&(idx, _)| !re_exports.covers(&graph.nodes[idx]))
            .collect();

        let callers = graph.resolved_callers();
        let mut findings: Vec<(usize, Finding)> = Vec::new();
        for &(idx, lang) in &audited {
            if let Some(finding) = classify(graph, idx, lang, callers.get(&idx)) {
                findings.push((idx, finding));
            }
        }
        annotate_outside_calls(graph, &mut findings);

        let modules = module_groups(graph, findings);
        let summary = summarize(&modules, audited.len());
        Self {
            schema_version: SCHEMA_VERSION,
            root: root.display().to_string(),
            language: graph.language,
            note: NOTE,
            audit: audit_scope(root, graph, &crate_dirs, public.len(), audited.len()),
            modules,
            resolution: graph.module_summary.clone(),
            summary,
        }
    }
}

/// Non-test functions declared public in a language that records the
/// fact, paired with that language.
fn public_nodes(graph: &CallGraph) -> Vec<(usize, AuditLang)> {
    graph
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| !node.is_test)
        .filter_map(|(idx, node)| {
            let lang = AuditLang::of(node)?;
            (node.visibility == lang.public()).then_some((idx, lang))
        })
        .collect()
}

/// Package directory of each Rust crate in the graph, relative to the
/// analyzed root — the handle for reading its manifest layout and crate
/// root back off disk.
fn rust_crate_dirs(graph: &CallGraph) -> BTreeMap<&str, &str> {
    let mut dirs: BTreeMap<&str, &str> = BTreeMap::new();
    for node in &graph.nodes {
        if AuditLang::of(node) != Some(AuditLang::Rust) {
            continue;
        }
        if let Some(crate_dir) = crate_dir_of(&node.file) {
            dirs.entry(crate_of(&node.module)).or_insert(crate_dir);
        }
    }
    dirs
}

fn audit_scope(
    root: &Path,
    graph: &CallGraph,
    crate_dirs: &BTreeMap<&str, &str>,
    public_function_count: usize,
    audited_function_count: usize,
) -> Audit {
    let crates: BTreeSet<&str> = graph
        .nodes
        .iter()
        .filter(|node| AuditLang::of(node) == Some(AuditLang::Rust))
        .map(|node| crate_of(&node.module))
        .collect();
    Audit {
        public_function_count,
        re_exported_function_count: public_function_count - audited_function_count,
        single_crate: crates.len() == 1,
        mixed_target_crates: crate_dirs
            .iter()
            .filter(|(_, crate_dir)| has_both_targets(&root.join(crate_dir)))
            .map(|(krate, _)| (*krate).to_owned())
            .collect(),
        crates: crates.into_iter().map(ToOwned::to_owned).collect(),
        unsupported_language_function_count: graph
            .nodes
            .iter()
            .filter(|node| !node.is_test && AuditLang::of(node).is_none())
            .count(),
    }
}

/// The package directory a Rust display path sits in — everything above
/// its `src/` — relative to the analyzed root. `None` for files outside
/// a `src/` tree, which carry no Cargo layout to inspect.
fn crate_dir_of(file: &str) -> Option<&str> {
    let file = file.trim_start_matches("./");
    if let Some((prefix, _)) = file.split_once("/src/") {
        return Some(prefix);
    }
    file.starts_with("src/").then_some("")
}

/// Whether a Cargo package has both a library and a binary root. They
/// compile to two crates sharing one name, which the module paths
/// cannot separate, so a `pub(crate)` proposed there may be reaching
/// across a real crate boundary.
fn has_both_targets(crate_root: &Path) -> bool {
    let src = crate_root.join("src");
    src.join("lib.rs").is_file() && (src.join("main.rs").is_file() || src.join("bin").is_dir())
}

/// Names each Rust crate root hands out with `pub use`.
///
/// A `pub use` puts the name itself in the crate's public API, so
/// narrowing what it points at does not merely risk breaking a caller —
/// it breaks the re-export statement. Those items are dropped from the
/// audit rather than reported, which is what keeps the report off a
/// library's intended surface.
///
/// The scan is textual and reads only `src/lib.rs` per crate. It knows
/// `pub use` items, brace groups, `as` aliases, and globs; it does not
/// follow `pub use` chains through intermediate modules, and it treats
/// `pub mod` as no evidence at all — a public module path is how most
/// Rust APIs are reached, and excluding on it would empty the report.
#[derive(Debug, Default)]
struct ReExports {
    /// Crate name → identifiers that crate re-exports by name.
    names: BTreeMap<String, BTreeSet<String>>,
    /// Crate name → module paths it re-exports wholesale (`pub use
    /// foo::*`), as written in the source.
    globs: BTreeMap<String, BTreeSet<String>>,
}

impl ReExports {
    fn scan(root: &Path, crate_dirs: &BTreeMap<&str, &str>) -> Self {
        let mut index = Self::default();
        for (krate, crate_dir) in crate_dirs {
            let Ok(source) = std::fs::read_to_string(root.join(crate_dir).join("src/lib.rs"))
            else {
                continue;
            };
            let (names, globs) = parse_pub_use(&source);
            if !names.is_empty() {
                index.names.insert((*krate).to_owned(), names);
            }
            if !globs.is_empty() {
                index.globs.insert((*krate).to_owned(), globs);
            }
        }
        index
    }

    /// Whether the crate root re-exports this function under its own
    /// name, or — for a method — the type that owns it. A method on a
    /// re-exported type is reachable through that type, so narrowing it
    /// is no safer than narrowing the type.
    fn covers(&self, node: &CallGraphNode) -> bool {
        let krate = crate_of(&node.module);
        let exposed = node.impl_owner.as_deref().unwrap_or(&node.name);
        if self
            .names
            .get(krate)
            .is_some_and(|names| names.contains(exposed))
        {
            return true;
        }
        // A glob is written relative to the crate root, so compare it
        // against the module path with the crate prefix dropped.
        let relative = node.module.strip_prefix(krate).unwrap_or(&node.module);
        let relative = relative.strip_prefix("::").unwrap_or(relative);
        self.globs.get(krate).is_some_and(|globs| {
            globs
                .iter()
                .any(|glob| in_scope(relative, normalize_use_path(glob), true))
        })
    }
}

/// Collect the identifiers and glob prefixes a source file re-exports
/// with `pub use`. Statements are taken line-first so a `pub use` inside
/// a doc comment or string cannot contribute.
fn parse_pub_use(source: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut names = BTreeSet::new();
    let mut globs = BTreeSet::new();
    let mut statement: Option<String> = None;
    for line in source.lines() {
        let line = line.trim();
        let body = match statement.as_mut() {
            Some(pending) => {
                pending.push(' ');
                pending.push_str(line);
                pending
            }
            None => {
                let Some(rest) = line.strip_prefix("pub use ") else {
                    continue;
                };
                statement.insert(rest.to_owned())
            }
        };
        let Some(end) = body.find(';') else {
            continue;
        };
        let body = body[..end].to_owned();
        statement = None;
        collect_use_tree(&body, "", &mut names, &mut globs);
    }
    (names, globs)
}

/// Split one `pub use` body into the names and glob prefixes it exposes,
/// recursing so a nested group (`a::{b, c::{d, e}}`) contributes its
/// leaves rather than the raw group text.
fn collect_use_tree(
    body: &str,
    prefix: &str,
    names: &mut BTreeSet<String>,
    globs: &mut BTreeSet<String>,
) {
    let body = body.trim();
    let Some(open) = body.find('{') else {
        collect_use_leaf(body, prefix, names, globs);
        return;
    };
    let inner_prefix = join_use_path(prefix, body[..open].trim().trim_end_matches("::"));
    for item in split_top_level(brace_group(&body[open..])) {
        collect_use_tree(item, &inner_prefix, names, globs);
    }
}

/// The text inside the brace group starting at `body`, which begins with
/// `{`, up to its matching close brace.
fn brace_group(body: &str) -> &str {
    let mut depth = 0usize;
    for (offset, ch) in body.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return &body[1..offset];
                }
            }
            _ => {}
        }
    }
    &body[1..]
}

fn join_use_path(prefix: &str, rest: &str) -> String {
    match (prefix.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_owned(),
        (_, true) => prefix.to_owned(),
        _ => format!("{prefix}::{rest}"),
    }
}

/// One leaf of a use tree: `foo`, `foo as bar`, `a::b`, or `a::*`.
fn collect_use_leaf(
    leaf: &str,
    prefix: &str,
    names: &mut BTreeSet<String>,
    globs: &mut BTreeSet<String>,
) {
    let path = leaf.split(" as ").next().unwrap_or(leaf).trim();
    if path.is_empty() {
        return;
    }
    let full = join_use_path(prefix, path);
    match full.strip_suffix("::*").or(full.strip_suffix('*')) {
        Some(glob) => {
            globs.insert(glob.trim_end_matches("::").to_owned());
        }
        None => {
            names.insert(name_last_segment(&full).to_owned());
        }
    }
}

/// Split a brace group on top-level commas, keeping nested groups whole.
fn split_top_level(inner: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (offset, ch) in inner.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(&inner[start..offset]);
                start = offset + 1;
            }
            _ => {}
        }
    }
    items.push(&inner[start..]);
    items
}

/// Drop the `crate::` / `self::` prefix a `use` path may carry so it can
/// be compared against a crate-relative module path.
fn normalize_use_path(path: &str) -> &str {
    path.strip_prefix("crate::")
        .or_else(|| path.strip_prefix("self::"))
        .unwrap_or(path)
}

/// Fold the caller modules into the narrowest scope containing all of
/// them, and turn that into a finding when it is narrower than the
/// declaration. Returns `None` for a function whose callers already need
/// the visibility it has.
fn classify(
    graph: &CallGraph,
    idx: usize,
    lang: AuditLang,
    callers: Option<&BTreeSet<usize>>,
) -> Option<Finding> {
    let node = &graph.nodes[idx];
    let callers: Vec<usize> = callers
        .map(|c| c.iter().copied().collect())
        .unwrap_or_default();
    let caller_modules: BTreeSet<&str> = callers
        .iter()
        .map(|&caller| graph.nodes[caller].module.as_str())
        .collect();

    let (scope, scope_module) = match lang {
        AuditLang::Rust => rust_scope(&node.module, &caller_modules)?,
        AuditLang::Go => go_scope(&node.module, &caller_modules)?,
    };
    Some(Finding {
        id: node.id.clone(),
        qualified_name: node.qualified_name.clone(),
        file: node.file.clone(),
        start_line: node.start_line,
        end_line: node.end_line,
        loc: node.weights.loc,
        visibility: node.visibility,
        caller_scope: scope,
        suggested_visibility: suggest(lang, scope, &node.module, scope_module),
        scope_module: scope_module.to_owned(),
        caller_count: callers.len(),
        test_caller_count: callers
            .iter()
            .filter(|&&caller| graph.nodes[caller].is_test)
            .count(),
        caller_modules: caller_modules.into_iter().map(ToOwned::to_owned).collect(),
        ambiguous_calls_outside_scope: 0,
        ubiquitous_name_calls_outside_scope: 0,
    })
}

/// Rust: fold every caller module into the deepest module that is an
/// ancestor of (or equal to) both the definition's module and all of
/// them. An empty fold means a caller in another crate, which is the
/// one case `pub` is actually needed for.
fn rust_scope<'a>(
    module: &'a str,
    caller_modules: &BTreeSet<&str>,
) -> Option<(CallerScope, &'a str)> {
    if caller_modules.is_empty() {
        return Some((CallerScope::NoResolvedCallers, module));
    }
    let mut scope = module;
    for caller_module in caller_modules {
        scope = common_ancestor(scope, caller_module);
    }
    if scope.is_empty() {
        return None;
    }
    if scope == module {
        return Some((CallerScope::SameModule, scope));
    }
    if scope == crate_of(module) {
        return Some((CallerScope::SameCrate, scope));
    }
    Some((CallerScope::AncestorModule, scope))
}

/// Go: the package is the whole boundary, so only callers in the very
/// same package leave room to unexport. A caller in a sub-package is as
/// external as any other.
fn go_scope<'a>(
    module: &'a str,
    caller_modules: &BTreeSet<&str>,
) -> Option<(CallerScope, &'a str)> {
    if caller_modules.is_empty() {
        return Some((CallerScope::NoResolvedCallers, module));
    }
    caller_modules
        .iter()
        .all(|&caller_module| caller_module == module)
        .then_some((CallerScope::SameModule, module))
}

/// The declaration that would still compile, spelled the way the
/// language writes it.
fn suggest(lang: AuditLang, scope: CallerScope, module: &str, scope_module: &str) -> String {
    match (lang, scope) {
        (_, CallerScope::NoResolvedCallers) => {
            "verify: no resolved caller in the analyzed tree".to_owned()
        }
        (AuditLang::Go, _) => "unexport: lowercase the initial letter".to_owned(),
        (AuditLang::Rust, CallerScope::SameModule) => "drop `pub`".to_owned(),
        (AuditLang::Rust, CallerScope::SameCrate) => "pub(crate)".to_owned(),
        (AuditLang::Rust, CallerScope::AncestorModule) => {
            if parent_of(module) == Some(scope_module) {
                "pub(super)".to_owned()
            } else {
                // `pub(in …)` paths are written from the crate root,
                // while graph modules carry the real crate name.
                format!("pub(in {})", rewrite_crate_prefix(scope_module))
            }
        }
    }
}

/// Count, per finding, the call sites that could still reach it from
/// outside the proposed scope: ambiguous sites carrying it in their
/// candidate set, and receiver calls the resolver declined to attribute
/// (see [`gated_by_name`]). Both are reasons to check a row before
/// narrowing it, and neither drops it.
fn annotate_outside_calls(graph: &CallGraph, findings: &mut [(usize, Finding)]) {
    if findings.is_empty() {
        return;
    }
    let index_by_id = graph.node_index_by_id();
    let mut by_node: BTreeMap<usize, usize> = BTreeMap::new();
    let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (slot, (idx, _)) in findings.iter().enumerate() {
        by_node.insert(*idx, slot);
        by_name
            .entry(graph.nodes[*idx].name.as_str())
            .or_default()
            .push(slot);
    }

    for edge in &graph.edges {
        if !matches!(
            edge.resolution,
            Resolution::Ambiguous | Resolution::Unresolved
        ) {
            continue;
        }
        // A call site with no enclosing function has no module to place
        // it in, so it counts as outside every scope.
        let caller_module = edge
            .from
            .as_deref()
            .and_then(|from| index_by_id.get(from))
            .map(|&from_idx| graph.nodes[from_idx].module.as_str());
        let slots: Vec<usize> = match edge.resolution {
            Resolution::Ambiguous => edge
                .candidates
                .iter()
                .filter_map(|candidate| index_by_id.get(candidate.as_str()))
                .filter_map(|idx| by_node.get(idx).copied())
                .collect(),
            _ => match edge.callee_name.as_deref() {
                Some(callee) => by_name
                    .get(callee)
                    .map(|slots| {
                        slots
                            .iter()
                            .copied()
                            .filter(|&slot| gated_by_name(&graph.nodes[findings[slot].0], callee))
                            .collect()
                    })
                    .unwrap_or_default(),
                None => Vec::new(),
            },
        };
        for slot in slots {
            let (idx, finding) = &mut findings[slot];
            let lang = AuditLang::of(&graph.nodes[*idx]);
            let inside = caller_module.is_some_and(|caller_module| {
                in_scope(
                    caller_module,
                    &finding.scope_module,
                    lang.is_some_and(AuditLang::scope_covers_descendants),
                )
            });
            if inside {
                continue;
            }
            if edge.resolution == Resolution::Ambiguous {
                finding.ambiguous_calls_outside_scope += edge.call_count;
            } else {
                finding.ubiquitous_name_calls_outside_scope += edge.call_count;
            }
        }
    }
}

/// Whether an unresolved site named `callee` is one the resolver
/// deliberately declined to attribute to `node`.
///
/// The resolver leaves a call site unresolved for three reasons: no
/// workspace function carries the name (then `node` is not among them),
/// the site was written as a path that no workspace qualified name ends
/// with (then it was checked against `node` and ruled out), or the site
/// is a receiver call on a name the language's standard library owns —
/// `.clone()`, `.get()`, `.map()` — where the name is the only evidence
/// and it is worthless. Only that last class can still be a caller of
/// `node`, and it is exactly the one this predicate keeps.
fn gated_by_name(node: &CallGraphNode, callee: &str) -> bool {
    AuditLang::of(node).is_some_and(|lang| lang.ubiquitous_names().contains(callee))
}

fn summarize(modules: &[ModuleGroup], audited_function_count: usize) -> Summary {
    let findings = || modules.iter().flat_map(|group| &group.findings);
    let count = |scope: CallerScope| findings().filter(|f| f.caller_scope == scope).count();
    let total = findings().count();
    Summary {
        over_exposed_count: total,
        over_exposed_share: if audited_function_count == 0 {
            0.0
        } else {
            total as f64 / audited_function_count as f64
        },
        same_module_count: count(CallerScope::SameModule),
        ancestor_module_count: count(CallerScope::AncestorModule),
        same_crate_count: count(CallerScope::SameCrate),
        no_resolved_caller_count: count(CallerScope::NoResolvedCallers),
        possible_external_caller_count: findings().filter(|f| f.possible_external_caller()).count(),
        module_count: modules.len(),
    }
}

/// Group findings by defining module, most findings first. Rows inside a
/// module follow the narrowing order (biggest reduction first), then
/// source order, so a module's section reads as an edit list.
fn module_groups(graph: &CallGraph, findings: Vec<(usize, Finding)>) -> Vec<ModuleGroup> {
    let mut by_module: BTreeMap<&str, Vec<Finding>> = BTreeMap::new();
    for (idx, finding) in findings {
        by_module
            .entry(graph.nodes[idx].module.as_str())
            .or_default()
            .push(finding);
    }
    let mut groups: Vec<ModuleGroup> = by_module
        .into_iter()
        .map(|(module, mut findings)| {
            findings.sort_by(|a, b| {
                (a.caller_scope, &a.file, a.start_line, &a.id).cmp(&(
                    b.caller_scope,
                    &b.file,
                    b.start_line,
                    &b.id,
                ))
            });
            ModuleGroup {
                module: module.to_owned(),
                finding_count: findings.len(),
                called_count: findings.iter().filter(|f| f.caller_count > 0).count(),
                findings,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        (Reverse(a.called_count), Reverse(a.finding_count), &a.module).cmp(&(
            Reverse(b.called_count),
            Reverse(b.finding_count),
            &b.module,
        ))
    });
    groups
}

/// Whether `module` lies inside `scope`: equal to it, or — where the
/// language inherits visibility downward — below it.
fn in_scope(module: &str, scope: &str, covers_descendants: bool) -> bool {
    module == scope
        || (covers_descendants
            && module
                .strip_prefix(scope)
                .is_some_and(|rest| rest.starts_with("::")))
}

/// The longest `::`-segment prefix shared by both module paths, borrowed
/// from the first.
fn common_ancestor<'a>(a: &'a str, b: &str) -> &'a str {
    let mut end = 0;
    for (left, right) in a.split("::").zip(b.split("::")) {
        if left != right {
            break;
        }
        if end > 0 {
            end += "::".len();
        }
        end += left.len();
    }
    &a[..end]
}

/// First segment of a Rust module path: the crate the item belongs to.
fn crate_of(module: &str) -> &str {
    module.split_once("::").map_or(module, |(head, _)| head)
}

fn parent_of(module: &str) -> Option<&str> {
    module.rsplit_once("::").map(|(parent, _)| parent)
}

/// Graph module paths start at the real crate name; Rust source spells
/// an in-crate path `crate::…`.
fn rewrite_crate_prefix(module: &str) -> String {
    match module.split_once("::") {
        Some((_, rest)) => format!("crate::{rest}"),
        None => "crate".to_owned(),
    }
}

fn format_markdown(report: &Report, top: Option<usize>) -> String {
    let limit = top.unwrap_or(DEFAULT_TOP);
    let summary = &report.summary;
    let mut out = format!(
        "# Over-exposed visibility: {} ({}/{} audited function(s), across {} module(s))\n",
        report.root,
        summary.over_exposed_count,
        report.audit.audited_function_count(),
        summary.module_count,
    );
    let _ = writeln!(out, "\n{}", report.note);
    render_audit(&mut out, &report.audit);

    if report.audit.audited_function_count() == 0 {
        out.push_str(
            "\n_No `pub` (Rust) or exported (Go) function was left to audit._ Export status is \
             only extracted for Rust and Go, TypeScript and Python functions are never audited, \
             and `pub use` re-exports are treated as intended API.\n",
        );
        return out;
    }
    if report.modules.is_empty() {
        out.push_str("\n_Every public function has a resolved caller that needs it._\n");
        return out;
    }

    render_counts(&mut out, summary);
    render_module_sections(
        &mut out,
        "Over-exposed by module (most called findings first",
        &report.modules,
        limit,
        FINDINGS_PER_MODULE,
    );
    render_module_confidence(
        &mut out,
        &report.resolution,
        "Call sites in these modules resolved worst, so a caller that needs the current visibility \
         is the most likely to have been missed — their functions are the least certain rows \
         above.",
    );
    out
}

fn render_audit(out: &mut String, audit: &Audit) {
    let _ = writeln!(
        out,
        "\nAudited {} of {} non-test public function(s) across {} Rust crate(s); {} left out as \
         `pub use` re-exports{}.",
        audit.audited_function_count(),
        audit.public_function_count,
        audit.crates.len(),
        audit.re_exported_function_count,
        if audit.unsupported_language_function_count > 0 {
            format!(
                "; {} TypeScript/Python function(s) skipped (their adapters extract no export \
                 status)",
                audit.unsupported_language_function_count,
            )
        } else {
            String::new()
        },
    );
    if audit.single_crate {
        let _ = writeln!(
            out,
            "Only one crate is in scope, so no cross-crate caller can exist in this graph: a \
             library's intended public API is indistinguishable from over-exposure here. Run \
             against the workspace root to tell them apart.",
        );
    }
    if !audit.mixed_target_crates.is_empty() {
        let _ = writeln!(
            out,
            "Library and binary roots share a package name in {}: those are two crates, and module \
             paths cannot separate them. A `pub(crate)` row whose callers sit in the binary needs \
             to stay `pub`.",
            audit
                .mixed_target_crates
                .iter()
                .map(|krate| format!("`{krate}`"))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
}

fn render_counts(out: &mut String, summary: &Summary) {
    let _ = writeln!(
        out,
        "\nNarrowing candidates: {} contained by their own module, {} by an ancestor module, {} by \
         their crate. {} have no resolved caller at all, and {} carry a call site outside the \
         proposed scope that might reach them.",
        summary.same_module_count,
        summary.ancestor_module_count,
        summary.same_crate_count,
        summary.no_resolved_caller_count,
        summary.possible_external_caller_count,
    );
}

impl ModuleSection for ModuleGroup {
    fn module(&self) -> &str {
        &self.module
    }

    fn item_count(&self) -> usize {
        self.finding_count
    }

    fn heading_detail(&self) -> String {
        format!(
            "{} finding(s), {} with resolved callers",
            self.finding_count, self.called_count,
        )
    }

    fn render_items(&self, out: &mut String, limit: usize) {
        for finding in self.findings.iter().take(limit) {
            let _ = writeln!(out, "- {}", render_finding(finding));
        }
    }
}

fn render_finding(finding: &Finding) -> String {
    let mut row = format!(
        "`{}` ({}:{}, {} LOC) → {}",
        finding.qualified_name,
        finding.file,
        finding.start_line,
        finding.loc,
        finding.suggested_visibility,
    );
    if finding.caller_count == 0 {
        row.push_str("; no resolved caller");
    } else {
        let _ = write!(
            row,
            "; {} caller(s) in {}",
            finding.caller_count,
            render_caller_modules(&finding.caller_modules),
        );
        if finding.test_caller_count > 0 {
            let _ = write!(row, ", {} in tests", finding.test_caller_count);
        }
    }
    if finding.possible_external_caller() {
        let _ = write!(
            row,
            " — verify first: {} ambiguous and {} unattributable receiver call site(s) outside \
             `{}` name it",
            finding.ambiguous_calls_outside_scope,
            finding.ubiquitous_name_calls_outside_scope,
            finding.scope_module,
        );
    }
    row
}

fn render_caller_modules(modules: &[String]) -> String {
    let listed = modules
        .iter()
        .take(CALLER_MODULES_PER_ROW)
        .map(|m| format!("`{m}`"))
        .collect::<Vec<_>>()
        .join(", ");
    let overflow = modules.len().saturating_sub(CALLER_MODULES_PER_ROW);
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
        let json = VisibilityAnalyzer::new()
            .analyze(path, OutputFormat::Json)
            .unwrap();
        serde_json::from_str(&json).unwrap()
    }

    fn analyze_md(path: &Path) -> String {
        VisibilityAnalyzer::new()
            .analyze(path, OutputFormat::Md)
            .unwrap()
    }

    /// `(qualified name, suggested visibility)` for every finding, in
    /// report order.
    fn suggestions(report: &Value) -> Vec<(String, String)> {
        report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["findings"].as_array().unwrap())
            .map(|f| {
                (
                    f["qualified_name"].as_str().unwrap().to_owned(),
                    f["suggested_visibility"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    /// The suggestion reported for one function, or `None` when it is
    /// not a finding — the callers it has already need what it declares.
    fn suggestion_for(report: &Value, qualified_name: &str) -> Option<String> {
        suggestions(report)
            .into_iter()
            .find(|(name, _)| name == qualified_name)
            .map(|(_, suggestion)| suggestion)
    }

    /// Write a minimal Cargo package so module paths carry a real crate
    /// name and the crate-root scan has a `src/lib.rs` to read.
    fn write_crate(dir: &Path, name: &str, files: &[(&str, &str)]) {
        write_file(
            dir,
            &format!("{name}/Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\n"),
        );
        for (path, source) in files {
            write_file(dir, &format!("{name}/src/{path}"), source);
        }
    }

    /// Each caller distance gets its own narrowing. `deep::target` is
    /// called from its own module, from a sibling under `deep`'s parent,
    /// from an unrelated top-level module, and — for `crossing` — from
    /// another crate, which is the one case `pub` is really needed for.
    const NARROWING_LIB: &str = "pub mod a;\npub mod b;\n";
    const NARROWING_A: &str = "pub mod deep;\npub mod sibling;\n";

    fn write_narrowing_crate(dir: &Path) {
        write_crate(
            dir,
            "app",
            &[
                ("lib.rs", NARROWING_LIB),
                ("a.rs", NARROWING_A),
                (
                    "a/deep.rs",
                    "pub mod deeper;\n\
                     pub fn module_only() -> usize { 1 }\n\
                     pub fn from_sibling() -> usize { 2 }\n\
                     pub fn from_far() -> usize { 3 }\n\
                     pub fn crossing() -> usize { 4 }\n\
                     pub fn local_caller() -> usize { module_only() }\n",
                ),
                ("a/deep/deeper.rs", "pub fn from_uncle() -> usize { 5 }\n"),
                (
                    "a/sibling.rs",
                    "pub fn calls_deep() -> usize {\n\
                     crate::a::deep::from_sibling() + crate::a::deep::deeper::from_uncle()\n\
                     }\n",
                ),
                (
                    "b.rs",
                    "pub fn calls_deep() -> usize { crate::a::deep::from_far() }\n",
                ),
            ],
        );
        write_crate(
            dir,
            "other",
            &[(
                "lib.rs",
                "pub fn calls_app() -> usize { app::a::deep::crossing() }\n",
            )],
        );
    }

    #[test]
    fn each_caller_distance_maps_to_the_narrowest_visibility_that_still_compiles() {
        let dir = tempfile::tempdir().unwrap();
        write_narrowing_crate(dir.path());

        let report = analyze_json(dir.path());
        assert_eq!(report["schema_version"], 1);
        let suggested = |name: &str| suggestion_for(&report, name);
        assert_eq!(
            suggested("app::a::deep::module_only").as_deref(),
            Some("drop `pub`"),
        );
        assert_eq!(
            suggested("app::a::deep::from_sibling").as_deref(),
            Some("pub(super)"),
            "the parent module has its own spelling",
        );
        assert_eq!(
            suggested("app::a::deep::deeper::from_uncle").as_deref(),
            Some("pub(in crate::a)"),
            "a grandparent needs the explicit path, written from the crate root",
        );
        assert_eq!(
            suggested("app::a::deep::from_far").as_deref(),
            Some("pub(crate)"),
        );
        assert_eq!(
            suggested("app::a::deep::crossing"),
            None,
            "a cross-crate caller needs the `pub` it has: {report}",
        );

        // One row per bucket, so a mis-bucketed finding cannot hide in a
        // total: `module_only`, `from_sibling`, `from_uncle`, `from_far`,
        // plus the callers, which nothing calls in turn.
        let summary = &report["summary"];
        assert_eq!(summary["same_module_count"], 1, "report: {report}");
        assert_eq!(summary["ancestor_module_count"], 2);
        assert_eq!(summary["same_crate_count"], 1);
        assert_eq!(summary["no_resolved_caller_count"], 4);
        assert_eq!(summary["over_exposed_count"], 8);
        // Eight of the nine audited public functions; `crossing` is the
        // one its callers still need.
        assert_eq!(summary["over_exposed_share"], 8.0 / 9.0);

        let deep = report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["module"] == "app::a::deep")
            .unwrap_or_else(|| panic!("report: {report}"));
        assert_eq!(deep["finding_count"], 4);
        assert_eq!(
            deep["called_count"], 3,
            "`crossing` is absent and `local_caller` has no caller: {deep}",
        );
    }

    /// The parent module is spelled `pub(super)` rather than the
    /// equivalent `pub(in …)` path.
    #[test]
    fn a_parent_module_caller_is_reported_as_pub_super() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[
                ("lib.rs", "pub mod parent;\n"),
                (
                    "parent.rs",
                    "pub mod child;\npub fn calls_child() -> usize { crate::parent::child::target() }\n",
                ),
                ("parent/child.rs", "pub fn target() -> usize { 1 }\n"),
            ],
        );

        assert_eq!(
            suggestion_for(&analyze_json(dir.path()), "app::parent::child::target").as_deref(),
            Some("pub(super)"),
        );
    }

    /// A private item is visible to the whole subtree below its module,
    /// so a caller in a descendant module still means "drop `pub`".
    #[test]
    fn a_caller_below_the_defining_module_still_permits_a_private_item() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[
                ("lib.rs", "pub mod top;\n"),
                ("top.rs", "pub mod inner;\npub fn target() -> usize { 1 }\n"),
                (
                    "top/inner.rs",
                    "pub fn calls_up() -> usize { crate::top::target() }\n",
                ),
            ],
        );

        assert_eq!(
            suggestion_for(&analyze_json(dir.path()), "app::top::target").as_deref(),
            Some("drop `pub`"),
        );
    }

    /// Go has one boundary. A caller in the same package leaves room to
    /// unexport; a caller in a sub-package is as external as any other,
    /// which is the opposite of the Rust rule above.
    #[rstest]
    #[case::same_package(
        "pkg/caller.go",
        "package pkg\n\nfunc CallsTarget() int { return Target() }\n",
        Some("unexport: lowercase the initial letter")
    )]
    #[case::sub_package(
        "pkg/sub/caller.go",
        "package sub\n\nfunc CallsTarget() int { return pkg.Target() }\n",
        None
    )]
    fn go_exports_narrow_only_for_callers_in_the_very_same_package(
        #[case] caller_path: &str,
        #[case] caller_source: &str,
        #[case] expected: Option<&str>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "pkg/target.go",
            "package pkg\n\nfunc Target() int { return 1 }\n",
        );
        write_file(dir.path(), caller_path, caller_source);

        let report = analyze_json(dir.path());
        assert_eq!(
            suggestion_for(&report, "pkg::Target").as_deref(),
            expected,
            "report: {report}",
        );
        if expected.is_some() {
            assert!(
                analyze_md(dir.path()).contains("unexport: lowercase the initial letter"),
                "Go narrowing is spelled as unexporting",
            );
        }
    }

    #[test]
    fn a_crate_root_re_export_takes_its_target_out_of_the_audit() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[
                (
                    "lib.rs",
                    "pub mod inner;\npub use inner::{Owner, published};\n",
                ),
                (
                    "inner.rs",
                    "pub struct Owner;\n\
                     impl Owner { pub fn method(&self) -> usize { 1 } }\n\
                     pub fn published() -> usize { 1 }\n\
                     pub fn private_to_the_crate() -> usize { 2 }\n\
                     pub fn caller() -> usize { private_to_the_crate() }\n",
                ),
            ],
        );

        let report = analyze_json(dir.path());
        let names: Vec<String> = suggestions(&report)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert!(
            !names.contains(&"app::inner::published".to_owned()),
            "a re-exported function is API, not over-exposure: {report}",
        );
        assert!(
            !names.contains(&"app::inner::Owner::method".to_owned()),
            "a method on a re-exported type is reachable through it: {report}",
        );
        assert!(
            names.contains(&"app::inner::private_to_the_crate".to_owned()),
            "an item no re-export names is still audited: {report}",
        );
        // `Owner`, `published`, and `Owner::method` — the type counts as
        // a re-export of its methods, not of itself (it is no function).
        assert_eq!(report["audit"]["re_exported_function_count"], 2);
        assert_eq!(report["audit"]["public_function_count"], 4);
        assert!(
            analyze_md(dir.path()).contains("Audited 2 of 4 non-test public function(s)"),
            "the audited count is the public count minus the re-exports",
        );
    }

    #[rstest]
    #[case::plain("pub use inner::target;", &["target"], &[])]
    #[case::alias("pub use inner::target as renamed;", &["target"], &[])]
    #[case::group("pub use inner::{one, two as three};", &["one", "two"], &[])]
    #[case::glob("pub use inner::*;", &[], &["inner"])]
    #[case::group_with_glob("pub use inner::{one, deep::*};", &["one"], &["inner::deep"])]
    #[case::nested_path("pub use a::b::target;", &["target"], &[])]
    #[case::crate_prefix("pub use crate::inner::*;", &[], &["crate::inner"])]
    #[case::nested_group("pub use a::{b, c::{d, e}};", &["b", "d", "e"], &[])]
    #[case::nested_group_with_glob("pub use a::{b, c::{d, *}};", &["b", "d"], &["a::c"])]
    #[case::sibling_after_a_nested_group("pub use a::{b::{c}, d};", &["c", "d"], &[])]
    #[case::not_public("use inner::target;", &[], &[])]
    #[case::in_a_comment("// pub use inner::target;", &[], &[])]
    fn pub_use_statements_yield_their_names_and_globs(
        #[case] source: &str,
        #[case] expected_names: &[&str],
        #[case] expected_globs: &[&str],
    ) {
        let (names, globs) = parse_pub_use(source);
        assert_eq!(
            names.iter().map(String::as_str).collect::<Vec<_>>(),
            expected_names
        );
        assert_eq!(
            globs.iter().map(String::as_str).collect::<Vec<_>>(),
            expected_globs
        );
    }

    #[rstest]
    #[case::both("a", "b", "a::b")]
    #[case::no_prefix("", "b", "b")]
    #[case::no_rest("a", "", "a")]
    #[case::neither("", "", "")]
    fn use_paths_join_without_dangling_separators(
        #[case] prefix: &str,
        #[case] rest: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(join_use_path(prefix, rest), expected);
    }

    #[rstest]
    #[case::flat("a, b", &["a", " b"])]
    #[case::nested_group_stays_whole("a, b::{c, d}, e", &["a", " b::{c, d}", " e"])]
    #[case::single_item("only", &["only"])]
    #[case::empty("", &[""])]
    fn brace_groups_split_on_top_level_commas_only(#[case] inner: &str, #[case] expected: &[&str]) {
        assert_eq!(split_top_level(inner), expected);
    }

    /// A brace group split across lines is one statement, and the
    /// terminating `;` is what ends it.
    #[test]
    fn a_multi_line_pub_use_is_read_as_one_statement() {
        let (names, globs) = parse_pub_use(
            "pub use inner::{\n    one,\n    two as renamed,\n};\npub fn not_a_use() {}\n",
        );
        assert_eq!(
            names.iter().map(String::as_str).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert!(globs.is_empty());
    }

    /// A glob covers everything under the module it names, so items in
    /// nested modules below it are re-exported too.
    #[test]
    fn a_glob_re_export_covers_the_module_subtree() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[
                (
                    "lib.rs",
                    "pub mod inner;\npub mod kept;\npub use inner::*;\n",
                ),
                ("inner.rs", "pub fn covered() -> usize { 1 }\n"),
                ("kept.rs", "pub fn audited() -> usize { 1 }\n"),
            ],
        );

        let names: Vec<String> = suggestions(&analyze_json(dir.path()))
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, ["app::kept::audited"]);
    }

    /// An ambiguous call the resolver could not place, and a receiver
    /// call on a name it refuses to attribute, both mean a caller may be
    /// hiding outside the proposed scope. Neither drops the row; both
    /// flag it.
    #[test]
    fn calls_that_could_not_be_attributed_flag_the_rows_they_might_reach() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[
                ("lib.rs", "pub mod one;\npub mod two;\npub mod caller;\n"),
                (
                    "one.rs",
                    "pub struct A;\nimpl A { pub fn shared(&self) -> usize { 1 } }\n\
                     pub fn quiet() -> usize { 2 }\n",
                ),
                (
                    "two.rs",
                    "pub struct B;\nimpl B { pub fn shared(&self) -> usize { 1 } }\n",
                ),
                (
                    "caller.rs",
                    "pub fn calls(v: &Vec<u8>) -> usize { shared() + v.clone().len() }\n",
                ),
            ],
        );

        let report = analyze_json(dir.path());
        let flagged: Vec<(&str, u64)> = report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["findings"].as_array().unwrap())
            .map(|f| {
                (
                    f["qualified_name"].as_str().unwrap(),
                    f["ambiguous_calls_outside_scope"].as_u64().unwrap(),
                )
            })
            .collect();
        assert!(
            flagged.contains(&("app::one::A::shared", 1))
                && flagged.contains(&("app::two::B::shared", 1)),
            "both candidates of the ambiguous call are flagged: {flagged:?}",
        );
        assert!(
            flagged.contains(&("app::one::quiet", 0)),
            "a function no unattributed site names stays unflagged: {flagged:?}",
        );

        assert_eq!(
            report["summary"]["possible_external_caller_count"], 2,
            "only the two ambiguous candidates count: {report}",
        );

        let md = analyze_md(dir.path());
        assert!(md.contains("verify first: 1 ambiguous"), "got: {md}");
        assert_eq!(
            md.matches("verify first").count(),
            2,
            "a row nothing names must not carry the caveat: {md}",
        );
    }

    /// Rust visibility is inherited downward, so an unattributable call
    /// from *below* the proposed scope is already inside it and must not
    /// argue against narrowing — while the same call is outside the
    /// scope of a function in a different subtree.
    #[test]
    fn an_unattributable_call_below_the_proposed_scope_is_inside_it() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[
                ("lib.rs", "pub mod one;\npub mod two;\n"),
                (
                    "one.rs",
                    "pub mod sub;\npub struct A;\n\
                     impl A { pub fn target(&self) -> usize { 1 } }\n\
                     pub fn local(a: &A) -> usize { A::target(a) }\n",
                ),
                (
                    "one/sub.rs",
                    "use crate::one::A;\npub fn from_below(a: &A) -> usize { a.target() }\n",
                ),
                (
                    "two.rs",
                    "pub struct B;\nimpl B { pub fn target(&self) -> usize { 2 } }\n",
                ),
            ],
        );

        let report = analyze_json(dir.path());
        let ambiguous_for = |name: &str| {
            report["modules"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|m| m["findings"].as_array().unwrap())
                .find(|f| f["qualified_name"] == name)
                .map(|f| f["ambiguous_calls_outside_scope"].as_u64().unwrap())
        };
        assert_eq!(
            ambiguous_for("app::one::A::target"),
            Some(0),
            "the ambiguous call sits under `app::one`, where a private item is visible: {report}",
        );
        assert_eq!(
            ambiguous_for("app::two::B::target"),
            Some(1),
            "the same call is outside `app::two`: {report}",
        );
    }

    /// `.clone()` is a name the resolver refuses to attribute from a
    /// receiver, so a workspace `clone` cannot be shown to have no
    /// outside caller — that is the one unresolved class worth counting.
    #[test]
    fn a_receiver_call_on_an_unattributable_name_counts_against_narrowing() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[
                ("lib.rs", "pub mod owner;\npub mod elsewhere;\n"),
                (
                    "owner.rs",
                    "pub struct W;\n\
                     impl W { pub fn clone(&self) -> usize { 1 } }\n\
                     pub fn helper() -> usize { 2 }\n\
                     pub fn local() -> usize { W.clone() + helper() }\n",
                ),
                (
                    "elsewhere.rs",
                    "pub fn unrelated(v: &Vec<u8>) -> usize { v.clone().len() }\n\
                     pub fn calls_out() -> usize { external::helper() }\n",
                ),
            ],
        );

        let report = analyze_json(dir.path());
        let clone = report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["findings"].as_array().unwrap())
            .find(|f| f["qualified_name"] == "app::owner::W::clone")
            .unwrap_or_else(|| panic!("report: {report}"));
        assert!(
            clone["ubiquitous_name_calls_outside_scope"]
                .as_u64()
                .unwrap()
                > 0,
            "got: {clone}",
        );
        let ordinary = report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["findings"].as_array().unwrap())
            .find(|f| f["qualified_name"] == "app::owner::helper")
            .unwrap_or_else(|| panic!("report: {report}"));
        assert_eq!(
            ordinary["ubiquitous_name_calls_outside_scope"], 0,
            "an unresolved path call the resolver already ruled out is not evidence: {ordinary}",
        );

        // The receiver call is the row's only evidence, so it alone has
        // to be enough to flag it.
        assert_eq!(clone["ambiguous_calls_outside_scope"], 0, "got: {clone}");
        assert_eq!(report["summary"]["possible_external_caller_count"], 1);
        assert!(
            analyze_md(dir.path()).contains("0 ambiguous and 1 unattributable receiver call"),
            "the caveat names the receiver call it rests on",
        );
    }

    /// A Go package is the whole boundary, so the sub-package call that
    /// Rust would treat as inside the scope is outside it here — the
    /// mirror of the Rust case above.
    #[test]
    fn a_go_sub_package_call_is_outside_the_package_scope() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "pkg/w.go",
            "package pkg\n\ntype W struct{}\n\nfunc (w W) String() string { return \"\" }\n\n\
             func Local(w W) string { return W.String(w) }\n",
        );
        write_file(
            dir.path(),
            "pkg/sub/s.go",
            "package sub\n\ntype T struct{}\n\nfunc Use(t T) string { return t.String() }\n",
        );

        let report = analyze_json(dir.path());
        let stringer = report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["findings"].as_array().unwrap())
            .find(|f| f["qualified_name"] == "pkg::W::String")
            .unwrap_or_else(|| panic!("report: {report}"));
        assert_eq!(
            stringer["caller_scope"], "same_module",
            "the typed path call inside the package is the scope: {stringer}",
        );
        assert_eq!(
            stringer["ubiquitous_name_calls_outside_scope"], 1,
            "`pkg/sub` is a different package, so its call is outside: {stringer}",
        );
    }

    /// Only a package holding *both* roots is two crates under one
    /// name. A library alone, or a binary alone, is one crate and needs
    /// no caveat.
    #[rstest]
    #[case::library_and_binary(
        &[("lib.rs", "pub mod inner;\n"), ("main.rs", "fn main() {}\n")],
        true
    )]
    #[case::library_and_bin_directory(
        &[("lib.rs", "pub mod inner;\n"), ("bin/tool.rs", "fn main() {}\n")],
        true
    )]
    #[case::library_only(&[("lib.rs", "pub mod inner;\n")], false)]
    #[case::binary_only(&[("main.rs", "fn main() {}\n")], false)]
    fn only_packages_holding_both_roots_are_named_as_a_caveat(
        #[case] roots: &[(&str, &str)],
        #[case] expected: bool,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let mut files = roots.to_vec();
        files.push(("inner.rs", "pub fn target() -> usize { 1 }\n"));
        write_crate(dir.path(), "app", &files);

        let report = analyze_json(dir.path());
        let named: Vec<String> = report["audit"]["mixed_target_crates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            named,
            if expected {
                vec!["app".to_owned()]
            } else {
                Vec::new()
            },
            "report: {report}",
        );
        assert_eq!(
            analyze_md(dir.path()).contains("Library and binary roots share a package name"),
            expected,
            "the caveat is rendered exactly when it applies",
        );
    }

    #[test]
    fn a_single_crate_run_says_cross_crate_callers_cannot_be_seen() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[
                ("lib.rs", "pub mod inner;\n"),
                ("inner.rs", "pub fn target() -> usize { 1 }\n"),
            ],
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["audit"]["single_crate"], true);
        assert!(
            analyze_md(dir.path()).contains("Only one crate is in scope"),
            "the single-crate caveat is rendered",
        );

        let mut two_crates = tempfile::tempdir().unwrap();
        write_crate(
            two_crates.path(),
            "app",
            &[("lib.rs", "pub fn target() -> usize { 1 }\n")],
        );
        write_crate(
            two_crates.path(),
            "other",
            &[("lib.rs", "pub fn other_target() -> usize { 1 }\n")],
        );
        let report = analyze_json(two_crates.path());
        assert_eq!(report["audit"]["single_crate"], false);
        assert!(!analyze_md(two_crates.path()).contains("Only one crate is in scope"));
        two_crates.disable_cleanup(false);
    }

    /// TypeScript and Python carry no export status, so their functions
    /// are counted as skipped instead of being judged either way.
    #[test]
    fn languages_without_export_status_are_counted_not_judged() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.ts",
            "export function exported(): number { return 1; }\n\
             export function caller(): number { return exported(); }\n",
        );
        write_file(dir.path(), "src/lib.py", "def helper():\n    return 1\n");
        write_file(
            dir.path(),
            "src/lib.test.ts",
            "export function spec(): number { return 1; }\n",
        );

        let report = analyze_json(dir.path());
        assert_eq!(report["audit"]["public_function_count"], 0);
        assert_eq!(
            report["audit"]["unsupported_language_function_count"], 3,
            "the count is of production functions; the test file's is not one: {report}",
        );
        assert!(report["modules"].as_array().unwrap().is_empty());

        let md = analyze_md(dir.path());
        assert!(
            md.contains("No `pub` (Rust) or exported (Go) function was left to audit."),
            "got: {md}",
        );
    }

    #[test]
    fn test_functions_are_never_audited_but_still_count_as_callers() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[
                ("lib.rs", "pub mod inner;\n"),
                (
                    "inner.rs",
                    "pub fn target() -> usize { 1 }\n\
                     #[cfg(test)]\n\
                     mod tests {\n\
                     pub fn helper() -> usize { super::target() }\n\
                     #[test]\n\
                     fn t() { assert_eq!(helper(), 1); }\n\
                     }\n",
                ),
            ],
        );

        let report = analyze_json(dir.path());
        let findings = suggestions(&report);
        assert_eq!(
            findings,
            [("app::inner::target".to_owned(), "drop `pub`".to_owned())],
            "the `pub fn` inside the test module is not an API surface: {report}",
        );
        let target = &report["modules"][0]["findings"][0];
        assert_eq!(target["caller_count"], 1);
        assert_eq!(target["test_caller_count"], 1);
        assert!(
            analyze_md(dir.path()).contains("1 in tests"),
            "a test-only caller is called out on the row",
        );
    }

    #[test]
    fn modules_rank_by_evidence_and_top_caps_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        let mut files = vec![("lib.rs", "pub mod a;\npub mod b;\npub mod c;\n".to_owned())];
        // `a` has a caller, `b` and `c` do not, so `a` must lead.
        files.push((
            "a.rs",
            "pub fn target() -> usize { 1 }\npub fn caller() -> usize { target() }\n".to_owned(),
        ));
        files.push(("b.rs", "pub fn lonely() -> usize { 1 }\n".to_owned()));
        files.push(("c.rs", "pub fn lonelier() -> usize { 1 }\n".to_owned()));
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, source)| (*path, source.as_str()))
            .collect();
        write_crate(dir.path(), "app", &files);

        let report = analyze_json(dir.path());
        let modules: Vec<&str> = report["modules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["module"].as_str().unwrap())
            .collect();
        assert_eq!(modules[0], "app::a", "report: {report}");

        let capped = VisibilityAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert_eq!(
            capped.lines().filter(|l| l.starts_with("### `")).count(),
            1,
            "got: {capped}",
        );
        assert!(
            capped.contains("+2 more module(s) not shown"),
            "got: {capped}"
        );
        assert!(
            !analyze_md(dir.path()).contains("module(s) not shown"),
            "nothing was dropped, so nothing is announced",
        );
    }

    #[rstest]
    #[case::one(&["a"], "`a`")]
    #[case::at_the_cap(&["a", "b", "c"], "`a`, `b`, `c`")]
    #[case::over_the_cap(&["a", "b", "c", "d", "e"], "`a`, `b`, `c` +2 more")]
    fn caller_modules_are_listed_up_to_the_cap_then_counted(
        #[case] modules: &[&str],
        #[case] expected: &str,
    ) {
        let modules: Vec<String> = modules.iter().map(|m| (*m).to_owned()).collect();
        assert_eq!(render_caller_modules(&modules), expected);
    }

    /// The module section heading is what tells an agent which module
    /// the findings below belong to and how many of them carry caller
    /// evidence, so both halves of it are asserted.
    #[test]
    fn each_module_section_is_headed_by_its_path_and_finding_counts() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[
                ("lib.rs", "pub mod inner;\n"),
                (
                    "inner.rs",
                    "pub fn called() -> usize { 1 }\n\
                     pub fn uncalled() -> usize { 2 }\n\
                     pub fn local() -> usize { called() }\n",
                ),
            ],
        );

        let md = analyze_md(dir.path());
        assert!(
            md.contains("### `app::inner` — 3 finding(s), 1 with resolved callers"),
            "got: {md}",
        );
    }

    /// Both sides of the per-module row cap: a module over it announces
    /// the remainder, a module at it announces nothing.
    #[test]
    fn the_per_module_row_cap_reports_only_a_real_remainder() {
        let over = tempfile::tempdir().unwrap();
        let mut source = String::new();
        for i in 0..(FINDINGS_PER_MODULE + 2) {
            let _ = writeln!(source, "pub fn f{i}() -> usize {{ {i} }}");
        }
        write_crate(
            over.path(),
            "app",
            &[("lib.rs", "pub mod inner;\n"), ("inner.rs", &source)],
        );
        let md = analyze_md(over.path());
        assert!(
            md.contains("+2 more (JSON output carries every row)"),
            "got: {md}",
        );
        assert_eq!(
            md.lines().filter(|l| l.starts_with("- `")).count(),
            FINDINGS_PER_MODULE,
        );
        // The counts line is the only place the bucket totals appear.
        assert!(
            md.contains("Narrowing candidates:"),
            "the summary counts are rendered: {md}",
        );

        let exact = tempfile::tempdir().unwrap();
        let mut source = String::new();
        for i in 0..FINDINGS_PER_MODULE {
            let _ = writeln!(source, "pub fn f{i}() -> usize {{ {i} }}");
        }
        write_crate(
            exact.path(),
            "app",
            &[("lib.rs", "pub mod inner;\n"), ("inner.rs", &source)],
        );
        let md = analyze_md(exact.path());
        assert!(
            !md.contains("more (JSON output carries every row)"),
            "nothing was dropped, so nothing is announced: {md}",
        );
    }

    /// `--only-tests` and `--exclude-tests` both reach the graph
    /// builder: one leaves no public function to audit, the other drops
    /// the test callers a finding would otherwise have.
    #[test]
    fn the_test_filters_reach_the_graph_the_audit_runs_on() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[
                ("lib.rs", "pub mod inner;\n"),
                (
                    "inner.rs",
                    "pub fn target() -> usize { 1 }\n\
                     #[cfg(test)]\n\
                     mod tests {\n\
                     #[test]\n\
                     fn t() { let _ = super::target(); }\n\
                     }\n",
                ),
            ],
        );

        let baseline = analyze_json(dir.path());
        assert_eq!(
            suggestion_for(&baseline, "app::inner::target").as_deref(),
            Some("drop `pub`"),
            "the test caller is what makes the module the scope: {baseline}",
        );

        let json = VisibilityAnalyzer::new()
            .with_exclude_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let excluded: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            suggestion_for(&excluded, "app::inner::target").as_deref(),
            Some("verify: no resolved caller in the analyzed tree"),
            "dropping the tests drops the only caller: {excluded}",
        );

        let json = VisibilityAnalyzer::new()
            .with_only_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let only_tests: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            only_tests["audit"]["public_function_count"], 0,
            "test functions are never audited, so nothing is left: {only_tests}",
        );
    }

    #[test]
    fn excluded_paths_leave_the_report_entirely() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[
                ("lib.rs", "pub mod kept;\npub mod dropped;\n"),
                ("kept.rs", "pub fn kept_one() -> usize { 1 }\n"),
                ("dropped.rs", "pub fn dropped_one() -> usize { 1 }\n"),
            ],
        );

        let json = VisibilityAnalyzer::new()
            .with_exclude_patterns(vec!["dropped.rs".to_owned()])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let report: Value = serde_json::from_str(&json).unwrap();
        let names: Vec<String> = suggestions(&report).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["app::kept::kept_one"]);
    }

    #[test]
    fn a_corpus_where_every_public_function_is_needed_says_so() {
        let dir = tempfile::tempdir().unwrap();
        write_crate(
            dir.path(),
            "app",
            &[("lib.rs", "pub fn target() -> usize { 1 }\n")],
        );
        write_crate(
            dir.path(),
            "other",
            &[("lib.rs", "pub fn caller() -> usize { app::target() }\n")],
        );

        let report = analyze_json(dir.path());
        assert_eq!(
            report["summary"]["over_exposed_count"], 1,
            "report: {report}"
        );
        // `other::caller` is the only unreferenced one left.
        assert_eq!(
            suggestions(&report)
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>(),
            ["other::caller"],
        );
    }

    #[test]
    fn an_empty_corpus_reports_nothing_to_audit() {
        let dir = tempfile::tempdir().unwrap();
        let report = analyze_json(dir.path());
        assert_eq!(report["audit"]["public_function_count"], 0);
        assert_eq!(report["summary"]["over_exposed_share"], 0.0);
        assert_eq!(report["language"], "unknown");
        assert!(report["modules"].as_array().unwrap().is_empty());
    }

    #[rstest]
    #[case("a::b::c", "a::b::d", "a::b")]
    #[case("a::b", "a::b", "a::b")]
    #[case("a::b", "c::d", "")]
    #[case("a::b", "a::bc", "a")]
    #[case("a", "a::b::c", "a")]
    fn common_ancestor_cuts_on_segment_boundaries(
        #[case] left: &str,
        #[case] right: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(common_ancestor(left, right), expected);
        assert_eq!(common_ancestor(right, left), expected);
    }

    /// Module paths built from a small alphabet, so collisions between
    /// segments and their prefixes (`b` vs `bc`) happen often enough to
    /// matter.
    fn module_path() -> impl Strategy<Value = String> {
        proptest::collection::vec(prop::sample::select(vec!["a", "b", "bc", "c"]), 1..5)
            .prop_map(|segments| segments.join("::"))
    }

    proptest! {
        /// The fold every Rust suggestion rests on: whatever
        /// `common_ancestor` returns must contain both inputs, and no
        /// longer prefix of either may.
        #[test]
        fn common_ancestor_is_the_deepest_shared_prefix(
            left in module_path(),
            right in module_path(),
        ) {
            let ancestor = common_ancestor(&left, &right);
            if ancestor.is_empty() {
                prop_assert_ne!(
                    left.split("::").next(),
                    right.split("::").next(),
                    "an empty ancestor means the paths diverge at the first segment",
                );
                return Ok(());
            }
            prop_assert!(in_scope(&left, ancestor, true));
            prop_assert!(in_scope(&right, ancestor, true));

            let depth = ancestor.split("::").count();
            let deeper = |path: &str| {
                path.split("::")
                    .take(depth + 1)
                    .collect::<Vec<_>>()
                    .join("::")
            };
            let (deeper_left, deeper_right) = (deeper(&left), deeper(&right));
            if deeper_left != ancestor {
                prop_assert!(
                    !in_scope(&right, &deeper_left, true),
                    "{deeper_left} is deeper than the reported ancestor yet contains {right}",
                );
            }
            if deeper_right != ancestor {
                prop_assert!(
                    !in_scope(&left, &deeper_right, true),
                    "{deeper_right} is deeper than the reported ancestor yet contains {left}",
                );
            }
        }

        /// Containment is symmetric with the ancestor fold: a module is
        /// inside a scope exactly when folding the two leaves that scope
        /// standing.
        #[test]
        fn containment_agrees_with_the_ancestor_fold(
            module in module_path(),
            scope in module_path(),
        ) {
            prop_assert_eq!(
                in_scope(&module, &scope, true),
                common_ancestor(&module, &scope) == scope,
            );
        }
    }

    #[rstest]
    #[case("a::b", "a::b", true, true)]
    #[case("a::b::c", "a::b", true, true)]
    #[case("a::b::c", "a::b", false, false)]
    #[case("a::bc", "a::b", true, false)]
    #[case("a", "a::b", true, false)]
    fn scope_containment_respects_segment_boundaries_and_language(
        #[case] module: &str,
        #[case] scope: &str,
        #[case] covers_descendants: bool,
        #[case] expected: bool,
    ) {
        assert_eq!(in_scope(module, scope, covers_descendants), expected);
    }
}
