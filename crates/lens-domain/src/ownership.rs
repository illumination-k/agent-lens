//! Code ownership from git history: how concentrated each file's
//! authorship is, and how many contributors touched it only marginally.
//!
//! Bird et al. (2011) split ownership into two dimensions — the share of
//! a file's commits its top author made, and the number of *minor*
//! contributors below a small share — and found the second one the more
//! defect-predictive. Both are computed here from commits whose author
//! identities the caller has already normalised (mailmap, bot filtering,
//! `Co-authored-by:` trailers). That normalisation is the hard part and
//! lives with the git reader; this module is the arithmetic.
//!
//! # Attribution rule
//!
//! Every counted commit carries one unit of credit per file it touched,
//! split evenly across its distinct authors. A solo commit gives its
//! author 1; a commit with one `Co-authored-by:` trailer gives each of
//! the two 0.5. Shares therefore sum to 1 per file, and a co-authored
//! commit neither counts twice nor erases the co-author.
//!
//! # Ranking
//!
//! `score = commits × (1 − top_author_share)`: low ownership weighted by
//! churn. A file nobody owns but nobody touches scores low, and a file
//! one author wrote alone scores zero however busy it is.

use std::collections::BTreeMap;

/// Default share of a file's commits below which a contributor is
/// *minor*, after Bird et al.
pub const DEFAULT_MINOR_THRESHOLD: f64 = 0.05;

/// Slack for comparing a share against the minor threshold: shares are
/// sums of `1 / n` credits, and a contributor sitting exactly on the
/// threshold must not become minor through float rounding.
const SHARE_EPSILON: f64 = 1e-9;

/// One commit, reduced to what ownership needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredCommit {
    /// Normalised author keys: the commit author plus every co-author,
    /// with bots already removed. Duplicates are tolerated and collapsed.
    pub authors: Vec<String>,
    /// Files the commit touched.
    pub files: Vec<String>,
}

/// Knobs for [`compute_ownership`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OwnershipThresholds {
    /// A contributor whose share of a file is strictly below this is
    /// minor. Exactly on it is not.
    pub minor_threshold: f64,
}

impl Default for OwnershipThresholds {
    fn default() -> Self {
        Self {
            minor_threshold: DEFAULT_MINOR_THRESHOLD,
        }
    }
}

/// One author's stake in one file.
#[derive(Debug, Clone, PartialEq)]
pub struct AuthorShare {
    pub author: String,
    /// Commit credit: whole commits for a solo author, fractions for a
    /// co-authored one.
    pub credit: f64,
    /// `credit / commits` of the file.
    pub share: f64,
}

/// Ownership of one file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileOwnership {
    pub path: String,
    /// Counted commits that touched the file.
    pub commits: u32,
    /// Every contributor, largest share first, ties by author key.
    pub authors: Vec<AuthorShare>,
    /// The first entry of [`Self::authors`]'s share.
    pub top_author_share: f64,
    /// Contributors whose share is below the minor threshold.
    pub minor_contributors: usize,
    /// `minor_contributors / authors`.
    pub minor_ratio: f64,
    /// `commits × (1 − top_author_share)`, the ranking key.
    pub score: f64,
}

impl FileOwnership {
    /// The top author's key. Never empty for a file that appears in a
    /// report, since a file only appears once a commit credited someone.
    pub fn top_author(&self) -> &str {
        self.authors.first().map_or("", |a| a.author.as_str())
    }
}

/// Fold commits into per-file ownership, ranked by
/// [`FileOwnership::score`] descending, then commits descending, then
/// path. Commits with no author or no file contribute nothing.
pub fn compute_ownership(
    commits: &[AuthoredCommit],
    thresholds: OwnershipThresholds,
) -> Vec<FileOwnership> {
    let mut per_file: BTreeMap<&str, (u32, BTreeMap<&str, f64>)> = BTreeMap::new();
    for commit in commits {
        let mut authors: Vec<&str> = commit.authors.iter().map(String::as_str).collect();
        authors.sort_unstable();
        authors.dedup();
        if authors.is_empty() {
            continue;
        }
        let credit = 1.0 / authors.len() as f64;
        let mut files: Vec<&str> = commit.files.iter().map(String::as_str).collect();
        files.sort_unstable();
        files.dedup();
        for file in files {
            let (count, credits) = per_file.entry(file).or_default();
            *count += 1;
            for author in &authors {
                *credits.entry(author).or_insert(0.0) += credit;
            }
        }
    }

    let mut out: Vec<FileOwnership> = per_file
        .into_iter()
        .map(|(path, (commits, credits))| file_ownership(path, commits, credits, thresholds))
        .collect();
    out.sort_by(|x, y| {
        y.score
            .total_cmp(&x.score)
            .then_with(|| y.commits.cmp(&x.commits))
            .then_with(|| x.path.cmp(&y.path))
    });
    out
}

fn file_ownership(
    path: &str,
    commits: u32,
    credits: BTreeMap<&str, f64>,
    thresholds: OwnershipThresholds,
) -> FileOwnership {
    let total = f64::from(commits);
    let mut authors: Vec<AuthorShare> = credits
        .into_iter()
        .map(|(author, credit)| AuthorShare {
            author: author.to_owned(),
            credit,
            share: credit / total,
        })
        .collect();
    authors.sort_by(|x, y| {
        y.credit
            .total_cmp(&x.credit)
            .then_with(|| x.author.cmp(&y.author))
    });
    let top_author_share = authors.first().map_or(0.0, |a| a.share);
    let minor_contributors = authors
        .iter()
        .filter(|a| a.share < thresholds.minor_threshold - SHARE_EPSILON)
        .count();
    let minor_ratio = if authors.is_empty() {
        0.0
    } else {
        minor_contributors as f64 / authors.len() as f64
    };
    FileOwnership {
        path: path.to_owned(),
        commits,
        top_author_share,
        minor_contributors,
        minor_ratio,
        score: total * (1.0 - top_author_share),
        authors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    fn commit(authors: &[&str], files: &[&str]) -> AuthoredCommit {
        AuthoredCommit {
            authors: authors.iter().map(|a| (*a).to_owned()).collect(),
            files: files.iter().map(|f| (*f).to_owned()).collect(),
        }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn a_single_author_owns_the_file_outright_and_scores_zero() {
        let rows = compute_ownership(
            &[commit(&["ann"], &["a.rs"]), commit(&["ann"], &["a.rs"])],
            OwnershipThresholds::default(),
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].commits, 2);
        assert_eq!(rows[0].top_author(), "ann");
        assert!(close(rows[0].top_author_share, 1.0));
        assert!(close(rows[0].score, 0.0));
        assert_eq!(rows[0].minor_contributors, 0);
    }

    /// The co-author rule: one commit, two authors, half each — so a
    /// trailer neither doubles the commit nor disappears.
    #[test]
    fn a_co_authored_commit_splits_its_credit() {
        let rows = compute_ownership(
            &[commit(&["ann", "claude"], &["a.rs"])],
            OwnershipThresholds::default(),
        );
        assert_eq!(rows[0].commits, 1);
        assert_eq!(rows[0].authors.len(), 2);
        assert!(close(rows[0].authors[0].credit, 0.5));
        assert!(close(rows[0].top_author_share, 0.5));
        assert!(close(rows[0].score, 0.5));
    }

    #[test]
    fn duplicate_authors_on_one_commit_collapse() {
        let rows = compute_ownership(
            &[commit(&["ann", "ann"], &["a.rs", "a.rs"])],
            OwnershipThresholds::default(),
        );
        assert_eq!(rows[0].commits, 1);
        assert_eq!(rows[0].authors.len(), 1);
        assert!(close(rows[0].authors[0].credit, 1.0));
    }

    /// The threshold boundary: with 20 commits one commit is exactly 5%,
    /// which is *not* minor; with 21 it is just under and is.
    #[rstest]
    #[case::exactly_on_the_threshold(20, 0)]
    #[case::just_under_the_threshold(21, 1)]
    fn the_minor_threshold_is_strict(#[case] total: usize, #[case] expected_minor: usize) {
        let mut commits: Vec<AuthoredCommit> =
            (1..total).map(|_| commit(&["owner"], &["a.rs"])).collect();
        commits.push(commit(&["drive-by"], &["a.rs"]));
        let rows = compute_ownership(&commits, OwnershipThresholds::default());
        assert_eq!(rows[0].minor_contributors, expected_minor);
        assert!(close(rows[0].minor_ratio, expected_minor as f64 / 2.0));
    }

    #[test]
    fn rows_rank_by_low_ownership_times_churn() {
        let rows = compute_ownership(
            &[
                // busy.rs: 4 commits, top share 0.5 → score 2.
                commit(&["ann"], &["busy.rs", "solo.rs"]),
                commit(&["ann"], &["busy.rs", "solo.rs"]),
                commit(&["bob"], &["busy.rs"]),
                commit(&["cat"], &["busy.rs"]),
                // quiet.rs: 2 commits, top share 0.5 → score 1.
                commit(&["ann"], &["quiet.rs"]),
                commit(&["bob"], &["quiet.rs"]),
            ],
            OwnershipThresholds::default(),
        );
        let order: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(order, ["busy.rs", "quiet.rs", "solo.rs"]);
        assert!(close(rows[0].score, 2.0));
        assert!(close(rows[1].score, 1.0));
        assert!(close(rows[2].score, 0.0));
    }

    #[test]
    fn a_commit_with_no_author_counts_for_nothing() {
        let rows = compute_ownership(&[commit(&[], &["a.rs"])], OwnershipThresholds::default());
        assert!(rows.is_empty());
    }

    proptest! {
        /// Shares are a partition of each file's commits, whatever mix
        /// of solo and co-authored commits produced them.
        #[test]
        fn shares_sum_to_one_and_scores_stay_in_range(
            raw in proptest::collection::vec(
                (proptest::collection::vec(0u8..5, 1..4), proptest::collection::vec(0u8..4, 1..4)),
                1..30,
            ),
        ) {
            let commits: Vec<AuthoredCommit> = raw
                .iter()
                .map(|(authors, files)| AuthoredCommit {
                    authors: authors.iter().map(|a| format!("a{a}")).collect(),
                    files: files.iter().map(|f| format!("f{f}")).collect(),
                })
                .collect();
            for row in compute_ownership(&commits, OwnershipThresholds::default()) {
                let sum: f64 = row.authors.iter().map(|a| a.share).sum();
                prop_assert!((sum - 1.0).abs() < 1e-9, "shares sum to {sum}");
                prop_assert!(row.score >= -1e-9);
                prop_assert!(row.score <= f64::from(row.commits) + 1e-9);
                prop_assert!(row.minor_contributors <= row.authors.len());
            }
        }
    }
}
