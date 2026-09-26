//! Pair scoring: the body score from the selected method, the signature
//! component, the same-trait exemption, and the blend of the two into the
//! one number the threshold is compared against.

use lens_domain::{
    DistanceBound, TSEDOptions, TreeNode, calculate_tsed_with_subtree_sizes,
    edit_distance_lower_bound, signature_components, tsed_distance_cutoff, tsed_from_distance,
};
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

    /// Lowest body score that reaches `threshold` given the pair's actual
    /// signature score. Exact where [`Self::body_candidate_threshold`] is
    /// generous, because scoring already knows the signature.
    fn body_needed(self, threshold: f64, signature_similarity: f64) -> f64 {
        if self.body <= 0.0 {
            return f64::NEG_INFINITY;
        }
        (threshold - self.signature * signature_similarity) / self.body
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
            if let Some(score) =
                score_candidate_pair(corpus, profiles, i, j, opts, weights, threshold)
            {
                stats.record(score, threshold);
            }
            stats
        })
        .reduce(ScoreStats::default, ScoreStats::merge)
        .sorted()
}

/// Score one candidate pair under TSED.
///
/// Before paying for APTED, a pair whose traversal-string lower bound
/// already proves it cannot reach `threshold` is cut short: its body
/// score is the bound's upper estimate, below what the threshold needs,
/// so it is recorded as below threshold exactly as the full score would
/// have been. Pass `threshold <= 0` to always compute the exact score.
pub(super) fn score_candidate_pair(
    corpus: &[OwnedUnit],
    profiles: &[TreeProfile],
    i: usize,
    j: usize,
    opts: &TSEDOptions,
    weights: ScoreWeights,
    threshold: f64,
) -> Option<PairScore> {
    let a = corpus.get(i)?;
    let b = corpus.get(j)?;
    let profile_a = profiles.get(i)?;
    let profile_b = profiles.get(j)?;
    let compare_values = opts.apted.compare_values;
    let body_a = a.body_tree();
    let body_b = b.body_tree();
    let signature = signature_components(a.signature(), b.signature());
    let signature_similarity = signature.signature_similarity.unwrap_or(1.0);
    let same_trait = same_trait_pair(a, b);
    let weights = if same_trait {
        ScoreWeights::BODY_ONLY
    } else {
        weights
    };
    let exact_match =
        is_exact_match_without_distance(profile_a, profile_b, body_a, body_b, compare_values);
    let mut bound_pruned = false;
    let body_similarity = if exact_match {
        1.0
    } else if let Some(upper) = body_upper_bound(
        profile_a,
        profile_b,
        body_a,
        body_b,
        opts,
        weights.body_needed(threshold, signature_similarity),
    )
    .filter(|&upper| clearly_below_threshold(weights.blend(upper, signature_similarity), threshold))
    {
        bound_pruned = true;
        upper
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
        bound_pruned,
    })
}

/// Slack under the threshold an upper-bound score must clear before the
/// exact score is skipped, so floating-point rounding in the bound can
/// only ever keep a pair for the exact score, never drop one the exact
/// score would have kept.
const SCORE_SLACK: f64 = 1e-9;

/// Whether an upper bound on a pair's score rules the threshold out.
/// Listed in `.cargo/mutants.toml`'s `exclude_re`: the slack only moves
/// the verdict for scores within rounding error of the threshold, which
/// no fixture can pin.
fn clearly_below_threshold(upper_score: f64, threshold: f64) -> bool {
    upper_score < threshold - SCORE_SLACK
}

/// An upper bound on the pair's TSED body score from the traversal-string
/// lower bound on the edit distance ([`edit_distance_lower_bound`]),
/// when that lower bound already exceeds the distance `needed` allows.
/// `None` means the pair may still reach `needed` and must be scored
/// exactly.
fn body_upper_bound(
    profile_a: &TreeProfile,
    profile_b: &TreeProfile,
    body_a: &TreeNode,
    body_b: &TreeNode,
    opts: &TSEDOptions,
    needed: f64,
) -> Option<f64> {
    let (size_a, size_b) = (profile_a.size, profile_b.size);
    let cutoff = tsed_distance_cutoff(size_a, size_b, needed, opts)?;
    let compare_values = opts.apted.compare_values;
    let bound = edit_distance_lower_bound(
        profile_a.traversal(body_a, compare_values),
        profile_b.traversal(body_b, compare_values),
        &opts.apted,
        cutoff,
    );
    match bound {
        DistanceBound::Exceeds(lower) => Some(tsed_from_distance(size_a, size_b, lower, opts)),
        DistanceBound::Within(_) => None,
    }
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
        bound_pruned: false,
    })
}

#[derive(Debug)]
pub(super) struct PairScore {
    pub(super) i: usize,
    pub(super) j: usize,
    pub(super) components: SimilarityComponents,
    pub(super) exact_match: bool,
    /// The body score is an upper bound from the edit-distance lower
    /// bound, not an exact APTED score; only ever set on pairs that fall
    /// below the threshold.
    pub(super) bound_pruned: bool,
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
    /// Below-threshold pairs whose APTED run the edit-distance lower
    /// bound made unnecessary. A subset of `below_threshold_count`.
    pub(super) bound_pruned_count: usize,
    pub(super) diff_filtered_count: usize,
}

impl ScoreStats {
    pub(super) fn record(&mut self, score: PairScore, threshold: f64) {
        if score.exact_match {
            self.exact_match_count += 1;
        }
        if score.bound_pruned {
            self.bound_pruned_count += 1;
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
        a.bound_pruned_count += b.bound_pruned_count;
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
                bound_pruned: false,
            },
            0.85,
        );
        stats.record(
            PairScore {
                i: 0,
                j: 2,
                components: components(0.25),
                exact_match: false,
                bound_pruned: false,
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
                bound_pruned_count: 0,
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
                bound_pruned: false,
            },
            0.85,
        );

        assert_eq!(stats.pairs.len(), 1);
        assert_eq!(stats.below_threshold_count, 0);
    }

    #[test]
    fn score_stats_count_bound_pruned_pairs_through_record_and_merge() {
        let pruned = |i| PairScore {
            i,
            j: i + 1,
            components: SimilarityComponents {
                similarity: 0.1,
                body_similarity: 0.1,
                signature_similarity: None,
                type_overlap: None,
                identifier_overlap: None,
                doc_overlap: None,
                same_trait: false,
            },
            exact_match: false,
            bound_pruned: true,
        };
        let mut left = ScoreStats::default();
        left.record(pruned(0), 0.85);
        let mut right = ScoreStats::default();
        right.record(pruned(2), 0.85);
        right.record(pruned(4), 0.85);
        assert_eq!(left.bound_pruned_count, 1);
        let merged = ScoreStats::merge(left, right);
        assert_eq!(merged.bound_pruned_count, 3);
        assert_eq!(merged.below_threshold_count, 3);
    }

    #[test]
    fn body_needed_subtracts_the_weighted_signature_score() {
        let weights = ScoreWeights {
            body: 0.8,
            signature: 0.2,
        };
        // (0.85 - 0.2 * 0.5) / 0.8
        assert!((weights.body_needed(0.85, 0.5) - 0.9375).abs() < 1e-12);
        assert_eq!(
            ScoreWeights {
                body: 0.0,
                signature: 1.0
            }
            .body_needed(0.85, 0.5),
            f64::NEG_INFINITY
        );
    }

    #[test]
    fn sorted_pair_key_orders_indices() {
        assert_eq!(sorted_pair_key(5, 3), (3, 5));
    }
}
