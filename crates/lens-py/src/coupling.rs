//! Python module-tree construction and import-edge extraction.
//!
//! The Python adapter models coupling at file / package-module granularity:
//! each `.py` file under a root directory is treated as one module path and
//! every `import` / `from ... import ...` statement becomes a `use` edge.
//! Like `lens-rust`, this module only extracts edges; metric aggregation lives
//! in `lens-domain`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lens_domain::{CouplingEdge, EdgeKind, ModulePath};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Stmt, StmtImport, StmtImportFrom};
use ruff_python_parser::{ParseError, parse_module};

/// Failures raised while building module nodes from Python files.
#[derive(Debug, thiserror::Error)]
pub enum CouplingError {
    /// Reading a `.py` file failed.
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Parsing a Python file failed.
    #[error("failed to parse {path:?}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: ParseError,
    },
    /// Root path must be either a Python file or a directory.
    #[error("unsupported root {path:?}: expected a .py file or directory")]
    UnsupportedRoot { path: PathBuf },
}

/// One Python module (file) in the scanned tree.
#[derive(Debug, Clone)]
pub struct PythonModule {
    pub path: ModulePath,
    pub file: PathBuf,
    pub body: Vec<Stmt>,
}

/// Build Python modules from `root`.
///
/// * `root` as file: one module named `crate`.
/// * `root` as directory: every `.py` file recursively, rooted at `crate`.
pub fn build_module_tree(root: &Path) -> Result<Vec<PythonModule>, CouplingError> {
    if root.is_file() {
        if root.extension().and_then(std::ffi::OsStr::to_str) != Some("py") {
            return Err(CouplingError::UnsupportedRoot {
                path: root.to_path_buf(),
            });
        }
        return Ok(vec![parse_one(root, ModulePath::new("crate"))?]);
    }
    if !root.is_dir() {
        return Err(CouplingError::UnsupportedRoot {
            path: root.to_path_buf(),
        });
    }

    let mut files = Vec::new();
    collect_py_files(root, &mut files)?;
    files.sort();

    let mut out = Vec::with_capacity(files.len());
    for file in files {
        let rel = match file.strip_prefix(root) {
            Ok(rel) => rel,
            Err(_) => {
                return Err(CouplingError::UnsupportedRoot {
                    path: root.to_path_buf(),
                });
            }
        };
        out.push(parse_one(&file, module_path_for_rel(rel))?);
    }
    Ok(out)
}

fn collect_py_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), CouplingError> {
    for entry in std::fs::read_dir(dir).map_err(|source| CouplingError::Io {
        path: dir.to_path_buf(),
        source,
    })? {
        let entry = entry.map_err(|source| CouplingError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_py_files(&path, out)?;
        } else if path.extension().and_then(std::ffi::OsStr::to_str) == Some("py") {
            out.push(path);
        }
    }
    Ok(())
}

fn parse_one(file: &Path, path: ModulePath) -> Result<PythonModule, CouplingError> {
    let source = std::fs::read_to_string(file).map_err(|source| CouplingError::Io {
        path: file.to_path_buf(),
        source,
    })?;
    let parsed = parse_module(&source)
        .map_err(|source| CouplingError::Parse {
            path: file.to_path_buf(),
            source,
        })?
        .into_syntax();
    Ok(PythonModule {
        path,
        file: file.to_path_buf(),
        body: parsed.body,
    })
}

fn module_path_for_rel(rel: &Path) -> ModulePath {
    let mut segs: Vec<String> = rel
        .iter()
        .map(|s| s.to_string_lossy().to_string())
        .collect();
    if let Some(last) = segs.last_mut()
        && let Some(stripped) = last.strip_suffix(".py")
    {
        *last = stripped.to_owned();
    }
    if segs.last().is_some_and(|s| s == "__init__") {
        segs.pop();
    }
    let mut out = String::from("crate");
    for s in segs {
        if s.is_empty() {
            continue;
        }
        out.push_str("::");
        out.push_str(&s);
    }
    ModulePath::new(out)
}

/// Collect every inter-module import edge in `modules`.
///
/// Edges are emitted with [`EdgeKind::Use`]. Self-loops and duplicates are not
/// filtered here.
pub fn extract_edges(modules: &[PythonModule]) -> Vec<CouplingEdge> {
    let mut known = HashSet::new();
    let mut py_to_mod = HashMap::new();
    for m in modules {
        known.insert(m.path.clone());
        py_to_mod.insert(path_to_python(&m.path), m.path.clone());
    }

    let mut edges = Vec::new();
    for module in modules {
        let current_py = path_to_python(&module.path);
        let mut v = ImportVisitor::new(&module.path, &current_py, &known, &py_to_mod);
        for stmt in &module.body {
            v.visit_stmt(stmt);
        }
        edges.extend(v.edges);
    }
    edges
}

fn path_to_python(path: &ModulePath) -> String {
    path.as_str()
        .strip_prefix("crate::")
        .or_else(|| path.as_str().strip_prefix("crate"))
        .unwrap_or(path.as_str())
        .replace("::", ".")
        .trim_matches('.')
        .to_owned()
}

struct ImportVisitor<'a> {
    from: &'a ModulePath,
    current_py: &'a str,
    known: &'a HashSet<ModulePath>,
    py_to_mod: &'a HashMap<String, ModulePath>,
    edges: Vec<CouplingEdge>,
}

impl<'a> ImportVisitor<'a> {
    fn new(
        from: &'a ModulePath,
        current_py: &'a str,
        known: &'a HashSet<ModulePath>,
        py_to_mod: &'a HashMap<String, ModulePath>,
    ) -> Self {
        Self {
            from,
            current_py,
            known,
            py_to_mod,
            edges: Vec::new(),
        }
    }

    fn push_abs_import(&mut self, module_name: &str) {
        let Some((to, symbol)) = resolve_module(module_name, self.py_to_mod, self.known) else {
            return;
        };
        self.edges.push(CouplingEdge {
            from: self.from.clone(),
            to,
            symbol,
            kind: EdgeKind::Use,
        });
    }

    fn push_from_import(&mut self, stmt: &StmtImportFrom) {
        let base = resolve_from_base(
            self.current_py,
            stmt.level,
            stmt.module.as_ref().map(|m| m.as_str()),
        );
        let Some(base) = base else {
            return;
        };

        for alias in &stmt.names {
            let imported = alias.name.as_str();
            if imported == "*" {
                let Some((to, _)) = resolve_module(&base, self.py_to_mod, self.known) else {
                    continue;
                };
                self.edges.push(CouplingEdge {
                    from: self.from.clone(),
                    to,
                    symbol: "*".to_owned(),
                    kind: EdgeKind::Use,
                });
                continue;
            }

            let joined = if base.is_empty() {
                imported.to_owned()
            } else {
                format!("{base}.{imported}")
            };

            if let Some((to, symbol)) = resolve_module(&joined, self.py_to_mod, self.known) {
                self.edges.push(CouplingEdge {
                    from: self.from.clone(),
                    to,
                    symbol,
                    kind: EdgeKind::Use,
                });
                continue;
            }

            if let Some((to, _)) = resolve_module(&base, self.py_to_mod, self.known) {
                self.edges.push(CouplingEdge {
                    from: self.from.clone(),
                    to,
                    symbol: imported.to_owned(),
                    kind: EdgeKind::Use,
                });
            }
        }
    }
}

impl<'a, 'ast> Visitor<'ast> for ImportVisitor<'a> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::Import(StmtImport { names, .. }) => {
                for alias in names {
                    self.push_abs_import(alias.name.as_str());
                }
            }
            Stmt::ImportFrom(from) => self.push_from_import(from),
            _ => walk_stmt(self, stmt),
        }
    }
}

fn resolve_module(
    name: &str,
    py_to_mod: &HashMap<String, ModulePath>,
    known: &HashSet<ModulePath>,
) -> Option<(ModulePath, String)> {
    if name.is_empty() {
        return None;
    }
    if let Some(m) = py_to_mod.get(name)
        && known.contains(m)
    {
        let symbol = name.rsplit('.').next().unwrap_or(name).to_owned();
        return Some((m.clone(), symbol));
    }

    let segs: Vec<&str> = name.split('.').collect();
    for i in (1..segs.len()).rev() {
        let prefix = segs[..i].join(".");
        if let Some(m) = py_to_mod.get(&prefix)
            && known.contains(m)
        {
            let symbol = segs.last().copied().unwrap_or_default().to_owned();
            return Some((m.clone(), symbol));
        }
    }
    None
}

fn resolve_from_base(current: &str, level: u32, module: Option<&str>) -> Option<String> {
    let mut segs: Vec<&str> = if level == 0 || current.is_empty() {
        Vec::new()
    } else {
        current.split('.').collect()
    };

    if level != 0 {
        let pops = level as usize;
        if pops > segs.len() {
            return None;
        }
        segs.truncate(segs.len() - pops);
    }

    if let Some(module) = module
        && !module.is_empty()
    {
        segs.extend(module.split('.'));
    }

    Some(segs.join("."))
}

#[cfg(test)]
mod tests {
    use lens_domain::{CouplingEdge, EdgeKind, ModulePath};

    use super::{build_module_tree, extract_edges};

    fn e(from: &str, to: &str, symbol: &str) -> CouplingEdge {
        CouplingEdge {
            from: ModulePath::new(from),
            to: ModulePath::new(to),
            symbol: symbol.to_owned(),
            kind: EdgeKind::Use,
        }
    }

    #[test]
    fn extracts_edges_for_import_and_from_import() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("a.py"), "class A: pass\n").expect("write a");
        std::fs::create_dir_all(root.path().join("pkg")).expect("pkg dir");
        std::fs::write(root.path().join("pkg").join("__init__.py"), "").expect("write init");
        std::fs::write(root.path().join("pkg").join("b.py"), "def f(): pass\n").expect("write b");
        std::fs::write(root.path().join("main.py"), "import a\nfrom pkg import b\n")
            .expect("write main");

        let modules = build_module_tree(root.path()).expect("tree");
        let mut edges = extract_edges(&modules);
        edges.sort_by(|l, r| {
            (&l.from, &l.to, &l.symbol, l.kind).cmp(&(&r.from, &r.to, &r.symbol, r.kind))
        });

        assert_eq!(
            edges,
            vec![
                e("crate::main", "crate::a", "a"),
                e("crate::main", "crate::pkg::b", "b"),
            ]
        );
    }

    #[test]
    fn resolves_relative_from_imports() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("pkg").join("sub")).expect("dirs");
        std::fs::write(root.path().join("pkg").join("__init__.py"), "").expect("init");
        std::fs::write(root.path().join("pkg").join("util.py"), "").expect("util");
        std::fs::write(root.path().join("pkg").join("sub").join("__init__.py"), "")
            .expect("sub init");
        std::fs::write(
            root.path().join("pkg").join("sub").join("mod.py"),
            "from .. import util\n",
        )
        .expect("mod");

        let modules = build_module_tree(root.path()).expect("tree");
        let edges = extract_edges(&modules);

        assert!(edges.contains(&e("crate::pkg::sub::mod", "crate::pkg::util", "util")));
    }

    #[test]
    fn rejects_non_python_file_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let txt = root.path().join("note.txt");
        std::fs::write(&txt, "hello").expect("write");

        let err = build_module_tree(&txt).expect_err("must reject non .py root");
        assert!(matches!(err, super::CouplingError::UnsupportedRoot { .. }));
    }

    #[test]
    fn relative_import_with_level_equal_to_depth_resolves_from_root() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("pkg").join("sub")).expect("dirs");
        std::fs::write(root.path().join("util.py"), "").expect("util");
        std::fs::write(root.path().join("pkg").join("__init__.py"), "").expect("pkg init");
        std::fs::write(root.path().join("pkg").join("sub").join("__init__.py"), "")
            .expect("sub init");
        std::fs::write(
            root.path().join("pkg").join("sub").join("mod.py"),
            "from ... import util
",
        )
        .expect("mod");

        let modules = build_module_tree(root.path()).expect("tree");
        let edges = extract_edges(&modules);

        assert!(edges.contains(&e("crate::pkg::sub::mod", "crate::util", "util")));
    }

    #[test]
    fn too_deep_relative_import_is_ignored() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("pkg").join("sub")).expect("dirs");
        std::fs::write(root.path().join("util.py"), "").expect("util");
        std::fs::write(root.path().join("pkg").join("__init__.py"), "").expect("pkg init");
        std::fs::write(root.path().join("pkg").join("sub").join("__init__.py"), "")
            .expect("sub init");
        std::fs::write(
            root.path().join("pkg").join("sub").join("mod.py"),
            "from .... import util
",
        )
        .expect("mod");

        let modules = build_module_tree(root.path()).expect("tree");
        let edges = extract_edges(&modules);

        assert!(!edges.contains(&e("crate::pkg::sub::mod", "crate::util", "util")));
    }

    #[test]
    fn relative_import_preserves_expected_parent_depth() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("a").join("b").join("c").join("d")).expect("dirs");
        std::fs::write(
            root.path().join("a").join("b").join("c").join("util.py"),
            "",
        )
        .expect("util");
        std::fs::write(
            root.path()
                .join("a")
                .join("b")
                .join("c")
                .join("d")
                .join("e.py"),
            "from .. import util
",
        )
        .expect("module");

        let modules = build_module_tree(root.path()).expect("tree");
        let edges = extract_edges(&modules);

        assert!(edges.contains(&e("crate::a::b::c::d::e", "crate::a::b::c::util", "util")));
    }

    #[test]
    fn resolve_from_base_allows_level_equal_to_current_depth() {
        let base = super::resolve_from_base("pkg.sub.mod", 3, None);
        assert_eq!(base.as_deref(), Some(""));
    }
}
