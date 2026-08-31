//! Context-span aggregation for Python module graphs.
//!
//! This layer wires the Python coupling extractor into
//! [`lens_domain::compute_context_spans`]. It intentionally reuses
//! [`lens_domain::compute_report`] first so duplicate edges and self-loops are
//! normalized exactly the same way as other language adapters.

use lens_domain::{ContextSpanReport, compute_context_spans, compute_report};

use crate::coupling::{PythonModule, extract_edges};

/// Compute context spans for already-parsed Python modules.
///
/// `direct` counts each module's unique outgoing neighbors, and `transitive`
/// counts unique modules reachable by one-or-more outgoing steps while
/// excluding the module itself.
///
/// Kept as the adapter's context-span entry point, mirroring
/// `lens_ts::extract_context_spans`: `analyze context-span` composes the
/// coupling primitives itself, so the tests below are this function's
/// in-tree callers.
pub fn extract_context_spans(modules: &[PythonModule]) -> ContextSpanReport {
    let module_paths = modules.iter().map(|m| m.path.clone()).collect::<Vec<_>>();
    let report = compute_report(&module_paths, extract_edges(modules));
    compute_context_spans(&module_paths, &report.edges)
}

#[cfg(test)]
mod tests {
    use lens_domain::ModulePath;

    use super::extract_context_spans;
    use crate::coupling::{CouplingError, build_module_tree};

    fn build_context_span_report(
        root: &std::path::Path,
    ) -> Result<lens_domain::ContextSpanReport, CouplingError> {
        Ok(extract_context_spans(&build_module_tree(root)?))
    }

    fn span<'a>(
        report: &'a lens_domain::ContextSpanReport,
        path: &str,
    ) -> &'a lens_domain::ModuleContextSpan {
        report
            .modules
            .iter()
            .find(|m| m.path == ModulePath::new(path))
            .expect("span")
    }

    #[test]
    fn computes_direct_and_transitive_for_chain() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("a.py"), "").expect("a");
        std::fs::write(root.path().join("b.py"), "import a\n").expect("b");
        std::fs::write(root.path().join("c.py"), "import b\n").expect("c");

        let report = build_context_span_report(root.path()).expect("report");

        let c = span(&report, "crate::c");
        assert_eq!(c.direct, 1);
        assert_eq!(c.transitive, 2);
        assert_eq!(
            c.reachable,
            vec![ModulePath::new("crate::a"), ModulePath::new("crate::b")]
        );

        let b = span(&report, "crate::b");
        assert_eq!(b.direct, 1);
        assert_eq!(b.transitive, 1);
        assert_eq!(b.reachable, vec![ModulePath::new("crate::a")]);

        let a = span(&report, "crate::a");
        assert_eq!(a.direct, 0);
        assert_eq!(a.transitive, 0);
        assert!(a.reachable.is_empty());
    }

    #[test]
    fn excludes_self_from_transitive_in_cycle() {
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::write(root.path().join("a.py"), "import b\n").expect("a");
        std::fs::write(root.path().join("b.py"), "import a\n").expect("b");

        let report = build_context_span_report(root.path()).expect("report");

        let a = span(&report, "crate::a");
        assert_eq!(a.direct, 1);
        assert_eq!(a.transitive, 1);
        assert_eq!(a.reachable, vec![ModulePath::new("crate::b")]);

        let b = span(&report, "crate::b");
        assert_eq!(b.direct, 1);
        assert_eq!(b.transitive, 1);
        assert_eq!(b.reachable, vec![ModulePath::new("crate::a")]);
    }
}
