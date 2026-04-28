//! Shared test helpers used across the crate's `#[cfg(test)]` modules and
//! the binary's CLI tests. Exposed (with `#[doc(hidden)]` on the module) so
//! that the `agent-lens` binary can reach them as `agent_lens::test_support::*`.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

/// Write `contents` to `dir/name`, creating any missing parent directories.
/// Panics on I/O failure — intended for tests only.
pub fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, contents).unwrap();
    path
}

/// Run `git <args>` against `dir` with hardened, isolated config so a host's
/// signing helper or `user.*` defaults can't make the test brittle. Panics
/// on non-zero exit — intended for tests only.
pub fn run_git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-c")
        .arg("commit.gpgsign=false")
        .arg("-c")
        .arg("tag.gpgsign=false")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed in {}", dir.display());
}
