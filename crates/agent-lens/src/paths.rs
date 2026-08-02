//! Locating the two directories the tool keeps re-deriving: the git
//! working-tree root and the user's home.
//!
//! Both had two implementations before — the git root as an ancestor walk
//! in the churn analyzer and as a `git rev-parse --show-toplevel`
//! subprocess in the hook setup path, and `$HOME` read directly in
//! `skills` while `hooks::setup_common` had a helper. Different
//! mechanisms for the same question can disagree, and the hook path runs
//! on every tool use, so both live here once.

use std::path::{Path, PathBuf};

/// Walk parents of `path` looking for a `.git` entry, returning the
/// directory that holds it — what git calls the working-tree root.
/// `None` when `path` is not inside a git tree.
///
/// The `.git` entry is checked for existence, not for being a directory,
/// so linked worktrees and submodules (where `.git` is a file) resolve
/// to their own root the way `git rev-parse --show-toplevel` would.
///
/// This is deliberately the ancestor walk rather than a `git` subprocess:
/// it needs no `git` on `PATH` and costs no process spawn, which matters
/// on the hook path where it runs per tool use. The trade-off is that
/// `GIT_DIR` / `GIT_WORK_TREE` overrides are not honored — nothing here
/// sets them, and a caller that needs them wants real git plumbing, not
/// this.
///
/// A relative `path` yields a relative root; canonicalize first when the
/// result is compared against absolute paths.
pub fn git_repo_root(path: &Path) -> Option<PathBuf> {
    let start = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    start
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The user's home directory from `$HOME`, or `None` when it is unset.
///
/// Callers map `None` onto their own "cannot resolve user scope" error so
/// the message names the file the user was trying to reach.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Resolve `$HOME/<relative>`, or `None` if `$HOME` is unset.
pub fn home_scoped_path(relative: &str) -> Option<PathBuf> {
    home_dir().map(|home| home.join(relative))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn git_repo_root_is_none_outside_a_git_tree() {
        let dir = tempfile::tempdir().unwrap();
        assert!(git_repo_root(dir.path()).is_none());
    }

    #[test]
    fn git_repo_root_finds_the_root_from_a_nested_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let nested = dir.path().join("nested/inner");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(git_repo_root(&nested).as_deref(), Some(dir.path()));
    }

    /// A linked worktree and a submodule both carry `.git` as a *file*
    /// pointing at the real git dir. Existence, not file type, is what
    /// makes them their own root.
    #[test]
    fn git_repo_root_accepts_a_git_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".git"), "gitdir: /elsewhere\n").unwrap();
        assert_eq!(git_repo_root(dir.path()).as_deref(), Some(dir.path()));
    }

    /// Passing a file resolves from its parent, so `analyze hotspot
    /// src/lib.rs` finds the same root as `analyze hotspot src/`.
    #[test]
    fn git_repo_root_starts_from_the_parent_of_a_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".git")).unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        assert_eq!(git_repo_root(&file).as_deref(), Some(dir.path()));
    }

    /// Both expectations read `$HOME` directly rather than going back
    /// through [`home_dir`] — deriving them from the function under test
    /// would make either assertion hold no matter what it returned.
    #[test]
    fn home_dir_reads_the_home_variable() {
        assert_eq!(home_dir(), std::env::var_os("HOME").map(PathBuf::from));
    }

    #[test]
    fn home_scoped_path_joins_onto_home() {
        let expected =
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude/settings.json"));
        assert_eq!(home_scoped_path(".claude/settings.json"), expected);
    }
}
