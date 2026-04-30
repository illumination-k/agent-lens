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

/// Create a tiny Rust crate inside an initialized git repo with two commits so
/// hotspot analysis has churn signal to rank.
pub fn init_repo_with_crate_for_session_summary(dir: &Path) {
    run_git(dir, &["init", "-q", "-b", "main"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    write_file(
        dir,
        "src/lib.rs",
        "pub mod a;
pub mod b;
",
    );
    write_file(
        dir,
        "src/a.rs",
        "use crate::b::Bar;
pub struct Foo;
fn _x(_b: Bar) {}
",
    );
    write_file(
        dir,
        "src/b.rs",
        r#"
pub struct Bar;
pub fn nest(n: i32) -> i32 {
    if n > 0 { if n > 1 { if n > 2 { if n > 3 { return n; } } } }
    0
}
"#,
    );
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-q", "-m", "initial"]);
    write_file(
        dir,
        "src/b.rs",
        r#"
pub struct Bar;
pub fn nest(n: i32) -> i32 {
    if n > 0 { if n > 1 { if n > 2 { if n > 3 { return n; } if n > 4 { return n + 1; } } } }
    0
}
"#,
    );
    run_git(dir, &["add", "src/b.rs"]);
    run_git(dir, &["commit", "-q", "-m", "tweak b"]);
}

/// Initialize a git repo with a Rust file but no top-level crate root
/// (`src/lib.rs` or `src/main.rs`).
pub fn init_repo_with_loose_rust_file(dir: &Path) {
    run_git(dir, &["init", "-q", "-b", "main"]);
    run_git(dir, &["config", "user.email", "test@example.com"]);
    run_git(dir, &["config", "user.name", "Test"]);
    write_file(
        dir,
        "loose.rs",
        r#"
pub fn nest(n: i32) -> i32 {
    if n > 0 { if n > 1 { if n > 2 { if n > 3 { return n; } } } }
    0
}
"#,
    );
    run_git(dir, &["add", "."]);
    run_git(dir, &["commit", "-q", "-m", "initial"]);
}
