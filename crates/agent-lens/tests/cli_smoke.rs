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

const SWEEP_RS: &str = r#"
fn alpha(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
fn beta(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
fn gamma(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs {
        if *x > 0 {
            total += x;
        }
    }
    total
}
fn delta(ys: &[i64]) -> i64 {
    let mut sum = 0;
    for y in ys {
        if *y > 1 {
            sum += y;
        }
    }
    sum
}
"#;

#[test]
fn analyze_similarity_sweep_clusters_at_floor_and_tags_survival() {
    // End-to-end through the real binary: the `--sweep` ladder must reach
    // the analyzer (not be silently dropped) and surface the per-cluster
    // survival tags in the markdown report.
    let dir = tempfile::tempdir().unwrap();
    let file = write_file(dir.path(), "lib.rs", SWEEP_RS);

    let output = agent_lens(
        &[
            "analyze",
            "similarity",
            file.to_str().unwrap(),
            "--format",
            "md",
            "--sweep",
            "0.6,0.75,0.85",
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
    assert!(stdout.contains("sweep [0.60, 0.75, 0.85]"), "got: {stdout}");
    // The verbatim pair survives the top rung; the structural pair only the
    // middle one. Both tags appearing proves the floor cut surfaced a pair a
    // plain 0.85 run would have dropped.
    assert!(stdout.contains("[survives ≥0.85]"), "got: {stdout}");
    assert!(stdout.contains("[survives ≥0.75]"), "got: {stdout}");
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

const BRANCHY_RS: &str = "fn branchy(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\n";

#[test]
fn run_profile_emits_combined_markdown_report() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", BRANCHY_RS);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\nformat = \"md\"\ntools = [\"complexity\", \"wrapper\"]\n\n[profile.audit.complexity]\nmin-score = 1\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "audit"], dir.path(), None);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("## complexity"), "got: {stdout}");
    assert!(stdout.contains("## wrapper"), "got: {stdout}");
    assert!(stdout.contains("`branchy`"), "got: {stdout}");
}

#[test]
fn run_profile_emits_combined_json_report() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", BRANCHY_RS);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\ntools = [\"complexity\"]\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "audit"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["profile"], "audit");
    assert_eq!(json["results"][0]["tool"], "complexity");
    assert!(json["results"][0]["report"].is_object(), "got: {json}");
}

#[test]
fn run_profile_drives_graph_query_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "src/lib.rs",
        "fn sink() {}\nfn caller() { sink(); }\n",
    );
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.trace]\npath = \"src\"\ntools = [\"graph-query\"]\n\n\
         [profile.trace.graph-query]\nquery = \"callers\"\nsymbol = \"sink\"\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "trace"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["results"][0]["tool"], "graph-query");
    let report = &json["results"][0]["report"];
    assert_eq!(report["status"], "ok");
    assert_eq!(report["results"][0]["qualified_name"], "crate::caller");
}

#[test]
fn run_profile_drives_impact_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "src/lib.rs",
        "fn sink() {}\nfn caller() { sink(); }\n",
    );
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.blast]\npath = \"src\"\ntools = [\"impact\"]\n\n\
         [profile.blast.impact]\nfunction = [\"sink\"]\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "blast"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["results"][0]["tool"], "impact");
    let report = &json["results"][0]["report"];
    assert_eq!(report["status"], "ok");
    assert_eq!(
        report["changed"][0]["direct_callers"][0]["qualified_name"],
        "crate::caller",
    );
}

#[test]
fn run_profile_rejects_graph_query_without_options_table() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", BRANCHY_RS);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.trace]\npath = \"src\"\ntools = [\"graph-query\"]\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "trace"], dir.path(), None);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("graph-query"), "got: {stderr}");
}

#[test]
fn run_resolves_target_relative_to_explicit_config_dir() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", BRANCHY_RS);
    write_file(
        dir.path(),
        "cfg/agent-lens.toml",
        "[profile.audit]\npath = \"../src\"\ntools = [\"complexity\"]\n",
    );

    let output = agent_lens(
        &["run", "audit", "--config", "cfg/agent-lens.toml"],
        dir.path(),
        None,
    );
    let json = stdout_json(&output);
    assert_eq!(json["profile"], "audit");
    assert_eq!(json["results"][0]["tool"], "complexity");
}

#[test]
fn run_with_unknown_profile_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\ntools = [\"complexity\"]\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "missing"], dir.path(), None);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("agent-lens failed"), "got: {stderr}");
}

#[test]
fn run_without_a_config_file_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(&["run", "audit"], dir.path(), None);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("agent-lens failed"), "got: {stderr}");
}

#[test]
fn help_md_emits_markdown_reference() {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(&["help", "--md"], dir.path(), None);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("# agent-lens"), "got: {stdout}");
    assert!(
        stdout.contains("### `agent-lens analyze similarity`"),
        "got: {stdout}",
    );
    assert!(stdout.contains("## `agent-lens skills`"), "got: {stdout}");
}

#[rstest]
#[case::index("## Command index")]
#[case::index_row("| `agent-lens analyze hotspot` |")]
#[case::routing("Pick an analyzer by question:")]
#[case::conventions("Reports go to stdout")]
#[case::root_example("    agent-lens help --md")]
#[case::analyzer_example(
    "    agent-lens analyze similarity src/ --sweep 0.6,0.75,0.85 --format md"
)]
#[case::setup_example("    agent-lens hook setup --scope user")]
fn help_md_carries_routing_index_and_examples(#[case] needle: &str) {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(&["help", "--md"], dir.path(), None);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(needle), "missing {needle}\ngot: {stdout}");
}

/// The routing table and conventions block are the payload of the plain
/// `--help` / `help` output, and `analyze --help` is the other place the
/// "which analyzer?" choice gets made, so all three must carry them.
#[rstest]
#[case::flag(&["--help"])]
#[case::subcommand(&["help"])]
#[case::analyze_group(&["analyze", "--help"])]
fn long_help_carries_the_analyzer_routing_table(#[case] args: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(args, dir.path(), None);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("Pick an analyzer by question:"),
        "got: {stdout}",
    );
    assert!(
        stdout.contains("what breaks if I change this?"),
        "got: {stdout}",
    );
    assert!(stdout.contains("Reports go to stdout"), "got: {stdout}");
}

/// The root help is the only place that can point an agent at the full
/// Markdown reference, so it must.
#[rstest]
#[case::flag(&["--help"])]
#[case::subcommand(&["help"])]
fn root_long_help_points_at_the_markdown_reference(#[case] args: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(args, dir.path(), None);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("agent-lens help --md"), "got: {stdout}");
}

/// An analyzer's example block has to survive clap's own rendering, not
/// just be attached to the command. Coverage that *every* analyzer has a
/// block lives in the `cli` unit tests, which need no process spawn.
#[test]
fn analyzer_long_help_ends_with_a_worked_invocation() {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(&["analyze", "impact", "--help"], dir.path(), None);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Examples:"), "got: {stdout}");
    assert!(
        stdout.contains("    agent-lens analyze impact src/ --function Resolver::resolve"),
        "got: {stdout}",
    );
}

#[test]
fn config_schema_emits_profile_and_tool_tables() {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(&["config", "schema"], dir.path(), None);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.starts_with("# agent-lens.toml schema"),
        "got: {stdout}",
    );
    assert!(stdout.contains("## `[profile.<name>]`"), "got: {stdout}");
    assert!(
        stdout.contains("### `[profile.<name>.similarity]`"),
        "got: {stdout}",
    );
    assert!(stdout.contains("```toml"), "got: {stdout}");
}

#[test]
fn skills_list_names_each_bundled_skill() {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(&["skills", "list"], dir.path(), None);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("## agent-lens"), "got: {stdout}");
    assert!(stdout.contains("## find-duplicates"), "got: {stdout}");
    assert!(
        stdout.contains("agent-lens skills install"),
        "got: {stdout}",
    );
}

#[test]
fn skills_install_project_writes_files_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(
        &["skills", "install", "--scope", "project"],
        dir.path(),
        None,
    );
    let json = stdout_json(&output);
    assert_eq!(json["wrote"], true);
    assert!(
        json["created"]
            .as_array()
            .unwrap()
            .contains(&"agent-lens".into())
    );

    let installed = dir.path().join(".claude/skills/agent-lens/SKILL.md");
    assert!(installed.exists());
    let contents = std::fs::read_to_string(&installed).unwrap();
    assert!(contents.contains("name: agent-lens"), "got: {contents}");

    let output = agent_lens(
        &["skills", "install", "--scope", "project"],
        dir.path(),
        None,
    );
    let json = stdout_json(&output);
    assert_eq!(json["wrote"], false);
    assert_eq!(json["unchanged"].as_array().unwrap().len(), 5);
}

#[test]
fn skills_install_dry_run_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(
        &["skills", "install", "--scope", "project", "--dry-run"],
        dir.path(),
        None,
    );
    let json = stdout_json(&output);
    assert_eq!(json["wrote"], false);
    assert!(!dir.path().join(".claude/skills").exists());
}

#[test]
fn skills_install_reports_conflict_until_forced() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join(".claude/skills/agent-lens/SKILL.md");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, "local edits\n").unwrap();

    let output = agent_lens(
        &["skills", "install", "--scope", "project"],
        dir.path(),
        None,
    );
    let json = stdout_json(&output);
    assert!(
        json["conflicts"]
            .as_array()
            .unwrap()
            .contains(&"agent-lens".into())
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "local edits\n");

    let output = agent_lens(
        &["skills", "install", "--scope", "project", "--force"],
        dir.path(),
        None,
    );
    let json = stdout_json(&output);
    assert!(
        json["updated"]
            .as_array()
            .unwrap()
            .contains(&"agent-lens".into())
    );
    assert!(
        std::fs::read_to_string(&target)
            .unwrap()
            .contains("name: agent-lens")
    );
}
