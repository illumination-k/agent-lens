//! Go module-tree construction and import-edge extraction.
//!
//! Coupling for Go is modeled at *package* granularity: every directory
//! that contains one or more `.go` files becomes a node, and each
//! `import "..."` statement that resolves under the local module
//! (declared in `go.mod`) becomes a [`EdgeKind::Use`] edge between two
//! package nodes. Imports of the standard library and external modules
//! are dropped because the analyzer is single-module by design.
//!
//! The mapping from filesystem layout to [`ModulePath`] mirrors the
//! Python adapter: the root is `crate`, and each subdirectory adds a
//! `::`-separated segment. The module name from `go.mod` is *only*
//! used to recognise local imports; the emitted [`ModulePath`] always
//! starts with `crate` so reports stay comparable across languages.
//!
//! Like the Python adapter, this module only extracts edges; metric
//! aggregation lives in `lens-domain`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use lens_domain::{CouplingEdge, EdgeKind, ModulePath};
use tree_sitter::Node;

use crate::parser::{GoParseError, parse_tree, unquote_go_string_literal};

/// Failures raised while building Go package nodes.
#[derive(Debug, thiserror::Error)]
pub enum CouplingError {
    /// Reading a `.go` file (or a directory entry) failed.
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Parsing a Go file failed.
    #[error("failed to parse {path:?}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: GoParseError,
    },
    /// Root path must be either a Go file or a directory.
    #[error("unsupported root {path:?}: expected a .go file or directory")]
    UnsupportedRoot { path: PathBuf },
}

/// One Go package (directory) in the scanned tree.
#[derive(Debug, Clone)]
pub struct GoPackage {
    /// `crate`-prefixed module path (e.g. `crate::pkg::util`).
    pub path: ModulePath,
    /// Directory containing the package, or the file itself when `root`
    /// was a single `.go` file.
    pub file: PathBuf,
    /// Resolved import paths for every `.go` file in this package. Local
    /// imports keep the full Go-style path (e.g.
    /// `github.com/me/proj/pkg/util`); external imports are filtered out
    /// when edges are extracted.
    imports: Vec<String>,
}

/// Build Go packages from `root`.
///
/// * `root` as `.go` file: one package named `crate` whose directory is
///   the file itself. No local imports can be resolved (the module
///   prefix is unknown).
/// * `root` as directory: every `.go` file recursively, grouped by its
///   parent directory. The first `go.mod` discovered at or above `root`
///   provides the module prefix used to recognise local imports.
pub fn build_module_tree(root: &Path) -> Result<Vec<GoPackage>, CouplingError> {
    if root.is_file() {
        if root.extension().and_then(std::ffi::OsStr::to_str) != Some("go") {
            return Err(CouplingError::UnsupportedRoot {
                path: root.to_path_buf(),
            });
        }
        let imports = parse_imports(root)?;
        return Ok(vec![GoPackage {
            path: ModulePath::new("crate"),
            file: root.to_path_buf(),
            imports,
        }]);
    }
    if !root.is_dir() {
        return Err(CouplingError::UnsupportedRoot {
            path: root.to_path_buf(),
        });
    }

    let mut files = Vec::new();
    collect_go_files(root, &mut files)?;
    files.sort();

    let mut by_dir: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for file in files {
        let dir = file.parent().unwrap_or(Path::new(".")).to_path_buf();
        by_dir.entry(dir).or_default().push(file);
    }

    let mut out = Vec::with_capacity(by_dir.len());
    for (dir, files) in by_dir {
        let rel = dir.strip_prefix(root).unwrap_or(&dir);
        let path = module_path_for_rel(rel);
        let mut imports = Vec::new();
        for file in &files {
            imports.extend(parse_imports(file)?);
        }
        out.push(GoPackage {
            path,
            file: dir,
            imports,
        });
    }
    Ok(out)
}

/// Collect every cross-package import edge in `packages`.
///
/// Edges are emitted with [`EdgeKind::Use`]. Imports that don't resolve
/// to a known local package (standard library, external modules) are
/// silently dropped; self-loops and duplicates are not filtered here
/// (`lens-domain::compute_report` handles that).
pub fn extract_edges(packages: &[GoPackage]) -> Vec<CouplingEdge> {
    let module_prefix = read_go_module_prefix(packages);
    let known: HashSet<&ModulePath> = packages.iter().map(|p| &p.path).collect();
    let go_to_module: HashMap<String, ModulePath> = packages
        .iter()
        .map(|p| {
            (
                go_path_for(&p.path, module_prefix.as_deref()),
                p.path.clone(),
            )
        })
        .collect();

    let mut edges = Vec::new();
    for pkg in packages {
        for import in &pkg.imports {
            let Some(target) = resolve_import(import, &go_to_module) else {
                continue;
            };
            if !known.contains(&target) {
                continue;
            }
            let symbol = import
                .rsplit('/')
                .next()
                .unwrap_or(import.as_str())
                .to_owned();
            edges.push(CouplingEdge {
                from: pkg.path.clone(),
                to: target,
                symbol,
                kind: EdgeKind::Use,
            });
        }
    }
    edges
}

fn collect_go_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), CouplingError> {
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
            collect_go_files(&path, out)?;
        } else if path.extension().and_then(std::ffi::OsStr::to_str) == Some("go") {
            out.push(path);
        }
    }
    Ok(())
}

fn module_path_for_rel(rel: &Path) -> ModulePath {
    let mut out = String::from("crate");
    for segment in rel.iter() {
        let s = segment.to_string_lossy();
        if s.is_empty() {
            continue;
        }
        out.push_str("::");
        out.push_str(&s);
    }
    ModulePath::new(out)
}

/// Parse a single `.go` file and pull every resolved import path out of
/// its `import` declarations. `_`-imports (used for side effects), `.`-
/// imports, and aliased imports are all included — the analyzer only
/// needs the import path itself for edge resolution.
fn parse_imports(file: &Path) -> Result<Vec<String>, CouplingError> {
    let source = std::fs::read_to_string(file).map_err(|source| CouplingError::Io {
        path: file.to_path_buf(),
        source,
    })?;
    let tree = parse_tree(&source).map_err(|source| CouplingError::Parse {
        path: file.to_path_buf(),
        source,
    })?;
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut cursor = tree.root_node().walk();
    for child in tree.root_node().named_children(&mut cursor) {
        if child.kind() == "import_declaration" {
            collect_import_specs(child, bytes, &mut out);
        }
    }
    Ok(out)
}

fn collect_import_specs(node: Node<'_>, source: &[u8], out: &mut Vec<String>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "import_spec" => {
                if let Some(path) = import_spec_path(child, source) {
                    out.push(path);
                }
            }
            "import_spec_list" => {
                let mut inner = child.walk();
                for spec in child.named_children(&mut inner) {
                    if spec.kind() == "import_spec"
                        && let Some(path) = import_spec_path(spec, source)
                    {
                        out.push(path);
                    }
                }
            }
            _ => {}
        }
    }
}

fn import_spec_path(spec: Node<'_>, source: &[u8]) -> Option<String> {
    let path = spec.child_by_field_name("path")?;
    let text = path.utf8_text(source).ok()?;
    Some(unquote_go_string_literal(text))
}

/// Convert a `crate`-prefixed [`ModulePath`] to a Go-style import path
/// using `module_prefix` from `go.mod` as the leading segment. Falls
/// back to a slash-separated relative path when no prefix is known —
/// good enough for tests and single-file analyses without `go.mod`.
fn go_path_for(module_path: &ModulePath, module_prefix: Option<&str>) -> String {
    let s = module_path.as_str();
    let rest = s
        .strip_prefix("crate::")
        .unwrap_or_else(|| if s == "crate" { "" } else { s });
    let local = rest.replace("::", "/");
    match (module_prefix, local.is_empty()) {
        (Some(prefix), true) => prefix.to_owned(),
        (Some(prefix), false) => format!("{prefix}/{local}"),
        (None, true) => String::new(),
        (None, false) => local,
    }
}

/// Resolve a Go import path string to a known [`ModulePath`] in the
/// scanned tree. Imports outside the local module return `None`.
///
/// Lookup is exact-match against the canonical Go-style path stored
/// in `go_to_module`. The map is built so that every package present
/// in the scan has a key for its full Go path (workspace prefix
/// included when a `go.mod` was found). Imports that don't have a key
/// in the map — standard library, external modules, vendored
/// packages — return `None`.
fn resolve_import(import: &str, go_to_module: &HashMap<String, ModulePath>) -> Option<ModulePath> {
    go_to_module.get(import).cloned()
}

/// Read `go.mod` from the deepest directory shared by `packages` (the
/// workspace root) and pluck the `module ...` line out of it. Returns
/// `None` when no `go.mod` is found, the file can't be read, or the
/// file is missing the `module` declaration entirely.
fn read_go_module_prefix(packages: &[GoPackage]) -> Option<String> {
    let mut roots: BTreeSet<&Path> = packages.iter().map(|p| p.file.as_path()).collect();
    while let Some(dir) = roots.pop_first() {
        let go_mod = dir.join("go.mod");
        if let Ok(text) = std::fs::read_to_string(&go_mod)
            && let Some(prefix) = parse_module_directive(&text)
        {
            return Some(prefix);
        }
        if let Some(parent) = dir.parent() {
            roots.insert(parent);
        }
    }
    None
}

/// Pluck the module name out of a `go.mod` body's `module ...`
/// directive. Returns `None` when no such directive is present.
///
/// Trivia lines (blank or `// comment`) are skipped implicitly: the
/// `strip_prefix("module")` check filters them out without a separate
/// guard, since neither shape begins with the literal `module` token.
fn parse_module_directive(text: &str) -> Option<String> {
    for raw in text.lines() {
        let line = raw.trim();
        let Some(rest) = line.strip_prefix("module") else {
            continue;
        };
        // The `module` keyword must be followed by whitespace or end of
        // line; otherwise we'd accidentally match identifiers like
        // `module_v2` declared as bare statements (in practice, none
        // exist in `go.mod`, but the guard keeps the parser honest).
        let Some(first) = rest.chars().next() else {
            continue;
        };
        if !first.is_whitespace() {
            continue;
        }
        // Drop any trailing `// comment` and the surrounding quotes
        // around the module path.
        let payload = rest.split("//").next().unwrap_or(rest).trim();
        let cleaned = payload.trim_matches(|c| c == '"' || c == '`').trim();
        if !cleaned.is_empty() {
            return Some(cleaned.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, contents: &str) -> PathBuf {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&path, contents).expect("write");
        path
    }

    fn edge(from: &str, to: &str, symbol: &str) -> CouplingEdge {
        CouplingEdge {
            from: ModulePath::new(from),
            to: ModulePath::new(to),
            symbol: symbol.to_owned(),
            kind: EdgeKind::Use,
        }
    }

    #[test]
    fn extracts_local_imports_under_module_prefix() {
        let root = tempfile::tempdir().expect("tempdir");
        write(root.path(), "go.mod", "module github.com/x/proj\n");
        write(
            root.path(),
            "main.go",
            "package main\n\nimport \"github.com/x/proj/pkg/util\"\n\nfunc main() { util.Run() }\n",
        );
        write(
            root.path(),
            "pkg/util/util.go",
            "package util\n\nfunc Run() {}\n",
        );

        let modules = build_module_tree(root.path()).expect("tree");
        let mut edges = extract_edges(&modules);
        edges.sort_by(|l, r| (&l.from, &l.to, &l.symbol).cmp(&(&r.from, &r.to, &r.symbol)));

        assert!(modules.iter().any(|m| m.path.as_str() == "crate"));
        assert!(
            modules
                .iter()
                .any(|m| m.path.as_str() == "crate::pkg::util")
        );
        assert_eq!(edges, vec![edge("crate", "crate::pkg::util", "util")]);
    }

    #[test]
    fn drops_standard_and_external_imports() {
        let root = tempfile::tempdir().expect("tempdir");
        write(root.path(), "go.mod", "module github.com/x/proj\n");
        write(
            root.path(),
            "main.go",
            "package main\n\nimport (\n    \"fmt\"\n    \"os\"\n    \"github.com/foo/bar\"\n)\n\nfunc main() { fmt.Println(os.Args, bar.Stuff) }\n",
        );

        let modules = build_module_tree(root.path()).expect("tree");
        let edges = extract_edges(&modules);
        assert!(
            edges.is_empty(),
            "external/stdlib imports must not produce edges, got {edges:?}",
        );
    }

    #[test]
    fn resolves_aliased_dot_and_blank_imports() {
        let root = tempfile::tempdir().expect("tempdir");
        write(root.path(), "go.mod", "module github.com/x/proj\n");
        write(
            root.path(),
            "main.go",
            "package main\n\nimport (\n    foo \"github.com/x/proj/pkg/foo\"\n    . \"github.com/x/proj/pkg/bar\"\n    _ \"github.com/x/proj/pkg/baz\"\n)\n\nfunc main() {}\n",
        );
        write(root.path(), "pkg/foo/foo.go", "package foo\n");
        write(root.path(), "pkg/bar/bar.go", "package bar\n");
        write(root.path(), "pkg/baz/baz.go", "package baz\n");

        let modules = build_module_tree(root.path()).expect("tree");
        let edges = extract_edges(&modules);
        let mut targets: Vec<&str> = edges
            .iter()
            .filter(|e| e.from.as_str() == "crate")
            .map(|e| e.to.as_str())
            .collect();
        targets.sort();
        targets.dedup();
        assert_eq!(
            targets,
            vec!["crate::pkg::bar", "crate::pkg::baz", "crate::pkg::foo"],
        );
    }

    #[test]
    fn groups_multiple_files_in_the_same_directory_into_one_package() {
        let root = tempfile::tempdir().expect("tempdir");
        write(root.path(), "go.mod", "module github.com/x/proj\n");
        write(
            root.path(),
            "pkg/util/a.go",
            "package util\n\nimport \"github.com/x/proj/pkg/dep\"\n\nvar _ = dep.X\n",
        );
        write(
            root.path(),
            "pkg/util/b.go",
            "package util\n\nfunc Helper() {}\n",
        );
        write(root.path(), "pkg/dep/dep.go", "package dep\n\nvar X = 1\n");

        let modules = build_module_tree(root.path()).expect("tree");
        let util = modules
            .iter()
            .find(|m| m.path.as_str() == "crate::pkg::util")
            .expect("util package");
        // Both source files in pkg/util collapse into one package.
        assert_eq!(
            modules
                .iter()
                .filter(|m| m.path.as_str() == "crate::pkg::util")
                .count(),
            1,
        );
        assert!(
            util.imports
                .iter()
                .any(|s| s == "github.com/x/proj/pkg/dep")
        );

        let edges = extract_edges(&modules);
        assert!(
            edges.iter().any(
                |e| e.from.as_str() == "crate::pkg::util" && e.to.as_str() == "crate::pkg::dep"
            )
        );
    }

    #[test]
    fn rejects_non_go_file_root() {
        let root = tempfile::tempdir().expect("tempdir");
        let txt = write(root.path(), "note.txt", "hi");
        let err = build_module_tree(&txt).expect_err("must reject non-.go root");
        assert!(matches!(err, CouplingError::UnsupportedRoot { .. }));
    }

    #[test]
    fn single_file_root_has_no_resolvable_imports() {
        let root = tempfile::tempdir().expect("tempdir");
        let go = write(
            root.path(),
            "main.go",
            "package main\n\nimport \"github.com/x/proj/pkg/util\"\n\nfunc main() { util.Run() }\n",
        );

        let modules = build_module_tree(&go).expect("tree");
        assert_eq!(modules.len(), 1);
        // No go.mod, no other packages — the import target isn't local.
        assert!(extract_edges(&modules).is_empty());
    }

    #[test]
    fn parses_module_directive_with_inline_comment() {
        let prefix = parse_module_directive("module github.com/x/y // hi\n\nrequire foo v1.0.0\n");
        assert_eq!(prefix.as_deref(), Some("github.com/x/y"));
    }

    #[test]
    fn parses_quoted_module_directive() {
        let prefix = parse_module_directive("module \"github.com/x/y\"\n");
        assert_eq!(prefix.as_deref(), Some("github.com/x/y"));
    }

    #[test]
    fn module_with_no_module_directive_is_none() {
        assert!(parse_module_directive("// only comments\n").is_none());
    }

    #[test]
    fn imports_without_module_prefix_only_match_exact_paths() {
        let root = tempfile::tempdir().expect("tempdir");
        // No go.mod — the only way an import resolves is when its path
        // exactly matches a discovered package's relative slash-form.
        write(
            root.path(),
            "main.go",
            "package main\n\nimport \"pkg/util\"\n\nfunc main() { util.Run() }\n",
        );
        write(
            root.path(),
            "pkg/util/util.go",
            "package util\n\nfunc Run() {}\n",
        );

        let modules = build_module_tree(root.path()).expect("tree");
        let edges = extract_edges(&modules);
        assert!(
            edges
                .iter()
                .any(|e| e.from.as_str() == "crate" && e.to.as_str() == "crate::pkg::util")
        );
    }

    /// `parse_module_directive` skips blank lines (`is_empty || starts
    /// with //`). With `||` flipped to `&&`, a `// comment-only` line at
    /// the top would no longer be skipped and the function would bail
    /// out on the first non-empty line. Pin the comment-then-module
    /// shape so the disjunction is exercised end-to-end.
    #[test]
    fn parse_module_directive_skips_leading_comment_lines() {
        let prefix =
            parse_module_directive("// header comment\n// another\nmodule github.com/x/y\n");
        assert_eq!(prefix.as_deref(), Some("github.com/x/y"));
    }

    /// Imports of the workspace root module path must resolve to
    /// `crate` (the root package), not produce a stray `crate::` slot.
    /// `go_path_for` handles the empty-tail case for the root, and the
    /// resolver looks up by exact key — both layers have to agree to
    /// keep the round-trip working.
    #[test]
    fn module_root_imports_resolve_to_crate_with_no_extra_segments() {
        let root = tempfile::tempdir().expect("tempdir");
        write(root.path(), "go.mod", "module github.com/x/proj\n");
        write(root.path(), "main.go", "package main\n\nfunc Helper() {}\n");
        write(
            root.path(),
            "pkg/util/util.go",
            "package util\n\nimport \"github.com/x/proj\"\n\nvar _ = proj.Helper\n",
        );

        let modules = build_module_tree(root.path()).expect("tree");
        let edges = extract_edges(&modules);
        assert!(
            edges
                .iter()
                .any(|e| e.from.as_str() == "crate::pkg::util" && e.to.as_str() == "crate"),
            "import of the exact module prefix must resolve to crate (root); got {edges:?}",
        );
    }

    /// `go_path_for` keeps `crate` as a special case: when the module
    /// path is exactly `crate`, the rendered import string drops the
    /// `crate` segment entirely and uses just the workspace prefix.
    /// The `==` mutation flips that, producing `crate/...` paths that
    /// no longer match the workspace prefix on lookup.
    #[test]
    fn root_package_renders_as_module_prefix_alone() {
        assert_eq!(
            go_path_for(&ModulePath::new("crate"), Some("github.com/x/proj")),
            "github.com/x/proj",
        );
        assert_eq!(
            go_path_for(
                &ModulePath::new("crate::pkg::util"),
                Some("github.com/x/proj")
            ),
            "github.com/x/proj/pkg/util",
        );
    }
}
