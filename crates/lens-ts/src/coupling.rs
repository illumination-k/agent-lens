//! File-level coupling extraction for TypeScript / JavaScript projects.
//!
//! Unlike `lens-rust` (which walks Rust `mod` trees), TS/JS coupling is
//! modeled at file granularity: each discovered source file is one module,
//! and relative `import` / `export ... from` edges become `EdgeKind::Use`
//! edges between those file modules.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use lens_domain::{CouplingEdge, EdgeKind, ModulePath};
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ExportAllDeclaration, ExportNamedDeclaration, ImportDeclaration, ImportDeclarationSpecifier,
    ImportExpression, Statement,
};
use oxc_ast_visit::{Visit, walk::walk_import_expression};
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
    let entry = normalize_path(entry);
    let mut discovered = Vec::new();
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
        let base = file.parent().unwrap_or_else(|| Path::new("."));
        for link in &links {
            if let Some(next) = resolve_relative_module(base, &link.specifier)
                && next.exists()
            {
                stack.push(normalize_path(&next));
            }
        }
        discovered.push((file.clone(), links));
    }

    let root_dir = common_source_root(&discovered);
    let mut out: Vec<TsModule> = discovered
        .into_iter()
        .map(|(file, links)| TsModule {
            path: file_to_module_path(&file, &root_dir),
            file,
            links,
        })
        .collect();
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

    let mut dynamic_imports = DynamicImportVisitor::default();
    dynamic_imports.visit_program(&ret.program);
    out.extend(dynamic_imports.links);

    Ok(out)
}

#[derive(Default)]
struct DynamicImportVisitor {
    links: Vec<ImportLink>,
}

impl<'a> Visit<'a> for DynamicImportVisitor {
    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        if let Some(specifier) = static_string_value(&it.source)
            && is_relative_specifier(specifier)
        {
            self.links.push(ImportLink {
                specifier: specifier.to_owned(),
                symbols: vec!["*".to_owned()],
            });
        }
        walk_import_expression(self, it);
    }
}

fn static_string_value<'a>(expr: &'a oxc_ast::ast::Expression<'a>) -> Option<&'a str> {
    match expr {
        oxc_ast::ast::Expression::StringLiteral(lit) => Some(lit.value.as_str()),
        oxc_ast::ast::Expression::TemplateLiteral(lit) if lit.quasis.len() == 1 => {
            lit.quasis[0].value.cooked.map(|cooked| cooked.as_str())
        }
        _ => None,
    }
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
    let specifier = normalize_module_specifier(specifier);
    let joined = base.join(specifier);
    if joined.extension().is_some() {
        return Dialect::from_path(&joined)
            .is_some()
            .then(|| normalize_path(&joined));
    }

    const EXTS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

    for ext in EXTS {
        let candidate = joined.with_extension(ext);
        if candidate.exists() {
            return Some(normalize_path(&candidate));
        }
    }
    for ext in EXTS {
        let candidate = joined.join(format!("index.{ext}"));
        if candidate.exists() {
            return Some(normalize_path(&candidate));
        }
    }
    None
}

fn normalize_module_specifier(specifier: &str) -> &str {
    specifier
        .split_once(['?', '#'])
        .map_or(specifier, |(path, _)| path)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

fn common_source_root(discovered: &[(PathBuf, Vec<ImportLink>)]) -> PathBuf {
    let Some((first, _)) = discovered.first() else {
        return PathBuf::from(".");
    };
    let mut root = first
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();

    for (file, _) in &discovered[1..] {
        let parent = file.parent().unwrap_or_else(|| Path::new("."));
        while !parent.starts_with(&root) {
            if !root.pop() {
                return PathBuf::from(".");
            }
        }
    }
    root
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

    #[test]
    fn skips_relative_non_code_asset_imports() {
        let root = mk_temp_project();
        let entry = root.join("src").join("main.ts");
        let util = root.join("src").join("util.ts");
        let css = root.join("src").join("styles.css");
        let image = root.join("src").join("logo.svg");
        std::fs::create_dir_all(entry.parent().expect("parent")).expect("mkdir src");
        std::fs::write(
            &entry,
            "import './styles.css'; import logo from './logo.svg?url'; import { util } from './util';",
        )
        .expect("write main");
        std::fs::write(&util, "export const util = 1;").expect("write util");
        std::fs::write(&css, "body { color: red; }").expect("write css");
        std::fs::write(&image, "<svg></svg>").expect("write svg");

        let modules = build_module_tree(&entry).expect("tree");
        let edges = extract_edges(&modules);

        assert_eq!(
            modules.iter().map(|m| m.path.as_str()).collect::<Vec<_>>(),
            vec!["crate::main", "crate::util"],
            "asset imports must not be parsed as source modules"
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].to.as_str(), "crate::util");

        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn follows_static_dynamic_imports() {
        let root = mk_temp_project();
        let route = root.join("src").join("routes").join("index.tsx");
        let main = root.join("src").join("main.ts");
        std::fs::create_dir_all(route.parent().expect("parent")).expect("mkdir routes");
        std::fs::write(
            &route,
            "export function Route(){ void import('../main'); return null; }",
        )
        .expect("write route");
        std::fs::write(&main, "export const start = () => 1;").expect("write main");

        let modules = build_module_tree(&route).expect("tree");
        let edges = extract_edges(&modules);

        assert!(
            modules.iter().any(|m| m.path.as_str() == "crate::main"),
            "dynamic import target must be included in the graph"
        );
        assert!(edges.iter().any(|e| e.from.as_str() == "crate::routes"
            && e.to.as_str() == "crate::main"
            && e.symbol == "*"
            && e.kind == EdgeKind::Use));

        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn follows_no_substitution_template_dynamic_imports() {
        let root = mk_temp_project();
        let route = root.join("src").join("routes").join("index.tsx");
        let main = root.join("src").join("main.ts");
        std::fs::create_dir_all(route.parent().expect("parent")).expect("mkdir routes");
        std::fs::write(
            &route,
            "export function Route(){ void import(`../main`); return null; }",
        )
        .expect("write route");
        std::fs::write(&main, "export const start = () => 1;").expect("write main");

        let modules = build_module_tree(&route).expect("tree");

        assert!(
            modules.iter().any(|m| m.path.as_str() == "crate::main"),
            "template literal without substitutions is a static import target"
        );

        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn ignores_template_dynamic_imports_with_substitutions() {
        let root = mk_temp_project();
        let route = root.join("src").join("routes").join("index.tsx");
        let main = root.join("src").join("main.ts");
        std::fs::create_dir_all(route.parent().expect("parent")).expect("mkdir routes");
        std::fs::write(
            &route,
            "const suffix = ''; export function Route(){ void import(`../main${suffix}`); return null; }",
        )
        .expect("write route");
        std::fs::write(&main, "export const start = () => 1;").expect("write main");

        let modules = build_module_tree(&route).expect("tree");
        let edges = extract_edges(&modules);

        assert!(!modules.iter().any(|m| m.path.as_str() == "crate::main"));
        assert!(edges.is_empty());

        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn normalize_path_removes_current_directory_components() {
        assert_eq!(
            normalize_path(Path::new("src/./routes/../main.ts")),
            PathBuf::from("src/main.ts")
        );
    }
}
