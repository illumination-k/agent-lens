//! End-to-end tests for `agent-lens baseline save` / `check`.
//!
//! Each test drives the binary as a subprocess to pin both the parser
//! wiring and the exit-code semantics CI relies on. The shared helpers
//! mirror those in `cli_smoke.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output, Stdio};

use agent_lens::test_support::{run_git, write_file};
use rstest::rstest;

fn agent_lens(args: &[&str], cwd: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_agent-lens"))
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap()
}

fn stdout_json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("invalid json on stdout: {e}\n{:?}", output))
}

fn assert_status(output: &Output, expected: i32) {
    let actual = output.status.code().unwrap_or(-1);
    if actual != expected {
        panic!(
            "expected exit {expected}, got {actual}\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[rstest]
#[case::complexity("complexity", "fn f(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\n")]
#[case::cohesion(
    "cohesion",
    "struct Thing { a: i32, b: i32 }\nimpl Thing { fn ga(&self) -> i32 { self.a } fn gb(&self) -> i32 { self.b } }\n"
)]
fn save_then_check_clean_returns_zero(#[case] analyzer: &str, #[case] src: &str) {
    let dir = tempfile::tempdir().unwrap();
    let file = write_file(dir.path(), "lib.rs", src);
    let snap = dir.path().join("snap.json");

    let save = agent_lens(
        &[
            "baseline",
            "save",
            analyzer,
            file.to_str().unwrap(),
            "--out",
            snap.to_str().unwrap(),
        ],
        dir.path(),
    );
    assert_status(&save, 0);
    assert!(snap.exists());

    let check = agent_lens(
        &[
            "baseline",
            "check",
            analyzer,
            file.to_str().unwrap(),
            "--baseline",
            snap.to_str().unwrap(),
        ],
        dir.path(),
    );
    assert_status(&check, 0);
    let report = stdout_json(&check);
    assert_eq!(report["analyzer"], analyzer);
    assert_eq!(report["summary"]["regressed"], 0);
}

#[test]
fn complexity_check_flags_worsened_function_with_exit_two() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_file(
        dir.path(),
        "lib.rs",
        "fn f(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\n",
    );
    let snap = dir.path().join("snap.json");

    let save = agent_lens(
        &[
            "baseline",
            "save",
            "complexity",
            file.to_str().unwrap(),
            "--out",
            snap.to_str().unwrap(),
        ],
        dir.path(),
    );
    assert_status(&save, 0);

    // Bump cognitive: nested if/else chain.
    std::fs::write(
        &file,
        "fn f(n: i32) -> i32 { if n > 0 { if n > 5 { if n > 10 { 1 } else { 2 } } else { 3 } } else { 0 } }\n",
    )
    .unwrap();

    let check = agent_lens(
        &[
            "baseline",
            "check",
            "complexity",
            file.to_str().unwrap(),
            "--baseline",
            snap.to_str().unwrap(),
        ],
        dir.path(),
    );
    assert_status(&check, 2);
    let report = stdout_json(&check);
    assert_eq!(report["summary"]["regressed"], 1);
    let regressions = report["regressions"].as_array().unwrap();
    assert_eq!(regressions[0]["id"]["name"], "f");
    assert_eq!(regressions[0]["kind"], "worsened");
    let cognitive_delta = regressions[0]["deltas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["metric"] == "cognitive")
        .expect("cognitive delta present");
    assert!(cognitive_delta["delta"].as_f64().unwrap() > 0.0);
}

#[test]
fn complexity_check_grandfathers_existing_debt() {
    // `debt` is already complex in the baseline. Re-run check without
    // touching it (just adding a trivial helper) → exit 0, no
    // regression on `debt`. Adding a new function with cognitive=0
    // doesn't trip strict policy either, so the run stays clean.
    let dir = tempfile::tempdir().unwrap();
    let src = "
fn debt(n: i32) -> i32 {
    if n > 0 { if n > 5 { if n > 10 { 1 } else { 2 } } else { 3 } } else { 0 }
}
";
    let file = write_file(dir.path(), "lib.rs", src);
    let snap = dir.path().join("snap.json");

    assert_status(
        &agent_lens(
            &[
                "baseline",
                "save",
                "complexity",
                file.to_str().unwrap(),
                "--out",
                snap.to_str().unwrap(),
            ],
            dir.path(),
        ),
        0,
    );

    // Append a trivial helper (cognitive=0); leave `debt` untouched.
    std::fs::write(&file, format!("{src}\nfn helper() {{}}\n")).unwrap();

    let check = agent_lens(
        &[
            "baseline",
            "check",
            "complexity",
            file.to_str().unwrap(),
            "--baseline",
            snap.to_str().unwrap(),
        ],
        dir.path(),
    );
    assert_status(&check, 0);
    let report = stdout_json(&check);
    assert_eq!(report["summary"]["regressed"], 0);
    assert_eq!(report["summary"]["new_items"], 1);
}

#[test]
fn complexity_new_function_strict_is_a_regression_with_exit_two() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_file(
        dir.path(),
        "lib.rs",
        "fn solo() { let x = 1; let _y = x; }\n",
    );
    let snap = dir.path().join("snap.json");
    assert_status(
        &agent_lens(
            &[
                "baseline",
                "save",
                "complexity",
                file.to_str().unwrap(),
                "--out",
                snap.to_str().unwrap(),
            ],
            dir.path(),
        ),
        0,
    );

    // Add a brand-new function with non-zero cognitive.
    std::fs::write(
        &file,
        "fn solo() { let x = 1; let _y = x; }\nfn newf(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\n",
    )
    .unwrap();

    let check = agent_lens(
        &[
            "baseline",
            "check",
            "complexity",
            file.to_str().unwrap(),
            "--baseline",
            snap.to_str().unwrap(),
        ],
        dir.path(),
    );
    assert_status(&check, 2);
    let report = stdout_json(&check);
    let regressions = report["regressions"].as_array().unwrap();
    assert!(regressions.iter().any(|r| r["id"]["name"] == "newf"));
    let new_reg = regressions
        .iter()
        .find(|r| r["id"]["name"] == "newf")
        .unwrap();
    assert_eq!(new_reg["kind"], "new");
}

#[test]
fn complexity_new_function_ignore_policy_does_not_fail() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_file(
        dir.path(),
        "lib.rs",
        "fn solo() { let x = 1; let _y = x; }\n",
    );
    let snap = dir.path().join("snap.json");
    assert_status(
        &agent_lens(
            &[
                "baseline",
                "save",
                "complexity",
                file.to_str().unwrap(),
                "--out",
                snap.to_str().unwrap(),
            ],
            dir.path(),
        ),
        0,
    );
    std::fs::write(
        &file,
        "fn solo() { let x = 1; let _y = x; }\nfn newf(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\n",
    )
    .unwrap();

    let check = agent_lens(
        &[
            "baseline",
            "check",
            "complexity",
            file.to_str().unwrap(),
            "--baseline",
            snap.to_str().unwrap(),
            "--new-item-policy",
            "ignore",
        ],
        dir.path(),
    );
    assert_status(&check, 0);
    let report = stdout_json(&check);
    assert_eq!(report["summary"]["regressed"], 0);
    assert_eq!(report["summary"]["new_items"], 1);
}

#[test]
fn complexity_diff_only_narrows_check_to_changed_functions() {
    // Two functions both worsened against the baseline: `pre`
    // (committed worsening) and `other` (unstaged worsening).
    // `git diff -U0` (the source the FilterConfig consults) only
    // reports unstaged hunks, so `--diff-only` keeps `other` and
    // drops `pre` from the current-side comparison. Without
    // `--diff-only`, both regressions appear.
    let dir = tempfile::tempdir().unwrap();
    run_git(dir.path(), &["init", "-q", "-b", "main"]);
    run_git(dir.path(), &["config", "user.email", "test@example.com"]);
    run_git(dir.path(), &["config", "user.name", "Test"]);
    let file = write_file(
        dir.path(),
        "lib.rs",
        "fn pre(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\nfn other(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\n",
    );
    run_git(dir.path(), &["add", "."]);
    run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

    let snap = dir.path().join("snap.json");
    assert_status(
        &agent_lens(
            &[
                "baseline",
                "save",
                "complexity",
                file.to_str().unwrap(),
                "--out",
                snap.to_str().unwrap(),
            ],
            dir.path(),
        ),
        0,
    );

    // Worsen `pre` and commit (so its diff is staged-then-committed
    // and won't appear in `git diff` unstaged).
    std::fs::write(
        &file,
        "fn pre(n: i32) -> i32 { if n > 0 { if n > 5 { if n > 10 { 1 } else { 2 } } else { 3 } } else { 0 } }\nfn other(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\n",
    )
    .unwrap();
    run_git(dir.path(), &["add", "lib.rs"]);
    run_git(dir.path(), &["commit", "-q", "-m", "worsen pre"]);

    // Worsen `other` in the working tree, leave it unstaged so the
    // unstaged `git diff` hunk only covers `other`.
    std::fs::write(
        &file,
        "fn pre(n: i32) -> i32 { if n > 0 { if n > 5 { if n > 10 { 1 } else { 2 } } else { 3 } } else { 0 } }\nfn other(n: i32) -> i32 { if n > 0 { if n > 5 { if n > 10 { 1 } else { 2 } } else { 3 } } else { 0 } }\n",
    )
    .unwrap();

    let no_filter = agent_lens(
        &[
            "baseline",
            "check",
            "complexity",
            file.to_str().unwrap(),
            "--baseline",
            snap.to_str().unwrap(),
        ],
        dir.path(),
    );
    assert_status(&no_filter, 2);
    let report = stdout_json(&no_filter);
    let names_no_filter: Vec<&str> = report["regressions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"]["name"].as_str().unwrap())
        .collect();
    assert!(names_no_filter.contains(&"pre"));
    assert!(names_no_filter.contains(&"other"));

    let filtered = agent_lens(
        &[
            "baseline",
            "check",
            "complexity",
            file.to_str().unwrap(),
            "--baseline",
            snap.to_str().unwrap(),
            "--diff-only",
        ],
        dir.path(),
    );
    assert_status(&filtered, 2);
    let report = stdout_json(&filtered);
    let names_filtered: Vec<&str> = report["regressions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"]["name"].as_str().unwrap())
        .collect();
    // `--diff-only` keeps the current-side items that overlap an
    // unstaged hunk — only `other`. `pre` is dropped from the
    // current-side and ends up in `removed_items` (not a regression).
    assert!(names_filtered.contains(&"other"), "got: {names_filtered:?}",);
    assert!(!names_filtered.contains(&"pre"), "got: {names_filtered:?}",);
}

#[test]
fn check_with_missing_baseline_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_file(dir.path(), "lib.rs", "fn solo() {}\n");
    let check = agent_lens(
        &[
            "baseline",
            "check",
            "complexity",
            file.to_str().unwrap(),
            "--baseline",
            dir.path().join("nope.json").to_str().unwrap(),
        ],
        dir.path(),
    );
    assert_status(&check, 1);
    let stderr = String::from_utf8_lossy(&check.stderr);
    assert!(stderr.contains("baseline"), "stderr: {stderr}");
}

#[test]
fn check_with_analyzer_mismatch_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_file(dir.path(), "lib.rs", "fn solo() {}\n");
    let snap = dir.path().join("snap.json");
    // Save complexity, check cohesion → mismatch.
    assert_status(
        &agent_lens(
            &[
                "baseline",
                "save",
                "complexity",
                file.to_str().unwrap(),
                "--out",
                snap.to_str().unwrap(),
            ],
            dir.path(),
        ),
        0,
    );
    let check = agent_lens(
        &[
            "baseline",
            "check",
            "cohesion",
            file.to_str().unwrap(),
            "--baseline",
            snap.to_str().unwrap(),
        ],
        dir.path(),
    );
    assert_status(&check, 1);
    let stderr = String::from_utf8_lossy(&check.stderr);
    assert!(stderr.contains("mismatch"), "stderr: {stderr}");
}

#[test]
fn save_writes_to_default_path_under_dot_agent_lens() {
    // Outside a git tree: defaults anchor at cwd.
    let dir = tempfile::tempdir().unwrap();
    let file = write_file(dir.path(), "lib.rs", "fn solo() {}\n");
    let save = agent_lens(
        &["baseline", "save", "complexity", file.to_str().unwrap()],
        dir.path(),
    );
    assert_status(&save, 0);
    let expected = dir.path().join(".agent-lens/baseline/complexity.json");
    assert!(
        expected.exists(),
        "expected snapshot at {}, stderr: {}",
        expected.display(),
        String::from_utf8_lossy(&save.stderr),
    );
}
