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

use std::path::{Path, PathBuf};

use lens_domain::ModulePath;
use syn::Item;

/// Failures raised while walking a crate's module tree.
#[derive(Debug)]
pub enum CouplingError {
    /// Reading a `.rs` file failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// `syn` rejected the file's contents.
    Parse {
        path: PathBuf,
        source: syn::Error,
    },
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
                        let leaf =
                            split_modules(inline_items, &child_path, parent_file, out)?;
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
        let a = tree
            .iter()
            .find(|m| m.path.as_str() == "crate::a")
            .unwrap();
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
}
