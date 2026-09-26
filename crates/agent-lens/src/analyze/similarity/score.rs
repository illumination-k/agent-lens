//! Pair scoring: the body score from the selected method, the signature
//! component, the same-trait exemption, and the blend of the two into the
//! one number the threshold is compared against.

use lens_domain::{TSEDOptions, TreeNode, calculate_tsed_with_subtree_sizes, signature_components};
use rayon::prelude::*;

use super::candidates::TreeProfile;
use super::corpus::OwnedUnit;

/// Relative weight of the body and signature components in a pair's
/// combined score. Always sums to 1.
#[derive(Debug, Clone, Copy)]
pub(super) struct ScoreWeights {
    pub(super) body: f64,
    pub(super) signature: f64,
}

impl ScoreWeights {
    /// Body-only weights, for pairs whose signature match carries no
    /// information: two implementations of the same trait share a
    /// signature by construction, so blending it in would inflate every
    /// `impl Display` against every other `impl Display`.
    pub(super) const BODY_ONLY: Self = Self {
        body: 1.0,
        signature: 0.0,
    };

    pub(super) fn blend(self, body_similarity: f64, signature_similarity: f64) -> f64 {
        (self.body * body_similarity) + (self.signature * signature_similarity)
    }

    /// Lowest body score that could still reach `threshold` once the
    /// signature component is added at its most generous. Used to relax
    /// the cheap candidate filters without dropping a pair the full
    /// score would have kept.
    pub(super) fn body_candidate_threshold(self, threshold: f64) -> f64 {
        ((threshold - self.signature) / self.body).clamp(0.0, 1.0)
    }
}

pub(super) fn is_exact_match_without_distance(
    profile_a: &TreeProfile,
    profile_b: &TreeProfile,
    a: &TreeNode,
    b: &TreeNode,
    compare_values: bool,
) -> bool {
    if profile_a.size != profile_b.size {
        return false;
    }
    if profile_a.exact_hash(compare_values) != profile_b.exact_hash(compare_values) {
        return false;
    }
    trees_match_without_distance(a, b, compare_values)
}

pub(super) fn score_candidate_pairs(
    corpus: &[OwnedUnit],
    profiles: &[TreeProfile],
    pairs: &[(usize, usize)],
    threshold: f64,
    opts: &TSEDOptions,
    weights: ScoreWeights,
) -> ScoreStats {
    pairs
        .par_iter()
        .fold(ScoreStats::default, |mut stats, &(i, j)| {
            if let Some(score) = score_candidate_pair(corpus, profiles, i, j, opts, weights) {
                stats.record(score, threshold);
            }
            stats
        })
        .reduce(ScoreStats::default, ScoreStats::merge)
        .sorted()
}

pub(super) fn score_candidate_pair(
    corpus: &[OwnedUnit],
    profiles: &[TreeProfile],
    i: usize,
    j: usize,
    opts: &TSEDOptions,
    weights: ScoreWeights,
) -> Option<PairScore> {
    let a = corpus.get(i)?;
    let b = corpus.get(j)?;
    let profile_a = profiles.get(i)?;
    let profile_b = profiles.get(j)?;
    let compare_values = opts.apted.compare_values;
    let body_a = a.body_tree();
    let body_b = b.body_tree();
    let exact_match =
        is_exact_match_without_distance(profile_a, profile_b, body_a, body_b, compare_values);
    let body_similarity = if exact_match {
        1.0
    } else {
        let sizes_a = profile_a.subtree_sizes(body_a);
        let sizes_b = profile_b.subtree_sizes(body_b);
        calculate_tsed_with_subtree_sizes(
            body_a,
            body_b,
            profile_a.size,
            profile_b.size,
            sizes_a,
            sizes_b,
            opts,
        )
    };
    let signature = signature_components(a.signature(), b.signature());
    let signature_similarity = signature.signature_similarity.unwrap_or(1.0);
    let same_trait = same_trait_pair(a, b);
    let weights = if same_trait {
        ScoreWeights::BODY_ONLY
    } else {
        weights
    };
    Some(PairScore {
        i,
        j,
        components: SimilarityComponents {
            similarity: weights.blend(body_similarity, signature_similarity),
            body_similarity,
            signature_similarity: signature.signature_similarity,
            type_overlap: signature.type_overlap,
            identifier_overlap: signature.identifier_overlap,
            doc_overlap: None,
            same_trait,
        },
        exact_match,
    })
}

/// Whether both units implement the same method of the same trait
/// (`impl Display for A`'s `fmt` against `impl Display for B`'s `fmt`).
/// The trait dictates that pair's shared signature, so it is scored on
/// the body alone and the report carries the annotation. Different
/// methods of one trait are not exempt: the trait only fixes each
/// method's own signature, so between two visitor methods a signature
/// mismatch is real evidence and keeps its weight.
pub(super) fn same_trait_pair(a: &OwnedUnit, b: &OwnedUnit) -> bool {
    let (Some(left), Some(right)) = (a.implements(), b.implements()) else {
        return false;
    };
    left == right && bare_method_name(a.name()) == bare_method_name(b.name())
}

/// Last `::` segment of a display name (`Owner::method` → `method`).
fn bare_method_name(name: &str) -> &str {
    name.rsplit_once("::").map_or(name, |(_, last)| last)
}

/// Score `pairs` for a method whose body score comes from precomputed
/// per-unit profiles: `body_similarity(i, j)` is the method's own
/// comparison, and everything around it — the signature component, the
/// same-trait exemption, the blend — is shared with TSED scoring.
pub(super) fn score_profile_pairs(
    corpus: &[OwnedUnit],
    pairs: &[(usize, usize)],
    threshold: f64,
    weights: ScoreWeights,
    body_similarity: impl Fn(usize, usize) -> Option<f64> + Sync,
) -> ScoreStats {
    pairs
        .par_iter()
        .fold(ScoreStats::default, |mut stats, &(i, j)| {
            if let Some(score) = body_similarity(i, j)
                .and_then(|body| score_profile_pair(corpus, i, j, body, weights))
            {
                stats.record(score, threshold);
            }
            stats
        })
        .reduce(ScoreStats::default, ScoreStats::merge)
        .sorted()
}

pub(super) fn score_profile_pair(
    corpus: &[OwnedUnit],
    i: usize,
    j: usize,
    body_similarity: f64,
    weights: ScoreWeights,
) -> Option<PairScore> {
    let a = corpus.get(i)?;
    let b = corpus.get(j)?;
    let signature = signature_components(a.signature(), b.signature());
    let signature_similarity = signature.signature_similarity.unwrap_or(1.0);
    let same_trait = same_trait_pair(a, b);
    let weights = if same_trait {
        ScoreWeights::BODY_ONLY
    } else {
        weights
    };
    Some(PairScore {
        i,
        j,
        components: SimilarityComponents {
            similarity: weights.blend(body_similarity, signature_similarity),
            body_similarity,
            signature_similarity: signature.signature_similarity,
            type_overlap: signature.type_overlap,
            identifier_overlap: signature.identifier_overlap,
            doc_overlap: None,
            same_trait,
        },
        exact_match: body_similarity >= 1.0,
    })
}

#[derive(Debug)]
pub(super) struct PairScore {
    pub(super) i: usize,
    pub(super) j: usize,
    pub(super) components: SimilarityComponents,
    pub(super) exact_match: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct SimilarityComponents {
    pub(super) similarity: f64,
    pub(super) body_similarity: f64,
    pub(super) signature_similarity: Option<f64>,
    pub(super) type_overlap: Option<f64>,
    pub(super) identifier_overlap: Option<f64>,
    /// Word-level overlap of the two functions' doc comments. A
    /// diagnostic component only — it does not feed `similarity` — and
    /// filled by [`annotate_doc_overlap`] after threshold filtering so
    /// the scoring hot path never tokenizes doc prose. `None` unless
    /// both sides carry doc text.
    pub(super) doc_overlap: Option<f64>,
    /// Both sides implement the same method of the same trait, so the
    /// signature component was excluded from `similarity` (the trait
    /// dictates the match).
    pub(super) same_trait: bool,
}

pub(super) fn sorted_pair_key(i: usize, j: usize) -> (usize, usize) {
    if i <= j { (i, j) } else { (j, i) }
}

#[derive(Debug, Default)]
pub(super) struct ScoreStats {
    pub(super) pairs: Vec<ScoredPair>,
    pub(super) exact_match_count: usize,
    pub(super) below_threshold_count: usize,
    pub(super) diff_filtered_count: usize,
}

impl ScoreStats {
    pub(super) fn record(&mut self, score: PairScore, threshold: f64) {
        if score.exact_match {
            self.exact_match_count += 1;
        }
        if score.components.similarity < threshold {
            self.below_threshold_count += 1;
            return;
        }
        self.pairs.push(ScoredPair {
            i: score.i,
            j: score.j,
            components: score.components,
        });
    }

    pub(super) fn merge(mut a: Self, mut b: Self) -> Self {
        a.below_threshold_count += b.below_threshold_count;
        a.diff_filtered_count += b.diff_filtered_count;
        a.exact_match_count += b.exact_match_count;
        a.pairs.append(&mut b.pairs);
        a
    }

    pub(super) fn sorted(mut self) -> Self {
        self.pairs.sort_by_key(|pair| (pair.i, pair.j));
        self
    }

    pub(super) fn scored_pair_count(&self) -> usize {
        self.pairs.len() + self.below_threshold_count
    }
}

#[derive(Debug, Clone)]
pub(super) struct ScoredPair {
    pub(super) i: usize,
    pub(super) j: usize,
    pub(super) components: SimilarityComponents,
}

fn trees_match_without_distance(a: &TreeNode, b: &TreeNode, compare_values: bool) -> bool {
    a.label == b.label
        && (!compare_values || a.value == b.value)
        && a.children.len() == b.children.len()
        && a.children
            .iter()
            .zip(&b.children)
            .all(|(a, b)| trees_match_without_distance(a, b, compare_values))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn score_stats_record_and_merge_preserve_counts() {
        fn components(similarity: f64) -> SimilarityComponents {
            SimilarityComponents {
                similarity,
                body_similarity: similarity,
                signature_similarity: None,
                type_overlap: None,
                identifier_overlap: None,
                doc_overlap: None,
                same_trait: false,
            }
        }

        let mut stats = ScoreStats::default();
        stats.record(
            PairScore {
                i: 0,
                j: 1,
                components: components(1.0),
                exact_match: true,
            },
            0.85,
        );
        stats.record(
            PairScore {
                i: 0,
                j: 2,
                components: components(0.25),
                exact_match: false,
            },
            0.85,
        );

        let merged = ScoreStats::merge(
            stats,
            ScoreStats {
                pairs: vec![ScoredPair {
                    i: 2,
                    j: 3,
                    components: components(0.9),
                }],
                exact_match_count: 2,
                below_threshold_count: 3,
                diff_filtered_count: 4,
            },
        );

        let pairs: Vec<_> = merged
            .pairs
            .iter()
            .map(|pair| (pair.i, pair.j, pair.components.similarity))
            .collect();
        assert_eq!(pairs, vec![(0, 1, 1.0), (2, 3, 0.9)]);
        assert_eq!(merged.exact_match_count, 3);
        assert_eq!(merged.below_threshold_count, 4);
        assert_eq!(merged.diff_filtered_count, 4);
    }

    #[test]
    fn score_stats_keeps_scores_equal_to_threshold() {
        let mut stats = ScoreStats::default();
        stats.record(
            PairScore {
                i: 2,
                j: 4,
                components: SimilarityComponents {
                    similarity: 0.85,
                    body_similarity: 1.0,
                    signature_similarity: Some(0.25),
                    type_overlap: Some(0.0),
                    identifier_overlap: Some(0.5),
                    doc_overlap: None,
                    same_trait: false,
                },
                exact_match: false,
            },
            0.85,
        );

        assert_eq!(stats.pairs.len(), 1);
        assert_eq!(stats.below_threshold_count, 0);
    }

    #[test]
    fn sorted_pair_key_orders_indices() {
        assert_eq!(sorted_pair_key(5, 3), (3, 5));
    }

    #[test]
    fn scored_pair_count_adds_kept_and_below_threshold_pairs() {
        let stats = ScoreStats {
            pairs: vec![ScoredPair {
                i: 0,
                j: 1,
                components: SimilarityComponents {
                    similarity: 0.9,
                    body_similarity: 0.9,
                    signature_similarity: None,
                    type_overlap: None,
                    identifier_overlap: None,
                    doc_overlap: None,
                    same_trait: false,
                },
            }],
            exact_match_count: 0,
            below_threshold_count: 3,
            diff_filtered_count: 0,
        };
        assert_eq!(stats.scored_pair_count(), 4);
    }

    fn node(label: &str, value: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode::with_children(label, value, children)
    }

    fn sample() -> TreeNode {
        node(
            "Block",
            "",
            vec![node("Let", "x", vec![]), node("Return", "x", vec![])],
        )
    }

    #[rstest]
    #[case::identical(sample(), true, true)]
    #[case::label_differs(node("Expr", "", sample().children), false, false)]
    #[case::value_differs_compared(
        node("Block", "", vec![node("Let", "y", vec![]), node("Return", "x", vec![])]),
        true,
        false
    )]
    #[case::value_differs_ignored(
        node("Block", "", vec![node("Let", "y", vec![]), node("Return", "x", vec![])]),
        false,
        true
    )]
    #[case::fewer_children(node("Block", "", vec![node("Let", "x", vec![])]), false, false)]
    #[case::child_label_differs(
        node("Block", "", vec![node("Let", "x", vec![]), node("Break", "x", vec![])]),
        false,
        false
    )]
    fn trees_match_without_distance_compares_the_whole_tree(
        #[case] other: TreeNode,
        #[case] compare_values: bool,
        #[case] expected: bool,
    ) {
        assert_eq!(
            trees_match_without_distance(&sample(), &other, compare_values),
            expected
        );
    }
}
