//! Context-span extraction for TypeScript / JavaScript projects.
//!
//! Builds a file-level module graph using [`crate::coupling::build_module_tree`],
//! extracts relative import/re-export edges, then delegates transitive-closure
//! math to [`lens_domain::compute_context_spans`].

use std::path::Path;

use lens_domain::{ContextSpanReport, ModulePath, compute_context_spans, compute_report};

use crate::coupling::{CouplingError, build_module_tree, extract_edges};

/// Failures produced while extracting context spans from a TS/JS entry file.
#[derive(Debug, thiserror::Error)]
pub enum ContextSpanError {
    #[error(transparent)]
    Coupling(#[from] CouplingError),
}

/// Build the file graph rooted at `entry` and compute per-module context spans.
pub fn extract_context_spans(entry: &Path) -> Result<ContextSpanReport, ContextSpanError> {
    let modules = build_module_tree(entry)?;
    let module_paths: Vec<ModulePath> = modules.iter().map(|m| m.path.clone()).collect();
    let edges = extract_edges(&modules);
    let report = compute_report(&module_paths, edges);
    Ok(compute_context_spans(&module_paths, &report.edges))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_temp_project() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "lens_ts_context_span_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).expect("create temp project");
        base
    }

    fn span<'a>(report: &'a ContextSpanReport, path: &str) -> &'a lens_domain::ModuleContextSpan {
        report
            .modules
            .iter()
            .find(|m| m.path.as_str() == path)
            .expect("module present")
    }

    #[test]
    fn context_span_counts_transitive_dependencies_in_chain() {
        // main -> a -> b, so main reaches {a, b}.
        let root = mk_temp_project();
        let entry = root.join("src").join("main.ts");
        let a = root.join("src").join("a.ts");
        let b = root.join("src").join("b.ts");
        std::fs::create_dir_all(entry.parent().expect("parent")).expect("mkdir src");
        std::fs::write(&entry, "import { f } from './a'; export const n = f();")
            .expect("write main");
        std::fs::write(&a, "import { g } from './b'; export const f = () => g();")
            .expect("write a");
        std::fs::write(&b, "export const g = () => 1;").expect("write b");

        let report = extract_context_spans(&entry).expect("context span");

        let main = span(&report, "crate::main");
        assert_eq!(main.direct, 1);
        assert_eq!(main.transitive, 2);
        assert_eq!(main.reachable.len(), 2);
        assert!(main.reachable.iter().any(|m| m.as_str() == "crate::a"));
        assert!(main.reachable.iter().any(|m| m.as_str() == "crate::b"));

        let b = span(&report, "crate::b");
        assert_eq!(b.direct, 0);
        assert_eq!(b.transitive, 0);
        assert!(b.reachable.is_empty());

        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn cycle_does_not_include_self_in_reachable_set() {
        let root = mk_temp_project();
        let entry = root.join("src").join("main.ts");
        let a = root.join("src").join("a.ts");
        std::fs::create_dir_all(entry.parent().expect("parent")).expect("mkdir src");
        std::fs::write(
            &entry,
            "import { a } from './a'; export const main = () => a();",
        )
        .expect("write main");
        std::fs::write(
            &a,
            "import { main } from './main'; export const a = () => main();",
        )
        .expect("write a");

        let report = extract_context_spans(&entry).expect("context span");
        let main = span(&report, "crate::main");

        assert_eq!(main.direct, 1);
        assert_eq!(main.transitive, 1);
        assert_eq!(main.reachable, vec![ModulePath::new("crate::a")]);

        std::fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn missing_entry_surfaces_coupling_io_error() {
        let missing = std::path::Path::new("/definitely/does/not/exist.ts");
        let err = extract_context_spans(missing).expect_err("must fail");
        assert!(matches!(
            err,
            ContextSpanError::Coupling(CouplingError::Io { .. })
        ));
    }
}
