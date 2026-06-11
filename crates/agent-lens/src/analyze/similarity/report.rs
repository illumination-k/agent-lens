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
    /// Body-scoring algorithm used: `tsed` or `token`. Surfaced because
    /// the two methods are not on the same score scale.
    method: &'static str,
    function_count: usize,
    /// Clustering cut applied. In sweep mode this is the ladder floor;
    /// otherwise the plain `--threshold`.
    threshold: f64,
    /// Multi-threshold sweep ladder (ascending), when `--sweep` is active.
    /// Present so a consumer can see which rungs the per-cluster
    /// `survives_at_threshold` annotations were drawn from.
    #[serde(skip_serializing_if = "Option::is_none")]
    sweep: Option<Vec<f64>>,
    min_lines: usize,
    cluster_count: usize,
    clusters: &'a [ClusterView<'a>],
}

impl<'a> Report<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        path: &Path,
        method: &'static str,
        threshold: f64,
        min_lines: usize,
        function_count: usize,
        sweep: Option<&[f64]>,
        clusters: &'a [ClusterView<'a>],
    ) -> Self {
        Self {
            root: path.display().to_string(),
            method,
            function_count,
            threshold,
            sweep: sweep.map(<[f64]>::to_vec),
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
    /// Highest sweep rung at which this cluster survives intact, set only
    /// in `--sweep` mode. `None` (and omitted from JSON) otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    survives_at_threshold: Option<f64>,
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
            survives_at_threshold: None,
            functions,
            pairs,
        }
    }
}

/// Tag each cluster with the highest sweep rung at which its complete-link
/// structure survives intact, then re-rank so the tightest (most clone-like)
/// survival bands sort first. A complete-link cluster keeps every internal
/// pair `>= min_similarity`, so it stays whole at any rung `<= min_similarity`
/// and breaks above it; the survival rung is therefore the largest ladder
/// value not exceeding `min_similarity`. The ladder floor is `<=` every
/// reported cluster's `min_similarity` (clustering ran at that floor), so
/// `survives_at_threshold` is always populated in sweep mode.
pub(super) fn annotate_sweep_survival(clusters: &mut [ClusterView<'_>], ladder: &[f64]) {
    for cluster in clusters.iter_mut() {
        cluster.survives_at_threshold = highest_surviving_rung(cluster.min_similarity, ladder);
    }
    clusters.sort_by(|a, b| {
        b.survives_at_threshold
            .partial_cmp(&a.survives_at_threshold)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                b.max_similarity
                    .partial_cmp(&a.max_similarity)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(b.size.cmp(&a.size))
    });
}

/// Highest `ladder` rung not exceeding `min_similarity` (the tightest pair a
/// complete-link cluster holds). A small epsilon absorbs float drift so a
/// cluster whose `min_similarity` lands exactly on a rung still counts as
/// surviving it. `ladder` is ascending; returns `None` only when every rung
/// is above `min_similarity`, which the sweep floor rules out in practice.
fn highest_surviving_rung(min_similarity: f64, ladder: &[f64]) -> Option<f64> {
    ladder
        .iter()
        .rev()
        .find(|&&rung| min_similarity + 1e-9 >= rung)
        .copied()
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
            name: f.name(),
            start_line: f.start_line(),
            end_line: f.end_line(),
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
    let cut = match &report.sweep {
        Some(ladder) => format!("sweep {}", format_ladder(ladder)),
        None => format!("threshold {:.2}", report.threshold),
    };
    let mut out = format!(
        "# Similarity report: {} ({} method, {} function(s), {}, min lines {})\n",
        report.root, report.method, report.function_count, cut, report.min_lines,
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
        let survival_tag = cluster
            .survives_at_threshold
            .map(|rung| format!(" [survives ≥{rung:.2}]"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "\n- {} functions, similarity {:.0}–{:.0}%{}",
            cluster.size,
            cluster.min_similarity * 100.0,
            cluster.max_similarity * 100.0,
            survival_tag,
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

/// Render a sweep ladder as `[0.60, 0.75, 0.85]` for the markdown header.
fn format_ladder(ladder: &[f64]) -> String {
    let rungs: Vec<String> = ladder.iter().map(|r| format!("{r:.2}")).collect();
    format!("[{}]", rungs.join(", "))
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
            shape: lens_domain::FunctionShape::from(lens_domain::FunctionDef {
                name: name.to_owned(),
                start_line: 1,
                end_line: 5,
                is_test: false,
                signature: None,
                tree: lens_domain::TreeNode::leaf("Block"),
            }),
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

    /// Bare cluster carrying only the stats the sweep ranking reads. The
    /// `functions`/`pairs` vectors stay empty so the value is `'static` and
    /// the sort/annotation logic can be exercised in isolation.
    fn cluster(min_similarity: f64, max_similarity: f64, size: usize) -> ClusterView<'static> {
        ClusterView {
            size,
            min_similarity,
            max_similarity,
            survives_at_threshold: None,
            functions: Vec::new(),
            pairs: Vec::new(),
        }
    }

    #[test]
    fn highest_surviving_rung_picks_largest_rung_not_above_min_similarity() {
        let ladder = [0.6, 0.75, 0.85];
        // Above the top rung → survives the top.
        assert_eq!(highest_surviving_rung(0.97, &ladder), Some(0.85));
        // Between rungs → survives the rung just below.
        assert_eq!(highest_surviving_rung(0.79, &ladder), Some(0.75));
        // Exactly on a rung → epsilon keeps it surviving that rung.
        assert_eq!(highest_surviving_rung(0.85, &ladder), Some(0.85));
        // At the floor → survives the floor.
        assert_eq!(highest_surviving_rung(0.60, &ladder), Some(0.6));
        // Below every rung → no rung survives.
        assert_eq!(highest_surviving_rung(0.50, &ladder), None);
    }

    #[test]
    fn annotate_sweep_survival_tags_each_cluster_from_its_min_similarity() {
        let mut clusters = vec![cluster(0.97, 0.97, 2), cluster(0.79, 0.79, 2)];
        annotate_sweep_survival(&mut clusters, &[0.6, 0.75, 0.85]);
        let tags: Vec<Option<f64>> = clusters.iter().map(|c| c.survives_at_threshold).collect();
        assert_eq!(tags, vec![Some(0.85), Some(0.75)]);
    }

    #[test]
    fn annotate_sweep_survival_ranks_by_survival_rung_before_max_similarity() {
        // The looser cluster has the higher `max_similarity`; survival rung
        // must still win the sort, so it lands second despite the bigger max.
        let mut clusters = vec![
            cluster(0.70, 0.99, 2), // survives 0.6
            cluster(0.95, 0.95, 2), // survives 0.85
        ];
        annotate_sweep_survival(&mut clusters, &[0.6, 0.85]);
        assert_eq!(
            clusters
                .iter()
                .map(|c| c.survives_at_threshold)
                .collect::<Vec<_>>(),
            vec![Some(0.85), Some(0.6)],
        );
    }

    #[test]
    fn annotate_sweep_survival_breaks_survival_ties_by_max_then_size() {
        // All three survive the same rung (0.8). Input order is deliberately
        // scrambled so a passing assertion proves the max- then size-keyed
        // tiebreak actually ran.
        let mut clusters = vec![
            cluster(0.82, 0.86, 2), // tie on rung, lowest max
            cluster(0.81, 0.90, 2), // tie on rung, highest max
            cluster(0.83, 0.86, 3), // ties on rung+max with the first, larger
        ];
        annotate_sweep_survival(&mut clusters, &[0.6, 0.8]);
        let order: Vec<(f64, usize)> = clusters
            .iter()
            .map(|c| (c.max_similarity, c.size))
            .collect();
        assert_eq!(order, vec![(0.90, 2), (0.86, 3), (0.86, 2)]);
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
