use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use lens_domain::SimilarCluster;
use serde::Serialize;

use super::{OwnedFunction, SimilarityComponents};

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
    pairs: Vec<PairView<'a>>,
}

impl<'a> ClusterView<'a> {
    pub(super) fn from_domain(
        corpus: &'a [OwnedFunction],
        cluster: SimilarCluster,
        pair_scores: &HashMap<(usize, usize), SimilarityComponents>,
    ) -> Self {
        let functions: Vec<FunctionRef<'a>> = cluster
            .members
            .iter()
            .filter_map(|i| corpus.get(*i).map(FunctionRef::from))
            .collect();
        let pairs = cluster_pair_views(corpus, &cluster.members, pair_scores);
        Self {
            size: functions.len(),
            min_similarity: cluster.min_similarity,
            max_similarity: cluster.max_similarity,
            functions,
            pairs,
        }
    }
}

fn cluster_pair_views<'a>(
    corpus: &'a [OwnedFunction],
    members: &[usize],
    pair_scores: &HashMap<(usize, usize), SimilarityComponents>,
) -> Vec<PairView<'a>> {
    let mut pairs = Vec::new();
    for (pos, &i) in members.iter().enumerate() {
        for &j in &members[pos + 1..] {
            let Some(components) = pair_scores.get(&sorted_pair_key(i, j)).copied() else {
                continue;
            };
            let Some(a) = corpus.get(i).map(FunctionRef::from) else {
                continue;
            };
            let Some(b) = corpus.get(j).map(FunctionRef::from) else {
                continue;
            };
            pairs.push(PairView {
                a,
                b,
                similarity: components.similarity,
                body_similarity: components.body_similarity,
                signature_similarity: components.signature_similarity,
                type_overlap: components.type_overlap,
                identifier_overlap: components.identifier_overlap,
            });
        }
    }
    pairs.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    pairs
}

fn sorted_pair_key(i: usize, j: usize) -> (usize, usize) {
    if i <= j { (i, j) } else { (j, i) }
}

#[derive(Debug, Clone, Copy, Serialize)]
struct FunctionRef<'a> {
    file: &'a str,
    name: &'a str,
    start_line: usize,
    end_line: usize,
    is_test: bool,
}

impl<'a> From<&'a OwnedFunction> for FunctionRef<'a> {
    fn from(f: &'a OwnedFunction) -> Self {
        Self {
            file: f.rel_path.as_str(),
            name: f.def.name.as_str(),
            start_line: f.def.start_line,
            end_line: f.def.end_line,
            is_test: f.is_test,
        }
    }
}

#[derive(Debug, Serialize)]
struct PairView<'a> {
    a: FunctionRef<'a>,
    b: FunctionRef<'a>,
    similarity: f64,
    body_similarity: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    signature_similarity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    type_overlap: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    identifier_overlap: Option<f64>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn owned_function(name: &str) -> OwnedFunction {
        OwnedFunction {
            file: PathBuf::from("lib.rs"),
            rel_path: "lib.rs".to_owned(),
            is_test: false,
            def: lens_domain::FunctionDef {
                name: name.to_owned(),
                start_line: 1,
                end_line: 5,
                is_test: false,
                signature: None,
                tree: lens_domain::TreeNode::leaf("Block"),
            },
        }
    }

    fn components(similarity: f64) -> SimilarityComponents {
        SimilarityComponents {
            similarity,
            body_similarity: similarity,
            signature_similarity: Some(similarity),
            type_overlap: Some(similarity),
            identifier_overlap: Some(similarity),
        }
    }

    #[test]
    fn cluster_pair_views_uses_all_unique_member_pairs_and_their_scores() {
        let corpus = vec![
            owned_function("alpha"),
            owned_function("beta"),
            owned_function("gamma"),
        ];
        let pair_scores = HashMap::from([
            ((0, 0), components(1.0)),
            ((0, 1), components(0.91)),
            ((0, 2), components(0.92)),
            ((1, 1), components(1.0)),
            ((1, 2), components(0.93)),
            ((2, 2), components(1.0)),
        ]);

        let pairs = cluster_pair_views(&corpus, &[0, 1, 2], &pair_scores);

        assert_eq!(pairs.len(), 3);
        assert!(pairs.iter().all(|pair| pair.a.name != pair.b.name));
        assert_eq!(
            pairs
                .iter()
                .map(|pair| (pair.a.name, pair.b.name, pair.similarity))
                .collect::<Vec<_>>(),
            vec![
                ("beta", "gamma", 0.93),
                ("alpha", "gamma", 0.92),
                ("alpha", "beta", 0.91),
            ],
        );
        assert_eq!(sorted_pair_key(2, 0), (0, 2));
    }
}
