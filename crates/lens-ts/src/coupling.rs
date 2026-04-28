//! File-level coupling extraction for TypeScript / JavaScript projects.
//!
//! Unlike `lens-rust` (which walks Rust `mod` trees), TS/JS coupling is
//! modeled at file granularity: each discovered source file is one module,
//! and relative `import` / `export ... from` edges become `EdgeKind::Use`
//! edges between those file modules.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use lens_domain::{CouplingEdge, EdgeKind, ModulePath};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ExportAllDeclaration, ExportNamedDeclaration, ImportDeclaration, ImportDeclarationSpecifier,
    Statement,
};
use oxc_parser::Parser;

use crate::parser::{Dialect, TsParseError};

/// Failures raised while discovering a TS/JS module graph.
#[derive(Debug, thiserror::Error)]
pub enum CouplingError {
    /// Reading a source file failed.
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// Parsing a source file failed.
    #[error("failed to parse {path:?}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: TsParseError,
    },
}

/// One discovered TS/JS source file.
#[derive(Debug, Clone)]
pub struct TsModule {
    pub path: ModulePath,
    pub file: PathBuf,
    links: Vec<ImportLink>,
}

/// Build the transitive file graph rooted at `entry` by following
/// relative module specifiers (`./` and `../`).
pub fn build_module_tree(entry: &Path) -> Result<Vec<TsModule>, CouplingError> {
    let entry = entry.to_path_buf();
    let root_dir = entry
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    let mut out = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut stack = vec![entry];

    while let Some(file) = stack.pop() {
        if !seen.insert(file.clone()) {
            continue;
        }
        let source = std::fs::read_to_string(&file).map_err(|source| CouplingError::Io {
            path: file.clone(),
            source,
        })?;
        let dialect = Dialect::from_path(&file).unwrap_or(Dialect::Ts);
        let links = parse_links(&source, dialect).map_err(|source| CouplingError::Parse {
            path: file.clone(),
            source,
        })?;
        let module_path = file_to_module_path(&file, &root_dir);
        let base = file.parent().unwrap_or_else(|| Path::new("."));
        for link in &links {
            if let Some(next) = resolve_relative_module(base, &link.specifier)
                && next.exists()
            {
                stack.push(next);
            }
        }
        out.push(TsModule {
            path: module_path,
            file: file.clone(),
            links,
        });
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Collect import/re-export edges between discovered modules.
pub fn extract_edges(modules: &[TsModule]) -> Vec<CouplingEdge> {
    let known_by_file: HashMap<&Path, &ModulePath> = modules
        .iter()
        .map(|m| (m.file.as_path(), &m.path))
        .collect();

    let mut edges = Vec::new();
    for module in modules {
        let base = module.file.parent().unwrap_or_else(|| Path::new("."));
        for link in &module.links {
            let Some(target_file) = resolve_relative_module(base, &link.specifier) else {
                continue;
            };
            let Some(target) = known_by_file.get(target_file.as_path()) else {
                continue;
            };
            for symbol in &link.symbols {
                edges.push(CouplingEdge {
                    from: module.path.clone(),
                    to: (*target).clone(),
                    symbol: symbol.clone(),
                    kind: EdgeKind::Use,
                });
            }
        }
    }
    edges
}

#[derive(Debug, Clone)]
struct ImportLink {
    specifier: String,
    symbols: Vec<String>,
}

fn parse_links(source: &str, dialect: Dialect) -> Result<Vec<ImportLink>, TsParseError> {
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, source, dialect.source_type()).parse();
    if !ret.errors.is_empty() {
        return Err(TsParseError::from_diagnostics(
            ret.errors.iter().map(|e| e.message.as_ref().to_owned()),
        ));
    }

    let mut out = Vec::new();
    for stmt in &ret.program.body {
        match stmt {
            Statement::ImportDeclaration(decl) => maybe_push_import(&mut out, decl),
            Statement::ExportNamedDeclaration(decl) => maybe_push_re_export_named(&mut out, decl),
            Statement::ExportAllDeclaration(decl) => maybe_push_re_export_all(&mut out, decl),
            _ => {}
        }
    }
    Ok(out)
}

fn maybe_push_import(out: &mut Vec<ImportLink>, decl: &ImportDeclaration<'_>) {
    let specifier = decl.source.value.to_string();
    if !is_relative_specifier(&specifier) {
        return;
    }
    let mut symbols = Vec::new();
    if let Some(specifiers) = &decl.specifiers {
        for spec in specifiers {
            match spec {
                ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => {
                    symbols.push("default".to_owned());
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => {
                    symbols.push("*".to_owned());
                }
                ImportDeclarationSpecifier::ImportSpecifier(s) => {
                    symbols.push(s.imported.name().to_string());
                }
            }
        }
    }
    if symbols.is_empty() {
        symbols.push("*".to_owned());
    }
    out.push(ImportLink { specifier, symbols });
}

fn maybe_push_re_export_named(out: &mut Vec<ImportLink>, decl: &ExportNamedDeclaration<'_>) {
    let Some(src) = &decl.source else { return };
    let specifier = src.value.to_string();
    if !is_relative_specifier(&specifier) {
        return;
    }
    let mut symbols = Vec::new();
    for spec in &decl.specifiers {
        symbols.push(spec.local.name().to_string());
    }
    if symbols.is_empty() {
        symbols.push("*".to_owned());
    }
    out.push(ImportLink { specifier, symbols });
}

fn maybe_push_re_export_all(out: &mut Vec<ImportLink>, decl: &ExportAllDeclaration<'_>) {
    let specifier = decl.source.value.to_string();
    if !is_relative_specifier(&specifier) {
        return;
    }
    out.push(ImportLink {
        specifier,
        symbols: vec!["*".to_owned()],
    });
}

fn is_relative_specifier(specifier: &str) -> bool {
    specifier.starts_with("./") || specifier.starts_with("../")
}

fn resolve_relative_module(base: &Path, specifier: &str) -> Option<PathBuf> {
    if !is_relative_specifier(specifier) {
        return None;
    }
    let joined = base.join(specifier);
    if joined.extension().is_some() {
        return Some(joined);
    }

    const EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

    for ext in EXTS {
        let candidate = joined.with_extension(ext);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    for ext in EXTS {
        let candidate = joined.join(format!("index.{ext}"));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    Some(joined)
}

fn file_to_module_path(file: &Path, root_dir: &Path) -> ModulePath {
    let rel = file.strip_prefix(root_dir).unwrap_or(file);
    let mut parts = vec!["crate".to_owned()];
    for comp in rel.components() {
        let mut s = comp.as_os_str().to_string_lossy().to_string();
        if let Some((stem, _)) = s.rsplit_once('.') {
            s = stem.to_owned();
        }
        if s == "index" {
            continue;
        }
        if !s.is_empty() {
            parts.push(s);
        }
    }
    ModulePath::new(parts.join("::"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_temp_project() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "lens_ts_coupling_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).expect("create temp project");
        base
    }

    #[test]
    fn builds_graph_and_extracts_use_edges_from_imports_and_reexports() {
        let root = mk_temp_project();
        let entry = root.join("src").join("main.ts");
        let util = root.join("src").join("util.ts");
        let shared = root.join("src").join("shared").join("index.ts");
        std::fs::create_dir_all(entry.parent().expect("parent")).expect("mkdir src");
        std::fs::create_dir_all(shared.parent().expect("parent")).expect("mkdir shared");

        std::fs::write(
            &entry,
            "import { add } from './util'; export * from './shared';",
        )
        .expect("write main");
        std::fs::write(
            &util,
            "export function add(a:number,b:number){ return a+b; }",
        )
        .expect("write util");
        std::fs::write(&shared, "export const k = 1;").expect("write shared");

        let modules = build_module_tree(&entry).expect("tree");
        let edges = extract_edges(&modules);

        assert!(modules.iter().any(|m| m.path.as_str() == "crate::main"));
        assert!(modules.iter().any(|m| m.path.as_str() == "crate::util"));
        assert!(modules.iter().any(|m| m.path.as_str() == "crate::shared"));

        assert!(edges.iter().any(|e| e.from.as_str() == "crate::main"
            && e.to.as_str() == "crate::util"
            && e.symbol == "add"
            && e.kind == EdgeKind::Use));
        assert!(edges.iter().any(|e| e.from.as_str() == "crate::main"
            && e.to.as_str() == "crate::shared"
            && e.symbol == "*"
            && e.kind == EdgeKind::Use));

        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn extracts_use_edges_from_named_reexports() {
        let root = mk_temp_project();
        let entry = root.join("src").join("main.ts");
        let util = root.join("src").join("util.ts");
        std::fs::create_dir_all(entry.parent().expect("parent")).expect("mkdir src");

        std::fs::write(&entry, "export { add as sum } from './util';").expect("write main");
        std::fs::write(
            &util,
            "export function add(a:number,b:number){ return a+b; }",
        )
        .expect("write util");

        let modules = build_module_tree(&entry).expect("tree");
        let edges = extract_edges(&modules);

        assert!(edges.iter().any(|e| e.from.as_str() == "crate::main"
            && e.to.as_str() == "crate::util"
            && e.symbol == "add"
            && e.kind == EdgeKind::Use));

        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn ignores_non_relative_imports_and_reexports() {
        let root = mk_temp_project();
        let entry = root.join("src").join("main.ts");
        let util = root.join("src").join("util.ts");
        std::fs::create_dir_all(entry.parent().expect("parent")).expect("mkdir src");
        std::fs::write(&util, "export const util = 1;").expect("write util");

        std::fs::write(
            &entry,
            "import { util } from 'util'; export { uniq } from 'lodash-es';",
        )
        .expect("write main");

        let modules = build_module_tree(&entry).expect("tree");
        let edges = extract_edges(&modules);

        assert_eq!(
            modules.len(),
            1,
            "bare specifiers must not be treated as local modules even when matching files exist"
        );
        assert!(
            edges.is_empty(),
            "non-relative imports/re-exports must not create local coupling edges"
        );

        std::fs::remove_dir_all(root).expect("cleanup");
    }
}
