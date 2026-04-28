use std::fmt::Write as _;
use std::path::Path;

use lens_domain::SimilarCluster;
use serde::Serialize;

use super::OwnedFunction;

#[derive(Debug, Serialize)]
pub(super) struct Report<'a> {
    /// Input path: a single source file, or the root directory walked.
    root: String,
    function_count: usize,
    threshold: f64,
    min_lines: usize,
    cluster_count: usize,
    clusters: &'a [ClusterView<'a>],
}

impl<'a> Report<'a> {
    pub(super) fn new(
        path: &Path,
        threshold: f64,
        min_lines: usize,
        function_count: usize,
        clusters: &'a [ClusterView<'a>],
    ) -> Self {
        Self {
            root: path.display().to_string(),
            function_count,
            threshold,
            min_lines,
            cluster_count: clusters.len(),
            clusters,
        }
    }
}

#[derive(Debug, Serialize)]
pub(super) struct ClusterView<'a> {
    size: usize,
    min_similarity: f64,
    max_similarity: f64,
    functions: Vec<FunctionRef<'a>>,
}

impl<'a> ClusterView<'a> {
    pub(super) fn from_domain(corpus: &'a [OwnedFunction], cluster: SimilarCluster) -> Self {
        let functions: Vec<FunctionRef<'a>> = cluster
            .members
            .iter()
            .filter_map(|i| corpus.get(*i).map(FunctionRef::from))
            .collect();
        Self {
            size: functions.len(),
            min_similarity: cluster.min_similarity,
            max_similarity: cluster.max_similarity,
            functions,
        }
    }
}

#[derive(Debug, Serialize)]
struct FunctionRef<'a> {
    file: &'a str,
    name: &'a str,
    start_line: usize,
    end_line: usize,
}

impl<'a> From<&'a OwnedFunction> for FunctionRef<'a> {
    fn from(f: &'a OwnedFunction) -> Self {
        Self {
            file: f.rel_path.as_str(),
            name: f.def.name.as_str(),
            start_line: f.def.start_line,
            end_line: f.def.end_line,
        }
    }
}

pub(super) fn format_markdown(report: &Report<'_>, top: Option<usize>) -> String {
    let mut out = format!(
        "# Similarity report: {} ({} function(s), threshold {:.2}, min lines {})\n",
        report.root, report.function_count, report.threshold, report.min_lines,
    );
    if report.clusters.is_empty() {
        out.push_str("\n_No similar function clusters at or above threshold._\n");
        return out;
    }
    let clusters = top.map_or(report.clusters, |limit| {
        &report.clusters[..report.clusters.len().min(limit)]
    });
    if let Some(limit) = top {
        let _ = writeln!(
            out,
            "\n## Top {} similar cluster(s) of {} total",
            limit.min(report.cluster_count),
            report.cluster_count
        );
    } else {
        let _ = writeln!(out, "\n## {} similar cluster(s)", report.cluster_count);
    }
    for cluster in clusters {
        // writeln! into a String cannot fail; the result is swallowed
        // deliberately rather than unwrapped to satisfy the workspace's
        // `unwrap_used` lint.
        let _ = writeln!(
            out,
            "\n- {} functions, similarity {:.0}–{:.0}%",
            cluster.size,
            cluster.min_similarity * 100.0,
            cluster.max_similarity * 100.0,
        );
        for f in &cluster.functions {
            let _ = writeln!(
                out,
                "  - {}:`{}` (L{}-{})",
                f.file, f.name, f.start_line, f.end_line,
            );
        }
    }
    out
}
