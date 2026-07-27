//! Heuristic call-site resolution.
//!
//! Maps [`CallShape`]s onto graph node ids without type inference:
//! lexical path candidates first, then last-segment fallbacks with
//! path-suffix and caller-crate narrowing. Every outcome records its
//! provenance ([`ResolutionMethod`]) and, when ambiguous, the full
//! candidate node-id set so downstream analyzers can widen traversals
//! instead of dropping the edge.
//!
//! The one place the name fallback is switched off is a receiver call
//! (`recv.foo()`) on a name the language's standard library defines on
//! nearly every value — see
//! [`GraphLanguage::ubiquitous_method_names`][super::model::GraphLanguage::ubiquitous_method_names].
//! There the name is the only evidence available and it is worthless,
//! so the site stays [`Resolution::Unresolved`] rather than becoming a
//! phantom edge into whichever workspace function happens to share the
//! name.

use std::collections::{HashMap, HashSet};

use lens_domain::{CallShape, ReceiverExprKind, SyntaxFact, qualify_module};

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
/// (file, qualified name) match.
pub(crate) struct CallerIndex {
    by_file_and_qualified_name: HashMap<(String, String), Vec<String>>,
}

impl CallerIndex {
    pub(crate) fn new(nodes: &[CallGraphNode]) -> Self {
        let mut by_file_and_qualified_name: HashMap<(String, String), Vec<String>> = HashMap::new();
        for node in nodes {
            by_file_and_qualified_name
                .entry((node.file.clone(), node.qualified_name.clone()))
                .or_default()
                .push(node.id.clone());
        }
        Self {
            by_file_and_qualified_name,
        }
    }

    pub(crate) fn resolve_in_file(&self, file: &str, qualified_name: &str) -> Option<String> {
        let ids = self
            .by_file_and_qualified_name
            .get(&(file.to_owned(), qualified_name.to_owned()))?;
        if ids.len() == 1 {
            return ids.first().cloned();
        }
        None
    }
}

pub(crate) struct Resolver {
    qualified: HashMap<String, Vec<String>>,
    last_segment: HashMap<String, Vec<String>>,
    id_to_qualified: HashMap<String, String>,
}

impl Resolver {
    pub(crate) fn new(nodes: &[CallGraphNode]) -> Self {
        let mut qualified: HashMap<String, Vec<String>> = HashMap::new();
        let mut last_segment: HashMap<String, Vec<String>> = HashMap::new();
        let mut id_to_qualified: HashMap<String, String> = HashMap::new();
        for node in nodes {
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
        }
    }

    /// `language` is the language of the file the call site lives in;
    /// it selects the ubiquitous-method-name table consulted for
    /// receiver calls.
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
        if site.has_receiver_expression() {
            return self.resolve_receiver_method(site, callee_name, language);
        }
        for candidate in lexical_candidates(site) {
            if let Some(ids) = self.qualified.get(&candidate) {
                return resolve_ids(ids, ResolutionMethod::Lexical);
            }
        }
        let Some(ids) = self.last_segment.get(callee_name) else {
            return ResolvedCall::unresolved();
        };
        // When the callee was written as a multi-segment path like
        // `Foo::new`, restrict the fallback to candidates whose
        // qualified name ends with that path. Catches calls reaching a
        // type through a glob import, and avoids mislabeling external
        // calls like `String::new()` as ambiguous against unrelated
        // workspace `new` methods.
        if let Some(callee_path) = site.callee_path()
            && callee_path.contains("::")
        {
            let narrowed = self.narrow_by_path_suffix(ids, &callee_path);
            return if narrowed.is_empty() {
                ResolvedCall::unresolved()
            } else {
                resolve_ids(&narrowed, ResolutionMethod::PathSuffix)
            };
        }
        resolve_ids(ids, ResolutionMethod::LastSegment)
    }

    fn resolve_self_method(&self, site: &CallShape, callee_name: &str) -> ResolvedCall {
        let Some(module) = site.caller_module() else {
            return ResolvedCall::unresolved();
        };
        let Some(owner) = site.caller_owner() else {
            return ResolvedCall::unresolved();
        };
        let candidate = qualify_module(module, &format!("{owner}::{callee_name}"));
        if let Some(ids) = self.qualified.get(&candidate) {
            return resolve_ids(ids, ResolutionMethod::SelfMethod);
        }
        ResolvedCall::unresolved()
    }

    /// Receiver method calls (`obj.foo()`) cannot be type-inferred
    /// without semantic analysis, so we resolve heuristically by
    /// last-segment match, then narrow ambiguous matches to the
    /// caller's crate.
    ///
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
        if language.ubiquitous_method_names().contains(callee_name) {
            return ResolvedCall::unresolved();
        }
        let Some(ids) = self.last_segment.get(callee_name) else {
            return ResolvedCall::unresolved();
        };
        self.resolve_with_crate_narrowing(ids, site)
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
            if let Some(path) = prefix_with_tail(module_segments(module), &segments, 1) {
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
                    1,
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

fn prefix_with_tail(
    mut prefix: Vec<String>,
    segments: &[&str],
    tail_start: usize,
) -> Option<String> {
    if tail_start > segments.len() {
        return None;
    }
    prefix.extend(segments.iter().skip(tail_start).map(|s| (*s).to_owned()));
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
            prefix_with_tail(vec!["crate".to_owned(), "m".to_owned()], &["self"], 1).as_deref(),
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

    fn receiver_site(name: &str) -> CallShape {
        CallShape {
            receiver_expr_kind: SyntaxFact::Known(ReceiverExprKind::Expression),
            ..site(name)
        }
    }

    fn node(qualified_name: &str) -> CallGraphNode {
        CallGraphNode {
            id: format!("src/lib.rs:{qualified_name}:1"),
            name: name_last_segment(qualified_name).to_owned(),
            qualified_name: qualified_name.to_owned(),
            file: "src/lib.rs".to_owned(),
            module: "crate::m".to_owned(),
            impl_owner: None,
            start_line: 1,
            end_line: 2,
            is_test: false,
            visibility: NodeVisibility::Unknown,
            weights: NodeWeights::default(),
            outgoing_calls: ResolutionCallCounts::default(),
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
