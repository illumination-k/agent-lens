//! Context-span aggregation for Go module graphs.
//!
//! Wires the Go coupling extractor into [`lens_domain::compute_context_spans`].
//! Reuses [`lens_domain::compute_report`] first so duplicate edges and
//! self-loops are normalized exactly the same way as the other language
//! adapters.

use std::path::Path;

use lens_domain::{ContextSpanReport, compute_context_spans, compute_report};

use crate::coupling::{CouplingError, GoPackage, build_module_tree, extract_edges};

/// Compute context spans for already-parsed Go packages.
///
/// `direct` counts each package's unique outgoing neighbors, and
/// `transitive` counts unique packages reachable by one-or-more outgoing
/// steps while excluding the package itself.
pub fn extract_context_spans(packages: &[GoPackage]) -> ContextSpanReport {
    let module_paths = packages.iter().map(|p| p.path.clone()).collect::<Vec<_>>();
    let report = compute_report(&module_paths, extract_edges(packages));
    compute_context_spans(&module_paths, &report.edges)
}

/// Build a Go package tree from `root` and compute its context spans.
///
/// `root` must be a `.go` file or a directory containing Go files
/// (typically a Go module rooted at `go.mod`).
pub fn build_context_span_report(root: &Path) -> Result<ContextSpanReport, CouplingError> {
    let modules = build_module_tree(root)?;
    Ok(extract_context_spans(&modules))
}

#[cfg(test)]
mod tests {
    use lens_domain::ModulePath;

    use super::build_context_span_report;

    fn write(root: &std::path::Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&path, contents).expect("write");
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
        // a → b → c (where main = crate::a, b = crate::b, c = crate::c).
        let root = tempfile::tempdir().expect("tempdir");
        write(root.path(), "go.mod", "module github.com/x/proj\n");
        write(
            root.path(),
            "a/a.go",
            "package a\n\nimport \"github.com/x/proj/b\"\n\nvar _ = b.X\n",
        );
        write(
            root.path(),
            "b/b.go",
            "package b\n\nimport \"github.com/x/proj/c\"\n\nvar X = c.Y\n",
        );
        write(root.path(), "c/c.go", "package c\n\nvar Y = 1\n");

        let report = build_context_span_report(root.path()).expect("report");

        let a = span(&report, "crate::a");
        assert_eq!(a.direct, 1);
        assert_eq!(a.transitive, 2);
        assert_eq!(
            a.reachable,
            vec![ModulePath::new("crate::b"), ModulePath::new("crate::c")]
        );

        let c = span(&report, "crate::c");
        assert_eq!(c.direct, 0);
        assert_eq!(c.transitive, 0);
        assert!(c.reachable.is_empty());
    }

    #[test]
    fn excludes_self_from_transitive_in_cycle() {
        let root = tempfile::tempdir().expect("tempdir");
        write(root.path(), "go.mod", "module github.com/x/proj\n");
        write(
            root.path(),
            "a/a.go",
            "package a\n\nimport \"github.com/x/proj/b\"\n\nvar _ = b.X\n",
        );
        write(
            root.path(),
            "b/b.go",
            "package b\n\nimport \"github.com/x/proj/a\"\n\nvar X = a.Y\n\nvar Y = 1\n",
        );

        let report = build_context_span_report(root.path()).expect("report");

        let a = span(&report, "crate::a");
        assert_eq!(a.direct, 1);
        assert_eq!(a.transitive, 1);
        assert_eq!(a.reachable, vec![ModulePath::new("crate::b")]);
    }
}
