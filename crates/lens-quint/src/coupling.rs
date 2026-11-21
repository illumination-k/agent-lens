//! Module-level coupling for Quint.
//!
//! The coupling unit is a Quint `module`, named `crate::<Module>` so the
//! shared labeler renders it as the bare module name. A file with several
//! modules contributes several units sharing one `file`.
//!
//! Edges:
//!
//! * `import` / `export` / instance declarations → [`EdgeKind::Use`] on
//!   the referenced module, with the imported name (or `*`) as symbol.
//! * a name or operator application inside a definition that `quint`'s
//!   name table resolves to another module's definition →
//!   [`EdgeKind::Call`].
//!
//! Each file is parsed in place ([`parse_path`]) so `from "./x"` imports
//! resolve and the name table reaches across files. Edges to modules
//! outside the walked tree are dropped.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use lens_domain::{CouplingEdge, CouplingError, EdgeKind, ModulePath, SourceFilter};

use crate::cli::{ParsedFile, parse_path};
use crate::ir::{Decl, Module};

const EXPECTED: &str = "a .qnt file or a directory containing .qnt files";

/// One Quint module and the edges leaving it.
#[derive(Debug, Clone)]
pub struct QuintModule {
    pub path: ModulePath,
    pub file: PathBuf,
    /// `(target module name, symbol, kind)`.
    refs: Vec<(String, String, EdgeKind)>,
}

/// Collect the Quint modules under `root`: a single `.qnt` file, or every
/// `.qnt` file a directory walk reaches and `filter` keeps. A walked file
/// that fails to parse is skipped with a warning; an explicit file
/// propagates its error.
pub fn build_module_tree(
    root: &Path,
    filter: &dyn SourceFilter,
) -> Result<Vec<QuintModule>, CouplingError> {
    let files =
        if root.is_file() {
            if root.extension().and_then(std::ffi::OsStr::to_str) != Some("qnt") {
                return Err(unsupported(root));
            }
            vec![root.to_path_buf()]
        } else if root.is_dir() {
            let mut files = lens_domain::collect_files_with_extension(root, "qnt", filter)
                .map_err(|source| CouplingError::Io {
                    path: root.to_path_buf(),
                    source,
                })?;
            files.sort();
            files
        } else {
            return Err(unsupported(root));
        };
    let named = root.is_file();
    let mut out = Vec::new();
    for file in files {
        let loaded = parse_path(&file).map_err(|e| CouplingError::parse(&file, e));
        let Some(parsed) = CouplingError::skip_unless_named(named, loaded)? else {
            continue;
        };
        out.extend(modules_of(&parsed, &file));
    }
    Ok(out)
}

fn unsupported(root: &Path) -> CouplingError {
    CouplingError::UnsupportedRoot {
        path: root.to_path_buf(),
        expected: EXPECTED,
    }
}

fn modules_of(parsed: &ParsedFile, file: &Path) -> Vec<QuintModule> {
    parsed
        .modules
        .iter()
        .map(|module| QuintModule {
            path: module_path(&module.name),
            file: file.to_path_buf(),
            refs: module_refs(parsed, module),
        })
        .collect()
}

fn module_path(name: &str) -> ModulePath {
    ModulePath::new(format!("crate::{name}"))
}

fn module_refs(parsed: &ParsedFile, module: &Module) -> Vec<(String, String, EdgeKind)> {
    let mut refs = BTreeSet::new();
    for decl in &module.declarations {
        match decl {
            Decl::Import(r) | Decl::Export(r) | Decl::Instance(r) => {
                let symbol = r.def_name.clone().unwrap_or_else(|| "*".to_owned());
                refs.insert((r.proto_name.clone(), symbol, EdgeKind::Use));
            }
            Decl::Def(def) => def.expr.walk(&mut |expr| {
                let Some(target) = parsed.resolved.get(&expr.id()) else {
                    return;
                };
                if let Some((owner, name)) = parsed.def_module.get(target)
                    && owner != &module.name
                {
                    refs.insert((owner.clone(), name.clone(), EdgeKind::Call));
                }
            }),
            Decl::Other => {}
        }
    }
    refs.into_iter().collect()
}

/// Resolve every module's references against the modules that were
/// collected, dropping self-edges and targets outside the tree.
pub fn extract_edges(modules: &[QuintModule]) -> Vec<CouplingEdge> {
    let known: BTreeSet<&ModulePath> = modules.iter().map(|m| &m.path).collect();
    let mut edges = Vec::new();
    for module in modules {
        for (target, symbol, kind) in &module.refs {
            let to = module_path(target);
            if to == module.path || !known.contains(&to) {
                continue;
            }
            edges.push(CouplingEdge {
                from: module.path.clone(),
                to,
                symbol: symbol.clone(),
                kind: *kind,
            });
        }
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{counter, helpers};

    fn edges_of(files: &[(&ParsedFile, &str)]) -> Vec<(String, String, String, EdgeKind)> {
        let modules: Vec<QuintModule> = files
            .iter()
            .flat_map(|(parsed, file)| modules_of(parsed, Path::new(file)))
            .collect();
        extract_edges(&modules)
            .into_iter()
            .map(|e| {
                (
                    e.from.as_str().to_owned(),
                    e.to.as_str().to_owned(),
                    e.symbol,
                    e.kind,
                )
            })
            .collect()
    }

    fn edge(
        from: &str,
        to: &str,
        symbol: &str,
        kind: EdgeKind,
    ) -> (String, String, String, EdgeKind) {
        (
            format!("crate::{from}"),
            format!("crate::{to}"),
            symbol.to_owned(),
            kind,
        )
    }

    #[test]
    fn imports_and_resolved_calls_become_edges() {
        let (c, h) = (counter(), helpers());
        let edges = edges_of(&[(&c.file, "counter.qnt"), (&h.file, "helpers.qnt")]);
        assert_eq!(
            edges,
            vec![
                edge("counter", "helpers", "*", EdgeKind::Use),
                edge("counter", "helpers", "inc", EdgeKind::Call),
                edge("extra", "helpers", "*", EdgeKind::Use),
                edge("extra", "helpers", "inc", EdgeKind::Call),
            ],
        );
    }

    #[test]
    fn edges_to_modules_outside_the_tree_are_dropped() {
        let c = counter();
        assert!(edges_of(&[(&c.file, "counter.qnt")]).is_empty());
    }

    #[test]
    fn a_file_root_must_be_qnt() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("spec.txt");
        std::fs::write(&file, "module m {}").unwrap();
        let err = build_module_tree(&file, &lens_domain::IncludeAll).unwrap_err();
        assert!(
            matches!(err, CouplingError::UnsupportedRoot { .. }),
            "{err}"
        );
    }

    /// Runs only where a `quint` CLI is on `PATH`.
    #[test]
    fn live_quint_builds_the_testdata_graph() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata");
        let modules = match build_module_tree(&dir, &lens_domain::IncludeAll) {
            Ok(modules) if !modules.is_empty() => modules,
            // Every file skipped: no quint here.
            Ok(_) => return,
            Err(err) => panic!("{err}"),
        };
        let edges = extract_edges(&modules);
        assert!(edges.iter().any(|e| e.from.as_str() == "crate::counter"
            && e.to.as_str() == "crate::helpers"
            && e.kind == EdgeKind::Call));
    }

    #[test]
    fn imported_files_do_not_contribute_modules() {
        // Parsing counter.qnt loads helpers.qnt too; only counter is its own.
        let c = counter();
        let names: Vec<_> = modules_of(&c.file, Path::new("counter.qnt"))
            .into_iter()
            .map(|m| m.path)
            .collect();
        assert_eq!(names, vec![ModulePath::new("crate::counter")]);
    }
}
