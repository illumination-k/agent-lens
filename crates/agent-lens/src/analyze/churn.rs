//! Shared git-churn extraction for the analyzers that join history with
//! source facts (`analyze hotspot`, `analyze risk`).
//!
//! Both analyzers need the same three things and must agree on all of
//! them, or their rankings stop being comparable: the working-tree root
//! git counts commits against, the per-file commit counts themselves,
//! and — the part that is easy to get wrong — a single path space to
//! join on. Git reports paths relative to the **repo root** while the
//! analyzers walk an arbitrary target path and describe files relative
//! to *that*. [`ChurnScope`] owns the conversion so a caller never joins
//! `crates/foo/src/lib.rs` against `src/lib.rs` and silently reports
//! zero churn for every file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use lens_domain::FileChurn;

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

/// A resolved analysis target, anchored in its git working tree.
#[derive(Debug, Clone)]
pub(crate) struct ChurnScope {
    /// The canonicalized analysis target (file or directory).
    target: PathBuf,
    /// Working-tree root containing [`Self::target`].
    repo_root: PathBuf,
    /// [`Self::target`] relative to the repo root, or `None` when the
    /// caller pointed at the repo root itself.
    scope_rel: Option<String>,
}

impl ChurnScope {
    /// Canonicalize `path` and locate the git working tree around it.
    pub(crate) fn resolve(path: &Path) -> Result<Self, ChurnError> {
        let target = path.canonicalize().map_err(|source| ChurnError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let repo_root =
            crate::paths::git_repo_root(&target).ok_or_else(|| ChurnError::NotInGitRepo {
                path: path.to_path_buf(),
            })?;
        let scope_rel = relative_to(&target, &repo_root);
        Ok(Self {
            target,
            repo_root,
            scope_rel,
        })
    }

    pub(crate) fn target(&self) -> &Path {
        &self.target
    }

    pub(crate) fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Per-file commit counts under this scope, keyed repo-root-relative.
    ///
    /// `since` is passed straight to git's `--since=`, so anything its
    /// `approxidate` parser accepts works. A renamed file is counted
    /// under each of its names; that is good enough for ranking.
    pub(crate) fn collect(&self, since: Option<&str>) -> Result<Vec<FileChurn>, ChurnError> {
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.repo_root)
            .arg("log")
            .arg("--pretty=format:")
            .arg("--name-only");
        if let Some(s) = since {
            cmd.arg(format!("--since={s}"));
        }
        if let Some(scope) = &self.scope_rel {
            cmd.arg("--").arg(scope);
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

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut counts: BTreeMap<String, u32> = BTreeMap::new();
        for line in stdout.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            *counts.entry(trimmed.to_owned()).or_insert(0) += 1;
        }
        Ok(counts
            .into_iter()
            .map(|(path, commits)| FileChurn { path, commits })
            .collect())
    }

    /// Repo-root-relative key for an absolute path inside the repo,
    /// falling back to the full path when it lies outside (which the
    /// walkers never produce, but a caller should not silently mis-key).
    pub(crate) fn key_for_absolute(&self, file: &Path) -> String {
        relative_to(file, &self.repo_root).unwrap_or_else(|| file.display().to_string())
    }

    /// Repo-root-relative key for a *target*-relative display path — the
    /// path space [`super::SourceFile::display_path`] and therefore every
    /// call-graph node id lives in.
    ///
    /// This is the join fix: prefixing with the target's own offset from
    /// the repo root puts graph-derived paths in git's path space. A
    /// single-file target is its own display path, so the offset already
    /// *is* the answer.
    pub(crate) fn key_for_display(&self, display_path: &str) -> String {
        if self.target.is_file() {
            return self
                .scope_rel
                .clone()
                .unwrap_or_else(|| display_path.replace('\\', "/"));
        }
        let display_path = display_path.replace('\\', "/");
        match &self.scope_rel {
            Some(scope) => format!("{scope}/{display_path}"),
            None => display_path,
        }
    }
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

        let scope = ChurnScope::resolve(dir.path()).unwrap();
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
        let scope = ChurnScope::resolve(dir.path()).unwrap();
        assert!(scope.collect(Some("2099-01-01")).unwrap().is_empty());
    }

    /// The whole reason this module exists: a subdirectory target still
    /// yields repo-root-relative churn keys, and display paths relative
    /// to that subdirectory are lifted into the same space.
    #[test]
    fn subdirectory_target_joins_on_repo_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let scope = ChurnScope::resolve(&dir.path().join("crates/app")).unwrap();

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

    #[test]
    fn repo_root_target_leaves_display_paths_untouched() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let scope = ChurnScope::resolve(dir.path()).unwrap();
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
        let scope = ChurnScope::resolve(&file).unwrap();
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
        let scope = ChurnScope::resolve(dir.path()).unwrap();
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
        let err = ChurnScope::resolve(dir.path()).unwrap_err();
        assert!(
            matches!(err, ChurnError::NotInGitRepo { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn missing_path_surfaces_an_io_error() {
        let err = ChurnScope::resolve(Path::new("/definitely/does/not/exist")).unwrap_err();
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
