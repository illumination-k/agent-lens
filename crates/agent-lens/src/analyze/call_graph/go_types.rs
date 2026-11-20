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

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use lens_domain::{CallShape, CalleeBinding, SyntaxFact, ValueTypeHint};
use lens_golang::{GoTypeDecl, GoTypeFacts, GoTypeKind};

use super::FileGraphInput;
use super::model::GraphLanguage;

/// How many declarations one lookup may pass through (a variable holding
/// a call's result whose type is an alias, a field of an embedded
/// field…) before it gives up. Bounds variable, alias and embedding
/// cycles, and the fan-out of a type embedding many others.
const LOOKUP_STEPS: usize = 64;

/// The steps left to one lookup — see [`LOOKUP_STEPS`].
struct Budget(Cell<usize>);

impl Budget {
    fn new() -> Self {
        Self(Cell::new(LOOKUP_STEPS))
    }

    /// Spend one step: `None` once none is left.
    fn step(&self) -> Option<()> {
        let left = self.0.get().checked_sub(1)?;
        self.0.set(left);
        Some(())
    }
}

/// Give every Go receiver call whose receiver type the hints pin down a
/// binding to the method it calls.
pub(crate) fn bind_go_receiver_calls(mut files: Vec<FileGraphInput>) -> Vec<FileGraphInput> {
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

/// Set each call's binding.
fn apply_bindings(file: &mut FileGraphInput, bindings: Vec<Option<CalleeBinding>>) {
    if bindings.iter().all(Option::is_none) {
        return;
    }
    // Copies the calls only when the index shares them.
    let calls = Arc::make_mut(&mut file.calls);
    for (site, binding) in calls.iter_mut().zip(bindings) {
        if let Some(binding) = binding {
            site.callee_binding = SyntaxFact::Known(binding);
        }
    }
}

/// `path`, then each shorter suffix of it that starts on a `::` boundary:
/// `github.com::x::cmp::Path`, `x::cmp::Path`, `cmp::Path`, `Path`.
fn path_suffixes(path: &str) -> impl Iterator<Item = &str> {
    std::iter::once(path).chain(
        path.match_indices("::")
            .map(move |(at, separator)| &path[at + separator.len()..]),
    )
}

/// Declarations keyed by qualified name, looked up either exactly or by
/// an import-rooted path ending with the key (`github.com::x::cmp::Path`
/// finds `cmp::Path`), the way `Resolver::narrow_by_import_path` reads a
/// Go import.
struct SuffixMap<V>(HashMap<String, V>);

impl<V> SuffixMap<V> {
    /// The longest suffix of `path` that is a key.
    fn key_for(&self, path: &str) -> Option<&str> {
        path_suffixes(path)
            .find_map(|suffix| self.0.get_key_value(suffix).map(|(key, _)| key.as_str()))
    }

    fn get(&self, path: &str) -> Option<&V> {
        self.0.get(self.key_for(path)?)
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
            types: SuffixMap(HashMap::new()),
            results: SuffixMap(HashMap::new()),
            vars: SuffixMap(HashMap::new()),
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
            if let Some(facts) = file.go_type_facts.as_deref() {
                index.add(facts);
            }
        }
        index
    }

    fn add(&mut self, facts: &'a GoTypeFacts) {
        for decl in &facts.types {
            self.types.0.insert(decl.qualified_name.clone(), decl);
        }
        for (name, results) in &facts.function_results {
            self.results.0.insert(name.clone(), results.as_slice());
        }
        for (name, hint) in &facts.package_vars {
            self.vars.0.insert(name.clone(), hint);
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
        let budget = Budget::new();
        if let Some(ty) = self.resolve(hint, &budget) {
            return self
                .method_on(ty, method, &budget)
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
        !path_suffixes(package).any(|suffix| self.packages.contains(suffix))
    }

    /// The workspace type (its qualified name) a hint denotes.
    fn resolve(&self, hint: &ValueTypeHint, budget: &Budget) -> Option<&'a str> {
        budget.step()?;
        match hint {
            ValueTypeHint::Named(path) => self.named(path, budget),
            ValueTypeHint::PackageVar(path) => self.resolve(self.vars.get(path)?, budget),
            ValueTypeHint::CallResult { callee, index } => {
                // `T(x)` converts: the value has type `T`. A package
                // cannot declare a function and a type of one name.
                if self.types.key_for(callee).is_some() {
                    return self.named(callee, budget);
                }
                let result = self.results.get(callee)?.get(*index)?.as_ref()?;
                self.resolve(result, budget)
            }
            ValueTypeHint::MethodResult {
                receiver,
                method,
                index,
            } => {
                let owner = self.resolve(receiver, budget)?;
                let declared = self.method_on(owner, method, budget)?;
                let result = self.results.get(&declared)?.get(*index)?.as_ref()?;
                self.resolve(result, budget)
            }
            ValueTypeHint::Field { owner, field } => {
                let owner = self.resolve(owner, budget)?;
                self.field_of(owner, field, budget)
            }
        }
    }

    /// A named type, through `type A = B` aliases.
    fn named(&self, path: &str, budget: &Budget) -> Option<&'a str> {
        let decl = *self.types.get(path)?;
        match &decl.alias_of {
            Some(target) => self.resolve(target, budget),
            None => Some(decl.qualified_name.as_str()),
        }
    }

    fn field_of(&self, owner: &str, field: &str, budget: &Budget) -> Option<&'a str> {
        budget.step()?;
        let decl = *self.types.0.get(owner)?;
        if let Some((_, hint)) = decl.fields.iter().find(|(name, _)| name == field) {
            return self.resolve(hint.as_ref()?, budget);
        }
        // A field promoted from an embedded struct.
        let mut promoted = decl
            .embedded
            .iter()
            .filter_map(|embedded| self.resolve(embedded, budget))
            .filter_map(|embedded| self.field_of(embedded, field, budget));
        match (promoted.next(), promoted.next()) {
            (Some(ty), None) => Some(ty),
            _ => None,
        }
    }

    /// The qualified name of method `method` on `owner`: declared on it,
    /// else promoted from exactly one embedded type. An interface
    /// declares no method node, so a call on one stays unbound.
    fn method_on(&self, owner: &str, method: &str, budget: &Budget) -> Option<String> {
        budget.step()?;
        let decl = *self.types.0.get(owner)?;
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
            .filter_map(|embedded| self.resolve(embedded, budget))
            .filter_map(|embedded| self.method_on(embedded, method, budget));
        match (promoted.next(), promoted.next()) {
            (Some(target), None) => Some(target),
            _ => None,
        }
    }
}
