//! Module-tree construction and edge extraction for Rust crates.
//!
//! The flow is split in two halves:
//!
//! 1. [`build_module_tree`] starts at a single `.rs` root file and
//!    recursively follows `mod foo;` declarations on disk plus inline
//!    `mod foo { ... }` blocks. The result is a flat list of
//!    [`CrateModule`]s, one per module path. Each module owns the
//!    `syn::Item`s that belong *directly* to it; nested module bodies
//!    are split out so a visitor walking a module's items will not see
//!    items belonging to its children.
//!
//! 2. Edge extraction (in a sibling file) consumes that list and produces
//!    [`lens_domain::CouplingEdge`]s. Keeping items already split by
//!    module makes the visitor stateless with respect to "which module
//!    am I in right now".
//!
//! Limitations carried forward to the analyzer's documentation:
//!
//! * `#[path = "..."]` on `mod` declarations is not honoured — the resolver
//!   only probes `<name>.rs` and `<name>/mod.rs`.
//! * Macros are invisible to `syn`; modules synthesised by macros never
//!   appear in the tree.
//! * Cross-crate references are out of scope; `build_module_tree` walks a
//!   single crate root.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lens_domain::{CouplingEdge, EdgeKind, ModulePath};
use syn::visit::Visit;
use syn::{ExprPath, Item, ItemImpl, ItemUse, TypePath, UseTree};

/// Failures raised while walking a crate's module tree.
#[derive(Debug)]
pub enum CouplingError {
    /// Reading a `.rs` file failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `syn` rejected the file's contents.
    Parse { path: PathBuf, source: syn::Error },
    /// `mod foo;` was declared but neither `foo.rs` nor `foo/mod.rs` was
    /// found in the parent directory.
    MissingMod {
        /// Module path of the declaring parent (e.g. `crate::a`).
        parent: String,
        /// Identifier as written in `mod <name>;`.
        name: String,
        /// Directory that was probed for the missing file.
        near: PathBuf,
    },
}

impl std::fmt::Display for CouplingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "failed to read {}: {source}", path.display()),
            Self::Parse { path, source } => {
                write!(f, "failed to parse {}: {source}", path.display())
            }
            Self::MissingMod { parent, name, near } => write!(
                f,
                "module `{parent}::{name}` declared but neither {0}.rs nor {0}/mod.rs found in {1}",
                name,
                near.display()
            ),
        }
    }
}

impl std::error::Error for CouplingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::MissingMod { .. } => None,
        }
    }
}

/// One node in a Rust module tree.
///
/// `items` holds the items that lexically belong to this module *only* —
/// any nested `Item::Mod` (inline or file-backed) has been popped out
/// into its own [`CrateModule`] and removed from this list. A visitor
/// walking `items` therefore sees exactly the references made by code
/// written inside this module's brace pair (or top-level file body).
#[derive(Debug, Clone)]
pub struct CrateModule {
    pub path: ModulePath,
    /// File the items came from. Inline modules share their parent's
    /// path here.
    pub file: PathBuf,
    pub items: Vec<Item>,
}

/// Build the module tree rooted at `root`.
///
/// `root` is expected to be a `.rs` file (typically `src/lib.rs` or
/// `src/main.rs`). The returned [`CrateModule`] for the root file uses
/// the path string `"crate"`.
///
/// Modules are appended in the order they are discovered: parent before
/// each of its children, depth-first. Callers that need a different
/// presentation order can re-sort the result.
pub fn build_module_tree(root: &Path) -> Result<Vec<CrateModule>, CouplingError> {
    let mut out = Vec::new();
    walk_file(root, ModulePath::new("crate"), &mut out)?;
    Ok(out)
}

fn walk_file(
    file: &Path,
    mod_path: ModulePath,
    out: &mut Vec<CrateModule>,
) -> Result<(), CouplingError> {
    let source = std::fs::read_to_string(file).map_err(|source| CouplingError::Io {
        path: file.to_path_buf(),
        source,
    })?;
    let parsed = syn::parse_file(&source).map_err(|source| CouplingError::Parse {
        path: file.to_path_buf(),
        source,
    })?;
    let items = split_modules(parsed.items, &mod_path, file, out)?;
    out.push(CrateModule {
        path: mod_path,
        file: file.to_path_buf(),
        items,
    });
    Ok(())
}

/// Walk `items` in-place: any `Item::Mod` entry is popped out into its
/// own [`CrateModule`] (recursing for inline content, descending to disk
/// for file-backed declarations) and removed from the returned `Vec`.
/// Non-mod items pass through unchanged.
fn split_modules(
    items: Vec<Item>,
    parent: &ModulePath,
    parent_file: &Path,
    out: &mut Vec<CrateModule>,
) -> Result<Vec<Item>, CouplingError> {
    let mut kept: Vec<Item> = Vec::with_capacity(items.len());
    for item in items {
        match item {
            Item::Mod(item_mod) => {
                let child_path = parent.child(&item_mod.ident.to_string());
                match item_mod.content {
                    Some((_, inline_items)) => {
                        let leaf = split_modules(inline_items, &child_path, parent_file, out)?;
                        out.push(CrateModule {
                            path: child_path,
                            file: parent_file.to_path_buf(),
                            items: leaf,
                        });
                    }
                    None => {
                        let resolved = resolve_mod_file(parent_file, &item_mod.ident.to_string())
                            .ok_or_else(|| CouplingError::MissingMod {
                            parent: parent.as_str().to_owned(),
                            name: item_mod.ident.to_string(),
                            near: parent_file
                                .parent()
                                .unwrap_or_else(|| Path::new("."))
                                .to_path_buf(),
                        })?;
                        walk_file(&resolved, child_path, out)?;
                    }
                }
            }
            other => kept.push(other),
        }
    }
    Ok(kept)
}

/// Collect every cross-module reference in `modules` as a list of
/// [`CouplingEdge`]s. Self-loops and duplicates are *not* filtered here
/// — that's the caller's responsibility (typically
/// [`lens_domain::compute_report`]).
///
/// External references (anything rooted at `std`, `core`, `alloc`, or an
/// unrecognised crate name) are silently dropped.
pub fn extract_edges(modules: &[CrateModule]) -> Vec<CouplingEdge> {
    let known: HashSet<ModulePath> = modules.iter().map(|m| m.path.clone()).collect();
    let mut edges = Vec::new();
    for module in modules {
        edges.extend(extract_module_edges(module, &known));
    }
    edges
}

fn extract_module_edges(module: &CrateModule, known: &HashSet<ModulePath>) -> Vec<CouplingEdge> {
    let mut visitor = EdgeVisitor::new(&module.path, known);
    // Pass 1: register `use` aliases so subsequent paths can resolve
    // bare identifiers via them. `use` items at file scope dominate
    // visibility for the whole module, regardless of source order.
    for item in &module.items {
        if let Item::Use(u) = item {
            visitor.visit_item_use(u);
        }
    }
    // Pass 2: walk every other item; nested `use` statements
    // (e.g. inside a function body) are picked up here too and
    // continue to extend the alias map for trailing items.
    for item in &module.items {
        if !matches!(item, Item::Use(_)) {
            visitor.visit_item(item);
        }
    }
    visitor.edges
}

/// Read off the simple ident sequence of a `syn::Path`, dropping any
/// generic arguments. The conversion is lossy for things like
/// `Vec<T>` but the only piece we need for module resolution is the
/// segment list.
fn path_to_segments(path: &syn::Path) -> Vec<String> {
    path.segments.iter().map(|s| s.ident.to_string()).collect()
}

/// Module-path resolution context.
///
/// Owns everything needed to take a written path (`crate::a::Foo`,
/// `super::b::g`, an aliased bare ident) and produce the absolute
/// `(module, symbol)` it refers to. Held separately from the visitor so
/// the resolution rules are testable on their own and the visitor only
/// touches state related to edge collection.
struct PathResolver<'a> {
    current: &'a ModulePath,
    known: &'a HashSet<ModulePath>,
    /// Local aliases introduced by `use` statements. Each entry maps a
    /// local name to the absolute segment list it stands for, e.g.
    /// `"Bar" -> ["crate", "a", "Foo"]` for `use crate::a::Foo as Bar;`.
    aliases: HashMap<String, Vec<String>>,
}

impl<'a> PathResolver<'a> {
    fn new(current: &'a ModulePath, known: &'a HashSet<ModulePath>) -> Self {
        Self {
            current,
            known,
            aliases: HashMap::new(),
        }
    }

    fn add_alias(&mut self, alias: String, target: Vec<String>) {
        self.aliases.insert(alias, target);
    }

    /// Apply prefix transformations (`crate`, `self`, `super`, aliases,
    /// external roots) and return an absolute segment list rooted at
    /// `"crate"`. Returns `None` for paths that point outside the crate
    /// or that cannot be anchored.
    fn absolutize(&self, segments: &[String]) -> Option<Vec<String>> {
        let first = segments.first()?;
        match first.as_str() {
            "crate" => Some(segments.to_vec()),
            "self" => self.absolutize_self(segments),
            "super" => self.absolutize_super(segments),
            "std" | "core" | "alloc" => None,
            other => self.absolutize_alias(other, segments),
        }
    }

    fn current_segments(&self) -> Vec<String> {
        self.current
            .as_str()
            .split("::")
            .map(String::from)
            .collect()
    }

    fn absolutize_self(&self, segments: &[String]) -> Option<Vec<String>> {
        // Bare `self` in expression position is the receiver value, not a
        // module reference; only `self::<x>::...` names a path inside the
        // current module.
        if segments.len() == 1 {
            return None;
        }
        let mut v = self.current_segments();
        v.extend(segments.iter().skip(1).cloned());
        Some(v)
    }

    fn absolutize_super(&self, segments: &[String]) -> Option<Vec<String>> {
        let mut v = self.current_segments();
        let mut iter = segments.iter();
        while let Some(s) = iter.next() {
            if s == "super" {
                if v.len() <= 1 {
                    return None;
                }
                v.pop();
            } else {
                v.push(s.clone());
                v.extend(iter.cloned());
                break;
            }
        }
        Some(v)
    }

    fn absolutize_alias(&self, head: &str, segments: &[String]) -> Option<Vec<String>> {
        let alias = self.aliases.get(head)?;
        let mut v = alias.clone();
        v.extend(segments.iter().skip(1).cloned());
        Some(v)
    }

    /// Resolve a path that names a value or type — the trailing segments
    /// after the longest matching module prefix become the symbol.
    fn resolve_path(&self, segments: &[String]) -> Option<(ModulePath, String)> {
        let absolute = self.absolutize(segments)?;
        if absolute.len() < 2 {
            return None;
        }
        for split in (1..absolute.len()).rev() {
            let candidate = absolute[..split].join("::");
            let module = ModulePath::new(candidate);
            if self.known.contains(&module) {
                let symbol = absolute[split..].join("::");
                return Some((module, symbol));
            }
        }
        None
    }

    /// Resolve a path that names a module itself (used for use-globs).
    fn resolve_module(&self, segments: &[String]) -> Option<ModulePath> {
        let absolute = self.absolutize(segments)?;
        let candidate = absolute.join("::");
        let module = ModulePath::new(candidate);
        if self.known.contains(&module) {
            Some(module)
        } else {
            None
        }
    }
}

/// Walk a `use` tree, recording an edge for each leaf and updating
/// `resolver`'s alias map. `prefix` accumulates the path segments seen
/// so far. Free function (rather than a method) so the resolver isn't
/// borrowed mutably for the whole walk.
fn walk_use_tree(
    resolver: &mut PathResolver<'_>,
    current: &ModulePath,
    edges: &mut Vec<CouplingEdge>,
    tree: &UseTree,
    prefix: &mut Vec<String>,
) {
    match tree {
        UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            walk_use_tree(resolver, current, edges, &p.tree, prefix);
            prefix.pop();
        }
        UseTree::Name(n) => {
            walk_use_leaf(
                resolver,
                current,
                edges,
                n.ident.to_string(),
                n.ident.to_string(),
                prefix,
            );
        }
        UseTree::Rename(r) => {
            walk_use_leaf(
                resolver,
                current,
                edges,
                r.ident.to_string(),
                r.rename.to_string(),
                prefix,
            );
        }
        UseTree::Glob(_) => {
            if let Some(target) = resolver.resolve_module(prefix) {
                edges.push(CouplingEdge {
                    from: current.clone(),
                    to: target,
                    symbol: "*".to_owned(),
                    kind: EdgeKind::Use,
                });
            }
        }
        UseTree::Group(g) => {
            for item in &g.items {
                walk_use_tree(resolver, current, edges, item, prefix);
            }
        }
    }
}

/// `tail` is the last segment that completes the absolute path; `alias`
/// is the local name the import binds (same as `tail` for `use ::Name`,
/// the renamed identifier for `use ::Name as Other`).
fn walk_use_leaf(
    resolver: &mut PathResolver<'_>,
    current: &ModulePath,
    edges: &mut Vec<CouplingEdge>,
    tail: String,
    alias: String,
    prefix: &[String],
) {
    let mut full = prefix.to_vec();
    full.push(tail);
    if let Some((target, symbol)) = resolver.resolve_path(&full) {
        resolver.add_alias(alias, full);
        edges.push(CouplingEdge {
            from: current.clone(),
            to: target,
            symbol,
            kind: EdgeKind::Use,
        });
    }
}

struct EdgeVisitor<'a> {
    current: &'a ModulePath,
    resolver: PathResolver<'a>,
    edges: Vec<CouplingEdge>,
}

impl<'a> EdgeVisitor<'a> {
    fn new(current: &'a ModulePath, known: &'a HashSet<ModulePath>) -> Self {
        Self {
            current,
            resolver: PathResolver::new(current, known),
            edges: Vec::new(),
        }
    }

    fn record(&mut self, target: ModulePath, symbol: String, kind: EdgeKind) {
        self.edges.push(CouplingEdge {
            from: self.current.clone(),
            to: target,
            symbol,
            kind,
        });
    }

    fn try_record(&mut self, segments: &[String], kind: EdgeKind) {
        if let Some((target, symbol)) = self.resolver.resolve_path(segments) {
            self.record(target, symbol, kind);
        }
    }
}

impl<'ast> Visit<'ast> for EdgeVisitor<'_> {
    fn visit_item_use(&mut self, u: &'ast ItemUse) {
        let mut prefix = Vec::new();
        walk_use_tree(
            &mut self.resolver,
            self.current,
            &mut self.edges,
            &u.tree,
            &mut prefix,
        );
    }

    fn visit_expr_path(&mut self, p: &'ast ExprPath) {
        if p.qself.is_none() {
            let segs = path_to_segments(&p.path);
            self.try_record(&segs, EdgeKind::Call);
        }
        syn::visit::visit_expr_path(self, p);
    }

    fn visit_type_path(&mut self, t: &'ast TypePath) {
        if t.qself.is_none() {
            let segs = path_to_segments(&t.path);
            self.try_record(&segs, EdgeKind::Type);
        }
        syn::visit::visit_type_path(self, t);
    }

    fn visit_item_impl(&mut self, i: &'ast ItemImpl) {
        if let syn::Type::Path(tp) = &*i.self_ty {
            self.try_record(&path_to_segments(&tp.path), EdgeKind::ImplFor);
        }
        if let Some((_, trait_path, _)) = &i.trait_ {
            self.try_record(&path_to_segments(trait_path), EdgeKind::ImplFor);
        }
        // Recurse into the body but skip the self_ty / trait_ to avoid
        // double-counting them as Type edges (they're already recorded
        // above as ImplFor; the bodies still need their own paths
        // visited).
        for item in &i.items {
            syn::visit::visit_impl_item(self, item);
        }
    }

    fn visit_item_mod(&mut self, _m: &'ast syn::ItemMod) {
        // Items inside nested modules belong to a different `CrateModule`
        // and have already been split out by `build_module_tree`. Stop
        // here so we don't attribute their references to the parent.
    }
}

fn resolve_mod_file(parent_file: &Path, name: &str) -> Option<PathBuf> {
    let module_dir = module_dir_of(parent_file);
    let flat = module_dir.join(format!("{name}.rs"));
    if flat.is_file() {
        return Some(flat);
    }
    let nested = module_dir.join(name).join("mod.rs");
    if nested.is_file() {
        return Some(nested);
    }
    None
}

/// Directory that a parent `.rs` file owns its children in. For
/// `lib.rs`, `main.rs`, and `mod.rs` the parent's own directory is the
/// owner; for any other file `<dir>/<stem>.rs`, children live in the
/// sibling directory `<dir>/<stem>/`. This mirrors `rustc`'s
/// non-`mod.rs` module layout rules.
fn module_dir_of(file: &Path) -> PathBuf {
    let dir = file.parent().unwrap_or_else(|| Path::new("."));
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    match stem {
        "lib" | "main" | "mod" => dir.to_path_buf(),
        other => dir.join(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn paths(modules: &[CrateModule]) -> Vec<&str> {
        modules.iter().map(|m| m.path.as_str()).collect()
    }

    #[test]
    fn root_only_yields_one_module() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "fn solo() {}\n");
        let tree = build_module_tree(&lib).unwrap();
        assert_eq!(paths(&tree), vec!["crate"]);
    }

    #[test]
    fn inline_modules_become_children() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(
            dir.path(),
            "lib.rs",
            r#"
            fn outer() {}
            mod inner {
                fn x() {}
                mod deep { fn y() {} }
            }
            "#,
        );
        let tree = build_module_tree(&lib).unwrap();
        let mut p: Vec<&str> = paths(&tree);
        p.sort();
        assert_eq!(p, vec!["crate", "crate::inner", "crate::inner::deep"]);
    }

    #[test]
    fn inline_modules_split_items_out_of_parent() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(
            dir.path(),
            "lib.rs",
            r#"
            fn outer() {}
            mod inner { fn x() {} }
            "#,
        );
        let tree = build_module_tree(&lib).unwrap();
        let crate_mod = tree.iter().find(|m| m.path.as_str() == "crate").unwrap();
        let inner = tree
            .iter()
            .find(|m| m.path.as_str() == "crate::inner")
            .unwrap();
        // The parent's items should not include the inline `mod inner` —
        // it's been popped out. Only `fn outer` remains.
        assert_eq!(crate_mod.items.len(), 1);
        assert!(matches!(crate_mod.items[0], Item::Fn(_)));
        // The child carries `fn x`.
        assert_eq!(inner.items.len(), 1);
        assert!(matches!(inner.items[0], Item::Fn(_)));
    }

    #[test]
    fn file_backed_mod_resolves_via_flat_layout() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "mod a;\n");
        write_file(dir.path(), "a.rs", "fn x() {}\n");
        let tree = build_module_tree(&lib).unwrap();
        let mut p: Vec<&str> = paths(&tree);
        p.sort();
        assert_eq!(p, vec!["crate", "crate::a"]);
    }

    #[test]
    fn file_backed_mod_resolves_via_directory_layout() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "mod a;\n");
        write_file(dir.path(), "a/mod.rs", "fn x() {}\n");
        let tree = build_module_tree(&lib).unwrap();
        let mut p: Vec<&str> = paths(&tree);
        p.sort();
        assert_eq!(p, vec!["crate", "crate::a"]);
    }

    #[test]
    fn flat_layout_wins_over_directory_layout() {
        // If both `a.rs` and `a/mod.rs` exist, `a.rs` is the canonical
        // file and should be picked.
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "mod a;\n");
        let flat = write_file(dir.path(), "a.rs", "// flat\n");
        write_file(dir.path(), "a/mod.rs", "// nested\n");
        let tree = build_module_tree(&lib).unwrap();
        let a = tree.iter().find(|m| m.path.as_str() == "crate::a").unwrap();
        assert_eq!(a.file, flat);
    }

    #[test]
    fn mod_rs_parent_keeps_children_in_same_directory() {
        // `crate::a` lives at `a/mod.rs`; its child `mod b;` should
        // resolve to a sibling within `a/`, not to `a/a/b.rs`.
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "mod a;\n");
        write_file(dir.path(), "a/mod.rs", "mod b;\n");
        write_file(dir.path(), "a/b.rs", "fn x() {}\n");
        let tree = build_module_tree(&lib).unwrap();
        let mut p: Vec<&str> = paths(&tree);
        p.sort();
        assert_eq!(p, vec!["crate", "crate::a", "crate::a::b"]);
    }

    #[test]
    fn nested_file_backed_mods_recurse() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "mod a;\n");
        write_file(dir.path(), "a.rs", "mod b;\n");
        write_file(dir.path(), "a/b.rs", "fn x() {}\n");
        let tree = build_module_tree(&lib).unwrap();
        let mut p: Vec<&str> = paths(&tree);
        p.sort();
        assert_eq!(p, vec!["crate", "crate::a", "crate::a::b"]);
    }

    #[test]
    fn missing_mod_file_surfaces_error() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "mod ghost;\n");
        let err = build_module_tree(&lib).unwrap_err();
        match err {
            CouplingError::MissingMod { parent, name, .. } => {
                assert_eq!(parent, "crate");
                assert_eq!(name, "ghost");
            }
            other => panic!("expected MissingMod, got {other:?}"),
        }
    }

    #[test]
    fn invalid_rust_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "fn ??? {");
        let err = build_module_tree(&lib).unwrap_err();
        assert!(matches!(err, CouplingError::Parse { .. }));
    }

    #[test]
    fn missing_root_surfaces_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let lib = dir.path().join("ghost.rs");
        let err = build_module_tree(&lib).unwrap_err();
        assert!(matches!(err, CouplingError::Io { .. }));
    }

    fn edges_for(src: &str) -> (Vec<CrateModule>, Vec<CouplingEdge>) {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", src);
        let tree = build_module_tree(&lib).unwrap();
        let edges = extract_edges(&tree);
        (tree, edges)
    }

    fn has_edge(
        edges: &[CouplingEdge],
        from: &str,
        to: &str,
        symbol: &str,
        kind: EdgeKind,
    ) -> bool {
        edges.iter().any(|e| {
            e.from.as_str() == from && e.to.as_str() == to && e.symbol == symbol && e.kind == kind
        })
    }

    #[test]
    fn use_statement_records_use_edge() {
        let src = r#"
            mod a { pub struct Foo; }
            mod b {
                use crate::a::Foo;
                fn _x(_f: Foo) {}
            }
        "#;
        let (_, edges) = edges_for(src);
        assert!(has_edge(
            &edges,
            "crate::b",
            "crate::a",
            "Foo",
            EdgeKind::Use
        ));
    }

    #[test]
    fn external_use_does_not_record_edge() {
        let src = r#"
            mod a {
                use std::collections::HashMap;
                fn _x() -> HashMap<u32, u32> { HashMap::new() }
            }
        "#;
        let (_, edges) = edges_for(src);
        assert!(
            !edges.iter().any(|e| e.symbol.contains("HashMap")),
            "found unexpected edge for std type: {edges:?}"
        );
    }

    #[test]
    fn glob_use_records_star_symbol() {
        let src = r#"
            mod a { pub struct Foo; pub struct Bar; }
            mod b { use crate::a::*; }
        "#;
        let (_, edges) = edges_for(src);
        assert!(has_edge(&edges, "crate::b", "crate::a", "*", EdgeKind::Use));
    }

    #[test]
    fn renamed_use_aliases_resolve_back_to_target() {
        let src = r#"
            mod a { pub struct Foo; }
            mod b {
                use crate::a::Foo as Bar;
                fn _x(_b: Bar) {}
            }
        "#;
        let (_, edges) = edges_for(src);
        // Use edge records the target symbol (Foo), not the local alias.
        assert!(has_edge(
            &edges,
            "crate::b",
            "crate::a",
            "Foo",
            EdgeKind::Use
        ));
        // Type edge from the function signature uses the alias `Bar`,
        // which expands to crate::a::Foo.
        assert!(has_edge(
            &edges,
            "crate::b",
            "crate::a",
            "Foo",
            EdgeKind::Type
        ));
    }

    #[test]
    fn super_prefix_resolves_to_parent_module() {
        let src = r#"
            mod a {
                pub struct Foo;
                pub mod inner {
                    fn _x(_f: super::Foo) {}
                }
            }
        "#;
        let (_, edges) = edges_for(src);
        assert!(has_edge(
            &edges,
            "crate::a::inner",
            "crate::a",
            "Foo",
            EdgeKind::Type
        ));
    }

    #[test]
    fn self_prefix_anchors_to_current_module() {
        let src = r#"
            mod a {
                pub struct Foo;
                fn _x() -> self::Foo { unimplemented!() }
            }
        "#;
        let (_, edges) = edges_for(src);
        // self::Foo from inside crate::a resolves to crate::a::Foo, but
        // Foo lives directly in crate::a — the longest matching module
        // is crate::a, so symbol is "Foo".
        assert!(has_edge(
            &edges,
            "crate::a",
            "crate::a",
            "Foo",
            EdgeKind::Type
        ));
        // Self-loops will be filtered downstream by compute_report; the
        // raw extractor still emits them so callers can see what was
        // referenced.
    }

    #[test]
    fn cross_module_function_call_records_call_edge() {
        let src = r#"
            mod a { pub fn helper() {} }
            mod b {
                fn _x() { crate::a::helper(); }
            }
        "#;
        let (_, edges) = edges_for(src);
        assert!(has_edge(
            &edges,
            "crate::b",
            "crate::a",
            "helper",
            EdgeKind::Call
        ));
    }

    #[test]
    fn type_reference_records_type_edge() {
        let src = r#"
            mod a { pub struct Foo; }
            mod b {
                fn _x(_f: crate::a::Foo) {}
            }
        "#;
        let (_, edges) = edges_for(src);
        assert!(has_edge(
            &edges,
            "crate::b",
            "crate::a",
            "Foo",
            EdgeKind::Type
        ));
    }

    #[test]
    fn impl_block_records_impl_for_edge() {
        let src = r#"
            mod a { pub trait Greet { fn hi(&self); } }
            mod b {
                pub struct Local;
                impl crate::a::Greet for Local {
                    fn hi(&self) {}
                }
            }
        "#;
        let (_, edges) = edges_for(src);
        // The trait path crosses to crate::a as ImplFor.
        assert!(has_edge(
            &edges,
            "crate::b",
            "crate::a",
            "Greet",
            EdgeKind::ImplFor
        ));
    }

    #[test]
    fn aliased_use_lets_bare_path_resolve() {
        let src = r#"
            mod a { pub fn helper() {} }
            mod b {
                use crate::a;
                fn _x() { a::helper(); }
            }
        "#;
        let (_, edges) = edges_for(src);
        assert!(has_edge(
            &edges,
            "crate::b",
            "crate::a",
            "helper",
            EdgeKind::Call
        ));
    }

    #[test]
    fn use_inside_function_still_records_edge() {
        // `use` deep inside an item is picked up on pass 2 because the
        // visitor recurses into the whole item tree.
        let src = r#"
            mod a { pub fn helper() {} }
            mod b {
                fn _x() {
                    use crate::a::helper;
                    helper();
                }
            }
        "#;
        let (_, edges) = edges_for(src);
        assert!(has_edge(
            &edges,
            "crate::b",
            "crate::a",
            "helper",
            EdgeKind::Use
        ));
    }

    #[test]
    fn nested_module_items_are_attributed_to_the_inner_module() {
        // `outer::inner` referencing `outer` should produce an edge
        // attributed to `crate::outer::inner`, not `crate::outer`.
        let src = r#"
            pub mod outer {
                pub struct Foo;
                pub mod inner {
                    fn _x(_f: super::Foo) {}
                }
            }
        "#;
        let (_, edges) = edges_for(src);
        assert!(has_edge(
            &edges,
            "crate::outer::inner",
            "crate::outer",
            "Foo",
            EdgeKind::Type
        ));
        // The outer module itself shouldn't carry the inner module's
        // edge.
        assert!(!has_edge(
            &edges,
            "crate::outer",
            "crate::outer",
            "Foo",
            EdgeKind::Type
        ));
    }

    #[test]
    fn bare_self_receiver_does_not_create_edge() {
        // `self` alone is the receiver value (`self.field`), not a path
        // referencing the current module. Without this guard, `self`
        // would absolutize to the current module path and falsely
        // resolve via longest-prefix match into the parent.
        let src = r#"
            mod a {
                pub mod b {
                    pub struct Foo { n: i32 }
                    impl Foo {
                        pub fn get(&self) -> i32 { self.n }
                    }
                }
            }
        "#;
        let (_, edges) = edges_for(src);
        // The only edge here would be a spurious one from crate::a::b
        // to crate::a (symbol "b"); confirm it isn't recorded.
        assert!(
            !edges.iter().any(|e| e.from.as_str() == "crate::a::b"
                && e.to.as_str() == "crate::a"
                && e.symbol == "b"),
            "spurious self-receiver edge: {edges:?}"
        );
    }

    #[test]
    fn use_grouping_expands_each_branch() {
        let src = r#"
            mod a { pub struct Foo; pub struct Bar; }
            mod b {
                use crate::a::{Foo, Bar};
            }
        "#;
        let (_, edges) = edges_for(src);
        assert!(has_edge(
            &edges,
            "crate::b",
            "crate::a",
            "Foo",
            EdgeKind::Use
        ));
        assert!(has_edge(
            &edges,
            "crate::b",
            "crate::a",
            "Bar",
            EdgeKind::Use
        ));
    }

    #[test]
    fn coupling_error_io_display_includes_path_and_source() {
        let err = CouplingError::Io {
            path: PathBuf::from("/tmp/x.rs"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/x.rs"), "got {msg}");
        assert!(msg.contains("missing"), "got {msg}");
        assert!(msg.starts_with("failed to read"), "got {msg}");
    }

    #[test]
    fn coupling_error_parse_display_includes_path_and_inner() {
        let parse_err = syn::parse_str::<syn::Expr>("fn???").unwrap_err();
        let err = CouplingError::Parse {
            path: PathBuf::from("/tmp/x.rs"),
            source: parse_err,
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/x.rs"), "got {msg}");
        assert!(msg.starts_with("failed to parse"), "got {msg}");
    }

    #[test]
    fn coupling_error_missing_mod_display_includes_parent_name_and_path() {
        let err = CouplingError::MissingMod {
            parent: "crate".to_owned(),
            name: "ghost".to_owned(),
            near: PathBuf::from("/tmp/proj"),
        };
        let msg = err.to_string();
        assert!(msg.contains("crate::ghost"), "got {msg}");
        assert!(msg.contains("ghost.rs"), "got {msg}");
        assert!(msg.contains("ghost/mod.rs"), "got {msg}");
        assert!(msg.contains("/tmp/proj"), "got {msg}");
    }

    #[test]
    fn coupling_error_io_and_parse_have_source() {
        use std::error::Error as _;
        let io_err = CouplingError::Io {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::other("boom"),
        };
        assert!(io_err.source().is_some());

        let parse_err = syn::parse_str::<syn::Expr>("fn???").unwrap_err();
        let parse_err = CouplingError::Parse {
            path: PathBuf::from("/tmp/x"),
            source: parse_err,
        };
        assert!(parse_err.source().is_some());
    }

    #[test]
    fn coupling_error_missing_mod_has_no_source() {
        use std::error::Error as _;
        let err = CouplingError::MissingMod {
            parent: "crate".to_owned(),
            name: "ghost".to_owned(),
            near: PathBuf::from("/tmp"),
        };
        assert!(err.source().is_none());
    }
}
