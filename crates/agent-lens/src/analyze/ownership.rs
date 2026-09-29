//! `analyze ownership` — author concentration and minor contributors per
//! file, from git history.
//!
//! Answers the question an agent should ask before an edit and no static
//! analyzer can: *does this file have an owner, or is it a commons many
//! people have touched shallowly?* On a low-ownership, high-churn file the
//! right move is to read more context, keep the change narrow, and route
//! the review deliberately.
//!
//! Like `co-change` this reads `git log` and never parses a file, so it
//! has no language matrix. Paths are emitted in the same
//! repo-root-relative space `hotspot`, `risk` and `co-change` use.
//!
//! # Identity normalisation
//!
//! Get this wrong and every figure is noise, so it is part of the
//! analyzer rather than a follow-up, and the rule is published in the
//! report because ownership numbers from tools that choose differently
//! are not comparable:
//!
//! * **mailmap**: authors come through `%aN` / `%aE`, and
//!   `Co-authored-by:` trailers through `git check-mailmap`, so one
//!   person with three addresses is one author. After that an identity
//!   is keyed by its lowercased email (its name when it has none).
//! * **Bots** are dropped by a glob over `Name <email>` (default
//!   `*[bot]*`), and what was dropped is counted rather than silently
//!   removed. A commit whose every author was a bot is dropped whole.
//! * **`Co-authored-by:` trailers** are authors. Agent-assisted commits
//!   attribute authorship there, and without them a repository like this
//!   one reports a single author for everything. A co-authored commit
//!   splits its one unit of credit evenly between its authors.
//!
//! The arithmetic lives in [`lens_domain::ownership`].
//!
//! Limitations, restated in the report:
//!
//! * A repository with one author has degenerate ownership by
//!   definition, and gets a note instead of a ranking.
//! * Commit counts are not lines: a formatting sweep is one commit like
//!   any other. Blame-based ownership is more faithful and far slower,
//!   and is not implemented here.
//! * Socio-technical congruence (who *talks* to whom) is out of scope:
//!   it needs review and issue traffic, which `git log` does not carry.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::PathBuf;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use lens_domain::{
    AuthoredCommit, DEFAULT_MINOR_THRESHOLD, FileOwnership, OwnershipThresholds, compute_ownership,
};
use serde::Serialize;
use tracing::warn;

use super::churn::{AuthoredCommitRaw, ChurnScope, Contact, ReportablePaths};
use super::error_from::impl_from_churn_error;
use super::{AnalyzePathFilter, AnalyzeRoots, OutputFormat, PathFilterError};

/// Report schema version, bumped when a consumer would have to change.
const SCHEMA_VERSION: u32 = 1;

/// The bot globs used when `--bot-pattern` is not given: GitHub's
/// `[bot]` suffix, which dependabot, renovate and github-actions all
/// carry in their author name and email.
pub const DEFAULT_BOT_PATTERNS: [&str; 1] = [r"*\[bot\]*"];

/// Contributors listed per file row. The top author is the finding; the
/// next two say whether ownership is contested or merely diluted.
const TOP_AUTHORS_PER_FILE: usize = 3;

/// The definitions, published in the output: two tools' "ownership" are
/// not comparable unless each says how it counted authors.
const NOTE: &str = "Ownership from git history, a prior for how carefully to edit, not a verdict \
     on the file. Each counted commit gives one unit of credit per file it touched, split evenly \
     between its author and every Co-authored-by trailer; `top_author_share` is the largest \
     author's credit over the file's commits, and a contributor under --minor-threshold of them \
     is minor. Identities go through .mailmap (trailers too, via git check-mailmap) and are then \
     keyed by lowercased email. Authors matching a --bot-pattern are dropped and counted; a \
     commit left with no author is dropped whole. Rows rank by commits × (1 − top_author_share): \
     low ownership weighted by churn. Commit counts, not lines: a formatting sweep counts like \
     any other commit.";

/// Errors raised while running the ownership analyzer.
#[derive(Debug, thiserror::Error)]
pub enum OwnershipError {
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
    /// A share is a fraction of a file's commits, so a threshold outside
    /// `[0.0, 1.0]` either marks nobody or everybody as minor — an answer
    /// to a question that was not asked.
    #[error("--minor-threshold must be within [0.0, 1.0]; got {value}")]
    MinorThresholdOutOfRange { value: f64 },
    #[error("invalid --bot-pattern {pattern:?}: {source}")]
    BotPattern {
        pattern: String,
        #[source]
        source: globset::Error,
    },
    #[error("failed to serialize report: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error(transparent)]
    PathFilter(#[from] PathFilterError),
}

impl_from_churn_error!(OwnershipError);

/// `analyze ownership` flags, and the `[profile.<name>.ownership]` table.
///
/// Written out rather than generated by `analyzer_options!` because
/// `minor-threshold` carries a real clap default that `Default` must
/// agree with.
#[derive(Debug, Clone, clap::Args, serde::Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct OwnershipOptions {
    /// Cap the markdown ranking to the top-N entries. JSON output always
    /// carries the full list.
    #[arg(long)]
    pub top: Option<usize>,
    /// Restrict history to commits in this `--since=` window. Accepts
    /// anything git's approxidate parser does (e.g. `90.days.ago`,
    /// `2024-01-01`).
    #[arg(long)]
    pub since: Option<String>,
    /// A contributor whose share of a file's commits is strictly below
    /// this is minor. Bird et al. use 0.05.
    #[arg(long, default_value_t = DEFAULT_MINOR_THRESHOLD)]
    pub minor_threshold: f64,
    /// Glob matched case-insensitively against `Name <email>`; a match
    /// is a bot and is dropped from authorship. Repeatable. Replaces the
    /// default `*\[bot\]*` rather than adding to it; `\` escapes a glob
    /// metacharacter.
    #[arg(long, value_name = "GLOB")]
    pub bot_pattern: Vec<String>,
    /// Keep bot authors instead of dropping them.
    #[arg(long)]
    pub include_bots: bool,
}

impl Default for OwnershipOptions {
    fn default() -> Self {
        Self {
            top: None,
            since: None,
            minor_threshold: DEFAULT_MINOR_THRESHOLD,
            bot_pattern: Vec::new(),
            include_bots: false,
        }
    }
}

/// Stateful ownership runner.
#[derive(Debug, Clone)]
pub struct OwnershipAnalyzer {
    since: Option<String>,
    top: Option<usize>,
    thresholds: OwnershipThresholds,
    bot_patterns: Vec<String>,
    include_bots: bool,
    path_filter: AnalyzePathFilter,
}

impl Default for OwnershipAnalyzer {
    fn default() -> Self {
        Self {
            since: None,
            top: None,
            thresholds: OwnershipThresholds::default(),
            bot_patterns: DEFAULT_BOT_PATTERNS.map(str::to_owned).to_vec(),
            include_bots: false,
            path_filter: AnalyzePathFilter::default(),
        }
    }
}

impl OwnershipAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a whole [`OwnershipOptions`] group. The CLI flags and the
    /// `[profile.<name>.ownership]` table are the same type, so this is
    /// the only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: OwnershipOptions) -> Self {
        let analyzer = self
            .with_top(opts.top)
            .with_since_opt(opts.since)
            .with_minor_threshold(opts.minor_threshold)
            .with_include_bots(opts.include_bots);
        if opts.bot_pattern.is_empty() {
            analyzer
        } else {
            analyzer.with_bot_patterns(opts.bot_pattern)
        }
    }

    /// Restrict history to commits made in the given git `--since=`
    /// window.
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

    /// Cap the markdown ranking to the top-N entries.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    pub fn with_minor_threshold(mut self, minor_threshold: f64) -> Self {
        self.thresholds.minor_threshold = minor_threshold;
        self
    }

    /// Replace the bot globs.
    pub fn with_bot_patterns(mut self, patterns: Vec<String>) -> Self {
        self.bot_patterns = patterns;
        self
    }

    pub fn with_include_bots(mut self, include_bots: bool) -> Self {
        self.include_bots = include_bots;
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

    /// Read `roots`' history and produce an ownership report in `format`.
    /// Every root must sit in the same working tree.
    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, OwnershipError> {
        let threshold = self.thresholds.minor_threshold;
        if !(0.0..=1.0).contains(&threshold) {
            return Err(OwnershipError::MinorThresholdOutOfRange { value: threshold });
        }
        let bots = self.compile_bot_patterns()?;
        let roots = roots.into();
        let scope = ChurnScope::resolve(&roots)?;
        let filter = self.path_filter.compile(scope.repo_root())?;
        let mut reportable = ReportablePaths::new(&filter, scope.repo_root());

        let shallow = scope.is_shallow();
        if shallow {
            warn!(
                "shallow clone: `git log` sees a truncated history, so authors before the graft \
                 are missing from every share. Fetch the full history (`git fetch --unshallow`) \
                 for a usable report."
            );
        }

        let mut raw = scope.collect_authored_commits(self.since.as_deref())?;
        for commit in &mut raw {
            commit.files.retain(|file| reportable.keeps(file));
        }
        raw.retain(|commit| !commit.files.is_empty());
        let co_authors = resolve_co_authors(&scope, &raw)?;
        let identities = normalise(&raw, &co_authors, bots.as_ref());
        let files = compute_ownership(&identities.commits, self.thresholds);

        let view = ReportView::new(&roots, &scope, self, shallow, &identities, &files);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&view).map_err(OwnershipError::Serialize)
            }
            OutputFormat::Md => Ok(format_md(&view, self.top)),
        }
    }

    /// The bot matcher, or `None` when bots are kept.
    fn compile_bot_patterns(&self) -> Result<Option<GlobSet>, OwnershipError> {
        if self.include_bots {
            return Ok(None);
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in &self.bot_patterns {
            let glob = GlobBuilder::new(pattern)
                .case_insensitive(true)
                .backslash_escape(true)
                .build()
                .map_err(|source| OwnershipError::BotPattern {
                    pattern: pattern.clone(),
                    source,
                })?;
            builder.add(glob);
        }
        builder
            .build()
            .map(Some)
            .map_err(|source| OwnershipError::BotPattern {
                pattern: self.bot_patterns.join(", "),
                source,
            })
    }
}

/// Split a `Name <email>` trailer value. A value with no `<email>` is all
/// name.
fn parse_contact(value: &str) -> Contact {
    let value = value.trim();
    if let Some(stripped) = value.strip_suffix('>')
        && let Some((name, email)) = stripped.rsplit_once('<')
    {
        return Contact {
            name: name.trim().to_owned(),
            email: email.trim().to_owned(),
        };
    }
    Contact {
        name: value.to_owned(),
        email: String::new(),
    }
}

/// Every distinct `Co-authored-by:` value, mapped through `.mailmap`.
///
/// Only values carrying an `<email>` go to `git check-mailmap`, which
/// rejects anything else; a bare name is used as written.
fn resolve_co_authors(
    scope: &ChurnScope,
    commits: &[AuthoredCommitRaw],
) -> Result<BTreeMap<String, Contact>, OwnershipError> {
    let distinct: BTreeSet<&str> = commits
        .iter()
        .flat_map(|commit| commit.co_authors.iter().map(String::as_str))
        .collect();
    let mut resolved = BTreeMap::new();
    let mut mailmappable = Vec::new();
    for value in distinct {
        let contact = parse_contact(value);
        if contact.email.is_empty() {
            resolved.insert(value.to_owned(), contact);
        } else {
            mailmappable.push(value.to_owned());
        }
    }
    if !mailmappable.is_empty() {
        let canonical: Vec<String> = mailmappable
            .iter()
            .map(|value| {
                let contact = parse_contact(value);
                format!("{} <{}>", contact.name, contact.email)
            })
            .collect();
        let mapped = scope.check_mailmap(&canonical)?;
        for (value, mapped) in mailmappable.into_iter().zip(mapped) {
            resolved.insert(value, parse_contact(&mapped));
        }
    }
    Ok(resolved)
}

/// Stable key for an identity after mailmap: its lowercased email, or its
/// lowercased name when it has none.
fn identity_key(contact: &Contact) -> String {
    if contact.email.is_empty() {
        contact.name.to_lowercase()
    } else {
        contact.email.to_lowercase()
    }
}

/// How an identity is spelled in the report: `Name <email>`, or the name
/// alone when there is no email.
fn spell(name: &str, email: &str) -> String {
    if email.is_empty() {
        name.to_owned()
    } else {
        format!("{name} <{email}>")
    }
}

/// The history after identity normalisation.
#[derive(Debug, Default)]
struct Identities {
    /// Counted commits, author keys only.
    commits: Vec<AuthoredCommit>,
    /// Author key → how the report spells it.
    display: BTreeMap<String, String>,
    /// Commits dropped whole because every author was a bot.
    bot_commit_count: usize,
    /// Bot identity (as spelled) → commits it was removed from.
    bots: BTreeMap<String, usize>,
    /// Counted commits carrying at least one co-author other than the
    /// author.
    co_authored_commit_count: usize,
}

/// Apply the identity rule to every commit: mailmapped contacts, bots
/// out, one key per person.
fn normalise(
    raw: &[AuthoredCommitRaw],
    co_authors: &BTreeMap<String, Contact>,
    bots: Option<&GlobSet>,
) -> Identities {
    let mut out = Identities::default();
    let mut spellings = Spellings::default();
    for commit in raw {
        let contacts = std::iter::once(&commit.author).chain(
            commit
                .co_authors
                .iter()
                .filter_map(|value| co_authors.get(value)),
        );
        let (humans, bots_here) = split_bots(contacts, bots);
        for bot in bots_here {
            *out.bots.entry(bot).or_insert(0) += 1;
        }
        let mut authors: Vec<String> = humans
            .into_iter()
            .filter_map(|contact| spellings.record(contact))
            .collect();
        authors.sort_unstable();
        authors.dedup();
        match authors.len() {
            0 => out.bot_commit_count += 1,
            n => {
                out.co_authored_commit_count += usize::from(n > 1);
                out.commits.push(AuthoredCommit {
                    authors,
                    files: commit.files.clone(),
                });
            }
        }
    }
    out.display = spellings.into_display();
    out
}

/// Separate a commit's contacts into people and bots, spelling each bot
/// once however many times it appears on the commit.
fn split_bots<'a>(
    contacts: impl Iterator<Item = &'a Contact>,
    bots: Option<&GlobSet>,
) -> (Vec<&'a Contact>, BTreeSet<String>) {
    let mut humans = Vec::new();
    let mut found = BTreeSet::new();
    for contact in contacts {
        let spelled = spell(&contact.name, &contact.email);
        if bots.is_some_and(|bots| bots.is_match(&spelled)) {
            found.insert(spelled);
        } else {
            humans.push(contact);
        }
    }
    (humans, found)
}

/// Identity key → spelling → occurrences, so the most common spelling of
/// a person wins the display and the choice does not depend on order.
#[derive(Debug, Default)]
struct Spellings(BTreeMap<String, BTreeMap<String, usize>>);

impl Spellings {
    /// Count one sighting of `contact` and return its key, or `None` for
    /// a contact with neither a name nor an email.
    fn record(&mut self, contact: &Contact) -> Option<String> {
        let key = identity_key(contact);
        if key.is_empty() {
            return None;
        }
        let spelled = spell(&contact.name, &contact.email.to_lowercase());
        *self
            .0
            .entry(key.clone())
            .or_default()
            .entry(spelled)
            .or_insert(0) += 1;
        Some(key)
    }

    /// Key → the spelling seen most often, ties to the smallest.
    fn into_display(self) -> BTreeMap<String, String> {
        self.0
            .into_iter()
            .map(|(key, spellings)| {
                let spelled = spellings
                    .into_iter()
                    .max_by(|(a, x), (b, y)| x.cmp(y).then_with(|| b.cmp(a)))
                    .map_or_else(|| key.clone(), |(spelled, _)| spelled);
                (key, spelled)
            })
            .collect()
    }
}

#[derive(Debug, Serialize)]
struct ReportView<'a> {
    schema_version: u32,
    target: String,
    repo_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<&'a str>,
    note: &'static str,
    /// Set when `git log` is reading a truncated history.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    shallow_clone: bool,
    minor_threshold: f64,
    /// The bot globs applied; empty when `--include-bots` kept them.
    bot_patterns: Vec<&'a str>,
    commit_count: usize,
    co_authored_commit_count: usize,
    bot_commit_count: usize,
    bots: Vec<BotView<'a>>,
    author_count: usize,
    file_count: usize,
    /// Set when the window holds a single author: ownership is then
    /// degenerate and `files` is left empty rather than ranked.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    single_author: bool,
    files: Vec<FileView<'a>>,
}

impl<'a> ReportView<'a> {
    fn new(
        roots: &AnalyzeRoots,
        scope: &ChurnScope,
        analyzer: &'a OwnershipAnalyzer,
        shallow_clone: bool,
        identities: &'a Identities,
        files: &'a [FileOwnership],
    ) -> Self {
        let author_count = identities
            .commits
            .iter()
            .flat_map(|commit| &commit.authors)
            .collect::<BTreeSet<_>>()
            .len();
        let single_author = author_count == 1;
        let mut bots: Vec<BotView<'a>> = identities
            .bots
            .iter()
            .map(|(identity, commits)| BotView {
                identity,
                commits: *commits,
            })
            .collect();
        bots.sort_by(|x, y| {
            y.commits
                .cmp(&x.commits)
                .then_with(|| x.identity.cmp(y.identity))
        });
        Self {
            schema_version: SCHEMA_VERSION,
            target: roots.display(),
            repo_root: scope.repo_root().display().to_string(),
            since: analyzer.since.as_deref(),
            note: NOTE,
            shallow_clone,
            minor_threshold: analyzer.thresholds.minor_threshold,
            bot_patterns: if analyzer.include_bots {
                Vec::new()
            } else {
                analyzer.bot_patterns.iter().map(String::as_str).collect()
            },
            commit_count: identities.commits.len(),
            co_authored_commit_count: identities.co_authored_commit_count,
            bot_commit_count: identities.bot_commit_count,
            bots,
            author_count,
            file_count: files.len(),
            single_author,
            files: if single_author {
                Vec::new()
            } else {
                files
                    .iter()
                    .map(|file| FileView::new(file, &identities.display))
                    .collect()
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct BotView<'a> {
    identity: &'a str,
    /// Commits this bot was removed from as an author.
    commits: usize,
}

#[derive(Debug, Serialize)]
struct FileView<'a> {
    path: &'a str,
    commits: u32,
    authors: usize,
    top_author: &'a str,
    top_author_share: f64,
    minor_contributors: usize,
    minor_ratio: f64,
    score: f64,
    /// The largest contributors, largest first.
    top_authors: Vec<AuthorView<'a>>,
}

impl<'a> FileView<'a> {
    fn new(file: &'a FileOwnership, display: &'a BTreeMap<String, String>) -> Self {
        let spelled = |key: &'a str| display.get(key).map_or(key, String::as_str);
        Self {
            path: file.path.as_str(),
            commits: file.commits,
            authors: file.authors.len(),
            top_author: spelled(file.top_author()),
            top_author_share: file.top_author_share,
            minor_contributors: file.minor_contributors,
            minor_ratio: file.minor_ratio,
            score: file.score,
            top_authors: file
                .authors
                .iter()
                .take(TOP_AUTHORS_PER_FILE)
                .map(|author| AuthorView {
                    author: spelled(author.author.as_str()),
                    credit: author.credit,
                    share: author.share,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct AuthorView<'a> {
    author: &'a str,
    credit: f64,
    share: f64,
}

const DEFAULT_TOP: usize = 20;

fn format_md(view: &ReportView<'_>, top: Option<usize>) -> String {
    let window = view
        .since
        .map_or_else(String::new, |since| format!(", since {since}"));
    let mut out = format!(
        "# Ownership: {} ({} commit(s), {} author(s){window})\n",
        view.target, view.commit_count, view.author_count,
    );
    let _ = writeln!(&mut out, "\n{}", view.note);
    if view.shallow_clone {
        out.push_str(
            "\n**Shallow clone**: the log is truncated, so authors before the graft are missing. \
             Run `git fetch --unshallow` before trusting this report.\n",
        );
    }

    let patterns = if view.bot_patterns.is_empty() {
        "kept (`--include-bots`)".to_owned()
    } else {
        format!("`{}`", view.bot_patterns.join("`, `"))
    };
    let _ = writeln!(
        &mut out,
        "\n## Summary\n\
         - commits: {} counted, {} with co-authors, {} dropped as bot-only\n\
         - bots: {patterns}",
        view.commit_count, view.co_authored_commit_count, view.bot_commit_count,
    );
    for bot in &view.bots {
        let _ = writeln!(
            &mut out,
            "  - {} removed from {} commit(s)",
            bot.identity, bot.commits
        );
    }
    let _ = writeln!(
        &mut out,
        "- authors: {}\n- files: {} (minor threshold {})",
        view.author_count, view.file_count, view.minor_threshold,
    );

    if view.commit_count == 0 {
        out.push_str("\n_No commits matched._\n");
        return out;
    }
    if view.single_author {
        out.push_str(
            "\n_One author in the window: ownership is degenerate here, so nothing is ranked._\n",
        );
        return out;
    }

    let contested: Vec<&FileView<'_>> = view.files.iter().filter(|f| f.score > 0.0).collect();
    if contested.is_empty() {
        out.push_str("\n_Every file has a single author._\n");
        return out;
    }
    let limit = top.unwrap_or(DEFAULT_TOP);
    let _ = writeln!(
        &mut out,
        "\n## Top {limit} files by low ownership × churn (commits × (1 − top share))\n\n\
         | file | commits | authors | top author | top share | minor | minor ratio |\n\
         | --- | ---: | ---: | --- | ---: | ---: | ---: |",
    );
    for file in contested.iter().take(limit) {
        let _ = writeln!(
            &mut out,
            "| {} | {} | {} | {} | {:.2} | {} | {:.2} |",
            file.path,
            file.commits,
            file.authors,
            file.top_author,
            file.top_author_share,
            file.minor_contributors,
            file.minor_ratio,
        );
    }
    let overflow = contested.len().saturating_sub(limit);
    if overflow > 0 {
        let _ = writeln!(
            &mut out,
            "\n+{overflow} more file(s) not shown (raise `--top`; JSON carries every row)."
        );
    }
    let solo = view.files.len() - contested.len();
    if solo > 0 {
        let _ = writeln!(
            &mut out,
            "\n{solo} file(s) with a single author are not ranked."
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
        run_git(dir, &["config", "user.email", "ann@example.com"]);
        run_git(dir, &["config", "user.name", "Ann"]);
    }

    /// Commit `files` as `author` (`Name <email>`), with `message` as the
    /// full commit message so trailers can ride along.
    fn commit_as(dir: &Path, author: &str, message: &str, files: &[&str]) {
        for (i, path) in files.iter().enumerate() {
            let existing = std::fs::read_to_string(dir.join(path)).unwrap_or_default();
            write_file(dir, path, &format!("{existing}// {author} {i}\n"));
        }
        run_git(dir, &["add", "-A"]);
        run_git(dir, &["commit", "-q", "--author", author, "-m", message]);
    }

    fn json(analyzer: &OwnershipAnalyzer, dir: &Path) -> serde_json::Value {
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
    fn shares_and_minor_contributors_come_from_commit_authors() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        for _ in 0..3 {
            commit_as(dir.path(), "Ann <ann@example.com>", "ann", &["src/a.rs"]);
        }
        commit_as(dir.path(), "Bob <bob@example.com>", "bob", &["src/a.rs"]);

        let parsed = json(&OwnershipAnalyzer::new(), dir.path());
        assert_eq!(parsed["commit_count"], 4, "got {parsed}");
        assert_eq!(parsed["author_count"], 2, "got {parsed}");
        let row = file_row(&parsed, "src/a.rs");
        assert_eq!(row["commits"], 4);
        assert_eq!(row["authors"], 2);
        assert_eq!(row["top_author"], "Ann <ann@example.com>");
        assert!(close(&row["top_author_share"], 0.75), "got {row}");
        assert!(close(&row["score"], 1.0), "got {row}");
        assert_eq!(row["minor_contributors"], 0);

        let strict = json(
            &OwnershipAnalyzer::new().with_minor_threshold(0.3),
            dir.path(),
        );
        assert_eq!(
            file_row(&strict, "src/a.rs")["minor_contributors"],
            1,
            "Bob's 0.25 is under 0.3: {strict}",
        );
    }

    /// One person, two addresses: without `.mailmap` two authors, with it
    /// one — which is the whole point of reading `%aN` / `%aE`.
    #[test]
    fn mailmap_collapses_one_person_with_two_addresses() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_as(dir.path(), "Ann <ann@example.com>", "one", &["src/a.rs"]);
        commit_as(dir.path(), "ann <ann@old.example>", "two", &["src/a.rs"]);
        commit_as(dir.path(), "Bob <bob@example.com>", "three", &["src/a.rs"]);

        let before = json(&OwnershipAnalyzer::new(), dir.path());
        assert_eq!(before["author_count"], 3, "got {before}");

        write_file(
            dir.path(),
            ".mailmap",
            "Ann <ann@example.com> <ann@old.example>\n",
        );
        let after = json(&OwnershipAnalyzer::new(), dir.path());
        assert_eq!(after["author_count"], 2, "got {after}");
        let row = file_row(&after, "src/a.rs");
        assert_eq!(row["top_author"], "Ann <ann@example.com>");
        assert!(close(&row["top_author_share"], 2.0 / 3.0), "got {row}");
    }

    /// Trailers are mailmapped too, even though git does not do it for
    /// them, and a co-author sharing the author's address is not a
    /// second author.
    #[test]
    fn co_author_trailers_go_through_the_mailmap() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        write_file(
            dir.path(),
            ".mailmap",
            "Bob <bob@example.com> <bob@old.example>\n",
        );
        commit_as(
            dir.path(),
            "Ann <ann@example.com>",
            "pair\n\nCo-authored-by: Robert <bob@old.example>",
            &["src/a.rs"],
        );
        commit_as(dir.path(), "Bob <bob@example.com>", "solo", &["src/a.rs"]);

        let parsed = json(&OwnershipAnalyzer::new(), dir.path());
        assert_eq!(parsed["author_count"], 2, "got {parsed}");
        let row = file_row(&parsed, "src/a.rs");
        assert_eq!(row["top_author"], "Bob <bob@example.com>", "got {row}");
        assert!(close(&row["top_author_share"], 0.75), "got {row}");
    }

    /// An agent-assisted repository: one human commits everything, and
    /// the agent is credited through a trailer. Without trailer parsing
    /// this is "one author, everything".
    #[test]
    fn co_authored_by_trailers_are_authors() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_as(
            dir.path(),
            "Ann <ann@example.com>",
            "feat\n\nCo-Authored-By: Claude <noreply@anthropic.com>",
            &["src/a.rs"],
        );
        commit_as(dir.path(), "Ann <ann@example.com>", "fix", &["src/a.rs"]);

        let parsed = json(&OwnershipAnalyzer::new(), dir.path());
        assert_eq!(parsed["author_count"], 2, "got {parsed}");
        assert_eq!(parsed["co_authored_commit_count"], 1, "got {parsed}");
        assert!(parsed.get("single_author").is_none(), "got {parsed}");
        let row = file_row(&parsed, "src/a.rs");
        assert!(close(&row["top_author_share"], 0.75), "got {row}");
        let claude = &row["top_authors"][1];
        assert_eq!(claude["author"], "Claude <noreply@anthropic.com>");
        assert!(close(&claude["credit"], 0.5), "got {row}");
    }

    #[test]
    fn bots_are_filtered_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_as(dir.path(), "Ann <ann@example.com>", "a", &["src/a.rs"]);
        commit_as(dir.path(), "Bob <bob@example.com>", "b", &["src/a.rs"]);
        for _ in 0..2 {
            commit_as(
                dir.path(),
                "dependabot[bot] <49699333+dependabot[bot]@users.noreply.github.com>",
                "bump",
                &["src/a.rs"],
            );
        }

        let parsed = json(&OwnershipAnalyzer::new(), dir.path());
        assert_eq!(parsed["commit_count"], 2, "got {parsed}");
        assert_eq!(parsed["bot_commit_count"], 2, "got {parsed}");
        assert_eq!(parsed["bots"][0]["commits"], 2, "got {parsed}");
        assert!(
            parsed["bots"][0]["identity"]
                .as_str()
                .is_some_and(|id| id.starts_with("dependabot[bot]")),
            "got {parsed}",
        );
        assert_eq!(file_row(&parsed, "src/a.rs")["commits"], 2);

        let kept = json(
            &OwnershipAnalyzer::new().with_include_bots(true),
            dir.path(),
        );
        assert_eq!(kept["commit_count"], 4, "got {kept}");
        assert_eq!(kept["bot_commit_count"], 0, "got {kept}");
        assert_eq!(kept["bot_patterns"], serde_json::json!([]), "got {kept}");

        let custom = json(
            &OwnershipAnalyzer::new().with_bot_patterns(vec!["bob *".to_owned()]),
            dir.path(),
        );
        assert_eq!(
            custom["commit_count"], 3,
            "a custom pattern replaces the default: {custom}"
        );
        assert_eq!(custom["bot_commit_count"], 1, "got {custom}");
    }

    #[rstest]
    #[case::json(OutputFormat::Json, "\"single_author\": true")]
    #[case::md(OutputFormat::Md, "ownership is degenerate here")]
    fn a_single_author_repository_gets_a_note_not_a_ranking(
        #[case] format: OutputFormat,
        #[case] needle: &str,
    ) {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_as(
            dir.path(),
            "Ann <ann@example.com>",
            "a",
            &["src/a.rs", "src/b.rs"],
        );
        commit_as(dir.path(), "Ann <ann@example.com>", "b", &["src/a.rs"]);
        let out = OwnershipAnalyzer::new()
            .analyze(dir.path(), format)
            .unwrap();
        assert!(out.contains(needle), "got {out}");
        if format == OutputFormat::Json {
            let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
            assert_eq!(parsed["files"], serde_json::json!([]), "got {parsed}");
            assert_eq!(parsed["file_count"], 2, "got {parsed}");
        }
    }

    #[test]
    fn the_markdown_report_ranks_contested_files_and_counts_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_as(
            dir.path(),
            "Ann <ann@example.com>",
            "a",
            &["src/a.rs", "src/solo.rs"],
        );
        commit_as(dir.path(), "Bob <bob@example.com>", "b", &["src/a.rs"]);
        let out = OwnershipAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(out.contains("| src/a.rs | 2 | 2 |"), "got {out}");
        assert!(!out.contains("| src/solo.rs |"), "got {out}");
        assert!(out.contains("1 file(s) with a single author"), "got {out}");
        assert!(
            out.contains("Co-authored-by trailer"),
            "the rule is published: {out}"
        );
    }

    #[test]
    fn top_caps_the_markdown_table_and_names_what_it_hid() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_as(dir.path(), "Ann <ann@example.com>", "a", &["a.rs", "b.rs"]);
        commit_as(dir.path(), "Bob <bob@example.com>", "b", &["a.rs", "b.rs"]);
        let out = OwnershipAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(out.contains("+1 more file(s) not shown"), "got {out}");
    }

    #[rstest]
    #[case::negative(-0.1)]
    #[case::above_one(1.5)]
    fn an_unreachable_minor_threshold_is_rejected(#[case] value: f64) {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        commit_as(dir.path(), "Ann <ann@example.com>", "a", &["a.rs"]);
        let error = OwnershipAnalyzer::new()
            .with_minor_threshold(value)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(
            matches!(error, OwnershipError::MinorThresholdOutOfRange { .. }),
            "got {error:?}",
        );
    }

    #[test]
    fn a_path_outside_a_git_tree_is_reported_as_such() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "lone.rs", "fn main() {}\n");
        let error = OwnershipAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(
            matches!(error, OwnershipError::NotInGitRepo { .. }),
            "got {error:?}",
        );
    }

    #[rstest]
    #[case::full("Ann <ann@example.com>", "Ann", "ann@example.com")]
    #[case::padded("  Ann Lee   <ann@example.com> ", "Ann Lee", "ann@example.com")]
    #[case::name_only("Ann", "Ann", "")]
    #[case::unclosed("Ann <ann@example.com", "Ann <ann@example.com", "")]
    fn trailer_values_split_into_name_and_email(
        #[case] value: &str,
        #[case] name: &str,
        #[case] email: &str,
    ) {
        assert_eq!(
            parse_contact(value),
            Contact {
                name: name.to_owned(),
                email: email.to_owned(),
            },
        );
    }
}
