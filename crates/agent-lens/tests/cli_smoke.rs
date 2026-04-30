#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use agent_lens::test_support::write_file;
use rstest::rstest;

fn agent_lens(args: &[&str], cwd: &Path, stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-lens"))
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    if let Some(input) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }

    child.wait_with_output().unwrap()
}

fn stdout_json(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn analyze_command_prints_report_with_single_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let file = write_file(
        dir.path(),
        "lib.rs",
        "fn branchy(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\n",
    );

    let output = agent_lens(
        &[
            "analyze",
            "complexity",
            file.to_str().unwrap(),
            "--format",
            "md",
            "--top",
            "1",
            "--min-score",
            "1",
        ],
        dir.path(),
        None,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Top 1 by complexity"), "got: {stdout}");
    assert!(stdout.contains("`branchy`"), "got: {stdout}");
    assert!(stdout.ends_with('\n'), "got: {stdout:?}");
    assert!(!stdout.ends_with("\n\n"), "got: {stdout:?}");
}

#[rstest]
#[case::claude_session_start(&["hook", "session-start", "summary"])]
#[case::claude_pre_tool_use(&["hook", "pre-tool-use", "complexity"])]
#[case::claude_post_tool_use(&["hook", "post-tool-use", "similarity"])]
#[case::codex_session_start(&["codex-hook", "session-start", "summary"])]
#[case::codex_pre_tool_use(&["codex-hook", "pre-tool-use", "complexity"])]
#[case::codex_post_tool_use(&["codex-hook", "post-tool-use", "similarity"])]
fn invalid_hook_payload_exits_nonzero_and_logs_error(#[case] args: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(args, dir.path(), Some("{}"));
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("agent-lens failed"), "got: {stderr}");
}

#[test]
fn hook_setup_project_writes_settings_json_and_reports_idempotence() {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(&["hook", "setup", "--scope", "project"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["wrote"], true);
    let settings = dir.path().join(".claude/settings.json");
    assert!(settings.exists());
    let contents = std::fs::read_to_string(&settings).unwrap();
    assert!(contents.contains("agent-lens hook session-start summary"));
    assert!(contents.contains("agent-lens hook pre-tool-use complexity"));
    assert!(contents.contains("agent-lens hook post-tool-use similarity"));

    let output = agent_lens(&["hook", "setup", "--scope", "project"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["wrote"], false);
}

#[test]
fn hook_setup_project_dry_run_leaves_settings_json_absent() {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(
        &["hook", "setup", "--scope", "project", "--dry-run"],
        dir.path(),
        None,
    );
    let json = stdout_json(&output);
    assert_eq!(json["wrote"], false);
    assert!(!dir.path().join(".claude/settings.json").exists());
}

#[test]
fn codex_hook_setup_project_writes_config_toml() {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(
        &["codex-hook", "setup", "--scope", "project"],
        dir.path(),
        None,
    );
    let json = stdout_json(&output);
    assert_eq!(json["wrote"], true);
    let config = dir.path().join(".codex/config.toml");
    assert!(config.exists());
    let contents = std::fs::read_to_string(&config).unwrap();
    assert!(contents.contains("agent-lens codex-hook session-start summary"));
    assert!(contents.contains("agent-lens codex-hook pre-tool-use complexity"));
    assert!(contents.contains("agent-lens codex-hook post-tool-use similarity"));
}
