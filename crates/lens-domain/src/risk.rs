//! Language-agnostic churn × blast-radius risk scoring.
//!
//! [`crate::hotspot`] ranks `commits × cognitive_max`, which cannot tell
//! a file that is "hot but leaf" (low stakes) from one that is "hot and
//! load-bearing" (where a defect propagates). This module scores the
//! second question: how much of the codebase leans on the code being
//! edited, weighted by how often that code changes. Network measures of
//! a call graph predict defects better than intra-function complexity
//! metrics (Zimmermann & Nagappan, *Predicting Defects using Network
//! Analysis on Dependency Graphs*), so the complexity axis is replaced
//! by a centrality axis.
//!
//! The composite is a **rank product**: each file is ranked by churn and
//! ranked by centrality, and the two ranks are multiplied. Rank product
//! is deterministic and needs no scale normalisation — commit counts and
//! PageRank scores live in incomparable units, so multiplying the raw
//! values (the hotspot formulation) would let whichever axis happens to
//! have the wider spread dominate. Ranks are unitless, so neither axis
//! can.
//!
//! Unlike hotspot's score, **lower is riskier**: rank 1 is the top of an
//! axis, so the smallest product is the file at the top of both.
//!
//! This module owns the **join and the ranking** only. Producing the
//! inputs is left to the CLI: churn comes from git, centrality from the
//! static call graph.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::hotspot::FileChurn;

/// Per-file call-graph centrality rollup, keyed by a path relative to
/// the repo root — the same key space [`FileChurn`] uses, which is what
/// makes the join possible at all.
#[derive(Debug, Clone, PartialEq)]
pub struct FileCentrality {
    pub path: String,
    /// Functions this file contributes to the call graph.
    pub function_count: usize,
    /// Total LOC across those functions.
    pub loc: usize,
    /// Largest PageRank importance over the file's functions. This is
    /// the centrality axis: a file is as load-bearing as its most
    /// load-bearing member.
    pub pagerank_max: f64,
    /// Summed PageRank importance over the file's functions. Reported
    /// as a raw component and used only to break `pagerank_max` ties,
    /// where it separates "one hub" from "many small hubs".
    pub pagerank_sum: f64,
    /// Largest transitive resolved-caller count (VFI) over the file's
    /// functions, or `None` when the caller closure was not computed.
    pub vfi_max: Option<usize>,
    /// Summed transitive resolved-caller count over the file's
    /// functions, or `None` when the caller closure was not computed.
    pub vfi_sum: Option<usize>,
}

impl Eq for FileCentrality {}

/// One row of the risk report.
///
/// Every raw component that fed the composite travels with it: an
/// agent acting on a ranking needs to see *why* a file ranks high, or
/// the ranking reads as an oracle rather than as evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct RiskEntry {
    pub path: String,
    pub commits: u32,
    pub function_count: usize,
    pub loc: usize,
    pub pagerank_max: f64,
    pub pagerank_sum: f64,
    pub vfi_max: Option<usize>,
    pub vfi_sum: Option<usize>,
    /// 1-based competition rank by descending `commits`.
    pub churn_rank: usize,
    /// 1-based competition rank by descending `pagerank_max`, ties
    /// broken by descending `pagerank_sum`.
    pub centrality_rank: usize,
    /// `churn_rank × centrality_rank`. **Lower is riskier.**
    pub rank_product: u64,
}

impl Eq for RiskEntry {}

/// Merge churn and centrality into a single rank-product ranking.
///
/// The universe is the files that contributed functions to the call
/// graph. Churn-only paths from git history are dropped: a path with no
/// centrality has no second axis to rank on, and deleted, generated, and
/// non-source files would otherwise flood the report. A file present in
/// the graph but absent from the churn table simply has `commits = 0` —
/// it is new, or outside the `--since` window, and still ranks on
/// centrality.
///
/// Both ranks are **competition ranks** (`1, 2, 2, 4`) over the reported
/// files only, so they are relative to the analyzed scope and not
/// comparable across runs with different scopes.
///
/// The result is sorted by ascending `rank_product`, ties broken by
/// descending `commits`, then descending `pagerank_max`, then
/// lexicographic path (for determinism).
pub fn compute_risk(churn: Vec<FileChurn>, centrality: Vec<FileCentrality>) -> Vec<RiskEntry> {
    let mut churn_by_path: BTreeMap<String, u32> = BTreeMap::new();
    for c in churn {
        // Defensive, matching `compute_hotspots`: a path appearing twice
        // means the upstream collector double-counted, and the agent
        // should see the worst case rather than whichever row landed last.
        let entry = churn_by_path.entry(c.path).or_insert(0);
        *entry = (*entry).max(c.commits);
    }
    let mut centrality_by_path: BTreeMap<String, FileCentrality> = BTreeMap::new();
    for fc in centrality {
        centrality_by_path.insert(fc.path.clone(), fc);
    }

    let rows: Vec<(String, u32, FileCentrality)> = centrality_by_path
        .into_iter()
        .map(|(path, fc)| {
            let commits = churn_by_path.get(&path).copied().unwrap_or(0);
            (path, commits, fc)
        })
        .collect();

    let churn_ranks = competition_ranks(&rows, |a, b| a.1.cmp(&b.1));
    let centrality_ranks = competition_ranks(&rows, |a, b| {
        a.2.pagerank_max
            .total_cmp(&b.2.pagerank_max)
            .then_with(|| a.2.pagerank_sum.total_cmp(&b.2.pagerank_sum))
    });

    let mut out: Vec<RiskEntry> = rows
        .into_iter()
        .zip(churn_ranks)
        .zip(centrality_ranks)
        .map(
            |(((path, commits, fc), churn_rank), centrality_rank)| RiskEntry {
                path,
                commits,
                function_count: fc.function_count,
                loc: fc.loc,
                pagerank_max: fc.pagerank_max,
                pagerank_sum: fc.pagerank_sum,
                vfi_max: fc.vfi_max,
                vfi_sum: fc.vfi_sum,
                churn_rank,
                centrality_rank,
                rank_product: (churn_rank as u64).saturating_mul(centrality_rank as u64),
            },
        )
        .collect();

    out.sort_by(|a, b| {
        a.rank_product
            .cmp(&b.rank_product)
            .then_with(|| b.commits.cmp(&a.commits))
            .then_with(|| b.pagerank_max.total_cmp(&a.pagerank_max))
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

/// Standard competition ranks (`1, 2, 2, 4`) over `items`, highest key
/// first: `key` orders two items *ascending*, and the item that compares
/// greatest gets rank 1. Items that compare equal share the lowest rank
/// they span, so the ranking never depends on input order.
///
/// Returned ranks are parallel to `items`.
fn competition_ranks<T>(items: &[T], key: impl Fn(&T, &T) -> Ordering) -> Vec<usize> {
    let mut order: Vec<usize> = (0..items.len()).collect();
    order.sort_by(|&a, &b| key(&items[b], &items[a]));

    let mut ranks = vec![0usize; items.len()];
    let mut rank = 0usize;
    for (position, &idx) in order.iter().enumerate() {
        let ties_with_previous =
            position > 0 && key(&items[idx], &items[order[position - 1]]) == Ordering::Equal;
        if !ties_with_previous {
            rank = position + 1;
        }
        ranks[idx] = rank;
    }
    ranks
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    fn churn(path: &str, commits: u32) -> FileChurn {
        FileChurn {
            path: path.to_owned(),
            commits,
        }
    }

    fn centrality(path: &str, pagerank_max: f64) -> FileCentrality {
        FileCentrality {
            path: path.to_owned(),
            function_count: 1,
            loc: 10,
            pagerank_max,
            pagerank_sum: pagerank_max,
            vfi_max: Some(1),
            vfi_sum: Some(1),
        }
    }

    fn entry<'a>(entries: &'a [RiskEntry], path: &str) -> &'a RiskEntry {
        entries
            .iter()
            .find(|e| e.path == path)
            .unwrap_or_else(|| panic!("no entry for {path} in {entries:?}"))
    }

    #[test]
    fn rank_product_multiplies_the_two_axis_ranks() {
        // hot_leaf tops churn (rank 1) but sits at the bottom of
        // centrality (rank 3); hub is the reverse; both_ranks is second
        // on both axes, so 2*2 beats 1*3.
        let entries = compute_risk(
            vec![
                churn("hot_leaf.rs", 30),
                churn("both.rs", 20),
                churn("hub.rs", 10),
            ],
            vec![
                centrality("hot_leaf.rs", 0.01),
                centrality("both.rs", 0.05),
                centrality("hub.rs", 0.30),
            ],
        );
        assert_eq!(entry(&entries, "hot_leaf.rs").churn_rank, 1);
        assert_eq!(entry(&entries, "hot_leaf.rs").centrality_rank, 3);
        assert_eq!(entry(&entries, "hot_leaf.rs").rank_product, 3);
        assert_eq!(entry(&entries, "both.rs").rank_product, 4);
        assert_eq!(entry(&entries, "hub.rs").rank_product, 3);
    }

    #[test]
    fn hot_and_load_bearing_outranks_hot_but_leaf() {
        // The whole point of the analyzer: churn alone must not win.
        // `leaf` is the churn leader, `load_bearing` is a close second
        // on churn and the runaway centrality leader.
        let entries = compute_risk(
            vec![
                churn("leaf.rs", 40),
                churn("load_bearing.rs", 39),
                churn("quiet.rs", 1),
            ],
            vec![
                centrality("leaf.rs", 0.001),
                centrality("load_bearing.rs", 0.400),
                centrality("quiet.rs", 0.100),
            ],
        );
        assert_eq!(entries[0].path, "load_bearing.rs");
        assert_eq!(entries[0].rank_product, 2);
    }

    #[test]
    fn entries_are_sorted_by_ascending_rank_product() {
        let entries = compute_risk(
            vec![churn("a.rs", 9), churn("b.rs", 5), churn("c.rs", 1)],
            vec![
                centrality("a.rs", 0.9),
                centrality("b.rs", 0.5),
                centrality("c.rs", 0.1),
            ],
        );
        let products: Vec<u64> = entries.iter().map(|e| e.rank_product).collect();
        assert_eq!(products, [1, 4, 9]);
        assert_eq!(entries[0].path, "a.rs");
    }

    #[test]
    fn files_with_only_churn_are_dropped() {
        // No centrality means no second axis; a rank product over one
        // axis is just that axis, so the row would be misleading.
        let entries = compute_risk(vec![churn("deleted.rs", 50)], vec![]);
        assert!(entries.is_empty(), "got {entries:?}");
    }

    #[test]
    fn files_with_only_centrality_are_kept_with_zero_churn() {
        // A brand-new file, or one outside the --since window, still
        // ranks on centrality.
        let entries = compute_risk(vec![], vec![centrality("fresh.rs", 0.2)]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].commits, 0);
        assert_eq!(entries[0].churn_rank, 1);
        assert_eq!(entries[0].centrality_rank, 1);
        assert_eq!(entries[0].rank_product, 1);
    }

    #[test]
    fn duplicate_churn_entries_keep_the_larger_count() {
        let entries = compute_risk(
            vec![churn("a.rs", 3), churn("a.rs", 7)],
            vec![centrality("a.rs", 0.1)],
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].commits, 7);
    }

    #[test]
    fn tied_values_share_a_rank_and_skip_the_next() {
        // Competition ranking: 1, 2, 2, 4 — never 1, 2, 2, 3.
        let entries = compute_risk(
            vec![
                churn("a.rs", 5),
                churn("b.rs", 5),
                churn("c.rs", 5),
                churn("d.rs", 1),
            ],
            vec![
                centrality("a.rs", 0.4),
                centrality("b.rs", 0.3),
                centrality("c.rs", 0.2),
                centrality("d.rs", 0.1),
            ],
        );
        let ranks: Vec<usize> = ["a.rs", "b.rs", "c.rs", "d.rs"]
            .into_iter()
            .map(|p| entry(&entries, p).churn_rank)
            .collect();
        assert_eq!(ranks, [1, 1, 1, 4]);
    }

    #[test]
    fn pagerank_sum_breaks_pagerank_max_ties() {
        // Two files whose hottest function is equally important: the one
        // with more importance overall is the more central file.
        let many = FileCentrality {
            pagerank_sum: 0.9,
            ..centrality("many_hubs.rs", 0.3)
        };
        let one = FileCentrality {
            pagerank_sum: 0.3,
            ..centrality("one_hub.rs", 0.3)
        };
        let entries = compute_risk(
            vec![churn("many_hubs.rs", 1), churn("one_hub.rs", 1)],
            vec![many, one],
        );
        assert_eq!(entry(&entries, "many_hubs.rs").centrality_rank, 1);
        assert_eq!(entry(&entries, "one_hub.rs").centrality_rank, 2);
    }

    #[test]
    fn fully_tied_files_break_by_path_for_determinism() {
        let entries = compute_risk(
            vec![churn("b.rs", 4), churn("a.rs", 4)],
            vec![centrality("b.rs", 0.5), centrality("a.rs", 0.5)],
        );
        let paths: Vec<&str> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.rs", "b.rs"]);
        assert!(entries.iter().all(|e| e.rank_product == 1));
    }

    #[test]
    fn missing_vfi_travels_through_the_join() {
        let without = FileCentrality {
            vfi_max: None,
            vfi_sum: None,
            ..centrality("a.rs", 0.1)
        };
        let entries = compute_risk(vec![churn("a.rs", 1)], vec![without]);
        assert_eq!(entries[0].vfi_max, None);
        assert_eq!(entries[0].vfi_sum, None);
    }

    #[test]
    fn empty_input_produces_empty_output() {
        assert!(compute_risk(vec![], vec![]).is_empty());
    }

    #[rstest]
    #[case::ascending(vec![1u32, 2, 3], vec![3, 2, 1])]
    #[case::all_tied(vec![7u32, 7, 7], vec![1, 1, 1])]
    #[case::single(vec![4u32], vec![1])]
    #[case::empty(vec![], vec![])]
    #[case::ties_skip_ranks(vec![9u32, 9, 5, 5, 1], vec![1, 1, 3, 3, 5])]
    fn competition_ranks_follow_the_1224_convention(
        #[case] values: Vec<u32>,
        #[case] expected: Vec<usize>,
    ) {
        assert_eq!(competition_ranks(&values, u32::cmp), expected);
    }

    proptest! {
        /// The three invariants the composite rests on: ranks are a
        /// permutation-stable 1..=n labelling, equal keys are ranked
        /// equally, and a strictly greater key never gets a worse rank.
        #[test]
        fn competition_ranks_are_monotone_and_bounded(values in prop::collection::vec(0u32..20, 1..30)) {
            let ranks = competition_ranks(&values, u32::cmp);
            prop_assert_eq!(ranks.len(), values.len());
            prop_assert!(ranks.iter().all(|&r| (1..=values.len()).contains(&r)));
            prop_assert!(ranks.contains(&1), "the top key must be rank 1: {:?}", ranks);
            for (i, &vi) in values.iter().enumerate() {
                for (j, &vj) in values.iter().enumerate() {
                    match vi.cmp(&vj) {
                        Ordering::Equal => prop_assert_eq!(ranks[i], ranks[j]),
                        Ordering::Greater => prop_assert!(ranks[i] < ranks[j]),
                        Ordering::Less => prop_assert!(ranks[i] > ranks[j]),
                    }
                }
            }
        }

        /// Ranking is scope-relative but order-independent: shuffling the
        /// inputs must not move a single row.
        #[test]
        fn ranking_is_independent_of_input_order(
            commits in prop::collection::vec(0u32..50, 1..12),
            scores in prop::collection::vec(0.0f64..1.0, 1..12),
        ) {
            let n = commits.len().min(scores.len());
            let churns: Vec<FileChurn> = (0..n).map(|i| churn(&format!("f{i}.rs"), commits[i])).collect();
            let centralities: Vec<FileCentrality> =
                (0..n).map(|i| centrality(&format!("f{i}.rs"), scores[i])).collect();

            let forward = compute_risk(churns.clone(), centralities.clone());
            let reversed = compute_risk(
                churns.into_iter().rev().collect(),
                centralities.into_iter().rev().collect(),
            );
            prop_assert_eq!(forward, reversed);
        }
    }
}
