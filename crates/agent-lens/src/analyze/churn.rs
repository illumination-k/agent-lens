//! Shared git-history extraction for the analyzers that read `git log`
//! (`analyze hotspot`, `analyze risk`, `analyze co-change`).
//!
//! Every one of them needs the same three things and must agree on all
//! of them, or their rankings stop being comparable: the working-tree
//! root git counts commits against, the history itself, and — the part
//! that is easy to get wrong — a single path space to join on. Git
//! reports paths relative to the **repo root** while the analyzers walk
//! an arbitrary target path and describe files relative to *that*.
//! [`ChurnScope`] owns the conversion so a caller never joins
//! `crates/foo/src/lib.rs` against `src/lib.rs` and silently reports
//! zero churn for every file.
//!
//! There is one `git log` invocation behind all of it, parsed into
//! [`RawCommit`]s. Churn ([`ChurnScope::collect`]) folds that stream into
//! per-file counts; co-change ([`ChurnScope::collect_commits`]) keeps the
//! per-commit file sets the counting throws away, which is the substrate
//! every history-based pair metric needs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lens_domain::{CommitFiles, FileChurn};
use tracing::debug;

use super::AnalyzeRoots;

/// Failures raised while asking git for churn.
///
/// Deliberately narrower than any analyzer's error type: each analyzer
/// converts these into its own public enum rather than exposing this one.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ChurnError {
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
}

/// A resolved analysis scope, anchored in its git working tree.
#[derive(Debug, Clone)]
pub(crate) struct ChurnScope {
    /// The canonicalized analysis targets (files or directories), in the
    /// order the caller gave them. Never empty.
    targets: Vec<PathBuf>,
    /// Working-tree root containing every entry of [`Self::targets`].
    repo_root: PathBuf,
    /// Each target relative to the repo root — the git pathspecs the log
    /// is scoped by. Empty means repo-wide, either because the caller
    /// pointed at the repo root or because one of several targets did.
    scope_rels: Vec<String>,
    /// The display base's offset from the repo root: what
    /// [`Self::key_for_display`] prefixes a target-relative path with.
    display_rel: Option<String>,
    /// Whether the scope is one single file, whose display path is the
    /// path as the caller spelled it rather than a base-relative one.
    single_file: bool,
}

impl ChurnScope {
    /// Canonicalize every root and locate the git working tree around
    /// them.
    ///
    /// All roots must live in the same working tree; the first one
    /// decides which, and a root outside it is reported as
    /// [`ChurnError::NotInGitRepo`] rather than silently producing zero
    /// churn for every file under it.
    pub(crate) fn resolve(roots: &AnalyzeRoots) -> Result<Self, ChurnError> {
        let mut targets = Vec::with_capacity(roots.paths().len());
        for root in roots.paths() {
            targets.push(canonicalize(root)?);
        }
        let Some(first) = targets.first() else {
            return Err(ChurnError::NotInGitRepo {
                path: PathBuf::from("."),
            });
        };
        let repo_root =
            crate::paths::git_repo_root(first).ok_or_else(|| ChurnError::NotInGitRepo {
                path: roots.paths()[0].clone(),
            })?;

        // A target that *is* the repo root makes the whole scope
        // repo-wide: pathspecs are a union, and the widest one wins.
        let mut scope_rels = Vec::with_capacity(targets.len());
        for (target, spelled) in targets.iter().zip(roots.paths()) {
            if !target.starts_with(&repo_root) {
                return Err(ChurnError::NotInGitRepo {
                    path: spelled.clone(),
                });
            }
            match relative_to(target, &repo_root) {
                Some(rel) => scope_rels.push(rel),
                None => {
                    scope_rels.clear();
                    break;
                }
            }
        }

        let single_file = roots.single().is_some_and(|root| root.is_file());
        let display_rel = if single_file {
            scope_rels.first().cloned()
        } else {
            relative_to(&canonicalize(roots.base())?, &repo_root)
        };
        Ok(Self {
            targets,
            repo_root,
            scope_rels,
            display_rel,
            single_file,
        })
    }

    /// The canonicalized analysis targets. Never empty.
    pub(crate) fn targets(&self) -> &[PathBuf] {
        &self.targets
    }

    pub(crate) fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Per-file commit counts under this scope, keyed repo-root-relative.
    ///
    /// `since` is passed straight to git's `--since=`, so anything its
    /// `approxidate` parser accepts works. Renames are *not* followed
    /// here: each entry counts under the name it carried in that commit,
    /// so a file that was renamed has its history split between the two
    /// names. That is good enough for ranking, and it is what `hotspot`
    /// and `risk` have always reported.
    pub(crate) fn collect(&self, since: Option<&str>) -> Result<Vec<FileChurn>, ChurnError> {
        let commits = self.raw_commits(since)?;
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
        for commit in commits {
            for entry in commit.entries {
                *counts.entry(entry.path).or_insert(0) += 1;
            }
        }
        Ok(counts
            .into_iter()
            .map(|(path, commits)| FileChurn { path, commits })
            .collect())
    }

    /// Per-commit file sets under this scope, newest first, keyed
    /// repo-root-relative and with renames followed.
    ///
    /// This is what [`Self::collect`] throws away: which files moved
    /// *together*. Renames matter more here than for counting, because a
    /// rename mid-history would otherwise split one pair's support
    /// between two names and hide the pattern behind
    /// `--min-support`. The walk runs newest-first, so a
    /// `R old new` entry means every older mention of `old` is really
    /// today's `new`, and the accumulated map is applied as the walk
    /// descends.
    ///
    /// Only commits touching at least one in-scope file appear, since
    /// that is all `git log` reports for a scoped pathspec. Merge commits
    /// carry no diff without `--diff-merges`, so they contribute nothing
    /// — which is the right default: a merge's file set is the union of a
    /// whole branch.
    pub(crate) fn collect_commits(
        &self,
        since: Option<&str>,
    ) -> Result<Vec<CommitFiles>, ChurnError> {
        let raw = self.raw_commits(since)?;
        // Historical path → the name that path's content goes by today.
        let mut renamed: BTreeMap<String, String> = BTreeMap::new();
        let mut out = Vec::with_capacity(raw.len());
        for commit in raw {
            let mut files = Vec::with_capacity(commit.entries.len());
            for entry in commit.entries {
                let current = renamed
                    .get(&entry.path)
                    .cloned()
                    .unwrap_or_else(|| entry.path.clone());
                if let Some(source) = entry.renamed_from {
                    // A copy leaves its source in place under its own
                    // name, so only a rename redirects older mentions.
                    renamed.insert(source, current.clone());
                }
                files.push(current);
            }
            files.sort_unstable();
            files.dedup();
            out.push(CommitFiles {
                date: commit.date,
                files,
            });
        }
        Ok(out)
    }

    /// Whether the working tree is a shallow clone, in which case any
    /// history-based metric is reading a truncated log.
    ///
    /// A git that cannot answer — one too old for
    /// `--is-shallow-repository`, or none on `PATH` — reads as "not
    /// shallow". This only decides whether a caveat is raised, and a
    /// missing diagnostic should not fail an analysis over it.
    pub(crate) fn is_shallow(&self) -> bool {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.repo_root)
            .arg("rev-parse")
            .arg("--is-shallow-repository")
            .output();
        let answered = match &output {
            Ok(output) => shallow_from_output(output),
            Err(source) => {
                debug!(%source, "could not run git to check for a shallow repository");
                None
            }
        };
        answered.unwrap_or(false)
    }

    /// One `git log` pass over this scope, parsed into commits.
    ///
    /// `--name-status -M` rather than `--name-only` so a rename arrives
    /// as one entry naming both paths instead of as an unlabelled
    /// destination. `-M` is passed explicitly so the output does not
    /// depend on the user's `diff.renames` setting.
    fn raw_commits(&self, since: Option<&str>) -> Result<Vec<RawCommit>, ChurnError> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo_root)
            .arg("log")
            .arg(format!("--pretty=format:{COMMIT_MARKER}%ad"))
            .arg("--date=short")
            .arg("--name-status")
            .arg("-M");
        if let Some(s) = since {
            cmd.arg(format!("--since={s}"));
        }
        if !self.scope_rels.is_empty() {
            cmd.arg("--");
            cmd.args(&self.scope_rels);
        }

        let output = cmd.output().map_err(|source| ChurnError::Io {
            path: self.repo_root.clone(),
            source,
        })?;
        if !output.status.success() {
            return Err(ChurnError::Git {
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        Ok(parse_raw_commits(&String::from_utf8_lossy(&output.stdout)))
    }

    /// Repo-root-relative key for an absolute path inside the repo,
    /// falling back to the full path when it lies outside (which the
    /// walkers never produce, but a caller should not silently mis-key).
    pub(crate) fn key_for_absolute(&self, file: &Path) -> String {
        relative_to(file, &self.repo_root).unwrap_or_else(|| file.display().to_string())
    }

    /// Repo-root-relative key for a *base*-relative display path — the
    /// path space [`super::SourceFile::display_path`] and therefore every
    /// call-graph node id lives in.
    ///
    /// This is the join fix: prefixing with the display base's own offset
    /// from the repo root puts graph-derived paths in git's path space.
    /// A single-file scope is its own display path, so the offset already
    /// *is* the answer.
    pub(crate) fn key_for_display(&self, display_path: &str) -> String {
        if self.single_file {
            return self
                .display_rel
                .clone()
                .unwrap_or_else(|| display_path.replace('\\', "/"));
        }
        let display_path = display_path.replace('\\', "/");
        match &self.display_rel {
            Some(base) => format!("{base}/{display_path}"),
            None => display_path,
        }
    }
}

/// Read `git rev-parse --is-shallow-repository`'s answer, or `None` when
/// the command did not answer.
///
/// A non-zero exit means the question was never asked successfully — a git
/// too old for the flag, say — so its stdout must not be read as an
/// answer. Keeping that distinct from a definite "not shallow" is what
/// stops the truncated-history caveat being raised, or suppressed, on the
/// strength of output nothing produced.
fn shallow_from_output(output: &Output) -> Option<bool> {
    if !output.status.success() {
        debug!(
            stderr = %String::from_utf8_lossy(&output.stderr).trim_end(),
            "git could not say whether the repository is shallow",
        );
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim() == "true")
}

/// Prefix `git log --pretty=format:` writes before each commit's date.
///
/// A NUL byte, because it is the one character a path cannot contain: a
/// line starting with it is a commit header, anything else is a
/// `--name-status` entry, and no quoting rule can blur the two.
const COMMIT_MARKER: &str = "%x00";

/// One commit as `git log --name-status` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawCommit {
    /// Author date, `YYYY-MM-DD` (`--date=short`).
    date: String,
    entries: Vec<RawEntry>,
}

/// One touched path inside a commit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RawEntry {
    /// The path as of this commit — the destination for a rename or copy.
    path: String,
    /// The origin path, set only for a rename (`R`). A copy (`C`) leaves
    /// its source in place, so it carries `None` and no older mention is
    /// redirected.
    renamed_from: Option<String>,
}

/// Split a `git log --pretty=format:%x00%ad --name-status` stream into
/// commits.
///
/// Unrecognised lines are skipped rather than guessed at: an entry with
/// no status field, or a rename with no destination, is a shape this
/// parser does not model, and inventing a path for it would put a
/// fabricated file in the report.
fn parse_raw_commits(stdout: &str) -> Vec<RawCommit> {
    let mut commits: Vec<RawCommit> = Vec::new();
    for line in stdout.lines() {
        if let Some(date) = line.strip_prefix('\0') {
            commits.push(RawCommit {
                date: date.trim().to_owned(),
                entries: Vec::new(),
            });
            continue;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.trim().is_empty() {
            continue;
        }
        let Some(commit) = commits.last_mut() else {
            continue;
        };
        if let Some(entry) = parse_raw_entry(trimmed) {
            commit.entries.push(entry);
        }
    }
    commits
}

/// Parse one `--name-status` line: a status field, then one path, or two
/// for a rename or copy.
fn parse_raw_entry(line: &str) -> Option<RawEntry> {
    let mut fields = line.split('\t');
    let status = fields.next()?;
    let first = fields.next()?;
    if first.is_empty() {
        return None;
    }
    // `R100` / `C75`: the number is the similarity index, which nothing
    // here uses — the letter is the whole signal.
    match status.as_bytes().first() {
        Some(b'R') => {
            let destination = fields.next()?;
            if destination.is_empty() {
                return None;
            }
            Some(RawEntry {
                path: destination.to_owned(),
                renamed_from: Some(first.to_owned()),
            })
        }
        Some(b'C') => {
            let destination = fields.next()?;
            if destination.is_empty() {
                return None;
            }
            Some(RawEntry {
                path: destination.to_owned(),
                renamed_from: None,
            })
        }
        Some(_) => Some(RawEntry {
            path: first.to_owned(),
            renamed_from: None,
        }),
        None => None,
    }
}

fn canonicalize(path: &Path) -> Result<PathBuf, ChurnError> {
    path.canonicalize().map_err(|source| ChurnError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Express `target` as a `/`-separated path relative to `base`,
/// returning `None` when `target == base` (i.e. the caller pointed at
/// the base itself).
fn relative_to(target: &Path, base: &Path) -> Option<String> {
    let rel = target.strip_prefix(base).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    Some(rel.to_string_lossy().replace('\\', "/"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};
    use rstest::rstest;

    fn init_repo(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        write_file(dir, "crates/app/src/lib.rs", "pub fn a() {}\n");
        write_file(dir, "crates/app/src/other.rs", "pub fn b() {}\n");
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", "initial"]);
    }

    #[test]
    fn churn_counts_commits_per_repo_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        write_file(
            dir.path(),
            "crates/app/src/lib.rs",
            "pub fn a() -> i32 { 1 }\n",
        );
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "tweak"]);

        let scope = ChurnScope::resolve(&AnalyzeRoots::from(dir.path())).unwrap();
        let churn = scope.collect(None).unwrap();
        let lib = churn
            .iter()
            .find(|c| c.path == "crates/app/src/lib.rs")
            .unwrap();
        assert_eq!(lib.commits, 2);
        assert_eq!(
            churn
                .iter()
                .find(|c| c.path == "crates/app/src/other.rs")
                .unwrap()
                .commits,
            1,
        );
    }

    #[test]
    fn since_window_scopes_the_history() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let scope = ChurnScope::resolve(&AnalyzeRoots::from(dir.path())).unwrap();
        assert!(scope.collect(Some("2099-01-01")).unwrap().is_empty());
    }

    /// Churn counts each name a file carried, without following the
    /// rename — the behaviour `hotspot` and `risk` have always had, and
    /// the reason the shared commit stream cannot apply the rename map on
    /// its way through [`ChurnScope::collect`].
    #[test]
    fn churn_counts_a_renamed_file_under_each_of_its_names() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        run_git(
            dir.path(),
            &["mv", "crates/app/src/lib.rs", "crates/app/src/renamed.rs"],
        );
        run_git(dir.path(), &["commit", "-q", "-m", "rename"]);

        let scope = ChurnScope::resolve(&AnalyzeRoots::from(dir.path())).unwrap();
        let churn = scope.collect(None).unwrap();
        let count = |path: &str| {
            churn
                .iter()
                .find(|c| c.path == path)
                .map_or(0, |c| c.commits)
        };
        assert_eq!(count("crates/app/src/lib.rs"), 1, "got {churn:?}");
        assert_eq!(count("crates/app/src/renamed.rs"), 1, "got {churn:?}");
    }

    #[test]
    fn commit_file_sets_are_newest_first_and_deduplicated() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        write_file(
            dir.path(),
            "crates/app/src/lib.rs",
            "pub fn a() -> u8 { 1 }\n",
        );
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "tweak lib"]);

        let scope = ChurnScope::resolve(&AnalyzeRoots::from(dir.path())).unwrap();
        let commits = scope.collect_commits(None).unwrap();
        assert_eq!(commits.len(), 2, "got {commits:?}");
        assert_eq!(commits[0].files, ["crates/app/src/lib.rs"]);
        assert_eq!(
            commits[1].files,
            ["crates/app/src/lib.rs", "crates/app/src/other.rs"],
        );
        assert!(
            commits[0].date >= commits[1].date,
            "commits must be newest first: {commits:?}",
        );
    }

    /// Without the rename map the pair `(lib.rs, other.rs)` would be
    /// split between the old and the new name and lose half its support.
    #[test]
    fn commit_file_sets_follow_renames_to_the_current_name() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        run_git(
            dir.path(),
            &["mv", "crates/app/src/lib.rs", "crates/app/src/renamed.rs"],
        );
        run_git(dir.path(), &["commit", "-q", "-m", "rename"]);

        let scope = ChurnScope::resolve(&AnalyzeRoots::from(dir.path())).unwrap();
        let commits = scope.collect_commits(None).unwrap();
        assert!(
            commits
                .iter()
                .all(|c| !c.files.contains(&"crates/app/src/lib.rs".to_owned())),
            "the pre-rename name must be rewritten: {commits:?}",
        );
        assert_eq!(
            commits.last().unwrap().files,
            ["crates/app/src/other.rs", "crates/app/src/renamed.rs"],
            "the initial commit must be keyed on today's name: {commits:?}",
        );
    }

    #[test]
    fn a_deletion_still_appears_in_the_commit_that_removed_it() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::remove_file(dir.path().join("crates/app/src/other.rs")).unwrap();
        run_git(dir.path(), &["add", "-A"]);
        run_git(dir.path(), &["commit", "-q", "-m", "drop other"]);

        let scope = ChurnScope::resolve(&AnalyzeRoots::from(dir.path())).unwrap();
        let commits = scope.collect_commits(None).unwrap();
        assert_eq!(commits[0].files, ["crates/app/src/other.rs"]);
    }

    /// The exit status is not a formality: a `rev-parse` that failed has
    /// not answered the question, so its stdout must not be read as an
    /// answer either.
    #[cfg(unix)]
    #[rstest]
    #[case::answered_yes(0, "true\n", Some(true))]
    #[case::answered_no(0, "false\n", Some(false))]
    #[case::failed_but_printed_yes(1 << 8, "true\n", None)]
    #[case::failed_and_printed_nothing(1 << 8, "", None)]
    fn shallow_is_read_only_from_a_successful_rev_parse(
        #[case] raw_status: i32,
        #[case] stdout: &str,
        #[case] expected: Option<bool>,
    ) {
        use std::os::unix::process::ExitStatusExt as _;
        let output = Output {
            status: std::process::ExitStatus::from_raw(raw_status),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        };
        assert_eq!(shallow_from_output(&output), expected);
    }

    #[test]
    fn an_ordinary_clone_is_not_reported_as_shallow() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let scope = ChurnScope::resolve(&AnalyzeRoots::from(dir.path())).unwrap();
        assert!(!scope.is_shallow());
    }

    /// A shallow clone truncates the log, so every history metric reads a
    /// partial history — the one case worth warning about, and the only
    /// way to test it is to make one.
    #[test]
    fn a_shallow_clone_is_detected() {
        let origin = tempfile::tempdir().unwrap();
        init_repo(origin.path());
        write_file(
            origin.path(),
            "crates/app/src/lib.rs",
            "pub fn a() -> u8 { 2 }\n",
        );
        run_git(origin.path(), &["add", "."]);
        run_git(origin.path(), &["commit", "-q", "-m", "second"]);

        let clone = tempfile::tempdir().unwrap();
        let target = clone.path().join("shallow");
        run_git(
            clone.path(),
            &[
                "clone",
                "-q",
                "--depth",
                "1",
                &format!("file://{}", origin.path().display()),
                &target.display().to_string(),
            ],
        );
        let scope = ChurnScope::resolve(&AnalyzeRoots::from(&target)).unwrap();
        assert!(scope.is_shallow());
    }

    #[rstest]
    #[case::added("A\tsrc/a.rs", Some(("src/a.rs", None)))]
    #[case::modified("M\tsrc/a.rs", Some(("src/a.rs", None)))]
    #[case::deleted("D\tsrc/a.rs", Some(("src/a.rs", None)))]
    #[case::renamed("R100\tsrc/old.rs\tsrc/new.rs", Some(("src/new.rs", Some("src/old.rs"))))]
    // A copy's source keeps existing under its own name, so nothing
    // older should be redirected onto the destination.
    #[case::copied("C75\tsrc/a.rs\tsrc/b.rs", Some(("src/b.rs", None)))]
    #[case::no_path("M", None)]
    #[case::empty_path("M\t", None)]
    #[case::rename_without_destination("R100\tsrc/old.rs", None)]
    fn name_status_entries_parse_by_their_status_letter(
        #[case] line: &str,
        #[case] expected: Option<(&str, Option<&str>)>,
    ) {
        assert_eq!(
            parse_raw_entry(line),
            expected.map(|(path, from)| RawEntry {
                path: path.to_owned(),
                renamed_from: from.map(ToOwned::to_owned),
            }),
        );
    }

    /// The NUL marker is what separates a commit header from a path, so
    /// entries arriving before any header — which would mean the stream
    /// is not the format this parser was given — are dropped rather than
    /// attributed to a commit that does not exist.
    #[test]
    fn entries_before_the_first_commit_header_are_dropped() {
        assert_eq!(parse_raw_commits("M\tsrc/stray.rs\n"), Vec::new());
    }

    #[test]
    fn blank_lines_between_commits_are_not_entries() {
        let commits = parse_raw_commits("\x002026-05-02\nM\ta\n\n\x002026-05-01\nA\ta\nA\tb\n");
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].date, "2026-05-02");
        assert_eq!(commits[0].entries.len(), 1);
        assert_eq!(commits[1].entries.len(), 2);
    }

    /// The whole reason this module exists: a subdirectory target still
    /// yields repo-root-relative churn keys, and display paths relative
    /// to that subdirectory are lifted into the same space.
    #[test]
    fn subdirectory_target_joins_on_repo_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let scope =
            ChurnScope::resolve(&AnalyzeRoots::from(&dir.path().join("crates/app"))).unwrap();

        let churn = scope.collect(None).unwrap();
        assert!(
            churn.iter().all(|c| c.path.starts_with("crates/app/")),
            "git paths must stay repo-relative: {churn:?}",
        );
        assert_eq!(
            scope.key_for_display("src/lib.rs"),
            "crates/app/src/lib.rs",
            "display paths must be lifted into git's path space",
        );
    }

    /// Several targets are one scope: the log is bounded by their union
    /// of pathspecs, and display paths key against their common base.
    #[test]
    fn several_targets_scope_the_log_to_their_union() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(dir.path(), "packages/core/lib.rs", "pub fn a() {}\n");
        write_file(dir.path(), "cli/main.rs", "fn main() {}\n");
        write_file(dir.path(), "web/app.ts", "export function c() {}\n");
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let roots = AnalyzeRoots::new([dir.path().join("packages"), dir.path().join("cli")]);
        let scope = ChurnScope::resolve(&roots).unwrap();
        let churn = scope.collect(None).unwrap();
        let paths: Vec<&str> = churn.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(
            paths,
            ["cli/main.rs", "packages/core/lib.rs"],
            "the untargeted `web` tree must stay out of the churn",
        );
        // Display paths are base-relative (base is the repo root here),
        // so they are already in git's path space.
        assert_eq!(
            scope.key_for_display("packages/core/lib.rs"),
            "packages/core/lib.rs",
        );
    }

    /// A nested common base still lifts display paths into git's space —
    /// the same join fix the single-target case needs, computed from the
    /// base rather than from one target.
    #[test]
    fn several_targets_under_a_subdirectory_key_on_repo_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let roots = AnalyzeRoots::new([
            dir.path().join("crates/app/src/lib.rs"),
            dir.path().join("crates/app/src/other.rs"),
        ]);
        let scope = ChurnScope::resolve(&roots).unwrap();
        assert_eq!(
            scope.key_for_display("lib.rs"),
            "crates/app/src/lib.rs",
            "display paths are relative to the targets' common ancestor",
        );
    }

    /// A target outside the first one's working tree is an error, not a
    /// silent zero-churn join.
    #[test]
    fn a_target_outside_the_working_tree_is_rejected() {
        let repo = tempfile::tempdir().unwrap();
        init_repo(repo.path());
        let outside = tempfile::tempdir().unwrap();
        write_file(outside.path(), "lone.rs", "fn x() {}\n");

        let roots =
            AnalyzeRoots::new([repo.path().join("crates/app"), outside.path().to_path_buf()]);
        let err = ChurnScope::resolve(&roots).unwrap_err();
        assert!(
            matches!(err, ChurnError::NotInGitRepo { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn repo_root_target_leaves_display_paths_untouched() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let scope = ChurnScope::resolve(&AnalyzeRoots::from(dir.path())).unwrap();
        assert_eq!(
            scope.key_for_display("crates/app/src/lib.rs"),
            "crates/app/src/lib.rs",
        );
    }

    /// A single-file target's display path is the path as the caller
    /// spelled it (relative to the CWD, or absolute), which carries no
    /// usable relation to the repo root — so the scope's own offset is
    /// the key.
    #[test]
    fn single_file_target_keys_on_its_own_repo_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let file = dir.path().join("crates/app/src/lib.rs");
        let scope = ChurnScope::resolve(&AnalyzeRoots::from(&file)).unwrap();
        assert_eq!(
            scope.key_for_display(&file.display().to_string()),
            "crates/app/src/lib.rs",
        );
        assert_eq!(scope.repo_root(), dir.path().canonicalize().unwrap());
    }

    #[test]
    fn absolute_paths_key_relative_to_the_repo_root() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let scope = ChurnScope::resolve(&AnalyzeRoots::from(dir.path())).unwrap();
        let file = scope.repo_root().join("crates/app/src/lib.rs");
        assert_eq!(scope.key_for_absolute(&file), "crates/app/src/lib.rs");
        assert_eq!(
            scope.key_for_absolute(Path::new("/elsewhere/x.rs")),
            "/elsewhere/x.rs",
            "a path outside the repo must not be silently mis-keyed",
        );
    }

    #[test]
    fn target_outside_a_git_working_tree_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "lone.rs", "fn x() {}\n");
        let err = ChurnScope::resolve(&AnalyzeRoots::from(dir.path())).unwrap_err();
        assert!(
            matches!(err, ChurnError::NotInGitRepo { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn missing_path_surfaces_an_io_error() {
        let err = ChurnScope::resolve(&AnalyzeRoots::from(Path::new("/definitely/does/not/exist")))
            .unwrap_err();
        assert!(matches!(err, ChurnError::Io { .. }), "got {err:?}");
    }

    #[rstest]
    #[case::nested("/repo/src/a.rs", "/repo", Some("src/a.rs"))]
    #[case::same_path("/repo", "/repo", None)]
    #[case::outside("/other/a.rs", "/repo", None)]
    fn relative_to_strips_the_base(
        #[case] target: &str,
        #[case] base: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(
            relative_to(Path::new(target), Path::new(base)),
            expected.map(ToOwned::to_owned),
        );
    }

    #[test]
    fn churn_error_display_carries_the_diagnostic() {
        let err = ChurnError::Git {
            stderr: "fatal: not a git repo\n".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("fatal: not a git repo"), "got {msg}");
        assert!(!msg.ends_with('\n'), "trailing newline should be trimmed");
    }
}
