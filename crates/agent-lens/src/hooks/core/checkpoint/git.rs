//! Git plumbing for the checkpoint: where a repository keeps shared
//! state, what a path held at a commit, which paths differ from `HEAD`,
//! and a throwaway copy of a commit's tree.
//!
//! Everything here shells out to `git` and degrades to `None` when that
//! fails — no `git` on `PATH`, a repository without commits, reflogs
//! turned off. The checkpoint then falls back to what the disk says.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::warn;

/// The git view of one checkout, from the session's working directory.
#[derive(Debug, Clone)]
pub(super) struct Repo {
    cwd: PathBuf,
    /// Shared by every worktree of the repository.
    pub(super) common_dir: PathBuf,
    toplevel: PathBuf,
    /// `cwd` relative to `toplevel`, `/`-terminated, empty at the top.
    prefix: String,
}

impl Repo {
    /// `None` outside a git checkout, or when `git` cannot run.
    pub(super) fn discover(cwd: &Path) -> Option<Self> {
        let out = git(
            cwd,
            &[
                "rev-parse",
                "--git-common-dir",
                "--show-toplevel",
                "--show-prefix",
            ],
        )?;
        let mut lines = out.lines();
        // Relative to `cwd` unless it lies elsewhere (a worktree's).
        let common_dir = cwd.join(lines.next()?);
        let common_dir = common_dir.canonicalize().unwrap_or(common_dir);
        let toplevel = PathBuf::from(lines.next()?);
        let prefix = lines.next().unwrap_or_default().to_owned();
        Some(Self {
            cwd: cwd.to_path_buf(),
            common_dir,
            toplevel,
            prefix,
        })
    }

    pub(super) fn head(&self) -> Option<String> {
        self.commit("HEAD")
    }

    /// The commit this checkout's `HEAD` pointed at just before
    /// `unix_secs`, from its (per-worktree) reflog: the newest entry
    /// strictly older than that second. A worktree created later has no
    /// such entry, and gets its oldest one — the commit it was created
    /// on. Strictly older, because reflog times are whole seconds and an
    /// entry in the session's first second is more likely its own work.
    pub(super) fn head_at(&self, unix_secs: u64) -> Option<String> {
        let out = git(
            &self.cwd,
            &[
                "log",
                "--walk-reflogs",
                "--date=unix",
                "--format=%H %gd",
                "HEAD",
            ],
        )?;
        let entries: Vec<(&str, u64)> = out
            .lines()
            .filter_map(|line| {
                let (commit, selector) = line.split_once(' ')?;
                let secs = selector.strip_prefix("HEAD@{")?.strip_suffix('}')?;
                Some((commit, secs.parse().ok()?))
            })
            .collect();
        entries
            .iter()
            .find(|(_, at)| *at < unix_secs)
            .or_else(|| entries.last())
            .map(|(commit, _)| (*commit).to_owned())
    }

    fn commit(&self, rev: &str) -> Option<String> {
        let spec = format!("{rev}^{{commit}}");
        let out = git(&self.cwd, &["rev-parse", "--verify", "--quiet", &spec])?;
        Some(out.trim().to_owned()).filter(|c| !c.is_empty())
    }

    /// Paths under `cwd` that differ from `HEAD` — modified, staged,
    /// deleted, or untracked — relative to `cwd`.
    pub(super) fn dirty(&self) -> Option<BTreeSet<String>> {
        let out = git(
            &self.cwd,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--no-renames",
                "--",
                ".",
            ],
        )?;
        Some(
            out.split('\0')
                .filter_map(|entry| entry.get(3..))
                .filter_map(|path| path.strip_prefix(self.prefix.as_str()))
                .map(str::to_owned)
                .collect(),
        )
    }

    /// `rel` (relative to `cwd`) as it was at `commit`, or `None` when it
    /// did not exist there.
    pub(super) fn show(&self, commit: &str, rel: &str) -> Option<String> {
        git(&self.cwd, &["show", &format!("{commit}:./{rel}")])
    }
}

/// `commit`'s tree checked out into a temporary directory, removed on
/// drop. Built from a private index, so the repository's own index and
/// worktree list are left alone.
#[derive(Debug)]
pub(super) struct Materialized {
    root: PathBuf,
    cwd: PathBuf,
}

impl Materialized {
    pub(super) fn new(repo: &Repo, commit: &str) -> Option<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let root = std::env::temp_dir().join(format!(
            "agent-lens-checkpoint-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root)
            .map_err(|e| warn!(path = %root.display(), error = %e, "checkpoint: cannot create a temp tree"))
            .ok()?;
        // Owned from here, so an early return still cleans up.
        let tree = Self {
            cwd: root.join("tree").join(&repo.prefix),
            root,
        };
        let index = tree.root.join("index");
        let prefix = format!("{}/", tree.root.join("tree").display());
        let with_index = |args: &[&str]| {
            run(Command::new("git")
                .arg("-C")
                .arg(&repo.toplevel)
                .args(args)
                .env("GIT_INDEX_FILE", &index))
        };
        with_index(&["read-tree", commit])?;
        with_index(&[
            "checkout-index",
            "--all",
            "--force",
            &format!("--prefix={prefix}"),
        ])?;
        Some(tree)
    }

    /// The session's working directory inside the copy.
    pub(super) fn cwd(&self) -> &Path {
        &self.cwd
    }
}

impl Drop for Materialized {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    run(Command::new("git").arg("-C").arg(cwd).args(args))
}

fn run(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};

    fn committed() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        write_file(dir.path(), "sub/a.rs", "fn a() {}\n");
        write_file(dir.path(), "top.rs", "fn top() {}\n");
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "one"]);
        dir
    }

    #[test]
    fn dirty_paths_are_relative_to_the_working_directory() {
        let dir = committed();
        write_file(dir.path(), "sub/a.rs", "fn a() { 1; }\n");
        write_file(dir.path(), "sub/new.rs", "fn n() {}\n");
        write_file(dir.path(), "top.rs", "fn top() { 2; }\n");
        let repo = Repo::discover(&dir.path().join("sub")).unwrap();
        assert_eq!(
            repo.dirty().unwrap(),
            BTreeSet::from(["a.rs".to_owned(), "new.rs".to_owned()])
        );
    }

    #[test]
    fn show_reads_a_path_at_a_commit() {
        let dir = committed();
        write_file(dir.path(), "sub/a.rs", "changed\n");
        let repo = Repo::discover(&dir.path().join("sub")).unwrap();
        let head = repo.head().unwrap();
        assert_eq!(repo.show(&head, "a.rs").as_deref(), Some("fn a() {}\n"));
        assert_eq!(repo.show(&head, "missing.rs"), None);
    }

    #[test]
    fn a_new_worktree_reads_back_its_creation_commit() {
        let dir = committed();
        let repo = Repo::discover(dir.path()).unwrap();
        let base = repo.head().unwrap();
        let wt = dir.path().join("wt");
        run_git(
            dir.path(),
            &["worktree", "add", "-q", "-b", "wt", wt.to_str().unwrap()],
        );
        write_file(&wt, "top.rs", "fn top() { 3; }\n");
        run_git(&wt, &["commit", "-q", "-am", "two"]);
        let wt_repo = Repo::discover(&wt).unwrap();
        assert_ne!(wt_repo.head().unwrap(), base);
        assert_eq!(
            wt_repo.common_dir.canonicalize().unwrap(),
            repo.common_dir.canonicalize().unwrap()
        );
        // Before the worktree existed, and in the second it was made:
        // its creation commit either way.
        assert_eq!(wt_repo.head_at(1).unwrap(), base);
        assert_eq!(wt_repo.head_at(u64::MAX).unwrap(), wt_repo.head().unwrap());
    }

    #[test]
    fn head_at_takes_the_newest_entry_before_the_moment() {
        let dir = committed();
        let repo = Repo::discover(dir.path()).unwrap();
        let first = repo.head().unwrap();
        write_file(dir.path(), "top.rs", "fn top() { 1; }\n");
        run_git(dir.path(), &["commit", "-q", "-am", "two"]);
        // Every entry falls in the same second or so: a moment after all
        // of them is the latest; one before all of them the oldest.
        assert_eq!(repo.head_at(u64::MAX).unwrap(), repo.head().unwrap());
        assert_eq!(repo.head_at(0).unwrap(), first);
    }

    #[test]
    fn a_materialized_tree_holds_the_commit_and_is_removed_on_drop() {
        let dir = committed();
        let repo = Repo::discover(&dir.path().join("sub")).unwrap();
        let head = repo.head().unwrap();
        write_file(dir.path(), "sub/a.rs", "changed\n");
        let tree = Materialized::new(&repo, &head).unwrap();
        let cwd = tree.cwd().to_path_buf();
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.rs")).unwrap(),
            "fn a() {}\n"
        );
        assert_eq!(
            repo.dirty().unwrap(),
            BTreeSet::from(["a.rs".to_owned()]),
            "own index untouched"
        );
        drop(tree);
        assert!(!cwd.exists());
    }
}
