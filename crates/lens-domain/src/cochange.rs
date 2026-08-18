//! Language-agnostic temporal (logical) coupling between files.
//!
//! Two files are *temporally coupled* when history says they tend to
//! change in the same commit. Following Gall et al.'s logical coupling,
//! each pair is described as an association rule over the commit
//! population: `support` is how many commits touched both, and
//! `confidence` is the conditional `P(b changed | a changed)`. The edge
//! is symmetric but the conditional is not — a test file that always
//! moves with its implementation is not the same finding as an
//! implementation that always drags its test along — so both directions
//! are reported.
//!
//! `lift` is the guard against the metric's most common false positive:
//! the two busiest files in a repository co-occur often simply because
//! each occurs often. Lift divides the observed co-occurrence by what
//! independence would predict, so `lift ≈ 1` means "these two are hot,
//! not coupled" and `lift > 1` means the pairing is more than chance.
//!
//! This module owns the **arithmetic** and the data shapes only.
//! Producing the input — asking git for per-commit file sets, following
//! renames, and mapping paths into the analyzer's path space — is the
//! CLI's job.
//!
//! Known limits, which callers should state in their own output:
//!
//! * A co-change edge says *that* two files changed together, never why
//!   or in which direction a dependency runs.
//! * Tangled commits inflate support, and squash-merge workflows make a
//!   whole branch look like one change.
//!   [`CoChangeThresholds::max_commit_files`] drops the worst offenders
//!   but is a blunt guard, not a fix.
//! * A short or shallow history produces a sparse graph. That is what
//!   [`CoChangeThresholds::min_support`] is for: an empty report is a
//!   better answer than a confident one drawn from two commits.

use std::collections::BTreeMap;

/// Default minimum co-change count for a pair to be reported.
pub const DEFAULT_MIN_SUPPORT: u32 = 3;

/// Default minimum for a pair's stronger confidence direction.
pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.5;

/// Default commit-size cap: commits touching more files than this are
/// dropped as tangled.
pub const DEFAULT_MAX_COMMIT_FILES: usize = 50;

/// One commit's touched file set, in the caller's path space.
///
/// Commits are supplied **newest first** — the order `git log` emits —
/// because that is what makes "how many commits ago did this pair last
/// move together" a single pass rather than a second traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitFiles {
    /// Commit date, formatted by the caller (`YYYY-MM-DD` from
    /// `git log --date=short`). Carried verbatim into the report so a
    /// dead pattern is visibly dead.
    pub date: String,
    /// Files the commit touched. Duplicates and ordering are tolerated:
    /// [`compute_cochange`] sorts and de-duplicates each set, so a path
    /// listed twice cannot double-count.
    pub files: Vec<String>,
}

/// Thresholds and guards applied while folding commits into pairs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CoChangeThresholds {
    /// Minimum `cochanges` (support) for a pair to be reported.
    pub min_support: u32,
    /// Minimum for a pair's *stronger* confidence direction. Gating on
    /// the weaker one would drop the asymmetric findings that are the
    /// most actionable — a config file that always moves with a
    /// migration, while the migration rarely moves with the config.
    pub min_confidence: f64,
    /// Commits touching more than this many files are excluded from the
    /// whole computation, not just from pairing: a squash merge or a
    /// repo-wide rename would otherwise couple everything to everything
    /// and dominate the ranking. Pair counting is quadratic in a
    /// commit's file count, so this doubles as the cost bound.
    pub max_commit_files: usize,
}

impl Default for CoChangeThresholds {
    fn default() -> Self {
        Self {
            min_support: DEFAULT_MIN_SUPPORT,
            min_confidence: DEFAULT_MIN_CONFIDENCE,
            max_commit_files: DEFAULT_MAX_COMMIT_FILES,
        }
    }
}

/// One reported file pair.
///
/// Every derived figure is printed next to the counts it came from, so
/// an agent can see whether a row is a real pattern (high support, high
/// confidence, lift well above 1) or an artifact of two busy files
/// (high support, lift near 1).
#[derive(Debug, Clone, PartialEq)]
pub struct CoChangePair {
    /// First path of the pair. Always lexicographically less than
    /// [`Self::b`], so a pair has one spelling.
    pub a: String,
    pub b: String,
    /// Commits that touched both files: the pair's support count.
    pub cochanges: u32,
    /// Commits that touched `a`, counted over the same population the
    /// support count was drawn from.
    pub commits_a: u32,
    /// Commits that touched `b`, likewise.
    pub commits_b: u32,
    /// `cochanges / commits_a` = P(b changed | a changed).
    pub confidence_a_to_b: f64,
    /// `cochanges / commits_b` = P(a changed | b changed).
    pub confidence_b_to_a: f64,
    /// Observed co-occurrence over what independence predicts:
    /// `(cochanges × commits) / (commits_a × commits_b)`. Near 1 means
    /// the pair is only as coupled as two files that busy would be by
    /// chance.
    pub lift: f64,
    /// Ranking score: `cochanges × max(confidence)`. Ranking on support
    /// alone puts the two busiest files in the repository on top of
    /// every report; ranking on confidence alone promotes pairs seen
    /// twice.
    pub score: f64,
    /// Date of the most recent commit touching both.
    pub last_cochange: String,
    /// How many counted commits back that was, `0` being the newest
    /// counted commit in the window. Commits dropped by
    /// [`CoChangeThresholds::max_commit_files`] are not counted here
    /// either, so this is a distance in the same population every other
    /// figure is drawn from.
    pub last_cochange_commits_ago: usize,
}

/// The folded co-change report.
#[derive(Debug, Clone, PartialEq)]
pub struct CoChangeReport {
    /// Commits that contributed: in the window, touching at least one
    /// file, and within the commit-size cap. This is the population
    /// every confidence and lift figure is computed against.
    pub commit_count: usize,
    /// Commits dropped by [`CoChangeThresholds::max_commit_files`].
    /// Reported rather than silently discarded: a repository whose
    /// workflow squashes every branch has most of its history here, and
    /// that explains a thin report better than the thin report does.
    pub skipped_commit_count: usize,
    /// Distinct files seen across the counted commits.
    pub file_count: usize,
    /// Pairs that co-changed at least once, before the thresholds. The
    /// difference between this and `pairs.len()` separates "this history
    /// has no coupling" from "the thresholds filtered it out".
    pub candidate_pair_count: usize,
    /// Surviving pairs, strongest first.
    pub pairs: Vec<CoChangePair>,
}

/// Accumulator for one pair while walking commits newest-first.
#[derive(Debug, Clone)]
struct PairAcc {
    cochanges: u32,
    /// Set once, on the first (therefore most recent) sighting.
    last_date: String,
    last_commits_ago: usize,
}

/// What the commit stream contains, before any threshold is applied:
/// the commit population, each file's commit count inside it, and the
/// raw support of every pair that ever moved together.
///
/// [`compute_cochange`] folds this straight into a ranked report and
/// keeps nothing a caller can query afterwards. The differential
/// analyzers need the opposite direction — *given* a pair the code
/// declares, how much history is behind it? — and there the pairs below
/// `min_support` are the interesting ones, which a ranked report has
/// dropped by construction.
#[derive(Debug, Clone, Default)]
pub struct CoChangeCounts {
    /// Commits touching each file, over the counted population only.
    per_file: BTreeMap<String, u32>,
    /// Co-change accumulator per pair, keyed with `a < b`.
    pairs: BTreeMap<(String, String), PairAcc>,
    /// Commits that contributed.
    counted: usize,
    /// Commits dropped by the size cap.
    skipped: usize,
}

/// How big the counted window was.
///
/// The four figures are one fact — the commit population every other
/// number in a report is drawn from — so they are read together rather
/// than one accessor at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoChangeTotals {
    /// Commits that contributed: in the window, touching at least one
    /// file, and within the commit-size cap.
    pub commit_count: usize,
    /// Commits dropped by [`CoChangeThresholds::max_commit_files`].
    pub skipped_commit_count: usize,
    /// Distinct files seen across the counted commits.
    pub file_count: usize,
    /// Pairs that co-changed at least once, before the thresholds.
    pub candidate_pair_count: usize,
}

impl CoChangeCounts {
    pub fn totals(&self) -> CoChangeTotals {
        CoChangeTotals {
            commit_count: self.counted,
            skipped_commit_count: self.skipped,
            file_count: self.per_file.len(),
            candidate_pair_count: self.pairs.len(),
        }
    }

    /// How many counted commits touched `file`. `0` for a file the
    /// window never saw — which is why a caller reasoning about "this
    /// pair never co-changed" has to check both sides moved at all.
    pub fn commits_for(&self, file: &str) -> u32 {
        self.per_file.get(file).copied().unwrap_or(0)
    }

    /// Raw support for a pair in either spelling, or `None` when the two
    /// never moved together inside the window.
    pub fn support(&self, a: &str, b: &str) -> Option<PairSupport<'_>> {
        let key = if a <= b {
            (a.to_owned(), b.to_owned())
        } else {
            (b.to_owned(), a.to_owned())
        };
        self.pairs.get(&key).map(|acc| PairSupport {
            cochanges: acc.cochanges,
            last_cochange: acc.last_date.as_str(),
            last_cochange_commits_ago: acc.last_commits_ago,
        })
    }
}

/// One pair's unfiltered history: how often it moved together, and when
/// it last did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PairSupport<'a> {
    pub cochanges: u32,
    pub last_cochange: &'a str,
    pub last_cochange_commits_ago: usize,
}

/// Fold per-commit file sets into ranked co-change pairs.
///
/// `commits` is newest-first. Commits with no files, and commits over
/// [`CoChangeThresholds::max_commit_files`], take no part: they neither
/// contribute pairs nor inflate the per-file counts, so `confidence` and
/// `lift` stay internally consistent — every figure in a row is drawn
/// from the same commit population, which is what
/// [`CoChangeReport::commit_count`] reports.
pub fn compute_cochange(commits: &[CommitFiles], thresholds: CoChangeThresholds) -> CoChangeReport {
    let counts = tally_cochange(commits, thresholds.max_commit_files);
    let totals = counts.totals();
    CoChangeReport {
        commit_count: totals.commit_count,
        skipped_commit_count: totals.skipped_commit_count,
        file_count: totals.file_count,
        candidate_pair_count: totals.candidate_pair_count,
        pairs: rank_cochange_pairs(&counts, thresholds),
    }
}

/// Count files and co-occurring pairs across the commit stream, keeping
/// the counts a caller can query rather than only the ranking they
/// support. This is the first half of [`compute_cochange`], and the two
/// see exactly the same commit population.
pub fn tally_cochange(commits: &[CommitFiles], max_commit_files: usize) -> CoChangeCounts {
    let tally = tally_commits(commits, max_commit_files);
    CoChangeCounts {
        per_file: tally
            .per_file
            .into_iter()
            .map(|(file, commits)| (file.to_owned(), commits))
            .collect(),
        pairs: tally
            .pairs
            .into_iter()
            .map(|((a, b), acc)| ((a.to_owned(), b.to_owned()), acc))
            .collect(),
        counted: tally.counted,
        skipped: tally.skipped,
    }
}

/// What one pass over the commit stream observed, borrowed from it.
#[derive(Debug)]
struct Tally<'a> {
    /// Commits touching each file, over the counted population only.
    per_file: BTreeMap<&'a str, u32>,
    /// Co-change accumulator per pair, keyed with `a < b`.
    pairs: BTreeMap<(&'a str, &'a str), PairAcc>,
    /// Commits that contributed.
    counted: usize,
    /// Commits dropped by the size cap.
    skipped: usize,
}

/// Count files and co-occurring pairs across the commit stream.
///
/// A commit with no files and a commit over `max_commit_files` are both
/// passed over entirely, so neither shows up in `per_file`: that is what
/// keeps every figure a row later reports drawn from one commit
/// population.
fn tally_commits(commits: &[CommitFiles], max_commit_files: usize) -> Tally<'_> {
    let mut tally = Tally {
        per_file: BTreeMap::new(),
        pairs: BTreeMap::new(),
        counted: 0,
        skipped: 0,
    };
    for commit in commits {
        let mut files: Vec<&str> = commit.files.iter().map(String::as_str).collect();
        files.sort_unstable();
        files.dedup();
        if files.is_empty() {
            continue;
        }
        if files.len() > max_commit_files {
            tally.skipped += 1;
            continue;
        }
        let commits_ago = tally.counted;
        tally.counted += 1;

        for file in &files {
            *tally.per_file.entry(file).or_insert(0) += 1;
        }
        // `files` is sorted, so this only ever visits pairs with `a < b`
        // and each pair has exactly one key.
        for (i, a) in files.iter().enumerate() {
            for b in files.iter().skip(i + 1) {
                let acc = tally.pairs.entry((a, b)).or_insert_with(|| PairAcc {
                    cochanges: 0,
                    // First sighting in a newest-first walk, so this is
                    // the pair's most recent co-change.
                    last_date: commit.date.clone(),
                    last_commits_ago: commits_ago,
                });
                acc.cochanges += 1;
            }
        }
    }
    tally
}

/// Score the tallied pairs and rank them, strongest first.
pub fn rank_cochange_pairs(
    counts: &CoChangeCounts,
    thresholds: CoChangeThresholds,
) -> Vec<CoChangePair> {
    let population = counts.counted as f64;
    let mut out: Vec<CoChangePair> = counts
        .pairs
        .iter()
        .filter(|(_, acc)| acc.cochanges >= thresholds.min_support)
        .filter_map(|((a, b), acc)| {
            // A pair only exists because some commit touched both, so
            // both per-file counts are at least `cochanges >= 1` and
            // neither denominator can be zero.
            let commits_a = *counts.per_file.get(a)?;
            let commits_b = *counts.per_file.get(b)?;
            let cochanges = f64::from(acc.cochanges);
            let confidence_a_to_b = cochanges / f64::from(commits_a);
            let confidence_b_to_a = cochanges / f64::from(commits_b);
            let strongest = confidence_a_to_b.max(confidence_b_to_a);
            if strongest < thresholds.min_confidence {
                return None;
            }
            Some(CoChangePair {
                a: a.clone(),
                b: b.clone(),
                cochanges: acc.cochanges,
                commits_a,
                commits_b,
                confidence_a_to_b,
                confidence_b_to_a,
                lift: (cochanges * population) / (f64::from(commits_a) * f64::from(commits_b)),
                score: cochanges * strongest,
                last_cochange: acc.last_date.clone(),
                last_cochange_commits_ago: acc.last_commits_ago,
            })
        })
        .collect();

    out.sort_by(|x, y| {
        y.score
            .total_cmp(&x.score)
            .then_with(|| y.cochanges.cmp(&x.cochanges))
            .then_with(|| x.a.cmp(&y.a))
            .then_with(|| x.b.cmp(&y.b))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// Commits are written newest-first, the order `git log` emits.
    fn commit(date: &str, files: &[&str]) -> CommitFiles {
        CommitFiles {
            date: date.to_owned(),
            files: files.iter().map(|f| (*f).to_owned()).collect(),
        }
    }

    /// Thresholds that keep every observed pair, so a test asserting on
    /// arithmetic is not also asserting on filtering.
    fn open() -> CoChangeThresholds {
        CoChangeThresholds {
            min_support: 1,
            min_confidence: 0.0,
            max_commit_files: 50,
        }
    }

    fn find<'a>(report: &'a CoChangeReport, a: &str, b: &str) -> &'a CoChangePair {
        report
            .pairs
            .iter()
            .find(|p| p.a == a && p.b == b)
            .unwrap_or_else(|| panic!("no {a}/{b} pair in {report:?}"))
    }

    #[test]
    fn support_confidence_and_lift_follow_the_definitions() {
        // a: 4 commits, b: 2 commits, both together twice, 5 commits.
        let commits = vec![
            commit("2026-05-05", &["a", "b"]),
            commit("2026-05-04", &["a", "b"]),
            commit("2026-05-03", &["a"]),
            commit("2026-05-02", &["a"]),
            commit("2026-05-01", &["c"]),
        ];
        let report = compute_cochange(&commits, open());
        assert_eq!(report.commit_count, 5);
        assert_eq!(report.file_count, 3);
        let pair = find(&report, "a", "b");
        assert_eq!(pair.cochanges, 2);
        assert_eq!(pair.commits_a, 4);
        assert_eq!(pair.commits_b, 2);
        assert!((pair.confidence_a_to_b - 0.5).abs() < 1e-9, "{pair:?}");
        assert!((pair.confidence_b_to_a - 1.0).abs() < 1e-9, "{pair:?}");
        // (2 * 5) / (4 * 2) = 1.25
        assert!((pair.lift - 1.25).abs() < 1e-9, "{pair:?}");
        // 2 * max(0.5, 1.0)
        assert!((pair.score - 2.0).abs() < 1e-9, "{pair:?}");
    }

    /// The whole point of `lift`: two files that change constantly
    /// co-occur constantly, and the report has to be able to say so.
    #[test]
    fn two_busy_files_that_only_overlap_by_chance_have_lift_near_one() {
        // Every commit touches `hot`; half of them touch `warm`, which
        // never changes without also touching `hot`. Confidence
        // warm→hot is 1.0, but the pairing carries no information.
        let mut commits = Vec::new();
        for i in 0..10 {
            let files: Vec<&str> = if i % 2 == 0 {
                vec!["hot", "warm"]
            } else {
                vec!["hot"]
            };
            commits.push(commit("2026-05-01", &files));
        }
        let report = compute_cochange(&commits, open());
        let pair = find(&report, "hot", "warm");
        assert_eq!(pair.cochanges, 5);
        assert!((pair.lift - 1.0).abs() < 1e-9, "{pair:?}");
    }

    #[test]
    fn a_pair_that_never_changes_apart_lifts_above_one() {
        let commits = vec![
            commit("2026-05-03", &["a", "b"]),
            commit("2026-05-02", &["a", "b"]),
            commit("2026-05-01", &["c"]),
        ];
        let report = compute_cochange(&commits, open());
        let pair = find(&report, "a", "b");
        assert!(pair.lift > 1.0, "{pair:?}");
        assert!((pair.confidence_a_to_b - 1.0).abs() < 1e-9, "{pair:?}");
        assert!((pair.confidence_b_to_a - 1.0).abs() < 1e-9, "{pair:?}");
    }

    #[test]
    fn ranking_is_support_times_the_stronger_confidence() {
        // `wide` co-changes more often but weakly in both directions;
        // `tight` co-changes less often but always together.
        let mut commits = vec![
            commit("2026-05-01", &["tight_a", "tight_b"]),
            commit("2026-05-01", &["tight_a", "tight_b"]),
            commit("2026-05-01", &["tight_a", "tight_b"]),
        ];
        for _ in 0..4 {
            commits.push(commit("2026-05-01", &["wide_a", "wide_b"]));
        }
        for _ in 0..20 {
            commits.push(commit("2026-05-01", &["wide_a"]));
            commits.push(commit("2026-05-01", &["wide_b"]));
        }
        let report = compute_cochange(&commits, open());
        assert_eq!(
            (report.pairs[0].a.as_str(), report.pairs[0].b.as_str()),
            ("tight_a", "tight_b"),
            "support alone would rank the wide pair first: {report:?}",
        );
    }

    #[test]
    fn the_most_recent_cochange_is_dated_and_counted_back_from_head() {
        let commits = vec![
            commit("2026-05-09", &["x"]),
            commit("2026-05-08", &["x"]),
            commit("2026-05-07", &["a", "b"]),
            commit("2026-05-06", &["a", "b"]),
        ];
        let report = compute_cochange(&commits, open());
        let pair = find(&report, "a", "b");
        assert_eq!(pair.last_cochange, "2026-05-07");
        assert_eq!(pair.last_cochange_commits_ago, 2);
    }

    /// The commit-size cap has to remove the commit from every figure,
    /// not only from pairing: a tangled commit that still bumped
    /// `commits_a` would deflate every confidence involving `a`.
    #[test]
    fn an_oversized_commit_contributes_to_nothing() {
        let commits = vec![
            commit("2026-05-02", &["a", "b", "c", "d"]),
            commit("2026-05-01", &["a", "b"]),
        ];
        let thresholds = CoChangeThresholds {
            max_commit_files: 3,
            ..open()
        };
        let report = compute_cochange(&commits, thresholds);
        assert_eq!(report.commit_count, 1);
        assert_eq!(report.skipped_commit_count, 1);
        assert_eq!(report.file_count, 2);
        let pair = find(&report, "a", "b");
        assert_eq!(pair.cochanges, 1);
        assert_eq!(pair.commits_a, 1);
        assert!((pair.confidence_a_to_b - 1.0).abs() < 1e-9, "{pair:?}");
        assert!(
            report.pairs.iter().all(|p| p.a != "c" && p.b != "c"),
            "got {report:?}",
        );
    }

    /// `max_commit_files` drops commits with *more* files than the cap, so
    /// a commit sitting exactly on it still counts. An off-by-one here
    /// silently redefines the documented flag and quietly shrinks the
    /// commit population every confidence figure is drawn from.
    #[test]
    fn a_commit_exactly_at_the_size_cap_is_counted() {
        let commits = vec![commit("2026-05-01", &["a", "b", "c"])];
        let report = compute_cochange(
            &commits,
            CoChangeThresholds {
                max_commit_files: 3,
                ..open()
            },
        );
        assert_eq!(report.commit_count, 1, "got {report:?}");
        assert_eq!(report.skipped_commit_count, 0, "got {report:?}");
        assert_eq!(report.pairs.len(), 3, "got {report:?}");
    }

    /// `min_confidence` is a minimum, so a pair sitting exactly on it is
    /// reported rather than dropped.
    #[test]
    fn a_pair_exactly_at_the_confidence_floor_is_kept() {
        // Each file changes twice and they overlap once, so both
        // conditionals are exactly 0.5.
        let commits = vec![
            commit("2026-05-03", &["a", "b"]),
            commit("2026-05-02", &["a"]),
            commit("2026-05-01", &["b"]),
        ];
        let report = compute_cochange(
            &commits,
            CoChangeThresholds {
                min_support: 1,
                min_confidence: 0.5,
                max_commit_files: 50,
            },
        );
        let pair = find(&report, "a", "b");
        assert!((pair.confidence_a_to_b - 0.5).abs() < 1e-9, "{pair:?}");
        assert!((pair.confidence_b_to_a - 0.5).abs() < 1e-9, "{pair:?}");
    }

    #[test]
    fn thresholds_filter_but_the_candidate_count_still_reports_what_was_there() {
        let commits = vec![
            commit("2026-05-03", &["a", "b"]),
            commit("2026-05-02", &["a", "b"]),
            commit("2026-05-01", &["c", "d"]),
        ];
        let report = compute_cochange(
            &commits,
            CoChangeThresholds {
                min_support: 2,
                ..open()
            },
        );
        assert_eq!(report.candidate_pair_count, 2);
        assert_eq!(report.pairs.len(), 1);
        assert_eq!(report.pairs[0].a, "a");
    }

    /// Gating on the weaker direction would drop exactly the asymmetric
    /// pairs worth reading — so the gate is on the stronger one.
    #[test]
    fn confidence_gating_reads_the_stronger_direction() {
        let mut commits = vec![commit("2026-05-01", &["impl", "config"])];
        for _ in 0..9 {
            commits.push(commit("2026-05-01", &["impl"]));
        }
        // confidence impl→config = 0.1, config→impl = 1.0.
        let kept = compute_cochange(
            &commits,
            CoChangeThresholds {
                min_support: 1,
                min_confidence: 0.9,
                max_commit_files: 50,
            },
        );
        assert_eq!(kept.pairs.len(), 1, "got {kept:?}");
        let dropped = compute_cochange(
            &commits,
            CoChangeThresholds {
                min_support: 1,
                min_confidence: 1.01,
                max_commit_files: 50,
            },
        );
        assert!(dropped.pairs.is_empty(), "got {dropped:?}");
    }

    #[test]
    fn a_path_repeated_inside_one_commit_cannot_double_count() {
        let commits = vec![commit("2026-05-01", &["a", "b", "a"])];
        let report = compute_cochange(&commits, open());
        assert_eq!(report.file_count, 2);
        let pair = find(&report, "a", "b");
        assert_eq!(pair.cochanges, 1);
        assert_eq!(pair.commits_a, 1);
    }

    #[test]
    fn pair_keys_are_ordered_regardless_of_the_order_files_arrive_in() {
        let report = compute_cochange(&[commit("2026-05-01", &["z", "a"])], open());
        assert_eq!(report.pairs.len(), 1);
        assert_eq!(report.pairs[0].a, "a");
        assert_eq!(report.pairs[0].b, "z");
    }

    #[rstest]
    #[case::no_commits(vec![])]
    #[case::only_empty_commits(vec![commit("2026-05-01", &[])])]
    #[case::single_file_commits(vec![commit("2026-05-01", &["a"]), commit("2026-05-02", &["b"])])]
    fn nothing_to_pair_yields_an_empty_report(#[case] commits: Vec<CommitFiles>) {
        let report = compute_cochange(&commits, open());
        assert!(report.pairs.is_empty(), "got {report:?}");
        assert_eq!(report.candidate_pair_count, 0);
    }

    /// An empty commit is not a dropped commit: it says nothing about
    /// the thresholds, so counting it as skipped would report a guard
    /// that never fired.
    #[test]
    fn an_empty_commit_is_neither_counted_nor_skipped() {
        let report = compute_cochange(&[commit("2026-05-01", &[])], open());
        assert_eq!(report.commit_count, 0);
        assert_eq!(report.skipped_commit_count, 0);
    }

    /// The counts a differential caller reads must describe the same
    /// commit population the ranked report was drawn from, or a "never
    /// co-changed" verdict is measured against a different history than
    /// the co-change rows next to it.
    #[test]
    fn the_tally_and_the_ranked_report_agree_on_the_population() {
        let commits = vec![
            commit("2026-05-04", &["a", "b", "c", "d"]),
            commit("2026-05-03", &["a", "b"]),
            commit("2026-05-02", &["a"]),
            commit("2026-05-01", &["c"]),
        ];
        let thresholds = CoChangeThresholds {
            max_commit_files: 3,
            ..open()
        };
        let counts = tally_cochange(&commits, thresholds.max_commit_files);
        let report = compute_cochange(&commits, thresholds);
        assert_eq!(
            counts.totals(),
            CoChangeTotals {
                commit_count: report.commit_count,
                skipped_commit_count: report.skipped_commit_count,
                file_count: report.file_count,
                candidate_pair_count: report.candidate_pair_count,
            },
        );
        assert_eq!(rank_cochange_pairs(&counts, thresholds), report.pairs);
    }

    #[test]
    fn per_file_counts_answer_for_files_the_window_never_saw() {
        let counts = tally_cochange(
            &[
                commit("2026-05-02", &["a", "b"]),
                commit("2026-05-01", &["a"]),
            ],
            50,
        );
        assert_eq!(counts.commits_for("a"), 2);
        assert_eq!(counts.commits_for("b"), 1);
        assert_eq!(counts.commits_for("never-touched"), 0);
    }

    /// The support lookup is what a declared dependency is checked
    /// against, and a caller holding a static edge has no reason to
    /// spell it in the pair's canonical order — so both spellings must
    /// answer, and a pair that never moved together must answer `None`
    /// rather than zero-valued support.
    #[rstest]
    #[case::canonical_order("a", "b")]
    #[case::reversed("b", "a")]
    fn pair_support_reads_in_either_spelling(#[case] first: &str, #[case] second: &str) {
        let counts = tally_cochange(
            &[
                commit("2026-05-03", &["a", "b"]),
                commit("2026-05-02", &["a", "b"]),
                commit("2026-05-01", &["c"]),
            ],
            50,
        );
        let support = counts.support(first, second).expect("the pair co-changed");
        assert_eq!(support.cochanges, 2);
        assert_eq!(support.last_cochange, "2026-05-03");
        assert_eq!(support.last_cochange_commits_ago, 0);
        assert!(counts.support("a", "c").is_none());
    }
}
