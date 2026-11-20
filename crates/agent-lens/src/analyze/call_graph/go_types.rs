//! Bind Go receiver calls through the static types their declarations
//! spell out.
//!
//! The Go adapter reads, per call site, a [`ValueTypeHint`] for the
//! receiver value (`p cmp.Path`, `s.curPtrs`, `CommandLine`,
//! `NewFlagSet(...)`), and per file the package-level declarations a hint
//! can refer to ([`GoTypeFacts`]). A hint from one file routinely names a
//! declaration of another (`var CommandLine = NewFlagSet(...)` in
//! `flag.go`, `CommandLine.BoolP(...)` in `bool.go`), so the join happens
//! here, over every file of the graph, before resolution.
//!
//! A call whose receiver type resolves to a workspace type that declares
//! (or promotes, through an embedded field) the called method gets that
//! method as its [`CalleeBinding::Declaration`]; the resolver then takes
//! it over the name heuristics. A hint that leads nowhere (a type from
//! outside the workspace, an interface, an unknown declaration) changes
//! nothing, so the call keeps the name-based resolution it had.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use lens_domain::{CallShape, CalleeBinding, SyntaxFact, ValueTypeHint};
use lens_golang::{GoTypeDecl, GoTypeFacts, GoTypeKind};

use super::FileGraphInput;
use super::model::GraphLanguage;

/// How many declarations a hint may pass through (a variable holding a
/// call's result whose type is an alias, a field of an embedded field…)
/// before the lookup gives up. Bounds alias and variable cycles.
const MAX_DEPTH: usize = 8;

/// Give every Go receiver call whose receiver type the hints pin down a
/// binding to the method it calls.
pub(crate) fn bind_go_receiver_calls(mut files: Vec<FileGraphInput>) -> Vec<FileGraphInput> {
    if !files
        .iter()
        .any(|file| file.language == GraphLanguage::Go && file.go_type_facts.is_some())
    {
        return files;
    }
    // Every binding is computed before any file changes: the index
    // borrows the facts of every file.
    let bindings: Vec<Vec<Option<CalleeBinding>>> = {
        let index = GoTypeIndex::new(&files);
        files
            .iter()
            .map(|file| {
                if file.language != GraphLanguage::Go {
                    return Vec::new();
                }
                file.calls
                    .iter()
                    .map(|site| index.binding_for(site))
                    .collect()
            })
            .collect()
    };
    for (file, bindings) in files.iter_mut().zip(bindings) {
        apply_bindings(file, bindings);
    }
    files
}

/// Set each call's binding, copying the file's call list only when one
/// of them changes.
fn apply_bindings(file: &mut FileGraphInput, bindings: Vec<Option<CalleeBinding>>) {
    if bindings.iter().all(Option::is_none) {
        return;
    }
    let mut calls = file.calls.as_ref().clone();
    for (site, binding) in calls.iter_mut().zip(bindings) {
        if let Some(binding) = binding {
            site.callee_binding = SyntaxFact::Known(binding);
        }
    }
    file.calls = Arc::new(calls);
}

/// Declarations keyed by qualified name, looked up either exactly or by
/// an import-rooted path ending with the key (`github.com::x::cmp::Path`
/// finds `cmp::Path`), the way `Resolver::narrow_by_import_path` reads a
/// Go import.
struct SuffixMap<V> {
    entries: HashMap<String, V>,
    by_last_segment: HashMap<String, Vec<String>>,
}

impl<V> SuffixMap<V> {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            by_last_segment: HashMap::new(),
        }
    }

    fn insert(&mut self, key: String, value: V) {
        if !self.entries.contains_key(&key) {
            self.by_last_segment
                .entry(last_segment(&key).to_owned())
                .or_default()
                .push(key.clone());
        }
        self.entries.insert(key, value);
    }

    /// The key `path` names: itself, else the one key it ends with on a
    /// segment boundary. Two such keys leave the path unresolved.
    fn key_for(&self, path: &str) -> Option<&str> {
        if let Some((key, _)) = self.entries.get_key_value(path) {
            return Some(key);
        }
        let mut matching = self
            .by_last_segment
            .get(last_segment(path))?
            .iter()
            .filter(|key| key.contains("::") && path.ends_with(&format!("::{key}")));
        match (matching.next(), matching.next()) {
            (Some(key), None) => Some(key),
            _ => None,
        }
    }

    fn get(&self, path: &str) -> Option<&V> {
        self.entries.get(self.key_for(path)?)
    }
}

struct GoTypeIndex<'a> {
    types: SuffixMap<&'a GoTypeDecl>,
    results: SuffixMap<&'a [Option<ValueTypeHint>]>,
    vars: SuffixMap<&'a ValueTypeHint>,
    /// Qualified names of every Go function and method.
    functions: HashSet<&'a str>,
    /// Module paths of every Go file: the packages the workspace holds.
    packages: HashSet<&'a str>,
}

impl<'a> GoTypeIndex<'a> {
    fn new(files: &'a [FileGraphInput]) -> Self {
        let mut index = Self {
            types: SuffixMap::new(),
            results: SuffixMap::new(),
            vars: SuffixMap::new(),
            functions: HashSet::new(),
            packages: HashSet::new(),
        };
        for file in files {
            if file.language != GraphLanguage::Go {
                continue;
            }
            index.packages.insert(file.module.as_str());
            index.functions.extend(
                file.functions
                    .iter()
                    .filter_map(|f| f.qualified_name.known_value().map(String::as_str)),
            );
            let Some(facts) = file.go_type_facts.as_deref() else {
                continue;
            };
            index.add(facts);
        }
        index
    }

    fn add(&mut self, facts: &'a GoTypeFacts) {
        for decl in &facts.types {
            self.types.insert(decl.qualified_name.clone(), decl);
        }
        for (name, results) in &facts.function_results {
            self.results.insert(name.clone(), results.as_slice());
        }
        for (name, hint) in &facts.package_vars {
            self.vars.insert(name.clone(), hint);
        }
    }

    /// The binding of a receiver call: the method it calls, when its
    /// receiver hint resolves to a workspace type with a method of the
    /// called name; [`CalleeBinding::External`] when the receiver is a
    /// value of a type, or a variable, of a package outside the
    /// workspace (`newSet *goflag.FlagSet`, `os.Stdout`), whose methods
    /// no workspace function can be.
    fn binding_for(&self, site: &CallShape) -> Option<CalleeBinding> {
        if site.callee_binding.known_value().is_some() {
            return None;
        }
        let hint = site.receiver_type.known_value()?;
        let method = site.callee_name()?;
        if let Some(ty) = self.resolve(hint, 0) {
            return self
                .method_on(ty, method, 0)
                .map(CalleeBinding::Declaration);
        }
        self.names_external_package(hint)
            .then_some(CalleeBinding::External)
    }

    /// Whether `hint` is a type or variable declared in a package the
    /// workspace does not hold. Only those two shapes: a call's result
    /// can be a workspace type even from an external package (a generic
    /// `slices.Max(xs)`).
    fn names_external_package(&self, hint: &ValueTypeHint) -> bool {
        let path = match hint {
            ValueTypeHint::Named(path) | ValueTypeHint::PackageVar(path) => path,
            _ => return false,
        };
        let Some((package, _)) = path.rsplit_once("::") else {
            return false;
        };
        // An import path names a workspace package when one of its
        // segment-boundary suffixes is a module path of the graph
        // (`github.com::x::proj::cmp` for `cmp`).
        let suffixes = std::iter::once(package).chain(
            package
                .match_indices("::")
                .map(|(at, separator)| &package[at + separator.len()..]),
        );
        !suffixes
            .into_iter()
            .any(|suffix| self.packages.contains(suffix))
    }

    /// The workspace type (its qualified name) a hint denotes.
    fn resolve(&self, hint: &ValueTypeHint, depth: usize) -> Option<&'a str> {
        if depth > MAX_DEPTH {
            return None;
        }
        match hint {
            ValueTypeHint::Named(path) => self.named(path, depth),
            ValueTypeHint::PackageVar(path) => self.resolve(self.vars.get(path)?, depth + 1),
            ValueTypeHint::CallResult { callee, index } => {
                // `T(x)` converts: the value has type `T`.
                if *index == 0
                    && !self.functions.contains(callee.as_str())
                    && self.types.key_for(callee).is_some()
                {
                    return self.named(callee, depth);
                }
                let result = self.results.get(callee)?.get(*index)?.as_ref()?;
                self.resolve(result, depth + 1)
            }
            ValueTypeHint::MethodResult {
                receiver,
                method,
                index,
            } => {
                let owner = self.resolve(receiver, depth + 1)?;
                let declared = self.method_on(owner, method, depth + 1)?;
                let result = self.results.get(&declared)?.get(*index)?.as_ref()?;
                self.resolve(result, depth + 1)
            }
            ValueTypeHint::Field { owner, field } => {
                let owner = self.resolve(owner, depth + 1)?;
                self.field_of(owner, field, depth + 1)
            }
        }
    }

    /// A named type, through `type A = B` aliases.
    fn named(&self, path: &str, depth: usize) -> Option<&'a str> {
        let key = self.types.key_for(path)?;
        let decl = *self.types.entries.get(key)?;
        match &decl.alias_of {
            Some(target) => self.resolve(target, depth + 1),
            None => Some(decl.qualified_name.as_str()),
        }
    }

    fn field_of(&self, owner: &str, field: &str, depth: usize) -> Option<&'a str> {
        if depth > MAX_DEPTH {
            return None;
        }
        let decl = *self.types.entries.get(owner)?;
        if let Some((_, hint)) = decl.fields.iter().find(|(name, _)| name == field) {
            return self.resolve(hint.as_ref()?, depth + 1);
        }
        // A field promoted from an embedded struct.
        let mut promoted = decl
            .embedded
            .iter()
            .filter_map(|embedded| self.resolve(embedded, depth + 1))
            .filter_map(|embedded| self.field_of(embedded, field, depth + 1));
        match (promoted.next(), promoted.next()) {
            (Some(ty), None) => Some(ty),
            _ => None,
        }
    }

    /// The qualified name of method `method` on `owner`: declared on it,
    /// else promoted from exactly one embedded type. An interface
    /// declares no method node, so a call on one stays unbound.
    fn method_on(&self, owner: &str, method: &str, depth: usize) -> Option<String> {
        if depth > MAX_DEPTH {
            return None;
        }
        let decl = *self.types.entries.get(owner)?;
        if decl.kind == GoTypeKind::Interface {
            return None;
        }
        let direct = format!("{owner}::{method}");
        if self.functions.contains(direct.as_str()) {
            return Some(direct);
        }
        // A field of that name (a func-typed field) is no method.
        if decl.fields.iter().any(|(name, _)| name == method) {
            return None;
        }
        let mut promoted = decl
            .embedded
            .iter()
            .filter_map(|embedded| self.resolve(embedded, depth + 1))
            .filter_map(|embedded| self.method_on(embedded, method, depth + 1));
        match (promoted.next(), promoted.next()) {
            (Some(target), None) => Some(target),
            _ => None,
        }
    }
}

fn last_segment(path: &str) -> &str {
    path.rsplit("::").next().unwrap_or(path)
}
