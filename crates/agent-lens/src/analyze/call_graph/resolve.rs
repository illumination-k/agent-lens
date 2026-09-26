//! Heuristic call-site resolution.
//!
//! Maps [`CallShape`]s onto graph node ids without type inference: an
//! adapter's semantic binding ([`CallShape::callee_binding`]) first, then
//! lexical path candidates, then last-segment fallbacks with path-suffix
//! and caller-crate narrowing. Every outcome records its
//! provenance ([`ResolutionMethod`]) and, when ambiguous, the full
//! candidate node-id set so downstream analyzers can widen traversals
//! instead of dropping the edge.
//!
//! The name fallback is switched off in two places, both for the same
//! reason — the name is the only evidence available and the language
//! already owns it, so a workspace match is a phantom edge rather than a
//! lucky hit:
//!
//! * a receiver call (`recv.foo()`) on a name the standard library
//!   defines on nearly every value — see
//!   [`GraphLanguage::ubiquitous_method_names`][super::model::GraphLanguage::ubiquitous_method_names];
//! * a plain call to a language builtin (`append(xs, x)`, `len(s)`) —
//!   see
//!   [`GraphLanguage::builtin_function_names`][super::model::GraphLanguage::builtin_function_names].
//!
//! Both leave the site [`Resolution::Unresolved`].
//!
//! A third case needs no name table at all: a callee bound in the
//! caller's own local scope (a closure or nested function held in a
//! local, a function-typed parameter). The adapter reports that as
//! [`CallShape::callee_is_locally_bound`], and the binding shadows every
//! definition outside the function, so the whole resolution ladder —
//! lexical candidates included — is skipped and the site stays
//! [`Resolution::Unresolved`].

use std::collections::{HashMap, HashSet};

use lens_domain::{CallShape, CalleeBinding, ReceiverExprKind, SyntaxFact, qualify_module};

use super::model::{CallGraphNode, GraphLanguage, Resolution, ResolutionMethod, name_last_segment};

/// Outcome of resolving one call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedCall {
    /// Target node id; `Some` only when `resolution` is `Resolved`.
    pub(crate) to: Option<String>,
    /// Sorted candidate node ids; non-empty only when `resolution` is
    /// `Ambiguous`.
    pub(crate) candidates: Vec<String>,
    pub(crate) resolution: Resolution,
    /// Strategy that produced `to`/`candidates`; `None` for
    /// unresolved and anonymous outcomes.
    pub(crate) method: Option<ResolutionMethod>,
}

impl ResolvedCall {
    fn resolved(id: String, method: ResolutionMethod) -> Self {
        Self {
            to: Some(id),
            candidates: Vec::new(),
            resolution: Resolution::Resolved,
            method: Some(method),
        }
    }

    fn ambiguous(mut candidates: Vec<String>, method: ResolutionMethod) -> Self {
        candidates.sort_unstable();
        candidates.dedup();
        Self {
            to: None,
            candidates,
            resolution: Resolution::Ambiguous,
            method: Some(method),
        }
    }

    fn unresolved() -> Self {
        Self {
            to: None,
            candidates: Vec::new(),
            resolution: Resolution::Unresolved,
            method: None,
        }
    }

    fn anonymous() -> Self {
        Self {
            to: None,
            candidates: Vec::new(),
            resolution: Resolution::Anonymous,
            method: None,
        }
    }
}

/// Attributes call sites to their enclosing function node by exact
/// (file, qualified name) match, and by line span where one file
/// declares that name twice.
pub(crate) struct CallerIndex {
    by_file_and_qualified_name: HashMap<(String, String), Vec<CallerSpan>>,
}

struct CallerSpan {
    id: String,
    start_line: usize,
    end_line: usize,
}

impl CallerIndex {
    pub(crate) fn new(nodes: &[CallGraphNode]) -> Self {
        let mut by_file_and_qualified_name: HashMap<(String, String), Vec<CallerSpan>> =
            HashMap::new();
        for node in nodes {
            by_file_and_qualified_name
                .entry((node.file.clone(), node.qualified_name.clone()))
                .or_default()
                .push(CallerSpan {
                    id: node.id.clone(),
                    start_line: node.start_line,
                    end_line: node.end_line,
                });
        }
        Self {
            by_file_and_qualified_name,
        }
    }

    /// The node a call on `line` of `file` inside `qualified_name` is
    /// made from. One file can declare the same name twice — Rust's
    /// `impl Display for Version` and `impl Debug for Version` both
    /// define `Version::fmt` — and then the span containing the call
    /// picks the caller.
    pub(crate) fn resolve_in_file(
        &self,
        file: &str,
        qualified_name: &str,
        line: usize,
    ) -> Option<String> {
        let spans = self
            .by_file_and_qualified_name
            .get(&(file.to_owned(), qualified_name.to_owned()))?;
        if let [only] = spans.as_slice() {
            return Some(only.id.clone());
        }
        let mut containing = spans
            .iter()
            .filter(|span| span.start_line <= line && line <= span.end_line);
        match (containing.next(), containing.next()) {
            (Some(span), None) => Some(span.id.clone()),
            _ => None,
        }
    }
}

pub(crate) struct Resolver {
    qualified: HashMap<String, Vec<String>>,
    last_segment: HashMap<String, Vec<String>>,
    id_to_qualified: HashMap<String, String>,
    /// Ids of nodes declared with an owner (a method, not a free
    /// function). Read where the call's shape decides which of the two
    /// it can reach — see [`GraphLanguage::call_shape_decides_owner`].
    methods: HashSet<String>,
}

impl Resolver {
    pub(crate) fn new(nodes: &[CallGraphNode]) -> Self {
        let mut qualified: HashMap<String, Vec<String>> = HashMap::new();
        let mut last_segment: HashMap<String, Vec<String>> = HashMap::new();
        let mut id_to_qualified: HashMap<String, String> = HashMap::new();
        let mut methods = HashSet::new();
        for node in nodes {
            if node.impl_owner.is_some() {
                methods.insert(node.id.clone());
            }
            qualified
                .entry(node.qualified_name.clone())
                .or_default()
                .push(node.id.clone());
            last_segment
                .entry(name_last_segment(&node.qualified_name).to_owned())
                .or_default()
                .push(node.id.clone());
            id_to_qualified.insert(node.id.clone(), node.qualified_name.clone());
        }
        Self {
            qualified,
            last_segment,
            id_to_qualified,
            methods,
        }
    }

    /// `language` is the language of the file the call site lives in;
    /// it selects the ubiquitous-method-name table consulted for
    /// receiver calls and the builtin table consulted for plain calls.
    pub(crate) fn resolve(&self, site: &CallShape, language: GraphLanguage) -> ResolvedCall {
        let Some(callee_name) = site.callee_name() else {
            return ResolvedCall::anonymous();
        };
        // `self.method()` — receiver is exactly `self`, so the callee must
        // be a method on the impl/trait owner. Resolve lexically to
        // `Owner::method` in the caller's module without type inference.
        if matches!(
            site.receiver_expr_kind,
            SyntaxFact::Known(ReceiverExprKind::SelfValue)
        ) {
            return self.resolve_self_method(site, callee_name);
        }
        // The adapter bound the name: a declaration that is a node is the
        // target, and a global or an external package's import is no
        // workspace function whatever its name. A declaration that is no
        // node (a module-scope value, a barrel's re-export) falls through
        // to the heuristics below.
        match site.callee_binding() {
            Some(CalleeBinding::Declaration(target)) => {
                if let Some(ids) = self.qualified.get(target) {
                    return resolve_ids(ids, ResolutionMethod::Binding);
                }
            }
            Some(CalleeBinding::External) => return ResolvedCall::unresolved(),
            None => {}
        }
        if site.has_receiver_expression() {
            return self.resolve_receiver_method(site, callee_name, language);
        }
        // `emit := func(...) {...}; emit(x)` — the callee is a local
        // binding, so the program never reaches a workspace definition of
        // that name. Skipping the ladder here rather than only the name
        // fallback is what the language itself does: the binding shadows
        // the enclosing module's own `emit` just as it shadows another
        // module's. Short idiomatic local names (`emit`, `send`, `next`,
        // `done`) collide with methods somewhere in any medium-sized
        // corpus, and the fabricated edge shows up as a module cycle in
        // `layers` and as inflated fan-in in `hubs`.
        if site.callee_is_locally_bound() {
            return ResolvedCall::unresolved();
        }
        for candidate in lexical_candidates(site) {
            if let Some(ids) = self.qualified.get(&candidate) {
                return resolve_ids(ids, ResolutionMethod::Lexical);
            }
        }
        // A bare call to a language builtin (`append(xs, x)`, `len(s)`)
        // is not a workspace call, but the name fallback below cannot
        // see that: it matches the last segment alone, so one workspace
        // function — or method, since methods are indexed by their last
        // segment too — absorbs every builtin call site in the corpus.
        // Lexical resolution above already had its chance, so a module
        // that defines and calls its own `append` keeps that edge.
        if language.builtin_function_names().contains(callee_name) {
            return ResolvedCall::unresolved();
        }
        let Some(ids) = self.last_segment.get(callee_name) else {
            return ResolvedCall::unresolved();
        };
        // A bare `Domain(b)` can name a function (or be a conversion to
        // the type `Domain`), never a method: Go has no implicit
        // receiver, so `UUID.Domain` is out of reach of that call, and
        // Rust needs a `Self::` / `Type::` path to reach an associated
        // function.
        let qualified_path = site.callee_path().is_some_and(|path| path.contains("::"));
        let ids = &self.with_owner_shape(ids, false, language, qualified_path);
        if ids.is_empty() {
            return ResolvedCall::unresolved();
        }
        // When the callee was written as a multi-segment path like
        // `Foo::new`, restrict the fallback to candidates whose
        // qualified name ends with that path. Catches calls reaching a
        // type through a glob import, and avoids mislabeling external
        // calls like `String::new()` as ambiguous against unrelated
        // workspace `new` methods.
        if let Some(callee_path) = site.callee_path()
            && callee_path.contains("::")
        {
            let narrowed = self.narrow_by_path(ids, site, &callee_path, language);
            return if narrowed.is_empty() {
                ResolvedCall::unresolved()
            } else {
                resolve_ids(&narrowed, ResolutionMethod::PathSuffix)
            };
        }
        resolve_ids(ids, ResolutionMethod::LastSegment)
    }

    /// `ids` narrowed to the ones a multi-segment `callee_path` can
    /// name: by suffix, then through the import binding its head, then
    /// (Rust) by the type anywhere in the crate the path starts with.
    fn narrow_by_path(
        &self,
        ids: &[String],
        site: &CallShape,
        callee_path: &str,
        language: GraphLanguage,
    ) -> Vec<String> {
        let mut narrowed = self.narrow_by_path_suffix(ids, callee_path);
        if narrowed.is_empty()
            && let Some(expanded) = import_expanded_path(site, callee_path)
        {
            narrowed = self.narrow_by_import_path(ids, &expanded);
        }
        if narrowed.is_empty() && language == GraphLanguage::Rust {
            narrowed = self.narrow_by_type_in_crate(ids, callee_path);
        }
        narrowed
    }

    fn resolve_self_method(&self, site: &CallShape, callee_name: &str) -> ResolvedCall {
        let Some(module) = site.caller_module() else {
            return ResolvedCall::unresolved();
        };
        let Some(owner) = site.caller_owner() else {
            return ResolvedCall::unresolved();
        };
        // A closure inside a method (`Ky::retry::closure#1`) sees the
        // method's `this`, so the owner is tried from the innermost
        // segment outwards until one declares the callee.
        let mut owner = owner;
        loop {
            let candidate = qualify_module(module, &format!("{owner}::{callee_name}"));
            if let Some(ids) = self.qualified.get(&candidate) {
                return resolve_ids(ids, ResolutionMethod::SelfMethod);
            }
            match owner.rsplit_once("::") {
                Some((outer, _)) => owner = outer,
                None => return ResolvedCall::unresolved(),
            }
        }
    }

    /// Receiver method calls (`obj.foo()`) cannot be type-inferred
    /// without semantic analysis, so we resolve heuristically by
    /// last-segment match, then narrow ambiguous matches to the
    /// caller's crate.
    ///
    /// * Receiver bound by an import, with a node at the imported path
    ///   (`text.shout()` after `from app import text`) →
    ///   [`Resolution::Resolved`] via [`ResolutionMethod::Lexical`].
    /// * Ubiquitous method name → [`Resolution::Unresolved`], whatever
    ///   the candidates. The name is the only evidence a receiver call
    ///   offers, and for `.clone()` / `.get()` / `.map()` it says
    ///   nothing: nearly every such site targets std, so a workspace
    ///   match is a phantom edge rather than a lucky hit.
    /// * 0 candidates → [`Resolution::Unresolved`] (likely external/std).
    /// * 1 candidate → [`Resolution::Resolved`].
    /// * Many candidates with exactly one in the caller's crate →
    ///   [`Resolution::Resolved`] for that crate-local match.
    /// * Otherwise → [`Resolution::Ambiguous`], carrying the narrowest
    ///   candidate set the heuristics reached.
    ///
    /// Residual false-positive risk is a workspace-specific name whose
    /// unique match is not the real callee. The crate narrowing keeps
    /// that bounded to the caller's crate when multiple candidates
    /// exist; users who want precision should prefer typed paths, which
    /// carry the owner in the path and so bypass this method entirely —
    /// including for ubiquitous names (`Foo::clone(x)` still resolves).
    fn resolve_receiver_method(
        &self,
        site: &CallShape,
        callee_name: &str,
        language: GraphLanguage,
    ) -> ResolvedCall {
        // `text.shout()` after `from app import text`: the receiver is an
        // imported name, so the import says what it is. Only an exact
        // node under the imported path counts — an imported *object*
        // (`from app.cfg import settings; settings.load()`) names no
        // such node and falls through to the name heuristics below.
        if let Some(expanded) = site
            .callee_path()
            .and_then(|path| import_expanded_path(site, &path))
            && let Some(ids) = self.qualified.get(&expanded)
        {
            return resolve_ids(ids, ResolutionMethod::Lexical);
        }
        if language.ubiquitous_method_names().contains(callee_name) {
            return ResolvedCall::unresolved();
        }
        let Some(ids) = self.last_segment.get(callee_name) else {
            return ResolvedCall::unresolved();
        };
        // `wg.Add(1)` on a local value is a method call; a package-level
        // `tag.Add` shares the name but no receiver reaches it. A
        // receiver the adapter knows holds a value (`retry.delay()` on a
        // parameter) says the same in any language.
        let ids = if matches!(
            site.receiver_expr_kind,
            SyntaxFact::Known(ReceiverExprKind::LocalValue)
        ) {
            self.with_owner(ids, true)
        } else {
            self.with_owner_shape(ids, true, language, false)
        };
        if ids.is_empty() {
            return ResolvedCall::unresolved();
        }
        self.resolve_with_crate_narrowing(&ids, site)
    }

    /// `ids` kept to methods (`method == true`) or to free functions,
    /// where the language lets the call's shape decide between the two;
    /// every id otherwise.
    fn with_owner_shape(
        &self,
        ids: &[String],
        method: bool,
        language: GraphLanguage,
        qualified_path: bool,
    ) -> Vec<String> {
        if !language.call_shape_decides_owner(qualified_path) {
            return ids.to_vec();
        }
        self.with_owner(ids, method)
    }

    /// `ids` kept to methods (`method == true`) or to free functions.
    fn with_owner(&self, ids: &[String], method: bool) -> Vec<String> {
        ids.iter()
            .filter(|id| self.methods.contains(id.as_str()) == method)
            .cloned()
            .collect()
    }

    fn resolve_with_crate_narrowing(&self, ids: &[String], site: &CallShape) -> ResolvedCall {
        if let [id] = ids {
            return ResolvedCall::resolved(id.clone(), ResolutionMethod::LastSegment);
        }
        let Some(caller_crate) = caller_crate_segment(site) else {
            return ResolvedCall::ambiguous(ids.to_vec(), ResolutionMethod::LastSegment);
        };
        let local: Vec<String> = ids
            .iter()
            .filter(|id| self.node_in_crate(id, caller_crate))
            .cloned()
            .collect();
        match local.as_slice() {
            [] => ResolvedCall::ambiguous(ids.to_vec(), ResolutionMethod::LastSegment),
            [id] => ResolvedCall::resolved(id.clone(), ResolutionMethod::CrateNarrowed),
            _ => ResolvedCall::ambiguous(local, ResolutionMethod::CrateNarrowed),
        }
    }

    fn node_in_crate(&self, id: &str, crate_name: &str) -> bool {
        self.id_to_qualified
            .get(id)
            .is_some_and(|qualified| qualified_in_crate(qualified, crate_name))
    }

    /// Candidates whose qualified name ends the import-expanded path, on
    /// segment boundaries, with at least the module segment the call
    /// named. An import path is rooted where the graph's module paths are
    /// not — a Go import carries the module path from `go.mod`
    /// (`example.com::shapes::geo`), the node only its package directory
    /// (`geo::Area`) — so the node is a suffix of the import rather than
    /// the other way round, which is what [`Self::narrow_by_path_suffix`]
    /// checks. Only consulted when that forward match found nothing: an
    /// aliased import (`shapes.Area()`) is the case it exists for.
    fn narrow_by_import_path(&self, ids: &[String], expanded: &str) -> Vec<String> {
        ids.iter()
            .filter(|id| {
                self.id_to_qualified
                    .get(id.as_str())
                    .is_some_and(|qualified| {
                        qualified.contains("::")
                            && (expanded == qualified
                                || expanded.ends_with(&format!("::{qualified}")))
                    })
            })
            .cloned()
            .collect()
    }

    /// Candidates named `Type::name` anywhere in `krate`, for a Rust
    /// path `krate::Type::name`. A type at a crate's root is usually a
    /// re-export, so the path says where the type is visible, not where
    /// its `impl` sits: `fastrand::Rng::new()` reaches `impl Rng` in
    /// `fastrand::global_rng`, which the plain suffix match cannot see.
    /// A longer path spells out the module and gets no such leeway (a
    /// `m::inner::Error` re-exported from a dependency is no workspace
    /// `Error`), and only a type-cased segment before the name
    /// qualifies, so `krate::module::func` stays with the suffix match.
    fn narrow_by_type_in_crate(&self, ids: &[String], callee_path: &str) -> Vec<String> {
        let segments: Vec<&str> = callee_path.split("::").collect();
        let [krate, owner, name] = segments.as_slice() else {
            return Vec::new();
        };
        if !owner.starts_with(char::is_uppercase) {
            return Vec::new();
        }
        let suffix = format!("::{owner}::{name}");
        ids.iter()
            .filter(|id| {
                self.id_to_qualified
                    .get(id.as_str())
                    .is_some_and(|qualified| {
                        qualified.ends_with(&suffix) && qualified_in_crate(qualified, krate)
                    })
            })
            .cloned()
            .collect()
    }

    fn narrow_by_path_suffix(&self, ids: &[String], callee_path: &str) -> Vec<String> {
        let suffix = format!("::{callee_path}");
        ids.iter()
            .filter(|id| {
                self.id_to_qualified
                    .get(id.as_str())
                    .is_some_and(|qualified| {
                        qualified == callee_path || qualified.ends_with(&suffix)
                    })
            })
            .cloned()
            .collect()
    }
}

/// `callee_path` with its first segment replaced by the import that
/// binds it (`shapes::Area` → `example.com::shapes::geo::Area` after
/// `import shapes "example.com/shapes/geo"`). `None` for a single
/// segment or a head no import binds.
fn import_expanded_path(site: &CallShape, callee_path: &str) -> Option<String> {
    let segments: Vec<&str> = callee_path.split("::").collect();
    if segments.len() < 2 {
        return None;
    }
    let target = alias_target(site, segments[0])?;
    prefix_with_tail(module_segments(target), &segments)
}

fn resolve_ids(ids: &[String], method: ResolutionMethod) -> ResolvedCall {
    if let [id] = ids {
        ResolvedCall::resolved(id.clone(), method)
    } else {
        ResolvedCall::ambiguous(ids.to_vec(), method)
    }
}

pub(crate) fn lexical_candidates(site: &CallShape) -> Vec<String> {
    let Some(callee_name) = site.callee_name() else {
        return Vec::new();
    };
    let Some(module) = site.caller_module() else {
        return Vec::new();
    };
    let Some(callee_path) = site.callee_path() else {
        return vec![qualify_module(module, callee_name)];
    };
    let segments: Vec<&str> = callee_path.split("::").collect();
    if segments.is_empty() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    match segments[0] {
        "crate" => candidates.push(callee_path.to_owned()),
        "self" => {
            if let Some(path) = prefix_with_tail(module_segments(module), &segments) {
                candidates.push(path);
            }
        }
        "super" => {
            if let Some(path) = resolve_super_path(module, &segments) {
                candidates.push(path);
            }
        }
        "Self" => {
            if let Some(owner) = site.caller_owner()
                && let Some(tail) = join_tail(&segments, 1)
            {
                candidates.push(qualify_module(module, &format!("{owner}::{tail}")));
            }
        }
        _ => {
            if segments.len() == 1 {
                candidates.push(qualify_module(module, callee_name));
            } else {
                candidates.push(qualify_module(module, &callee_path));
            }
            if let Some(alias_target) = alias_target(site, segments[0])
                && let Some(path) = prefix_with_tail(
                    alias_target.split("::").map(ToOwned::to_owned).collect(),
                    &segments,
                )
            {
                candidates.push(path);
            }
        }
    }
    if segments.len() == 1
        && let Some(alias_target) = alias_target(site, segments[0])
    {
        candidates.push(alias_target.to_owned());
    }
    dedupe_preserving_order(candidates)
}

fn alias_target<'a>(site: &'a CallShape, alias: &str) -> Option<&'a str> {
    site.visible_imports
        .iter()
        .rev()
        .find(|entry| {
            matches!(
                &entry.local_alias,
                SyntaxFact::Known(Some(local_alias)) if local_alias == alias
            )
        })
        .and_then(|entry| entry.imported_module.known_value())
        .map(String::as_str)
}

fn module_segments(module: &str) -> Vec<String> {
    module.split("::").map(ToOwned::to_owned).collect()
}

/// Join `prefix` with everything past `segments`' first element — the
/// `self` keyword or alias already accounted for by `prefix`. (Every
/// caller skipped exactly one segment, so the old `tail_start`
/// parameter was `analyze parameters`' first constant-argument finding
/// about its own sources.)
fn prefix_with_tail(mut prefix: Vec<String>, segments: &[&str]) -> Option<String> {
    if segments.is_empty() {
        return None;
    }
    prefix.extend(segments.iter().skip(1).map(|s| (*s).to_owned()));
    Some(prefix.join("::"))
}

fn resolve_super_path(module: &str, segments: &[&str]) -> Option<String> {
    let mut absolute = module_segments(module);
    for segment in segments {
        if *segment == "super" {
            if absolute.len() <= 1 {
                return None;
            }
            absolute.pop();
        } else {
            absolute.push((*segment).to_owned());
        }
    }
    Some(absolute.join("::"))
}

fn join_tail(segments: &[&str], start: usize) -> Option<String> {
    if start >= segments.len() {
        None
    } else {
        Some(segments[start..].join("::"))
    }
}

fn dedupe_preserving_order(items: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            out.push(item);
        }
    }
    out
}

/// First segment of the call site's lexical module — the crate that
/// owns the caller. `None` when the module is unknown or empty.
fn caller_crate_segment(site: &CallShape) -> Option<&str> {
    site.caller_module()
        .and_then(|module| module.split("::").next())
        .filter(|s| !s.is_empty())
}

fn qualified_in_crate(qualified: &str, crate_name: &str) -> bool {
    qualified == crate_name
        || qualified
            .strip_prefix(crate_name)
            .is_some_and(|rest| rest.starts_with("::"))
}

#[cfg(test)]
mod tests {
    use super::super::model::{NodeVisibility, NodeWeights, ResolutionCallCounts};
    use super::*;
    use lens_domain::{ImportShape, ReceiverExprKind};
    use rstest::rstest;

    fn site(path: &str) -> CallShape {
        CallShape {
            callee_display_name: SyntaxFact::Known(path.rsplit("::").next().map(ToOwned::to_owned)),
            callee_path_segments: SyntaxFact::Known(
                path.split("::").map(ToOwned::to_owned).collect(),
            ),
            caller_module: SyntaxFact::Known("crate::m".to_owned()),
            caller_qualified_name: SyntaxFact::Known(Some("crate::m::caller".to_owned())),
            caller_owner: SyntaxFact::Known(Some("S".to_owned())),
            receiver_expr_kind: SyntaxFact::Known(ReceiverExprKind::None),
            arguments: SyntaxFact::Unknown,
            callee_is_locally_bound: SyntaxFact::Known(false),
            callee_binding: SyntaxFact::Unknown,
            lexical_resolution: lens_domain::LexicalResolutionStatus::NotAttempted,
            visible_imports: vec![
                ImportShape {
                    local_alias: SyntaxFact::Known(Some("parse".to_owned())),
                    imported_module: SyntaxFact::Known("crate::a::parse".to_owned()),
                    exported_symbol: SyntaxFact::Unknown,
                },
                ImportShape {
                    local_alias: SyntaxFact::Known(Some("a".to_owned())),
                    imported_module: SyntaxFact::Known("crate::a".to_owned()),
                    exported_symbol: SyntaxFact::Unknown,
                },
            ],
            line: 1,
        }
    }

    #[rstest]
    #[case::absolute("crate::a::parse", &["crate::a::parse"])]
    #[case::self_relative("self::parse", &["crate::m::parse"])]
    #[case::super_relative("super::parse", &["crate::parse"])]
    #[case::self_type("Self::helper", &["crate::m::S::helper"])]
    #[case::local_type("S::helper", &["crate::m::S::helper"])]
    #[case::imported_module_alias("a::parse", &["crate::m::a::parse", "crate::a::parse"])]
    #[case::imported_function_alias("parse", &["crate::m::parse", "crate::a::parse"])]
    fn lexical_candidate_generation_is_ordered(#[case] path: &str, #[case] expected: &[&str]) {
        assert_eq!(lexical_candidates(&site(path)), expected);
    }

    #[test]
    fn lexical_path_helpers_handle_boundaries() {
        assert_eq!(
            prefix_with_tail(vec!["crate".to_owned(), "m".to_owned()], &["self"]).as_deref(),
            Some("crate::m"),
        );
        assert_eq!(resolve_super_path("crate", &["super", "parse"]), None);
        assert_eq!(
            resolve_super_path("crate::a::b", &["super", "super", "parse"]).as_deref(),
            Some("crate::parse"),
        );
        assert_eq!(join_tail(&["Self"], 1), None);
        assert_eq!(join_tail(&["Self", "parse"], 1).as_deref(), Some("parse"));
    }

    #[rstest]
    #[case::single_segment("parse", None)]
    #[case::unbound_head("b::parse", None)]
    #[case::two_segments("a::parse", Some("crate::a::parse"))]
    #[case::three_segments("a::S::helper", Some("crate::a::S::helper"))]
    fn import_expanded_path_replaces_an_imported_head(
        #[case] path: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(import_expanded_path(&site(path), path).as_deref(), expected);
    }

    fn receiver_site(name: &str) -> CallShape {
        CallShape {
            receiver_expr_kind: SyntaxFact::Known(ReceiverExprKind::Expression),
            ..site(name)
        }
    }

    /// A node whose owner is the segment before the name when that
    /// segment is type-cased (`crate::m::W::clone` is a method of `W`,
    /// `crate::m::helper` a free function).
    fn node(qualified_name: &str) -> CallGraphNode {
        let impl_owner = qualified_name
            .rsplit("::")
            .nth(1)
            .filter(|owner| owner.starts_with(char::is_uppercase))
            .map(ToOwned::to_owned);
        CallGraphNode {
            id: format!("src/lib.rs:{qualified_name}:1"),
            name: name_last_segment(qualified_name).to_owned(),
            qualified_name: qualified_name.to_owned(),
            file: "src/lib.rs".to_owned(),
            module: "crate::m".to_owned(),
            impl_owner,
            owner_kind: None,
            start_line: 1,
            end_line: 2,
            is_test: false,
            visibility: NodeVisibility::Unknown,
            param_count: None,
            param_names: None,
            has_receiver: None,
            attributes: None,
            weights: NodeWeights::default(),
            outgoing_calls: ResolutionCallCounts::default(),
            delegation: None,
        }
    }

    /// The name tables are per-language, so the same workspace and the
    /// same call site resolve differently depending on which adapter
    /// produced the call: `clone` is a `std` method in Rust and an
    /// ordinary identifier everywhere else.
    #[rstest]
    #[case::rust_denies_clone(GraphLanguage::Rust, "clone", Resolution::Unresolved)]
    #[case::rust_allows_workspace_name(GraphLanguage::Rust, "with_children", Resolution::Resolved)]
    #[case::typescript_denies_map(GraphLanguage::TypeScript, "map", Resolution::Unresolved)]
    #[case::typescript_allows_rust_name(GraphLanguage::TypeScript, "clone", Resolution::Resolved)]
    #[case::python_denies_append(GraphLanguage::Python, "append", Resolution::Unresolved)]
    #[case::python_allows_rust_name(GraphLanguage::Python, "clone", Resolution::Resolved)]
    #[case::go_denies_string(GraphLanguage::Go, "String", Resolution::Unresolved)]
    #[case::go_allows_rust_name(GraphLanguage::Go, "clone", Resolution::Resolved)]
    fn receiver_calls_on_ubiquitous_names_stay_unresolved_per_language(
        #[case] language: GraphLanguage,
        #[case] callee: &str,
        #[case] expected: Resolution,
    ) {
        let nodes: Vec<CallGraphNode> = ["clone", "with_children", "map", "append", "String"]
            .into_iter()
            .map(|name| node(&format!("crate::m::W::{name}")))
            .collect();
        let resolver = Resolver::new(&nodes);

        let call = resolver.resolve(&receiver_site(callee), language);

        assert_eq!(call.resolution, expected, "{callee} under {language:?}");
        assert_eq!(call.to.is_some(), expected == Resolution::Resolved);
    }

    /// A builtin is called bare, so it never reaches the receiver table
    /// — but the name fallback still applies, and a workspace method
    /// sharing the name would otherwise absorb every call site.
    #[rstest]
    #[case::go_denies_append(GraphLanguage::Go, "append", Resolution::Unresolved)]
    #[case::go_denies_len(GraphLanguage::Go, "len", Resolution::Unresolved)]
    #[case::go_allows_workspace_name(GraphLanguage::Go, "with_children", Resolution::Resolved)]
    #[case::python_denies_len(GraphLanguage::Python, "len", Resolution::Unresolved)]
    #[case::python_allows_go_builtin(GraphLanguage::Python, "append", Resolution::Resolved)]
    #[case::typescript_denies_parse_int(
        GraphLanguage::TypeScript,
        "parseInt",
        Resolution::Unresolved
    )]
    #[case::rust_denies_drop(GraphLanguage::Rust, "drop", Resolution::Unresolved)]
    #[case::rust_allows_go_builtin(GraphLanguage::Rust, "append", Resolution::Resolved)]
    fn plain_calls_to_builtins_stay_unresolved_per_language(
        #[case] language: GraphLanguage,
        #[case] callee: &str,
        #[case] expected: Resolution,
    ) {
        let nodes: Vec<CallGraphNode> = ["append", "len", "parseInt", "drop", "with_children"]
            .into_iter()
            .map(|name| node(&format!("crate::other::{name}")))
            .collect();
        let resolver = Resolver::new(&nodes);

        let call = resolver.resolve(&site(callee), language);

        assert_eq!(call.resolution, expected, "{callee} under {language:?}");
        assert_eq!(call.to.is_some(), expected == Resolution::Resolved);
    }

    /// Shadowing is legal, and a module calling the function it defined
    /// resolves lexically before the builtin table is ever consulted.
    #[test]
    fn a_module_calling_its_own_shadowing_definition_still_resolves() {
        let nodes = vec![node("crate::m::append")];
        let resolver = Resolver::new(&nodes);

        let call = resolver.resolve(&site("append"), GraphLanguage::Go);

        assert_eq!(call.resolution, Resolution::Resolved);
        assert_eq!(call.method, Some(ResolutionMethod::Lexical));
    }

    /// A path call carries the owner, so the table must not touch it —
    /// this is the evidence the receiver form lacks.
    #[rstest]
    #[case::typed_path("W::clone")]
    #[case::self_type_path("Self::clone")]
    fn typed_path_calls_on_ubiquitous_names_still_resolve(#[case] path: &str) {
        let nodes = vec![node("crate::m::W::clone"), node("crate::m::S::clone")];
        let resolver = Resolver::new(&nodes);

        let call = resolver.resolve(&site(path), GraphLanguage::Rust);

        assert_eq!(call.resolution, Resolution::Resolved, "{path}");
        assert!(call.to.is_some(), "{path}");
    }

    /// `self.clone()` is not a bare name match: the caller's own `impl`
    /// owner supplies the type, so the table leaves it alone.
    #[test]
    fn self_method_calls_on_ubiquitous_names_still_resolve() {
        let nodes = vec![node("crate::m::S::clone")];
        let resolver = Resolver::new(&nodes);
        let self_site = CallShape {
            receiver_expr_kind: SyntaxFact::Known(ReceiverExprKind::SelfValue),
            ..site("clone")
        };

        let call = resolver.resolve(&self_site, GraphLanguage::Rust);

        assert_eq!(call.resolution, Resolution::Resolved);
        assert_eq!(call.method, Some(ResolutionMethod::SelfMethod));
    }

    /// In a closure inside a method the callee is looked up on the
    /// enclosing owners, innermost first: an arrow keeps the method's
    /// `this`.
    #[rstest]
    #[case::method_owner("S", Some("crate::m::S::helper"))]
    #[case::closure_owner("S::run::closure#1", Some("crate::m::S::helper"))]
    #[case::inner_owner_wins("S::Inner", Some("crate::m::S::Inner::helper"))]
    #[case::no_owner_declares_it("T::run", None)]
    fn self_method_calls_search_enclosing_owners(
        #[case] owner: &str,
        #[case] expected: Option<&str>,
    ) {
        let nodes = vec![
            node("crate::m::S::helper"),
            node("crate::m::S::Inner::helper"),
        ];
        let resolver = Resolver::new(&nodes);
        let self_site = CallShape {
            receiver_expr_kind: SyntaxFact::Known(ReceiverExprKind::SelfValue),
            caller_owner: SyntaxFact::Known(Some(owner.to_owned())),
            ..site("helper")
        };

        let call = resolver.resolve(&self_site, GraphLanguage::TypeScript);

        let expected_id = expected.map(|qualified| format!("src/lib.rs:{qualified}:1"));
        assert_eq!(call.to, expected_id);
    }

    /// A receiver the adapter knows holds a value reaches methods only,
    /// in any language: `retry.delay()` is never the free `delay`. A
    /// plain `Expression` receiver keeps both in TypeScript, where an
    /// object literal or a module object may hold the free function.
    #[rstest]
    #[case::value_receiver(ReceiverExprKind::LocalValue, &["crate::m::W::delay"])]
    #[case::expression_receiver(
        ReceiverExprKind::Expression,
        &["crate::m::W::delay", "crate::m::delay"]
    )]
    fn value_receivers_reach_methods_only(
        #[case] receiver: ReceiverExprKind,
        #[case] expected: &[&str],
    ) {
        let nodes = vec![node("crate::m::W::delay"), node("crate::m::delay")];
        let resolver = Resolver::new(&nodes);
        let call_site = CallShape {
            receiver_expr_kind: SyntaxFact::Known(receiver),
            ..site("delay")
        };

        let call = resolver.resolve(&call_site, GraphLanguage::TypeScript);

        let mut reached: Vec<String> = call.to.into_iter().chain(call.candidates).collect();
        reached.sort();
        let mut expected: Vec<String> = expected
            .iter()
            .map(|qualified| format!("src/lib.rs:{qualified}:1"))
            .collect();
        expected.sort();
        assert_eq!(reached, expected);
    }

    /// Crate narrowing must not become a back door: several candidates
    /// with one in the caller's crate is still no evidence for a name
    /// `std` defines on everything.
    #[test]
    fn crate_narrowing_does_not_rescue_ubiquitous_receiver_names() {
        let nodes = vec![node("crate::m::W::clone"), node("other::W::clone")];
        let resolver = Resolver::new(&nodes);

        let call = resolver.resolve(&receiver_site("clone"), GraphLanguage::Rust);

        assert_eq!(call.resolution, Resolution::Unresolved);
        assert!(call.candidates.is_empty());
        assert_eq!(call.method, None);
    }

    fn locally_bound_site(path: &str) -> CallShape {
        CallShape {
            callee_is_locally_bound: SyntaxFact::Known(true),
            ..site(path)
        }
    }

    /// A closure or function-typed parameter shadows every workspace
    /// definition of its name, so the whole ladder is skipped — including
    /// the last-segment fallback that would otherwise mint an edge to an
    /// unrelated module's `emit`.
    #[rstest]
    #[case::name_fallback_to_another_module("other::W::emit")]
    #[case::name_fallback_to_a_free_function("other::emit")]
    #[case::lexical_match_in_the_callers_own_module("crate::m::emit")]
    fn locally_bound_callees_never_resolve(#[case] defined_at: &str) {
        let nodes = vec![node(defined_at)];
        let resolver = Resolver::new(&nodes);

        let call = resolver.resolve(&locally_bound_site("emit"), GraphLanguage::Go);

        assert_eq!(call.resolution, Resolution::Unresolved, "{defined_at}");
        assert_eq!(call.to, None);
        assert!(call.candidates.is_empty());
        assert_eq!(call.method, None);
    }

    /// The flag is per call site: the same name resolves normally where
    /// nothing binds it locally.
    #[test]
    fn the_same_name_resolves_where_it_is_not_locally_bound() {
        let nodes = vec![node("other::emit")];
        let resolver = Resolver::new(&nodes);

        let call = resolver.resolve(&site("emit"), GraphLanguage::Go);

        assert_eq!(call.resolution, Resolution::Resolved);
        assert_eq!(call.method, Some(ResolutionMethod::LastSegment));
    }

    /// `SyntaxFact::Unknown` means the adapter does not track scopes, not
    /// that the callee is shadowed — those sites keep resolving.
    #[test]
    fn unknown_local_binding_facts_do_not_suppress_resolution() {
        let nodes = vec![node("other::emit")];
        let resolver = Resolver::new(&nodes);
        let unknown = CallShape {
            callee_is_locally_bound: SyntaxFact::Unknown,
            ..site("emit")
        };

        let call = resolver.resolve(&unknown, GraphLanguage::Go);

        assert_eq!(call.resolution, Resolution::Resolved);
    }

    /// A receiver call names a method on the receiver's type, which a
    /// local of the same name does not shadow. Adapters only set the flag
    /// for bare calls, but pin the ordering so a future move of the check
    /// above `resolve_receiver_method` is caught.
    #[test]
    fn receiver_calls_are_unaffected_by_the_local_binding_flag() {
        let nodes = vec![node("crate::m::W::emit")];
        let resolver = Resolver::new(&nodes);
        let receiver = CallShape {
            callee_is_locally_bound: SyntaxFact::Known(true),
            ..receiver_site("emit")
        };

        let call = resolver.resolve(&receiver, GraphLanguage::Go);

        assert_eq!(call.resolution, Resolution::Resolved);
    }

    /// In Go and Rust a bare call reaches only free functions and a
    /// receiver call only methods, so the name fallback drops the other
    /// kind; the other languages keep both, since there the shape does
    /// not decide it.
    #[rstest]
    #[case::rust_receiver_call_skips_function(GraphLanguage::Rust, true, "crate::smoke::u8", None)]
    #[case::rust_bare_call_skips_method(GraphLanguage::Rust, false, "crate::m::Rng::u8", None)]
    #[case::rust_receiver_call_reaches_method(
        GraphLanguage::Rust,
        true,
        "crate::m::Rng::u8",
        Some("crate::m::Rng::u8")
    )]
    #[case::rust_bare_call_reaches_function(
        GraphLanguage::Rust,
        false,
        "crate::smoke::u8",
        Some("crate::smoke::u8")
    )]
    #[case::go_bare_call_skips_method(GraphLanguage::Go, false, "crate::m::UUID::Domain", None)]
    #[case::go_receiver_call_skips_function(GraphLanguage::Go, true, "crate::tag::Add", None)]
    #[case::go_bare_call_reaches_function(
        GraphLanguage::Go,
        false,
        "crate::tag::Add",
        Some("crate::tag::Add")
    )]
    #[case::go_receiver_call_reaches_method(
        GraphLanguage::Go,
        true,
        "crate::m::UUID::Domain",
        Some("crate::m::UUID::Domain")
    )]
    #[case::python_bare_call_keeps_method(
        GraphLanguage::Python,
        false,
        "crate::m::UUID::Domain",
        Some("crate::m::UUID::Domain")
    )]
    #[case::typescript_receiver_call_keeps_function(
        GraphLanguage::TypeScript,
        true,
        "crate::tag::Add",
        Some("crate::tag::Add")
    )]
    fn call_shape_decides_method_or_function(
        #[case] language: GraphLanguage,
        #[case] receiver: bool,
        #[case] defined: &str,
        #[case] expected: Option<&str>,
    ) {
        let resolver = Resolver::new(&[node(defined)]);
        let name = name_last_segment(defined);
        let site = if receiver {
            receiver_site(name)
        } else {
            CallShape {
                caller_module: SyntaxFact::Known("crate::other".to_owned()),
                ..site(name)
            }
        };

        let call = resolver.resolve(&site, language);

        assert_eq!(
            call.to
                .map(|id| resolver.id_to_qualified[&id].clone())
                .as_deref(),
            expected
        );
    }

    /// A Rust path keeps methods in reach whatever its length: `Rng::u8`
    /// names the associated function, and `fastrand::Rng::new` reaches
    /// `impl Rng` wherever in the `fastrand` crate it sits. A longer path
    /// spells out its module and a lowercase segment before the name is
    /// a module, so neither gets the crate-wide match.
    #[rstest]
    #[case::type_path("Rng::u8", "crate::m::Rng::u8", Some("crate::m::Rng::u8"))]
    #[case::root_reexport(
        "fastrand::Rng::new",
        "fastrand::global_rng::Rng::new",
        Some("fastrand::global_rng::Rng::new")
    )]
    #[case::other_crate("other::Rng::new", "fastrand::global_rng::Rng::new", None)]
    #[case::spelled_out_module("fastrand::inner::Rng::new", "fastrand::global_rng::Rng::new", None)]
    #[case::module_path("fastrand::global::new", "fastrand::x::global::new", None)]
    fn rust_paths_reach_methods(
        #[case] path: &str,
        #[case] defined: &str,
        #[case] expected: Option<&str>,
    ) {
        let resolver = Resolver::new(&[node(defined)]);
        let call_site = CallShape {
            caller_module: SyntaxFact::Known("fastrand::tests".to_owned()),
            ..site(path)
        };

        let call = resolver.resolve(&call_site, GraphLanguage::Rust);

        assert_eq!(
            call.to
                .map(|id| resolver.id_to_qualified[&id].clone())
                .as_deref(),
            expected,
            "{path}"
        );
        if expected.is_some() {
            assert_eq!(call.method, Some(ResolutionMethod::PathSuffix));
        }
    }

    /// `a.parse()` after `from crate import a`: the receiver is an
    /// imported module, so the import resolves the call where the name
    /// alone would be ambiguous. Regression for #579, where renaming the
    /// import (`from crate import a as b`) flipped this edge between
    /// resolved and ambiguous.
    #[test]
    fn receiver_calls_through_an_imported_module_resolve_lexically() {
        let nodes = vec![node("crate::a::parse"), node("crate::other::parse")];
        let resolver = Resolver::new(&nodes);

        let call = resolver.resolve(&receiver_site("a::parse"), GraphLanguage::Python);

        assert_eq!(call.resolution, Resolution::Resolved);
        assert_eq!(call.to.as_deref(), Some("src/lib.rs:crate::a::parse:1"));
        assert_eq!(call.method, Some(ResolutionMethod::Lexical));
    }

    /// An imported *object* names no node under the import path, so the
    /// receiver call keeps the ordinary name heuristics.
    #[test]
    fn receiver_calls_on_an_imported_object_keep_the_name_heuristics() {
        let nodes = vec![node("crate::m::W::run"), node("other::W::run")];
        let resolver = Resolver::new(&nodes);

        let call = resolver.resolve(&receiver_site("parse::run"), GraphLanguage::Python);

        assert_eq!(call.resolution, Resolution::Resolved);
        assert_eq!(call.to.as_deref(), Some("src/lib.rs:crate::m::W::run:1"));
        assert_eq!(call.method, Some(ResolutionMethod::CrateNarrowed));
    }

    /// `shapes.Area()` after `import shapes "example.com/shapes/geo"`:
    /// the alias hides the package name the forward path-suffix match
    /// needs, so the import path decides — the node's `geo::Area` ends it.
    /// Regression for #579, where aliasing a Go import left every call
    /// through it unresolved.
    #[rstest]
    #[case::aliased("shapes", "shapes::Area", Some("src/lib.rs:geo::Area:1"))]
    #[case::default_alias("geo", "geo::Area", Some("src/lib.rs:geo::Area:1"))]
    #[case::unbound_head("shapes", "other::Area", None)]
    fn import_paths_narrow_a_package_call_to_the_imported_package(
        #[case] alias: &str,
        #[case] path: &str,
        #[case] expected: Option<&str>,
    ) {
        let nodes = vec![node("geo::Area"), node("decoy::Area")];
        let resolver = Resolver::new(&nodes);
        let call_site = CallShape {
            caller_module: SyntaxFact::Known("app".to_owned()),
            caller_qualified_name: SyntaxFact::Known(Some("app::Run".to_owned())),
            visible_imports: vec![ImportShape {
                local_alias: SyntaxFact::Known(Some(alias.to_owned())),
                imported_module: SyntaxFact::Known("example.com::shapes::geo".to_owned()),
                exported_symbol: SyntaxFact::Known(None),
            }],
            ..site(path)
        };

        let call = resolver.resolve(&call_site, GraphLanguage::Go);

        assert_eq!(call.to.as_deref(), expected, "{path}");
        match expected {
            Some(_) => assert_eq!(call.method, Some(ResolutionMethod::PathSuffix)),
            None => assert_eq!(call.resolution, Resolution::Unresolved),
        }
    }

    #[test]
    fn resolve_ids_sorts_ambiguous_candidates() {
        let ids = vec!["b.rs:f:2".to_owned(), "a.rs:f:1".to_owned()];
        let call = resolve_ids(&ids, ResolutionMethod::LastSegment);
        assert_eq!(call.resolution, Resolution::Ambiguous);
        assert_eq!(call.to, None);
        assert_eq!(call.candidates, ["a.rs:f:1", "b.rs:f:2"]);
        assert_eq!(call.method, Some(ResolutionMethod::LastSegment));
    }
}
