//! Language-agnostic change entropy: how *scattered* change activity is.
//!
//! Churn counts how often a file changed. Hassan (2009) found that the
//! more predictive signal is not the count but the **spread**: a period
//! in which the work was concentrated in one place is a different kind of
//! period from one in which the same number of edited lines was smeared
//! over thirty files, and it is the second kind that accumulates onto a
//! file as history complexity.
//!
//! Every figure here is Shannon entropy over one *change set* — a set of
//! files with a weight each — so the definitions are worth stating
//! exactly, because two tools' "change entropy" are otherwise not
//! comparable:
//!
//! * **Weight** is the number of changed lines (insertions + deletions)
//!   the file received. Hassan weights by the *number of modifications*
//!   instead; lines are used here so the same definition covers a single
//!   pending edit, where every file is modified exactly once and a
//!   modification count would make every pending change look maximally
//!   scattered. A file whose diff carries no line change (a mode change)
//!   has no weight and takes no part.
//! * **Entropy** is `H = -Σ pᵢ·log₂ pᵢ` in bits, over `pᵢ = wᵢ / Σw`.
//! * **Normalisation** is `H' = H / log₂(n)`, `n` being the number of
//!   weighted files in the change set, so change sets of different size
//!   compare: `H' = 0` is all change in one file and `H' = 1` is change
//!   spread perfectly evenly. `n <= 1` has no scatter to measure and is
//!   defined as 0 rather than as a division by zero.
//! * **Period** is an ISO week or a calendar month in UTC
//!   ([`Period::key`]), never "N days back from now": a window measured
//!   from the clock makes two runs a day apart disagree about which
//!   commits share a bucket.
//! * **Attribution** onto a file follows Hassan's *weighted* HCPF
//!   variant: file `i` takes `pᵢ(p) · H'(p)` from period `p`, and
//!   [`FileEntropy::history_complexity`] is that summed over the
//!   periods. The unweighted variants (spread uniformly over the changed
//!   files, or given whole to each) are deliberately absent rather than
//!   offered as a flag nobody can choose between. Because the shares in
//!   a period sum to 1, a period hands out exactly its own entropy — so
//!   the file column is a decomposition of the period column, not a
//!   second opinion on it.
//!
//! This module owns the arithmetic and the data shapes only. Producing
//! the input — asking git for per-commit line counts, following renames,
//! and mapping paths into the analyzer's path space — is the CLI's job.
//!
//! Known limits, which callers should state in their own output:
//!
//! * Squash-merge and merge-heavy workflows collapse a branch into one
//!   commit, which raises measured scatter for reasons that have nothing
//!   to do with the code. [`ChangeEntropyThresholds::max_commit_files`]
//!   drops the worst offenders whole, exactly as `cochange` does, but it
//!   is a blunt guard rather than a fix.
//! * A period with almost no activity produces unstable entropy, so
//!   [`ChangeEntropyThresholds::min_commits_per_period`] omits it rather
//!   than reporting a figure drawn from two commits.
//! * This is a prior, not a gate. A high row says "changes around this
//!   file were unfocused", never "this file is wrong".

use std::collections::BTreeMap;

pub use crate::cochange::DEFAULT_MAX_COMMIT_FILES;

/// Default minimum commit count for a period to be reported.
pub const DEFAULT_MIN_COMMITS_PER_PERIOD: u32 = 3;

/// The bucket commits are grouped into before entropy is measured.
///
/// Both spellings are UTC and calendar-anchored, so the bucket a commit
/// falls in is a property of the commit rather than of when the report
/// was run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Period {
    /// ISO-8601 week, keyed `YYYY-Www` on the ISO week-*year* — which is
    /// not the calendar year at either end of December.
    #[default]
    Week,
    /// Calendar month, keyed `YYYY-MM`.
    Month,
}

impl Period {
    /// The bucket key for a `YYYY-MM-DD` date, or `None` when the date
    /// is not that shape.
    ///
    /// Keys sort lexicographically in chronological order, which is what
    /// lets the report order periods without carrying a second sort key.
    pub fn key(self, date: &str) -> Option<String> {
        let (year, month, day) = parse_ymd(date)?;
        match self {
            Self::Week => {
                let (iso_year, week) = iso_week(year, month, day);
                Some(format!("{iso_year:04}-W{week:02}"))
            }
            Self::Month => Some(format!("{year:04}-{month:02}")),
        }
    }

    /// How the period is named in prose and in `--period`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Week => "week",
            Self::Month => "month",
        }
    }
}

/// One file's share of a change set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    /// Changed lines: insertions + deletions. A binary file, whose line
    /// counts git reports as `-`, is the caller's to weigh.
    pub lines: u64,
}

/// One commit's changed files, in the caller's path space.
///
/// Commits are supplied **newest first** — the order `git log` emits.
/// A path listed twice has its weights summed, so a caller need not
/// de-duplicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitChanges {
    /// Commit date as `YYYY-MM-DD`, in UTC. Anything else is unbucketable
    /// and the commit is skipped.
    pub date: String,
    pub files: Vec<FileChange>,
}

/// Guards applied while folding commits into periods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeEntropyThresholds {
    /// Periods with fewer counted commits than this are omitted whole:
    /// entropy over two commits is noise wearing a number's clothes.
    pub min_commits_per_period: u32,
    /// Commits touching more files than this take part in nothing — not
    /// the periods, not the per-file rows, not the reference
    /// distribution. Same guard, same default, and the same bluntness as
    /// [`crate::cochange`]'s.
    pub max_commit_files: usize,
}

impl Default for ChangeEntropyThresholds {
    fn default() -> Self {
        Self {
            min_commits_per_period: DEFAULT_MIN_COMMITS_PER_PERIOD,
            max_commit_files: DEFAULT_MAX_COMMIT_FILES,
        }
    }
}

/// Entropy of one change set, with the inputs it was computed from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scatter {
    /// Files carrying a non-zero weight: the `n` of the normalisation.
    pub file_count: usize,
    pub changed_lines: u64,
    /// `H` in bits, before normalisation.
    pub bits: f64,
    /// `H / log₂(file_count)`, in `[0, 1]`. Zero for a single file.
    pub normalised: f64,
}

impl Scatter {
    /// Measure a change set. Weights are summed per path first, so a
    /// path mentioned twice cannot count as two files.
    pub fn of(files: &[FileChange]) -> Self {
        Self::from_weights(&fold_weights(files))
    }

    fn from_weights(weights: &BTreeMap<&str, u64>) -> Self {
        let counted: Vec<u64> = weights.values().copied().filter(|w| *w > 0).collect();
        let changed_lines = counted.iter().sum();
        let file_count = counted.len();
        if file_count <= 1 {
            return Self {
                file_count,
                changed_lines,
                bits: 0.0,
                normalised: 0.0,
            };
        }
        let total = changed_lines as f64;
        let bits: f64 = -counted
            .iter()
            .map(|w| {
                let p = *w as f64 / total;
                p * p.log2()
            })
            .sum::<f64>();
        Self {
            file_count,
            changed_lines,
            bits,
            // log₂(n) for n >= 2 is positive, so this cannot divide by
            // zero; the clamp only absorbs floating-point overshoot at
            // the perfectly uniform end.
            normalised: (bits / (file_count as f64).log2()).clamp(0.0, 1.0),
        }
    }
}

/// One reported period.
#[derive(Debug, Clone, PartialEq)]
pub struct PeriodEntropy {
    /// `YYYY-Www` or `YYYY-MM`, per [`Period::key`].
    pub key: String,
    pub commit_count: usize,
    pub scatter: Scatter,
}

/// One period's contribution to one file's history complexity.
#[derive(Debug, Clone, PartialEq)]
pub struct FilePeriodContribution {
    pub period: String,
    /// The file's share of the period's changed lines.
    pub share: f64,
    /// The period's normalised entropy.
    pub period_entropy: f64,
    /// `share × period_entropy`, the weighted HCPF term.
    pub contribution: f64,
}

/// One reported file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileEntropy {
    pub path: String,
    /// Hassan's History Complexity Metric under the weighted variant:
    /// the sum of this file's per-period contributions. Unbounded above
    /// — it accumulates one term per period the file changed in — so it
    /// ranks files against each other, and does not read as a score out
    /// of anything.
    pub history_complexity: f64,
    /// Counted commits touching the file, inside counted periods.
    pub commits: u32,
    pub changed_lines: u64,
    /// The file's periods, largest contribution first.
    pub periods: Vec<FilePeriodContribution>,
}

/// A sorted sample of normalised entropies, for asking where one change
/// sits among the rest.
///
/// A scatter figure on its own is not actionable: 0.78 is high or
/// unremarkable depending entirely on what this repository's commits
/// usually look like. That reference is part of the metric, so it
/// travels with it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntropyDistribution {
    sorted: Vec<f64>,
}

impl EntropyDistribution {
    pub fn new(mut values: Vec<f64>) -> Self {
        values.sort_by(f64::total_cmp);
        Self { sorted: values }
    }

    pub fn sample_count(&self) -> usize {
        self.sorted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sorted.is_empty()
    }

    /// The `q`-quantile by nearest-rank, `q` in `[0, 1]`. `None` for an
    /// empty sample: an invented median is worse than no median.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.sorted.is_empty() {
            return None;
        }
        let last = self.sorted.len() - 1;
        let index = (q.clamp(0.0, 1.0) * last as f64).round() as usize;
        self.sorted.get(index.min(last)).copied()
    }

    pub fn median(&self) -> Option<f64> {
        self.quantile(0.5)
    }

    /// Percentage of samples at or below `value`. `None` for an empty
    /// sample.
    pub fn percentile_rank(&self, value: f64) -> Option<f64> {
        if self.sorted.is_empty() {
            return None;
        }
        let below = self.sorted.partition_point(|s| *s <= value);
        Some(100.0 * below as f64 / self.sorted.len() as f64)
    }
}

/// The folded change-entropy report.
#[derive(Debug, Clone, PartialEq)]
pub struct ChangeEntropyReport {
    /// Commits that contributed: in the window, carrying at least one
    /// weighted file, dated, and within the commit-size cap.
    pub commit_count: usize,
    /// Commits dropped by [`ChangeEntropyThresholds::max_commit_files`].
    pub skipped_commit_count: usize,
    /// Periods dropped by
    /// [`ChangeEntropyThresholds::min_commits_per_period`], reported so a
    /// thin report is visibly the guard's doing rather than the history's.
    pub thin_period_count: usize,
    /// Distinct files across the counted periods.
    pub file_count: usize,
    /// Counted periods, newest first.
    pub periods: Vec<PeriodEntropy>,
    /// Files, highest history complexity first.
    pub files: Vec<FileEntropy>,
    /// Per-commit normalised entropy over every counted commit — the
    /// reference distribution a single pending change is read against.
    /// Drawn from all counted commits, including those in periods the
    /// minimum-commit guard dropped: the question it answers ("is this
    /// change scattered for this repository?") is about commits, not
    /// periods.
    pub commit_entropy: EntropyDistribution,
}

/// Fold per-commit changed-line counts into periods and per-file rows.
///
/// `commits` is newest-first. A commit with no weighted file, an
/// unparseable date, or more files than the cap takes part in nothing,
/// so every figure in the report is drawn from the one population
/// [`ChangeEntropyReport::commit_count`] reports.
pub fn compute_change_entropy(
    commits: &[CommitChanges],
    period: Period,
    thresholds: ChangeEntropyThresholds,
) -> ChangeEntropyReport {
    let tally = tally_periods(commits, period, thresholds.max_commit_files);
    let (kept, thin): (Vec<_>, Vec<_>) = tally
        .periods
        .into_iter()
        .partition(|(_, acc)| acc.commit_count >= thresholds.min_commits_per_period as usize);

    let mut periods: Vec<PeriodEntropy> = Vec::with_capacity(kept.len());
    let mut files: BTreeMap<&str, FileAcc> = BTreeMap::new();
    for (key, acc) in &kept {
        let scatter = Scatter::from_weights(&acc.weights);
        let total = scatter.changed_lines as f64;
        for (&path, &lines) in &acc.weights {
            if lines == 0 {
                continue;
            }
            let share = lines as f64 / total;
            let file = files.entry(path).or_default();
            file.history_complexity += share * scatter.normalised;
            file.commits += acc.commits_per_file.get(path).copied().unwrap_or(0);
            file.changed_lines += lines;
            file.periods.push(FilePeriodContribution {
                period: key.clone(),
                share,
                period_entropy: scatter.normalised,
                contribution: share * scatter.normalised,
            });
        }
        periods.push(PeriodEntropy {
            key: key.clone(),
            commit_count: acc.commit_count,
            scatter,
        });
    }
    // Keys sort chronologically, so the newest-first order the rest of
    // the history analyzers use is a reverse of the map order.
    periods.reverse();

    ChangeEntropyReport {
        commit_count: tally.counted,
        skipped_commit_count: tally.skipped,
        thin_period_count: thin.len(),
        file_count: files.len(),
        periods,
        files: rank_files(files),
        commit_entropy: EntropyDistribution::new(tally.commit_entropies),
    }
}

/// Per-file accumulator across periods.
#[derive(Debug, Default)]
struct FileAcc {
    history_complexity: f64,
    commits: u32,
    changed_lines: u64,
    periods: Vec<FilePeriodContribution>,
}

fn rank_files(files: BTreeMap<&str, FileAcc>) -> Vec<FileEntropy> {
    let mut out: Vec<FileEntropy> = files
        .into_iter()
        .map(|(path, mut acc)| {
            acc.periods.sort_by(|x, y| {
                y.contribution
                    .total_cmp(&x.contribution)
                    .then_with(|| x.period.cmp(&y.period))
            });
            FileEntropy {
                path: path.to_owned(),
                history_complexity: acc.history_complexity,
                commits: acc.commits,
                changed_lines: acc.changed_lines,
                periods: acc.periods,
            }
        })
        .collect();
    out.sort_by(|x, y| {
        y.history_complexity
            .total_cmp(&x.history_complexity)
            .then_with(|| y.changed_lines.cmp(&x.changed_lines))
            .then_with(|| x.path.cmp(&y.path))
    });
    out
}

/// One period's accumulated weights, borrowed from the commit stream.
#[derive(Debug, Default)]
struct PeriodAcc<'a> {
    commit_count: usize,
    weights: BTreeMap<&'a str, u64>,
    commits_per_file: BTreeMap<&'a str, u32>,
}

#[derive(Debug)]
struct Tally<'a> {
    periods: BTreeMap<String, PeriodAcc<'a>>,
    commit_entropies: Vec<f64>,
    counted: usize,
    skipped: usize,
}

fn tally_periods<'a>(
    commits: &'a [CommitChanges],
    period: Period,
    max_commit_files: usize,
) -> Tally<'a> {
    let mut tally = Tally {
        periods: BTreeMap::new(),
        commit_entropies: Vec::new(),
        counted: 0,
        skipped: 0,
    };
    for commit in commits {
        let weights = fold_weights(&commit.files);
        let scatter = Scatter::from_weights(&weights);
        if scatter.file_count == 0 {
            continue;
        }
        if scatter.file_count > max_commit_files {
            tally.skipped += 1;
            continue;
        }
        let Some(key) = period.key(&commit.date) else {
            continue;
        };
        tally.counted += 1;
        tally.commit_entropies.push(scatter.normalised);
        let acc = tally.periods.entry(key).or_default();
        acc.commit_count += 1;
        for (path, lines) in weights {
            if lines == 0 {
                continue;
            }
            *acc.weights.entry(path).or_insert(0) += lines;
            *acc.commits_per_file.entry(path).or_insert(0) += 1;
        }
    }
    tally
}

/// Sum a change set's weights per path, so one path is one file however
/// often it was listed.
fn fold_weights(files: &[FileChange]) -> BTreeMap<&str, u64> {
    let mut weights: BTreeMap<&str, u64> = BTreeMap::new();
    for file in files {
        *weights.entry(file.path.as_str()).or_insert(0) += file.lines;
    }
    weights
}

/// The module a path belongs to: its parent directory, or `.` at the
/// repository root.
///
/// Directory *is* module here because this metric never parses a file —
/// the same reason the analyzer has no language matrix. It is what makes
/// "this edit spans six modules" answerable for `.toml` and `.md` too.
pub fn module_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) => "/",
        Some(index) => &path[..index],
        None => ".",
    }
}

fn parse_ymd(date: &str) -> Option<(i32, u32, u32)> {
    let mut parts = date.trim().split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard
/// Hinnant's `days_from_civil`).
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = i64::from(if month <= 2 { year - 1 } else { year });
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = i64::from((month + 9) % 12);
    let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// ISO weekday, Monday = 1 … Sunday = 7. 1970-01-01 was a Thursday.
fn iso_weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7) + 1
}

/// ISO-8601 week-year and week number.
///
/// The week-year is not the calendar year at either end of December: the
/// week a date belongs to is the one holding its Thursday, so
/// 2019-12-30 is `2020-W01` and 2021-01-01 is `2020-W53`. Getting this
/// wrong splits one week's commits across two buckets, which is exactly
/// the non-determinism the period definition exists to avoid.
fn iso_week(year: i32, month: u32, day: u32) -> (i32, u32) {
    let days = days_from_civil(year, month, day);
    let ordinal = days - days_from_civil(year, 1, 1) + 1;
    let weekday = iso_weekday(days);
    let week = (ordinal - weekday + 10) / 7;
    if week < 1 {
        return (year - 1, iso_weeks_in_year(year - 1));
    }
    if week > i64::from(iso_weeks_in_year(year)) {
        return (year + 1, 1);
    }
    (year, week as u32)
}

/// 52 or 53, per the ISO rule: a year has 53 weeks when it starts on a
/// Thursday, or is a leap year starting on a Wednesday.
///
/// The two conditions are a disjunction, not two independent increments
/// — 2015 satisfies both, and adding them would give it 54 weeks.
fn iso_weeks_in_year(year: i32) -> u32 {
    /// Weekday of 31 December, in the shifted numbering the ISO
    /// long-year rule is stated in.
    fn december_31(year: i32) -> i32 {
        (year + year.div_euclid(4) - year.div_euclid(100) + year.div_euclid(400)).rem_euclid(7)
    }
    if december_31(year) == 4 || december_31(year - 1) == 3 {
        53
    } else {
        52
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    /// Commits are written newest-first, the order `git log` emits.
    fn commit(date: &str, files: &[(&str, u64)]) -> CommitChanges {
        CommitChanges {
            date: date.to_owned(),
            files: files
                .iter()
                .map(|(path, lines)| FileChange {
                    path: (*path).to_owned(),
                    lines: *lines,
                })
                .collect(),
        }
    }

    /// Thresholds that keep every period, so a test asserting on
    /// arithmetic is not also asserting on filtering.
    fn open() -> ChangeEntropyThresholds {
        ChangeEntropyThresholds {
            min_commits_per_period: 1,
            max_commit_files: 50,
        }
    }

    fn find<'a>(report: &'a ChangeEntropyReport, path: &str) -> &'a FileEntropy {
        report
            .files
            .iter()
            .find(|f| f.path == path)
            .unwrap_or_else(|| panic!("no {path} row in {report:?}"))
    }

    /// The dates that separate a correct ISO week from a plausible one:
    /// both December boundaries, both directions, and a 53-week year.
    #[rstest]
    #[case("2026-08-18", "2026-W34")]
    #[case("2026-01-01", "2026-W01")]
    #[case("2021-01-01", "2020-W53")]
    #[case("2016-01-01", "2015-W53")]
    #[case("2015-12-31", "2015-W53")]
    #[case("2019-12-30", "2020-W01")]
    #[case("2020-12-31", "2020-W53")]
    #[case("2004-12-31", "2004-W53")]
    #[case("1977-01-01", "1976-W53")]
    #[case("2000-01-01", "1999-W52")]
    fn iso_week_keys_match_the_calendar(#[case] date: &str, #[case] expected: &str) {
        assert_eq!(Period::Week.key(date).as_deref(), Some(expected));
    }

    #[rstest]
    #[case("2026-08-18", "2026-08")]
    #[case("2026-01-31", "2026-01")]
    #[case("1999-12-01", "1999-12")]
    fn month_keys_are_the_calendar_month(#[case] date: &str, #[case] expected: &str) {
        assert_eq!(Period::Month.key(date).as_deref(), Some(expected));
    }

    #[rstest]
    #[case::empty("")]
    #[case::not_a_date("HEAD")]
    #[case::iso_timestamp("2026-08-18T12:00:00Z")]
    #[case::month_out_of_range("2026-13-01")]
    #[case::day_out_of_range("2026-01-32")]
    #[case::too_many_fields("2026-01-01-01")]
    fn an_unbucketable_date_has_no_key(#[case] date: &str) {
        assert_eq!(Period::Week.key(date), None, "{date}");
        assert_eq!(Period::Month.key(date), None, "{date}");
    }

    /// Days in `month` of `year`, for walking a calendar day by day.
    fn days_in_month(year: i32, month: u32) -> u32 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
            _ => 28,
        }
    }

    /// A week key must change on exactly one weekday, and must never go
    /// backwards. Walking real calendars catches the boundary bugs a
    /// handful of spot dates can miss — a week split across two buckets
    /// is silent, and it is what the "deterministic periods" requirement
    /// is guarding against.
    #[rstest]
    #[case(1999)]
    #[case(2004)]
    #[case(2020)]
    #[case(2026)]
    fn week_keys_advance_only_on_mondays_and_never_go_backwards(#[case] year: i32) {
        let mut previous: Option<(String, u32, u32)> = None;
        for month in 1..=12 {
            for day in 1..=days_in_month(year, month) {
                let date = format!("{year:04}-{month:02}-{day:02}");
                let key = Period::Week.key(&date).unwrap_or_else(|| panic!("{date}"));
                let weekday = iso_weekday(days_from_civil(year, month, day));
                if let Some((previous_key, previous_month, previous_day)) = &previous {
                    let changed = &key != previous_key;
                    assert_eq!(
                        changed,
                        weekday == 1,
                        "{date} (weekday {weekday}) moved from {previous_key} to {key} \
                         after {previous_month:02}-{previous_day:02}",
                    );
                    assert!(&key >= previous_key, "{date}: {key} < {previous_key}");
                }
                previous = Some((key, month, day));
            }
        }
    }

    #[test]
    fn a_single_file_change_set_has_no_scatter() {
        let scatter = Scatter::of(&[FileChange {
            path: "a".to_owned(),
            lines: 99,
        }]);
        assert_eq!(scatter.file_count, 1);
        assert_eq!(scatter.changed_lines, 99);
        assert!(scatter.normalised.abs() < 1e-12, "{scatter:?}");
    }

    #[test]
    fn an_even_spread_is_maximal_scatter() {
        let files: Vec<FileChange> = (0..8)
            .map(|i| FileChange {
                path: format!("f{i}"),
                lines: 10,
            })
            .collect();
        let scatter = Scatter::of(&files);
        assert!((scatter.normalised - 1.0).abs() < 1e-12, "{scatter:?}");
        // Eight equally likely outcomes is exactly three bits.
        assert!((scatter.bits - 3.0).abs() < 1e-12, "{scatter:?}");
    }

    /// A file whose diff moved no line is not a file the change touched,
    /// and counting it would inflate the `n` the normalisation divides
    /// by — dragging every scatter figure down for free.
    #[test]
    fn a_zero_weight_file_takes_no_part() {
        let scatter = Scatter::of(&[
            FileChange {
                path: "a".to_owned(),
                lines: 4,
            },
            FileChange {
                path: "mode-change-only".to_owned(),
                lines: 0,
            },
        ]);
        assert_eq!(scatter.file_count, 1);
        assert!(scatter.normalised.abs() < 1e-12, "{scatter:?}");
    }

    #[test]
    fn a_path_repeated_in_one_change_set_is_one_file() {
        let scatter = Scatter::of(&[
            FileChange {
                path: "a".to_owned(),
                lines: 3,
            },
            FileChange {
                path: "a".to_owned(),
                lines: 5,
            },
        ]);
        assert_eq!(scatter.file_count, 1);
        assert_eq!(scatter.changed_lines, 8);
    }

    #[test]
    fn periods_bucket_by_iso_week_and_are_reported_newest_first() {
        // 2026-08-16 is a Sunday, so it closes the week 2026-08-17
        // (Monday) opens — the boundary a naive 7-day window gets wrong.
        let commits = vec![
            commit("2026-08-17", &[("a", 10), ("b", 10)]),
            commit("2026-08-16", &[("a", 10), ("b", 10)]),
        ];
        let report = compute_change_entropy(&commits, Period::Week, open());
        let keys: Vec<&str> = report.periods.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec!["2026-W34", "2026-W33"], "got {report:?}");
    }

    #[test]
    fn a_month_period_merges_what_a_week_period_splits() {
        let commits = vec![
            commit("2026-08-17", &[("a", 10), ("b", 10)]),
            commit("2026-08-16", &[("a", 10), ("b", 10)]),
        ];
        let report = compute_change_entropy(&commits, Period::Month, open());
        assert_eq!(report.periods.len(), 1, "got {report:?}");
        assert_eq!(report.periods[0].key, "2026-08");
        assert_eq!(report.periods[0].commit_count, 2);
    }

    /// The attribution choice, stated as a test: the file that took the
    /// larger share of a period's changed lines takes the larger share
    /// of its entropy. Under the unweighted variant these two rows would
    /// be equal.
    #[test]
    fn attribution_is_weighted_by_each_files_share() {
        let commits = vec![commit("2026-08-17", &[("big", 90), ("small", 10)])];
        let report = compute_change_entropy(&commits, Period::Week, open());
        let big = find(&report, "big");
        let small = find(&report, "small");
        assert!(
            big.history_complexity > small.history_complexity,
            "{big:?} vs {small:?}",
        );
        assert!((big.periods[0].share - 0.9).abs() < 1e-12, "{big:?}");
        assert!((small.periods[0].share - 0.1).abs() < 1e-12, "{small:?}");
    }

    /// A period hands out exactly its own entropy, because the shares it
    /// splits it by sum to 1. That is what makes the file column a
    /// decomposition of the period column rather than a second opinion.
    #[test]
    fn a_periods_contributions_sum_to_its_entropy() {
        let commits = vec![
            commit("2026-08-17", &[("a", 30), ("b", 20)]),
            commit("2026-08-18", &[("b", 10), ("c", 40)]),
        ];
        let report = compute_change_entropy(&commits, Period::Week, open());
        assert_eq!(report.periods.len(), 1, "got {report:?}");
        let handed_out: f64 = report
            .files
            .iter()
            .flat_map(|f| f.periods.iter())
            .map(|p| p.contribution)
            .sum();
        assert!(
            (handed_out - report.periods[0].scatter.normalised).abs() < 1e-12,
            "handed out {handed_out}, period has {:?}",
            report.periods[0],
        );
    }

    #[test]
    fn history_complexity_accumulates_across_periods() {
        let commits = vec![
            commit("2026-08-24", &[("a", 50), ("b", 50)]),
            commit("2026-08-17", &[("a", 50), ("b", 50)]),
        ];
        let report = compute_change_entropy(&commits, Period::Week, open());
        let a = find(&report, "a");
        assert_eq!(a.periods.len(), 2, "{a:?}");
        assert_eq!(a.commits, 2, "{a:?}");
        assert_eq!(a.changed_lines, 100, "{a:?}");
        // Two evenly-split weeks: 0.5 of a full bit of scatter, twice.
        assert!((a.history_complexity - 1.0).abs() < 1e-12, "{a:?}");
    }

    /// The commit-size cap has to remove the commit from every figure,
    /// the reference distribution included: a squash merge left in it
    /// would raise the bar every pending change is measured against.
    #[test]
    fn an_oversized_commit_contributes_to_nothing() {
        let commits = vec![
            commit("2026-08-17", &[("a", 1), ("b", 1), ("c", 1), ("d", 1)]),
            commit("2026-08-17", &[("a", 1), ("b", 1)]),
        ];
        let report = compute_change_entropy(
            &commits,
            Period::Week,
            ChangeEntropyThresholds {
                max_commit_files: 3,
                ..open()
            },
        );
        assert_eq!(report.commit_count, 1, "got {report:?}");
        assert_eq!(report.skipped_commit_count, 1, "got {report:?}");
        assert_eq!(report.file_count, 2, "got {report:?}");
        assert_eq!(report.commit_entropy.sample_count(), 1, "got {report:?}");
        assert!(report.files.iter().all(|f| f.path != "c"), "got {report:?}");
    }

    /// `max_commit_files` drops commits with *more* files than the cap,
    /// so one sitting exactly on it still counts.
    #[test]
    fn a_commit_exactly_at_the_size_cap_is_counted() {
        let commits = vec![commit("2026-08-17", &[("a", 1), ("b", 1), ("c", 1)])];
        let report = compute_change_entropy(
            &commits,
            Period::Week,
            ChangeEntropyThresholds {
                max_commit_files: 3,
                ..open()
            },
        );
        assert_eq!(report.commit_count, 1, "got {report:?}");
        assert_eq!(report.skipped_commit_count, 0, "got {report:?}");
    }

    #[test]
    fn a_thin_period_is_omitted_and_counted() {
        let commits = vec![
            commit("2026-08-24", &[("a", 5), ("b", 5)]),
            commit("2026-08-17", &[("a", 5), ("b", 5)]),
            commit("2026-08-18", &[("a", 5), ("b", 5)]),
        ];
        let report = compute_change_entropy(
            &commits,
            Period::Week,
            ChangeEntropyThresholds {
                min_commits_per_period: 2,
                ..open()
            },
        );
        assert_eq!(report.periods.len(), 1, "got {report:?}");
        assert_eq!(report.periods[0].key, "2026-W34", "got {report:?}");
        assert_eq!(report.thin_period_count, 1, "got {report:?}");
        // The dropped period leaves the file rows, but not the reference
        // distribution: that one is about commits.
        assert_eq!(find(&report, "a").commits, 2);
        assert_eq!(report.commit_entropy.sample_count(), 3, "got {report:?}");
    }

    #[test]
    fn a_commit_with_an_unbucketable_date_is_skipped() {
        let report = compute_change_entropy(
            &[commit("not-a-date", &[("a", 1), ("b", 1)])],
            Period::Week,
            open(),
        );
        assert_eq!(report.commit_count, 0, "got {report:?}");
        assert_eq!(report.skipped_commit_count, 0, "got {report:?}");
        assert!(report.periods.is_empty(), "got {report:?}");
    }

    #[rstest]
    #[case::no_commits(vec![])]
    #[case::only_empty_commits(vec![commit("2026-08-17", &[])])]
    #[case::only_zero_weight_files(vec![commit("2026-08-17", &[("a", 0)])])]
    fn nothing_to_measure_yields_an_empty_report(#[case] commits: Vec<CommitChanges>) {
        let report = compute_change_entropy(&commits, Period::Week, open());
        assert_eq!(report.commit_count, 0, "got {report:?}");
        assert!(report.files.is_empty(), "got {report:?}");
        assert!(report.commit_entropy.is_empty(), "got {report:?}");
    }

    #[test]
    fn the_distribution_reports_where_a_value_sits() {
        let distribution = EntropyDistribution::new(vec![0.4, 0.0, 0.8, 0.2, 1.0]);
        assert_eq!(distribution.sample_count(), 5);
        assert!((distribution.median().unwrap_or(-1.0) - 0.4).abs() < 1e-12);
        assert!((distribution.percentile_rank(0.4).unwrap_or(-1.0) - 60.0).abs() < 1e-12);
        assert!((distribution.percentile_rank(1.0).unwrap_or(-1.0) - 100.0).abs() < 1e-12);
        assert!(distribution.percentile_rank(-0.1).unwrap_or(-1.0).abs() < 1e-12);
    }

    /// An empty sample has no median and no percentile: inventing one
    /// would let a report compare a pending change against a history it
    /// never read.
    #[test]
    fn an_empty_distribution_answers_nothing() {
        let distribution = EntropyDistribution::default();
        assert_eq!(distribution.median(), None);
        assert_eq!(distribution.percentile_rank(0.5), None);
    }

    #[rstest]
    #[case("crates/agent-lens/src/lib.rs", "crates/agent-lens/src")]
    #[case("README.md", ".")]
    #[case("a/b", "a")]
    #[case("/rooted", "/")]
    fn module_of_is_the_parent_directory(#[case] path: &str, #[case] expected: &str) {
        assert_eq!(module_of(path), expected);
    }

    proptest! {
        /// The DoD property, both halves: an even spread is maximal, and
        /// a single-file change set has no scatter at all.
        #[test]
        fn an_even_spread_is_always_maximal(count in 2usize..40, lines in 1u64..10_000) {
            let files: Vec<FileChange> = (0..count)
                .map(|i| FileChange { path: format!("f{i}"), lines })
                .collect();
            let scatter = Scatter::of(&files);
            prop_assert!((scatter.normalised - 1.0).abs() < 1e-9, "{scatter:?}");
        }

        #[test]
        fn one_file_never_scatters(lines in 1u64..10_000) {
            let scatter = Scatter::of(&[FileChange { path: "a".to_owned(), lines }]);
            prop_assert_eq!(scatter.normalised, 0.0);
        }

        /// Normalisation is what makes two periods of different size
        /// comparable, so leaving the unit range would silently break
        /// every comparison the metric exists for.
        #[test]
        fn normalised_scatter_stays_in_the_unit_range(
            weights in prop::collection::vec(0u64..5_000, 1..30),
        ) {
            let files: Vec<FileChange> = weights
                .iter()
                .enumerate()
                .map(|(i, lines)| FileChange { path: format!("f{i}"), lines: *lines })
                .collect();
            let scatter = Scatter::of(&files);
            prop_assert!((0.0..=1.0).contains(&scatter.normalised), "{scatter:?}");
            prop_assert!(scatter.bits >= 0.0, "{scatter:?}");
        }

        /// Moving weight onto the file that already has most of it is
        /// concentration, and concentration cannot read as more scatter.
        #[test]
        fn concentrating_weight_cannot_raise_scatter(
            others in prop::collection::vec(1u64..500, 1..12),
            extra in 1u64..5_000,
        ) {
            let mut files: Vec<FileChange> = others
                .iter()
                .enumerate()
                .map(|(i, lines)| FileChange { path: format!("f{i}"), lines: *lines })
                .collect();
            let leader = files.iter().map(|f| f.lines).max().unwrap_or(1);
            files.push(FileChange { path: "leader".to_owned(), lines: leader });
            let before = Scatter::of(&files);
            if let Some(last) = files.last_mut() {
                last.lines += extra;
            }
            let after = Scatter::of(&files);
            prop_assert!(after.normalised <= before.normalised + 1e-9, "{before:?} -> {after:?}");
        }

        /// Every period distributes exactly its own entropy, whatever
        /// the history looks like.
        #[test]
        fn contributions_always_decompose_the_period(
            weights in prop::collection::vec(1u64..500, 2..15),
        ) {
            let files: Vec<(String, u64)> = weights
                .iter()
                .enumerate()
                .map(|(i, lines)| (format!("f{i}"), *lines))
                .collect();
            let commits = vec![CommitChanges {
                date: "2026-08-17".to_owned(),
                files: files
                    .iter()
                    .map(|(path, lines)| FileChange { path: path.clone(), lines: *lines })
                    .collect(),
            }];
            let report = compute_change_entropy(&commits, Period::Week, open());
            let handed_out: f64 = report
                .files
                .iter()
                .flat_map(|f| f.periods.iter())
                .map(|p| p.contribution)
                .sum();
            let period = report.periods.first().map_or(0.0, |p| p.scatter.normalised);
            prop_assert!((handed_out - period).abs() < 1e-9, "{handed_out} vs {period}");
        }
    }
}
