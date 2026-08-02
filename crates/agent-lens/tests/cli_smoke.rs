#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use agent_lens::test_support::{run_git, write_file};
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

const NAPI_SIBLING_RS: &str = r#"
pub struct Summary;

impl Summary {
    pub fn from_raw(raw: &Raw) -> Summary {
        let title = raw.title.clone();
        let authors = raw.authors.clone();
        let keywords = raw.keywords.clone();
        let year = raw.year;
        Summary { title, authors, keywords, year }
    }
}
"#;

const WASM_SIBLING_RS: &str = r#"
pub struct JsSummary;

impl JsSummary {
    pub fn from_raw(raw: &Raw) -> JsSummary {
        let title = raw.title.clone();
        let year = raw.year;
        JsSummary { title, year }
    }
}
"#;

/// Name-anchored pairing has to be reachable both ways an analyzer is
/// driven: the `--paired-by` flag and the `paired-by` profile key. The
/// fixture is a pair that clustering cannot report at all — it drifted
/// below the default threshold — so a passing assertion also proves the
/// mode is doing something the default report cannot.
#[rstest]
#[case::flag(None)]
#[case::profile_key(Some(
    "[profile.drift]\npath = \".\"\nformat = \"md\"\ntools = [\"similarity\"]\n\n[profile.drift.similarity]\npaired-by = \"name\"\n"
))]
fn analyze_similarity_paired_by_reports_drifted_siblings(#[case] config: Option<&str>) {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "napi.rs", NAPI_SIBLING_RS);
    write_file(dir.path(), "wasm.rs", WASM_SIBLING_RS);

    let output = match config {
        None => agent_lens(
            &[
                "analyze",
                "similarity",
                ".",
                "--format",
                "md",
                "--paired-by",
                "name",
            ],
            dir.path(),
            None,
        ),
        Some(toml) => {
            std::fs::write(dir.path().join("agent-lens.toml"), toml).unwrap();
            agent_lens(&["run", "drift"], dir.path(), None)
        }
    };
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("paired by qualified"), "got: {stdout}");
    assert!(stdout.contains("`summary::from_raw`"), "got: {stdout}");
    assert!(
        stdout.contains("napi.rs:`Summary::from_raw`"),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("wasm.rs:`JsSummary::from_raw`"),
        "got: {stdout}"
    );
}

const MIRROR_STRUCT_A_RS: &str = r#"
pub struct Config {
    pub host: String,
    pub port: u16,
    pub retries: u32,
}
"#;

const MIRROR_STRUCT_B_RS: &str = r#"
pub struct JsConfig {
    pub host: String,
    pub port: u16,
    pub retries: u32,
}
"#;

/// The types target has to be reachable both ways an analyzer is
/// driven: the `--target` flag and the `target` profile key. The fixture
/// is two mirror structs with no function bodies at all, so a passing
/// assertion also proves the run compared type definitions rather than
/// silently falling back to the (empty) function corpus.
#[rstest]
#[case::flag(None)]
#[case::profile_key(Some(
    "[profile.shapes]\npath = \".\"\nformat = \"md\"\ntools = [\"similarity\"]\n\n[profile.shapes.similarity]\ntarget = \"types\"\n"
))]
fn analyze_similarity_target_types_reports_mirror_structs(#[case] config: Option<&str>) {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "a.rs", MIRROR_STRUCT_A_RS);
    write_file(dir.path(), "b.rs", MIRROR_STRUCT_B_RS);

    let output = match config {
        None => agent_lens(
            &[
                "analyze",
                "similarity",
                ".",
                "--format",
                "md",
                "--target",
                "types",
            ],
            dir.path(),
            None,
        ),
        Some(toml) => {
            std::fs::write(dir.path().join("agent-lens.toml"), toml).unwrap();
            agent_lens(&["run", "shapes"], dir.path(), None)
        }
    };
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("2 type(s)"), "got: {stdout}");
    assert!(stdout.contains("2 types, similarity"), "got: {stdout}");
    assert!(stdout.contains("a.rs:`Config`"), "got: {stdout}");
    assert!(stdout.contains("b.rs:`JsConfig`"), "got: {stdout}");
}

/// JSON is the machine-facing contract: the types target must carry the
/// `target` discriminator and per-unit `kind` labels.
#[test]
fn analyze_similarity_target_types_json_carries_target_and_kind() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "a.rs", MIRROR_STRUCT_A_RS);
    write_file(dir.path(), "b.rs", MIRROR_STRUCT_B_RS);

    let output = agent_lens(
        &["analyze", "similarity", ".", "--target", "types"],
        dir.path(),
        None,
    );
    let json = stdout_json(&output);
    assert_eq!(json["target"], "types", "got {json}");
    let units = json["clusters"][0]["units"].as_array().unwrap();
    assert!(units.iter().all(|u| u["kind"] == "struct"), "got {json}");
}

#[test]
fn analyze_similarity_target_types_rejects_paired_by_method() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "a.rs", MIRROR_STRUCT_A_RS);

    let output = agent_lens(
        &[
            "analyze",
            "similarity",
            ".",
            "--target",
            "types",
            "--paired-by",
            "method",
        ],
        dir.path(),
        None,
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--paired-by method is incompatible with --target types"),
        "got: {stderr}"
    );
}

const DOCUMENTED_PAIR_RS: &str = r#"
/// Validate the user id before persisting.
fn validate_user(id: u64) -> bool {
    let raw = id;
    if raw == 0 {
        false
    } else {
        raw > 10
    }
}

/// Validate the order id before persisting.
fn validate_order(id: u64) -> bool {
    let raw = id;
    if raw == 0 {
        false
    } else {
        raw > 10
    }
}
"#;

/// The doc-overlap rollup has to be reachable both ways an analyzer is
/// driven: the `--doc-overlap` flag and the `doc-overlap` profile key.
#[rstest]
#[case::flag(None)]
#[case::profile_key(Some(
    "[profile.audit]\npath = \".\"\nformat = \"md\"\ntools = [\"similarity\"]\n\n[profile.audit.similarity]\nthreshold = 0.8\ndoc-overlap = true\n"
))]
fn similarity_markdown_reports_doc_overlap(#[case] config: Option<&str>) {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "lib.rs", DOCUMENTED_PAIR_RS);

    let output = match config {
        None => agent_lens(
            &[
                "analyze",
                "similarity",
                ".",
                "--format",
                "md",
                "--threshold",
                "0.8",
                "--doc-overlap",
            ],
            dir.path(),
            None,
        ),
        Some(toml) => {
            std::fs::write(dir.path().join("agent-lens.toml"), toml).unwrap();
            agent_lens(&["run", "audit"], dir.path(), None)
        }
    };
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("doc overlap 67–67% (1/1 pairs documented)"),
        "got: {stdout}",
    );
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
fn run_profile_drives_untested_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "src/lib.rs",
        "pub fn covered() -> usize { 1 }\n\
         pub fn never_reached() -> usize { 2 }\n\
         #[cfg(test)]\n\
         mod tests {\n\
         use super::*;\n\
         #[test]\n\
         fn t() { covered(); }\n\
         }\n",
    );
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.gaps]\npath = \"src\"\ntools = [\"untested\"]\n\n\
         [profile.gaps.untested]\ntop = 5\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "gaps"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["results"][0]["tool"], "untested");
    let report = &json["results"][0]["report"];
    assert_eq!(report["summary"]["untested_function_count"], 1);
    assert_eq!(
        report["modules"][0]["functions"][0]["qualified_name"],
        "crate::never_reached",
    );
}

#[test]
fn run_profile_drives_visibility_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", "pub mod inner;\n");
    write_file(
        dir.path(),
        "src/inner.rs",
        "pub fn target() -> usize { 1 }\n\
         pub fn caller() -> usize { target() }\n",
    );
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.exposure]\npath = \"src\"\ntools = [\"visibility\"]\n\n\
         [profile.exposure.visibility]\ntop = 5\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "exposure"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["results"][0]["tool"], "visibility");
    let report = &json["results"][0]["report"];
    let findings: Vec<(&str, &str)> = report["modules"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["findings"].as_array().unwrap())
        .map(|f| {
            (
                f["qualified_name"].as_str().unwrap(),
                f["suggested_visibility"].as_str().unwrap(),
            )
        })
        .collect();
    assert!(
        findings.contains(&("crate::inner::target", "drop `pub`")),
        "got: {findings:?}",
    );
}

#[test]
fn run_profile_drives_delegation_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "src/lib.rs",
        "pub mod api {
    pub fn save(id: usize) -> usize { crate::service::save(id) }
}
pub mod service {
    pub fn save(id: usize) -> usize { crate::db::insert(id) }
}
pub mod db {
    pub fn insert(id: usize) -> usize { id + 1 }
    pub fn other(id: usize) -> usize { id }
}
",
    );
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.layers]\npath = \"src\"\ntools = [\"delegation\"]\n\n\
         [profile.layers.delegation]\ntop = 5\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "layers"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["results"][0]["tool"], "delegation");
    let chain = &json["results"][0]["report"]["chains"][0];
    assert_eq!(chain["depth"], 2);
    assert_eq!(chain["terminus"]["qualified_name"], "crate::db::insert");
    let hops: Vec<&str> = chain["hops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hop| hop["qualified_name"].as_str().unwrap())
        .collect();
    assert_eq!(hops, ["crate::api::save", "crate::service::save"]);
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

/// The mistake this catches is writing the profile path relative to the
/// shell's cwd: from `dir` the config is at `cfg/`, so `src` resolves to
/// `cfg/src`, which does not exist even though `dir/src` does. The
/// message has to say that rather than blame the file extension.
#[test]
fn run_reports_a_missing_profile_path_as_a_path_error() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", BRANCHY_RS);
    write_file(
        dir.path(),
        "cfg/agent-lens.toml",
        "[profile.audit]\npath = \"src\"\ntools = [\"complexity\"]\n",
    );

    let output = agent_lens(
        &["run", "audit", "--config", "cfg/agent-lens.toml"],
        dir.path(),
        None,
    );

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("does not exist"), "got: {stderr}");
    assert!(stderr.contains("audit"), "got: {stderr}");
    assert!(stderr.contains("agent-lens.toml"), "got: {stderr}");
    assert!(
        !stderr.contains("unsupported file extension"),
        "got: {stderr}"
    );
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

/// `analyze risk` is the only analyzer that needs *both* a git working
/// tree and a call graph, so its end-to-end wiring is worth spawning the
/// binary for: a missing subcommand registration or a broken path-space
/// join both show up as an empty or zero-churn report here.
#[test]
fn analyze_risk_ranks_a_git_tracked_crate() {
    let dir = tempfile::tempdir().unwrap();
    run_git(dir.path(), &["init", "-q", "-b", "main"]);
    run_git(dir.path(), &["config", "user.email", "test@example.com"]);
    run_git(dir.path(), &["config", "user.name", "Test"]);
    write_file(dir.path(), "src/lib.rs", "pub mod core;\npub mod app;\n");
    write_file(dir.path(), "src/core.rs", "pub fn sink() {}\n");
    write_file(
        dir.path(),
        "src/app.rs",
        "pub fn one() { crate::core::sink(); }\npub fn two() { crate::core::sink(); }\n",
    );
    run_git(dir.path(), &["add", "."]);
    run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

    let json = stdout_json(&agent_lens(&["analyze", "risk", "."], dir.path(), None));
    let files = json["files"].as_array().unwrap();
    let core = files
        .iter()
        .find(|f| f["path"] == "src/core.rs")
        .unwrap_or_else(|| panic!("no src/core.rs row in {json}"));
    assert_eq!(core["centrality_rank"], 1, "got {json}");
    assert!(core["commits"].as_u64().unwrap() >= 1, "got {json}");
    assert_eq!(core["rank_product"], 1, "got {json}");
}

#[rstest]
#[case::index("## Command index")]
#[case::index_row("| `agent-lens analyze hotspot` |")]
#[case::risk_index_row("| `agent-lens analyze risk` |")]
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
