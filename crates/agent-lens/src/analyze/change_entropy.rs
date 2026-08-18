//! `analyze change-entropy` — Hassan-style change scatter from git
//! history, and a pre-commit verdict on the pending change.
//!
//! `hotspot` already answers "this file changes a lot". The question it
//! cannot answer is whether the change activity *around* a file was
//! focused or smeared, which is the part Hassan (2009) found predictive:
//! a month of concentrated work is a different month from one in which
//! the same volume of edits was spread over thirty files, and the second
//! kind accumulates onto every file it touched.
//!
//! Two readings, which is why this is its own surface rather than a
//! column on `hotspot`:
//!
//! * **Retrospective** (the default): per-period entropy and the
//!   per-file history complexity it decomposes into. A high row is a
//!   landmine beyond what `commits × cognitive` already says.
//! * **Pre-commit** (`--diff-only`): the pending working-tree change as
//!   one change set — files touched, modules spanned, its entropy, and
//!   where that sits in the distribution of commits this repository
//!   actually makes. A scattered edit is a prompt to split the commit,
//!   which is feedback only worth anything before the commit exists.
//!
//! Like `co-change`, this reads `git log` and never parses a file, so it
//! has no language matrix: `.toml`, `.md`, workflow YAML and fixtures
//! all count. Paths are emitted in the same repo-root-relative space
//! `hotspot`, `risk` and `co-change` use, so joining a row against a
//! churn or centrality row is a key lookup.
//!
//! The arithmetic, and the definitions behind every figure, live in
//! [`lens_domain::change_entropy`]. What lives here is the git reading,
//! the path filtering, and the two report shapes.
//!
//! Limitations, restated in the report because a scatter number is easy
//! to over-read:
//!
//! * This is a prior, never a gate. A high row says change around the
//!   file was unfocused; it does not say the file is wrong.
//! * Squash-merge workflows collapse a branch into one commit, which
//!   raises measured scatter for reasons that have nothing to do with
//!   the code. `--max-commit-files` drops the worst offenders whole, and
//!   how often it fired is reported.
//! * A shallow clone reads a truncated history, so the reference
//!   distribution a `--diff-only` verdict compares against is drawn from
//!   whatever commits survived the graft. The report says so.
//! * Pathspec scoping cuts both ways: pointing at one directory measures
//!   only the part of each change that landed in it, so a repo-wide
//!   verdict wants a repo-wide path.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use lens_domain::{
    ChangeEntropyReport, ChangeEntropyThresholds, DEFAULT_MAX_COMMIT_FILES,
    DEFAULT_MIN_COMMITS_PER_PERIOD, EntropyDistribution, FileChange, FileEntropy,
    FilePeriodContribution, PeriodEntropy, Scatter, compute_change_entropy, module_of,
};
use serde::Serialize;
use tracing::warn;

use super::churn::{ChurnScope, ReportablePaths};
use super::error_from::impl_from_churn_error;
use super::format::format_optional_f64;
use super::options::analyzer_options;
use super::{
    AnalyzePathFilter, AnalyzeRoots, DiffScope, OutputFormat, PathFilterError, parse_diff_range,
};

/// Report schema version, bumped when a consumer would have to change.
const SCHEMA_VERSION: u32 = 1;

/// The definitions, published in the output rather than only in the
/// docs: two tools' "change entropy" are not comparable unless each says
/// which weight, which base, and which normalisation it used.
const NOTE: &str = "Change scatter, not a defect count: a high row says change around this file \
     was unfocused, never that the file is wrong — treat it as a prior and do not gate on it. \
     `entropy` is Shannon entropy over the per-file share of changed lines (insertions + \
     deletions) in the change set, in bits, divided by log2(files) so change sets of different \
     size compare: 0 is all of it in one file, 1 is spread perfectly evenly. Periods are ISO \
     weeks or calendar months in UTC, never N days back from now, so two runs a day apart agree. \
     A period's entropy is attributed to its files by Hassan's weighted variant — each file \
     takes its share of the period's changed lines times the period's entropy — so a file column \
     decomposes the period column exactly. Squashed and tangled commits inflate scatter; commits \
     over --max-commit-files are dropped whole and counted under skipped_commit_count.";

/// Errors raised while running the change-entropy analyzer.
#[derive(Debug, thiserror::Error)]
pub enum ChangeEntropyError {
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `git` is missing or returned a non-zero exit status. The captured
    /// stderr is forwarded so the agent has a useful diagnostic.
    #[error("git failed: {}", stderr.trim_end())]
    Git { stderr: String },
    /// The provided path is not inside any git working tree.
    #[error("{path:?} is not inside a git working tree")]
    NotInGitRepo { path: PathBuf },
    #[error("failed to serialize report: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error(transparent)]
    PathFilter(#[from] PathFilterError),
}

impl_from_churn_error!(ChangeEntropyError);

/// The bucket `--period` names.
///
/// A mirror of [`lens_domain::Period`], which cannot derive
/// `clap::ValueEnum` because the domain crate does not depend on clap.
/// The spelling and the meaning live there; this is the flag.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum, serde::Deserialize, Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Period {
    /// ISO-8601 week, keyed `YYYY-Www`.
    #[default]
    Week,
    /// Calendar month, keyed `YYYY-MM`.
    Month,
}

impl Period {
    /// How the period is named in prose and in tables. Delegates to the
    /// domain spelling so the flag and the report cannot drift.
    fn as_str(self) -> &'static str {
        lens_domain::Period::from(self).as_str()
    }
}

impl From<Period> for lens_domain::Period {
    fn from(period: Period) -> Self {
        match period {
            Period::Week => Self::Week,
            Period::Month => Self::Month,
        }
    }
}

/// `analyze change-entropy` flags, and the
/// `[profile.<name>.change-entropy]` table.
///
/// Written out rather than generated by `analyzer_options!` because the
/// guards carry real clap defaults: `Default` and the `default_value_t`
/// attributes must agree, so both read the same consts from
/// `lens_domain::change_entropy`.
#[derive(Debug, Clone, clap::Args, serde::Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct ChangeEntropyOptions {
    /// Cap the markdown ranking to the top-N entries. JSON output always
    /// carries the full list.
    #[arg(long)]
    pub top: Option<usize>,
    /// Restrict history to commits in this `--since=` window. Accepts
    /// anything git's approxidate parser does (e.g. `90.days.ago`,
    /// `2024-01-01`). Every period, every file row, and the reference
    /// distribution are computed inside the window.
    #[arg(long)]
    pub since: Option<String>,
    /// The bucket entropy is measured over. Entropy is defined per
    /// period, not per repository, and both spellings are UTC and
    /// calendar-anchored so a commit's bucket does not depend on when
    /// the report was run.
    #[arg(long, value_enum, default_value_t = Period::Week)]
    pub period: Period,
    /// Report the pending working-tree change as one change set instead
    /// of the history: files touched, modules spanned, its entropy, and
    /// where that sits among the commits this repository makes. The
    /// history is still read, because a scatter figure with no reference
    /// distribution is not actionable.
    #[arg(long)]
    pub diff_only: bool,
    /// Like `--diff-only`, but for the change in the given git revision
    /// range, as `git diff <range>` (`HEAD~1..HEAD`, `main...topic`).
    /// Reads committed history instead of the working tree.
    #[arg(
        long,
        value_name = "RANGE",
        conflicts_with = "diff_only",
        value_parser = parse_diff_range,
    )]
    pub diff_range: Option<String>,
    /// Periods with fewer counted commits than this are omitted:
    /// entropy over two commits is noise wearing a number's clothes.
    #[arg(long, default_value_t = DEFAULT_MIN_COMMITS_PER_PERIOD)]
    pub min_commits: u32,
    /// Commits touching more files than this take part in nothing — not
    /// the periods, not the file rows, not the reference distribution. A
    /// squash merge would otherwise read as one enormously scattered
    /// change. Same guard, same default, as `co-change`.
    #[arg(long, default_value_t = DEFAULT_MAX_COMMIT_FILES)]
    pub max_commit_files: usize,
}

impl Default for ChangeEntropyOptions {
    fn default() -> Self {
        Self {
            top: None,
            since: None,
            period: Period::Week,
            diff_only: false,
            diff_range: None,
            min_commits: DEFAULT_MIN_COMMITS_PER_PERIOD,
            max_commit_files: DEFAULT_MAX_COMMIT_FILES,
        }
    }
}

analyzer_options!(@diff_accessors ChangeEntropyOptions);

/// Stateful change-entropy runner.
///
/// The default guards come from [`ChangeEntropyThresholds::default`],
/// the same consts [`ChangeEntropyOptions`] spells as its
/// `default_value_t`, so an unconfigured analyzer and a bare command
/// line agree.
#[derive(Debug, Clone, Default)]
pub struct ChangeEntropyAnalyzer {
    since: Option<String>,
    top: Option<usize>,
    period: Period,
    diff: DiffScope,
    thresholds: ChangeEntropyThresholds,
    path_filter: AnalyzePathFilter,
}

impl ChangeEntropyAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a whole [`ChangeEntropyOptions`] group. The CLI flags and
    /// the `[profile.<name>.change-entropy]` table are the same type, so
    /// this is the only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: ChangeEntropyOptions) -> Self {
        let diff = opts.diff_scope();
        self.with_top(opts.top)
            .with_since_opt(opts.since)
            .with_period(opts.period)
            .with_diff_scope(diff)
            .with_min_commits(opts.min_commits)
            .with_max_commit_files(opts.max_commit_files)
    }

    /// Restrict history to commits made in the given git `--since=`
    /// window. Anything git's `approxidate` parser accepts works.
    pub fn with_since(mut self, since: impl Into<String>) -> Self {
        self.since = Some(since.into());
        self
    }

    /// Like [`Self::with_since`] but accepts an `Option`, leaving the
    /// window unchanged when `None` is passed.
    pub fn with_since_opt(mut self, since: Option<String>) -> Self {
        if let Some(s) = since {
            self.since = Some(s);
        }
        self
    }

    /// Cap the markdown report's tables to the top-N entries. JSON
    /// output always carries the full list. `None` keeps every row.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    pub fn with_period(mut self, period: Period) -> Self {
        self.period = period;
        self
    }

    /// Report the change the given diff names instead of the history.
    pub fn with_diff_scope(mut self, diff: DiffScope) -> Self {
        self.diff = diff;
        self
    }

    pub fn with_diff_only(self, diff_only: bool) -> Self {
        self.with_diff_scope(DiffScope::new(diff_only, None))
    }

    pub fn with_min_commits(mut self, min_commits: u32) -> Self {
        self.thresholds.min_commits_per_period = min_commits;
        self
    }

    pub fn with_max_commit_files(mut self, max_commit_files: usize) -> Self {
        self.thresholds.max_commit_files = max_commit_files;
        self
    }

    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_only_tests(only_tests);
        self
    }

    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_exclude_tests(exclude_tests);
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.path_filter = self.path_filter.with_exclude_patterns(exclude);
        self
    }

    /// Read `roots`' history and produce a change-entropy report in
    /// `format`. Accepts a single path or several — see [`AnalyzeRoots`];
    /// every root must sit in the same working tree.
    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, ChangeEntropyError> {
        let roots = roots.into();
        let scope = ChurnScope::resolve(&roots)?;
        let filter = self.path_filter.compile(scope.repo_root())?;
        let mut reportable = ReportablePaths::new(&filter, scope.repo_root());

        let shallow = scope.is_shallow();
        if shallow {
            warn!(
                "shallow clone: `git log` sees a truncated history, so periods before the graft \
                 are missing and the reference distribution a --diff-only verdict compares \
                 against is drawn from whatever survived. Fetch the full history \
                 (`git fetch --unshallow`) for a usable report."
            );
        }

        let mut commits = scope.collect_commit_changes(self.since.as_deref())?;
        for commit in &mut commits {
            commit.files.retain(|file| reportable.keeps(&file.path));
        }
        let report = compute_change_entropy(&commits, self.period.into(), self.thresholds);

        if !self.diff.is_enabled() {
            let view = HistoryView::new(&roots, &scope, self, shallow, &report);
            return self.render(&view, |view| format_history(view, self.top), format);
        }

        let mut pending = scope.collect_diff_changes(&self.diff)?;
        pending.retain(|file| reportable.keeps(&file.path));
        let view = VerdictView::new(&roots, &scope, self, shallow, &report, &pending);
        self.render(&view, |view| format_verdict(view, self.top), format)
    }

    fn render<V: Serialize>(
        &self,
        view: &V,
        markdown: impl Fn(&V) -> String,
        format: OutputFormat,
    ) -> Result<String, ChangeEntropyError> {
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(view).map_err(ChangeEntropyError::Serialize)
            }
            OutputFormat::Md => Ok(markdown(view)),
        }
    }
}

/// The fields both report shapes open with: what was analyzed, over
/// which window, under which guards.
#[derive(Debug, Serialize)]
struct ScopeView<'a> {
    schema_version: u32,
    target: String,
    repo_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<&'a str>,
    period: Period,
    note: &'static str,
    /// Set when `git log` is reading a truncated history, so a consumer
    /// of the JSON sees the same caveat the stderr warning carries.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    shallow_clone: bool,
    thresholds: ThresholdsView,
}

impl<'a> ScopeView<'a> {
    fn new(
        roots: &AnalyzeRoots,
        scope: &ChurnScope,
        analyzer: &'a ChangeEntropyAnalyzer,
        shallow_clone: bool,
    ) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            target: roots.display(),
            repo_root: scope.repo_root().display().to_string(),
            since: analyzer.since.as_deref(),
            period: analyzer.period,
            note: NOTE,
            shallow_clone,
            thresholds: ThresholdsView {
                min_commits_per_period: analyzer.thresholds.min_commits_per_period,
                max_commit_files: analyzer.thresholds.max_commit_files,
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct ThresholdsView {
    min_commits_per_period: u32,
    max_commit_files: usize,
}

/// The distribution a single change is read against, and the summary of
/// it a reader needs to place a number.
#[derive(Debug, Serialize)]
struct DistributionView {
    /// Commits the distribution was drawn from.
    commit_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    median: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p75: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    p90: Option<f64>,
}

impl DistributionView {
    fn new(distribution: &EntropyDistribution) -> Self {
        Self {
            commit_count: distribution.sample_count(),
            median: distribution.median(),
            p75: distribution.quantile(0.75),
            p90: distribution.quantile(0.90),
        }
    }
}

/// How many of a file's periods travel with its row.
///
/// The point of the column is "which periods put this number here", and
/// the three largest answer that. Every period a file changed in would
/// turn one row into a log.
const TOP_PERIODS_PER_FILE: usize = 3;

#[derive(Debug, Serialize)]
struct HistoryView<'a> {
    #[serde(flatten)]
    scope: ScopeView<'a>,
    commit_count: usize,
    skipped_commit_count: usize,
    thin_period_count: usize,
    period_count: usize,
    file_count: usize,
    commit_entropy: DistributionView,
    periods: Vec<PeriodView<'a>>,
    files: Vec<FileView<'a>>,
}

impl<'a> HistoryView<'a> {
    fn new(
        roots: &AnalyzeRoots,
        scope: &ChurnScope,
        analyzer: &'a ChangeEntropyAnalyzer,
        shallow_clone: bool,
        report: &'a ChangeEntropyReport,
    ) -> Self {
        Self {
            scope: ScopeView::new(roots, scope, analyzer, shallow_clone),
            commit_count: report.commit_count,
            skipped_commit_count: report.skipped_commit_count,
            thin_period_count: report.thin_period_count,
            period_count: report.periods.len(),
            file_count: report.file_count,
            commit_entropy: DistributionView::new(&report.commit_entropy),
            periods: report.periods.iter().map(PeriodView::from).collect(),
            files: report.files.iter().map(FileView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct PeriodView<'a> {
    period: &'a str,
    commits: usize,
    files: usize,
    changed_lines: u64,
    entropy: f64,
    entropy_bits: f64,
}

impl<'a> From<&'a PeriodEntropy> for PeriodView<'a> {
    fn from(period: &'a PeriodEntropy) -> Self {
        Self {
            period: period.key.as_str(),
            commits: period.commit_count,
            files: period.scatter.file_count,
            changed_lines: period.scatter.changed_lines,
            entropy: period.scatter.normalised,
            entropy_bits: period.scatter.bits,
        }
    }
}

#[derive(Debug, Serialize)]
struct FileView<'a> {
    path: &'a str,
    history_complexity: f64,
    commits: u32,
    changed_lines: u64,
    periods: usize,
    /// The periods that put this number here, largest first.
    top_periods: Vec<ContributionView<'a>>,
}

impl<'a> From<&'a FileEntropy> for FileView<'a> {
    fn from(file: &'a FileEntropy) -> Self {
        Self {
            path: file.path.as_str(),
            history_complexity: file.history_complexity,
            commits: file.commits,
            changed_lines: file.changed_lines,
            periods: file.periods.len(),
            top_periods: file
                .periods
                .iter()
                .take(TOP_PERIODS_PER_FILE)
                .map(ContributionView::from)
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct ContributionView<'a> {
    period: &'a str,
    share: f64,
    period_entropy: f64,
    contribution: f64,
}

impl<'a> From<&'a FilePeriodContribution> for ContributionView<'a> {
    fn from(contribution: &'a FilePeriodContribution) -> Self {
        Self {
            period: contribution.period.as_str(),
            share: contribution.share,
            period_entropy: contribution.period_entropy,
            contribution: contribution.contribution,
        }
    }
}

#[derive(Debug, Serialize)]
struct VerdictView<'a> {
    #[serde(flatten)]
    scope: ScopeView<'a>,
    /// Which diff was measured, spelled the way it was asked for.
    diff: String,
    pending: PendingView<'a>,
    reference: ReferenceView,
}

impl<'a> VerdictView<'a> {
    fn new(
        roots: &AnalyzeRoots,
        scope: &ChurnScope,
        analyzer: &'a ChangeEntropyAnalyzer,
        shallow_clone: bool,
        report: &ChangeEntropyReport,
        pending: &'a [FileChange],
    ) -> Self {
        let scatter = Scatter::of(pending);
        Self {
            scope: ScopeView::new(roots, scope, analyzer, shallow_clone),
            diff: match &analyzer.diff {
                DiffScope::Range(range) => range.clone(),
                _ => "working tree".to_owned(),
            },
            pending: PendingView::new(pending, scatter),
            reference: ReferenceView {
                distribution: DistributionView::new(&report.commit_entropy),
                percentile: report.commit_entropy.percentile_rank(scatter.normalised),
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct PendingView<'a> {
    files_touched: usize,
    modules_spanned: usize,
    changed_lines: u64,
    entropy: f64,
    entropy_bits: f64,
    modules: Vec<ModuleView<'a>>,
    files: Vec<PendingFileView<'a>>,
}

impl<'a> PendingView<'a> {
    fn new(pending: &'a [FileChange], scatter: Scatter) -> Self {
        let total = scatter.changed_lines as f64;
        let mut files: Vec<PendingFileView<'a>> = pending
            .iter()
            .filter(|file| file.lines > 0)
            .map(|file| PendingFileView {
                path: file.path.as_str(),
                changed_lines: file.lines,
                share: if total > 0.0 {
                    file.lines as f64 / total
                } else {
                    0.0
                },
            })
            .collect();
        files.sort_by(|x, y| {
            y.changed_lines
                .cmp(&x.changed_lines)
                .then_with(|| x.path.cmp(y.path))
        });

        let mut modules: BTreeMap<&'a str, ModuleView<'a>> = BTreeMap::new();
        for file in &files {
            let module = modules.entry(module_of(file.path)).or_insert(ModuleView {
                module: module_of(file.path),
                files: 0,
                changed_lines: 0,
            });
            module.files += 1;
            module.changed_lines += file.changed_lines;
        }
        let mut modules: Vec<ModuleView<'a>> = modules.into_values().collect();
        modules.sort_by(|x, y| {
            y.changed_lines
                .cmp(&x.changed_lines)
                .then_with(|| x.module.cmp(y.module))
        });

        Self {
            files_touched: scatter.file_count,
            modules_spanned: modules.len(),
            changed_lines: scatter.changed_lines,
            entropy: scatter.normalised,
            entropy_bits: scatter.bits,
            modules,
            files,
        }
    }
}

#[derive(Debug, Serialize)]
struct ModuleView<'a> {
    module: &'a str,
    files: usize,
    changed_lines: u64,
}

#[derive(Debug, Serialize)]
struct PendingFileView<'a> {
    path: &'a str,
    changed_lines: u64,
    share: f64,
}

#[derive(Debug, Serialize)]
struct ReferenceView {
    #[serde(flatten)]
    distribution: DistributionView,
    /// Percentage of counted commits scattering no more than the pending
    /// change does. Absent when the window held no commit to compare
    /// against — an invented percentile would read as a judgement drawn
    /// from a history nothing read.
    #[serde(skip_serializing_if = "Option::is_none")]
    percentile: Option<f64>,
}

const DEFAULT_TOP: usize = 20;

/// Decimals every entropy figure is rendered to. Two: a scatter figure
/// is a prior read against a median, not a measurement to three places.
const ENTROPY_PRECISION: usize = 2;

fn format_scope(out: &mut String, scope: &ScopeView<'_>) {
    let _ = writeln!(out, "\n{}", scope.note);
    if scope.shallow_clone {
        out.push_str(
            "\n**Shallow clone**: the log is truncated, so periods before the graft are missing \
             and the reference distribution is drawn from whatever survived. Run \
             `git fetch --unshallow` before trusting this report.\n",
        );
    }
}

fn format_history(view: &HistoryView<'_>, top: Option<usize>) -> String {
    let window = view
        .scope
        .since
        .map_or_else(String::new, |since| format!(", since {since}"));
    let mut out = format!(
        "# Change entropy: {} ({} {}(s) over {} commit(s){window})\n",
        view.scope.target,
        view.period_count,
        view.scope.period.as_str(),
        view.commit_count,
    );
    format_scope(&mut out, &view.scope);

    let _ = writeln!(
        &mut out,
        "\n## Summary\n\
         - commits: {} counted, {} skipped over `--max-commit-files {}`\n\
         - {}s: {} reported, {} omitted under `--min-commits {}`\n\
         - files: {}\n\
         - commit scatter: median {}, p75 {}, p90 {} over {} commit(s)",
        view.commit_count,
        view.skipped_commit_count,
        view.scope.thresholds.max_commit_files,
        view.scope.period.as_str(),
        view.period_count,
        view.thin_period_count,
        view.scope.thresholds.min_commits_per_period,
        view.file_count,
        format_optional_f64(view.commit_entropy.median, ENTROPY_PRECISION),
        format_optional_f64(view.commit_entropy.p75, ENTROPY_PRECISION),
        format_optional_f64(view.commit_entropy.p90, ENTROPY_PRECISION),
        view.commit_entropy.commit_count,
    );

    if view.files.is_empty() {
        out.push_str(if view.commit_count == 0 {
            "\n_No commits matched._\n"
        } else {
            "\n_No period met `--min-commits`._\n"
        });
        return out;
    }

    let limit = top.unwrap_or(DEFAULT_TOP);
    let _ = writeln!(
        &mut out,
        "\n## Top {limit} files by history complexity (Σ share × {} entropy)\n",
        view.scope.period.as_str(),
    );
    let _ = writeln!(
        &mut out,
        "| file | hcm | commits | lines | {}s | top {}s |",
        view.scope.period.as_str(),
        view.scope.period.as_str(),
    );
    let _ = writeln!(&mut out, "| --- | ---: | ---: | ---: | ---: | --- |");
    for file in view.files.iter().take(limit) {
        let periods = file
            .top_periods
            .iter()
            .map(|p| format!("{} {:.2}", p.period, p.contribution))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            &mut out,
            "| {} | {:.3} | {} | {} | {} | {periods} |",
            file.path, file.history_complexity, file.commits, file.changed_lines, file.periods,
        );
    }
    let overflow = view.files.len().saturating_sub(limit);
    if overflow > 0 {
        let _ = writeln!(
            &mut out,
            "\n+{overflow} more file(s) not shown (raise `--top`; JSON carries every row)."
        );
    }

    let _ = writeln!(
        &mut out,
        "\n## {}s, newest first\n",
        view.scope.period.as_str(),
    );
    let _ = writeln!(
        &mut out,
        "| {} | entropy | bits | commits | files | lines |",
        view.scope.period.as_str(),
    );
    let _ = writeln!(&mut out, "| --- | ---: | ---: | ---: | ---: | ---: |");
    for period in view.periods.iter().take(limit) {
        let _ = writeln!(
            &mut out,
            "| {} | {:.2} | {:.2} | {} | {} | {} |",
            period.period,
            period.entropy,
            period.entropy_bits,
            period.commits,
            period.files,
            period.changed_lines,
        );
    }
    let overflow = view.periods.len().saturating_sub(limit);
    if overflow > 0 {
        let _ = writeln!(
            &mut out,
            "\n+{overflow} more {}(s) not shown (raise `--top`; JSON carries every row).",
            view.scope.period.as_str(),
        );
    }
    out
}

fn format_verdict(view: &VerdictView<'_>, top: Option<usize>) -> String {
    let pending = &view.pending;
    let mut out = format!(
        "# Change entropy of the pending change: {} ({})\n",
        view.scope.target, view.diff,
    );
    format_scope(&mut out, &view.scope);

    if pending.files_touched == 0 {
        out.push_str("\n_No changed lines in this diff._\n");
        return out;
    }

    let _ = writeln!(
        &mut out,
        "\n## Verdict\n\n\
         | files | modules | lines | entropy | repo median | p90 | percentile |\n\
         | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n\
         | {} | {} | {} | {:.2} | {} | {} | {} |",
        pending.files_touched,
        pending.modules_spanned,
        pending.changed_lines,
        pending.entropy,
        format_optional_f64(view.reference.distribution.median, ENTROPY_PRECISION),
        format_optional_f64(view.reference.distribution.p90, ENTROPY_PRECISION),
        view.reference
            .percentile
            .map_or_else(|| "n/a".to_owned(), |p| format!("p{p:.0}")),
    );
    let _ = writeln!(
        &mut out,
        "\nRead against {} counted commit(s) in the window. A percentile near the top with more \
         than a couple of modules spanned is the case for splitting this into separate commits; \
         one file carrying most of the lines is a focused change however many files it touched.",
        view.reference.distribution.commit_count,
    );

    let limit = top.unwrap_or(DEFAULT_TOP);
    let _ = writeln!(&mut out, "\n## Modules\n\n| module | files | lines |");
    let _ = writeln!(&mut out, "| --- | ---: | ---: |");
    for module in pending.modules.iter().take(limit) {
        let _ = writeln!(
            &mut out,
            "| {} | {} | {} |",
            module.module, module.files, module.changed_lines,
        );
    }

    let _ = writeln!(&mut out, "\n## Files\n\n| file | lines | share |");
    let _ = writeln!(&mut out, "| --- | ---: | ---: |");
    for file in pending.files.iter().take(limit) {
        let _ = writeln!(
            &mut out,
            "| {} | {} | {:.2} |",
            file.path, file.changed_lines, file.share,
        );
    }
    let overflow = pending.files.len().saturating_sub(limit);
    if overflow > 0 {
        let _ = writeln!(
            &mut out,
            "\n+{overflow} more file(s) not shown (raise `--top`; JSON carries every row)."
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::test_support::{run_git, write_file};
    use rstest::rstest;

    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
    }

    /// Commit `files` (path, line count) on a fixed author date, which
    /// is what makes the period keys in these assertions the same
    /// whenever the suite runs.
    fn commit(dir: &Path, date: &str, files: &[(&str, usize)]) {
        for (path, lines) in files {
            write_file(dir, path, &body(*lines));
        }
        run_git(dir, &["add", "-A"]);
        run_git(
            dir,
            &[
                "commit",
                "-q",
                "-m",
                date,
                &format!("--date={date}T12:00:00Z"),
            ],
        );
    }

    fn body(lines: usize) -> String {
        (0..lines).map(|i| format!("// line {i}\n")).collect()
    }

    /// Three scattered weekly commits — one new ten-line file each, so
    /// the week spreads evenly over three files — followed by one
    /// focused commit in the next week. The two periods sit either side
    /// of the ISO week boundary: 2026-08-16 is a Sunday.
    fn init_history(dir: &Path) {
        init_repo(dir);
        commit(dir, "2026-08-10", &[("src/a.rs", 10)]);
        commit(dir, "2026-08-12", &[("src/b.rs", 10)]);
        commit(dir, "2026-08-16", &[("src/c.rs", 10)]);
        commit(dir, "2026-08-17", &[("src/focus.rs", 30)]);
    }

    fn analyzer() -> ChangeEntropyAnalyzer {
        // The default of 3 would drop the single-commit week these
        // fixtures use to show a *focused* period.
        ChangeEntropyAnalyzer::new().with_min_commits(1)
    }

    fn json(analyzer: &ChangeEntropyAnalyzer, dir: &Path) -> serde_json::Value {
        let out = analyzer.analyze(dir, OutputFormat::Json).unwrap();
        serde_json::from_str(&out).unwrap()
    }

    fn file_row<'a>(parsed: &'a serde_json::Value, path: &str) -> &'a serde_json::Value {
        parsed["files"]
            .as_array()
            .and_then(|files| files.iter().find(|f| f["path"] == path))
            .unwrap_or_else(|| panic!("no {path} row in {parsed}"))
    }

    fn close(value: &serde_json::Value, expected: f64) -> bool {
        value.as_f64().is_some_and(|v| (v - expected).abs() < 1e-9)
    }

    #[test]
    fn periods_are_iso_weeks_and_carry_the_entropy_of_their_spread() {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        let parsed = json(&analyzer(), dir.path());

        assert_eq!(parsed["commit_count"], 4, "got {parsed}");
        assert_eq!(parsed["file_count"], 4, "got {parsed}");
        let periods = parsed["periods"].as_array().unwrap();
        assert_eq!(periods.len(), 2, "got {parsed}");
        // Newest first. 2026-08-16 is a Sunday, so it closes W33.
        assert_eq!(periods[0]["period"], "2026-W34", "got {parsed}");
        assert!(close(&periods[0]["entropy"], 0.0), "got {parsed}");
        assert_eq!(periods[1]["period"], "2026-W33", "got {parsed}");
        assert_eq!(periods[1]["files"], 3, "got {parsed}");
        assert_eq!(periods[1]["changed_lines"], 30, "got {parsed}");
        // Three files, evenly: maximal scatter, and log2(3) bits of it.
        assert!(close(&periods[1]["entropy"], 1.0), "got {parsed}");
        assert!(
            close(&periods[1]["entropy_bits"], 3f64.log2()),
            "got {parsed}",
        );
    }

    /// The attribution rule, end to end: each of the scattered week's
    /// three files takes a third of its entropy, and the file that had a
    /// week to itself takes none — which is the whole point of measuring
    /// scatter rather than churn, since `focus.rs` has the most changed
    /// lines of any file here.
    #[test]
    fn history_complexity_ranks_scattered_files_over_a_focused_one() {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        let parsed = json(&analyzer(), dir.path());

        let scattered = file_row(&parsed, "src/a.rs");
        assert!(
            close(&scattered["history_complexity"], 1.0 / 3.0),
            "got {parsed}",
        );
        assert_eq!(scattered["commits"], 1, "got {parsed}");
        assert_eq!(scattered["changed_lines"], 10, "got {parsed}");
        assert_eq!(scattered["top_periods"][0]["period"], "2026-W33");

        let focused = file_row(&parsed, "src/focus.rs");
        assert!(close(&focused["history_complexity"], 0.0), "got {parsed}");
        assert_eq!(focused["changed_lines"], 30, "got {parsed}");
        let ranked = parsed["files"].as_array().unwrap();
        assert_eq!(
            ranked[ranked.len() - 1]["path"],
            "src/focus.rs",
            "got {parsed}"
        );
    }

    #[test]
    fn the_period_flag_merges_what_weeks_split() {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        let parsed = json(&analyzer().with_period(Period::Month), dir.path());
        let periods = parsed["periods"].as_array().unwrap();
        assert_eq!(periods.len(), 1, "got {parsed}");
        assert_eq!(periods[0]["period"], "2026-08", "got {parsed}");
        assert_eq!(periods[0]["commits"], 4, "got {parsed}");
    }

    #[test]
    fn a_period_under_the_commit_floor_is_omitted_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        let parsed = json(
            &ChangeEntropyAnalyzer::new().with_min_commits(3),
            dir.path(),
        );
        let periods = parsed["periods"].as_array().unwrap();
        assert_eq!(periods.len(), 1, "got {parsed}");
        assert_eq!(periods[0]["period"], "2026-W33", "got {parsed}");
        assert_eq!(parsed["thin_period_count"], 1, "got {parsed}");
        // The dropped week takes its file with it, but not its commit:
        // the reference distribution is about commits.
        assert!(
            parsed["files"]
                .as_array()
                .is_some_and(|files| files.iter().all(|f| f["path"] != "src/focus.rs")),
            "got {parsed}",
        );
        assert_eq!(parsed["commit_entropy"]["commit_count"], 4, "got {parsed}");
    }

    /// The pre-commit reading. The verdict has to carry a reference
    /// distribution: 0.92 means nothing without knowing what this
    /// repository's commits usually look like.
    #[test]
    fn diff_only_measures_the_pending_change_against_the_history() {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        write_file(dir.path(), "src/a.rs", &body(20));
        write_file(dir.path(), "docs/guide.md", &body(20));
        run_git(dir.path(), &["add", "-N", "docs/guide.md"]);

        let parsed = json(&analyzer().with_diff_only(true), dir.path());
        let pending = &parsed["pending"];
        assert_eq!(pending["files_touched"], 2, "got {parsed}");
        assert_eq!(pending["modules_spanned"], 2, "got {parsed}");
        assert!(
            pending["entropy"].as_f64().unwrap_or(-1.0) > 0.0,
            "got {parsed}"
        );
        assert_eq!(parsed["diff"], "working tree", "got {parsed}");
        assert_eq!(parsed["reference"]["commit_count"], 4, "got {parsed}");
        assert!(
            parsed["reference"]["percentile"].is_number(),
            "got {parsed}"
        );
        let modules: Vec<&str> = pending["modules"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m["module"].as_str())
            .collect();
        assert!(modules.contains(&"docs"), "got {parsed}");
        assert!(modules.contains(&"src"), "got {parsed}");
        // The verdict replaces the history listing rather than joining it.
        assert!(parsed.get("files").is_none(), "got {parsed}");
        assert!(parsed.get("periods").is_none(), "got {parsed}");
    }

    /// Two files touched equally is maximal scatter by definition, and
    /// the shares that produced it travel with the verdict — a number
    /// with no visible inputs is one an agent cannot check.
    #[test]
    fn the_verdict_publishes_the_shares_behind_its_entropy() {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        write_file(dir.path(), "src/a.rs", &body(20));
        write_file(dir.path(), "src/b.rs", &body(20));

        let parsed = json(&analyzer().with_diff_only(true), dir.path());
        assert!(close(&parsed["pending"]["entropy"], 1.0), "got {parsed}");
        let files = parsed["pending"]["files"].as_array().unwrap();
        assert_eq!(files.len(), 2, "got {parsed}");
        assert!(close(&files[0]["share"], 0.5), "got {parsed}");
        assert_eq!(
            files[0]["changed_lines"].as_u64().unwrap_or(0),
            files[1]["changed_lines"].as_u64().unwrap_or(0),
            "got {parsed}",
        );
    }

    #[rstest]
    #[case::json(OutputFormat::Json, "\"files_touched\": 0")]
    #[case::md(OutputFormat::Md, "_No changed lines in this diff._")]
    fn a_clean_working_tree_has_no_pending_change(
        #[case] format: OutputFormat,
        #[case] needle: &str,
    ) {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        let out = analyzer()
            .with_diff_only(true)
            .analyze(dir.path(), format)
            .unwrap();
        assert!(out.contains(needle), "got {out}");
    }

    #[test]
    fn a_diff_range_measures_a_committed_change_instead() {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        let parsed = json(
            &analyzer().with_diff_scope(DiffScope::Range("HEAD~1..HEAD".to_owned())),
            dir.path(),
        );
        assert_eq!(parsed["diff"], "HEAD~1..HEAD", "got {parsed}");
        assert_eq!(parsed["pending"]["files_touched"], 1, "got {parsed}");
        assert_eq!(parsed["pending"]["files"][0]["path"], "src/focus.rs");
    }

    /// The one analyzer with no language matrix has to prove it: a
    /// `.md` / `.toml` pair no AST-based analyzer here can even open.
    #[test]
    fn non_source_files_are_covered() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit(dir.path(), "2026-08-10", &[("notes.md", 10)]);
        commit(dir.path(), "2026-08-11", &[("app.toml", 10)]);
        let parsed = json(&analyzer(), dir.path());
        assert_eq!(parsed["file_count"], 2, "got {parsed}");
        file_row(&parsed, "notes.md");
        file_row(&parsed, "app.toml");
    }

    /// History is full of files that were deleted months ago, and an
    /// agent cannot go read one.
    #[test]
    fn a_file_no_longer_in_the_tree_leaves_the_report() {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        run_git(dir.path(), &["rm", "-q", "src/a.rs"]);
        run_git(
            dir.path(),
            &["commit", "-q", "-m", "drop", "--date=2026-08-18T12:00:00Z"],
        );
        let parsed = json(&analyzer(), dir.path());
        assert!(
            parsed["files"]
                .as_array()
                .is_some_and(|files| files.iter().all(|f| f["path"] != "src/a.rs")),
            "got {parsed}",
        );
    }

    #[test]
    fn exclude_patterns_drop_files_from_every_figure() {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        let parsed = json(
            &analyzer().with_exclude_patterns(vec!["src/a.rs".to_owned()]),
            dir.path(),
        );
        assert_eq!(parsed["file_count"], 3, "got {parsed}");
        assert!(
            parsed["files"]
                .as_array()
                .is_some_and(|files| files.iter().all(|f| f["path"] != "src/a.rs")),
            "got {parsed}",
        );
    }

    /// The definitions are part of the output on purpose: two tools'
    /// "change entropy" are not comparable without them.
    #[rstest]
    #[case::history(false)]
    #[case::verdict(true)]
    fn every_report_publishes_its_definitions(#[case] diff_only: bool) {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        let out = analyzer()
            .with_diff_only(diff_only)
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(out.contains("divided by log2(files)"), "got {out}");
        assert!(
            out.contains("ISO weeks or calendar months in UTC"),
            "got {out}"
        );
    }

    #[test]
    fn the_markdown_report_names_the_period_it_bucketed_by() {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        let out = analyzer()
            .with_period(Period::Month)
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(out.contains("1 month(s) over 4 commit(s)"), "got {out}");
        assert!(out.contains("| 2026-08 |"), "got {out}");
    }

    /// The two empty reports mean different things — no history in the
    /// window at all, versus history every period floor rejected — so
    /// they must not say the same thing.
    #[rstest]
    #[case::window_matched_nothing(
        ChangeEntropyAnalyzer::new().with_since("2099-01-01"),
        "_No commits matched._",
    )]
    #[case::window_matched_nothing_via_option(
        ChangeEntropyAnalyzer::new().with_since_opt(Some("2099-01-01".to_owned())),
        "_No commits matched._",
    )]
    #[case::every_period_was_too_thin(
        ChangeEntropyAnalyzer::new().with_min_commits(99),
        "_No period met `--min-commits`._",
    )]
    fn an_empty_report_says_which_kind_of_empty_it_is(
        #[case] analyzer: ChangeEntropyAnalyzer,
        #[case] expected: &str,
    ) {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        let out = analyzer.analyze(dir.path(), OutputFormat::Md).unwrap();
        assert!(out.contains(expected), "got {out}");
    }

    /// `--top` is a rendering cap, not a filter, so what it hid has to be
    /// visible — and an uncapped report must not carry the line at all,
    /// which is what stops "+0 more" from ever being printed.
    #[test]
    fn top_caps_the_markdown_tables_and_names_what_it_hid() {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        let capped = analyzer()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(capped.contains("+3 more file(s) not shown"), "got {capped}");
        assert!(capped.contains("+1 more week(s) not shown"), "got {capped}");

        let full = analyzer().analyze(dir.path(), OutputFormat::Md).unwrap();
        assert!(!full.contains("not shown"), "got {full}");
    }

    #[test]
    fn the_verdict_file_table_is_capped_the_same_way() {
        let dir = tempfile::tempdir().unwrap();
        init_history(dir.path());
        write_file(dir.path(), "src/a.rs", &body(20));
        write_file(dir.path(), "src/b.rs", &body(20));

        let capped = analyzer()
            .with_diff_only(true)
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(capped.contains("+1 more file(s) not shown"), "got {capped}");

        let full = analyzer()
            .with_diff_only(true)
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(!full.contains("not shown"), "got {full}");
    }

    #[test]
    fn a_path_outside_a_git_tree_is_reported_as_such() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "lone.rs", "fn main() {}\n");
        let error = analyzer()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(
            matches!(error, ChangeEntropyError::NotInGitRepo { .. }),
            "got {error:?}",
        );
    }
}
