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

/// Two functions with different names, different signatures, and
/// different surrounding work, sharing one repeated four-statement
/// fragment. Function-granularity comparison cannot see it — the bodies
/// as wholes are not similar — which is the whole reason `--target
/// blocks` exists.
const SHARED_BLOCK_A_RS: &str = r#"
fn fetch_article(id: u64) -> String {
    let client = build_client();
    let url = format!("{}/article/{}", base_url(), id);
    let request = client.get(&url);
    let response = request.send();
    let body = response.text();
    body
}
"#;

const SHARED_BLOCK_B_RS: &str = r#"
fn fetch_author(name: &str, retries: u32) -> Vec<String> {
    let mut collected = Vec::new();
    for _ in 0..retries {
        collected.push(name.to_owned());
    }
    let client = build_client();
    let url = format!("{}/author/{}", base_url(), name);
    let request = client.get(&url);
    let response = request.send();
    let body = response.text();
    collected.push(body);
    collected
}
"#;

/// The blocks target has to be reachable both ways an analyzer is
/// driven: the `--target` flag and the `target` profile key.
#[rstest]
#[case::flag(None)]
#[case::profile_key(Some(
    "[profile.fragments]\npath = \".\"\nformat = \"md\"\ntools = [\"similarity\"]\n\n[profile.fragments.similarity]\ntarget = \"blocks\"\n"
))]
fn analyze_similarity_target_blocks_reports_repeated_fragments(#[case] config: Option<&str>) {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "a.rs", SHARED_BLOCK_A_RS);
    write_file(dir.path(), "b.rs", SHARED_BLOCK_B_RS);

    let output = match config {
        None => agent_lens(
            &[
                "analyze",
                "similarity",
                ".",
                "--format",
                "md",
                "--target",
                "blocks",
            ],
            dir.path(),
            None,
        ),
        Some(toml) => {
            std::fs::write(dir.path().join("agent-lens.toml"), toml).unwrap();
            agent_lens(&["run", "fragments"], dir.path(), None)
        }
    };
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("block(s)"), "got: {stdout}");
    assert!(stdout.contains("2 blocks, similarity"), "got: {stdout}");
    assert!(
        stdout.contains("in 2 function(s) across 2 file(s)"),
        "got: {stdout}",
    );
    // The report quotes the repeated source and breaks occurrences down
    // by file rather than listing every member.
    assert!(
        stdout.contains("let request = client.get(&url);"),
        "got: {stdout}"
    );
    assert!(
        stdout.contains("occurrences: a.rs ×1, b.rs ×1"),
        "got: {stdout}",
    );
}

/// A function whose only duplication is with itself must report
/// nothing: sliding windows overlap by construction, and without the
/// overlap filter every multi-statement function would report as a
/// cluster of its own sub-windows.
#[test]
fn analyze_similarity_target_blocks_ignores_overlapping_windows() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "a.rs", SHARED_BLOCK_A_RS);

    let output = agent_lens(
        &["analyze", "similarity", ".", "--target", "blocks"],
        dir.path(),
        None,
    );
    let json = stdout_json(&output);
    assert_eq!(json["target"], "blocks", "got {json}");
    assert_eq!(json["cluster_count"], 0, "got {json}");
}

#[test]
fn analyze_similarity_target_blocks_rejects_paired_by() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "a.rs", SHARED_BLOCK_A_RS);

    let output = agent_lens(
        &[
            "analyze",
            "similarity",
            ".",
            "--target",
            "blocks",
            "--paired-by",
            "name",
        ],
        dir.path(),
        None,
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--paired-by is incompatible with --target blocks"),
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

/// End-to-end shape of the monorepo case: two sibling trees, one
/// invocation, one report whose clusters span both.
#[test]
fn analyze_accepts_several_paths_and_reports_across_them() {
    const BODY: &str = "\
fn %NAME%(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
";
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "packages/core/src/lib.rs",
        &BODY.replace("%NAME%", "alpha"),
    );
    write_file(
        dir.path(),
        "cli/src/main.rs",
        &BODY.replace("%NAME%", "beta"),
    );
    write_file(
        dir.path(),
        "web/src/lib.rs",
        &BODY.replace("%NAME%", "gamma"),
    );

    let output = agent_lens(
        &[
            "analyze",
            "similarity",
            "packages",
            "cli",
            "--format",
            "md",
            "--threshold",
            "0.5",
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
    assert!(
        stdout.contains("Similarity report: packages, cli"),
        "the report names every root: {stdout}",
    );
    assert!(stdout.contains("packages/core/src/lib.rs"), "got: {stdout}");
    assert!(stdout.contains("cli/src/main.rs"), "got: {stdout}");
    assert!(
        !stdout.contains("web/src/lib.rs"),
        "an untargeted tree must stay out: {stdout}",
    );
}

/// `coupling` grows one module graph from one entry, so it keeps the
/// single-PATH signature and says so rather than ignoring the extra.
#[test]
fn analyze_coupling_rejects_a_second_path() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "a/src/lib.rs", "pub fn a() {}\n");
    write_file(dir.path(), "b/src/lib.rs", "pub fn b() {}\n");

    let output = agent_lens(&["analyze", "coupling", "a", "b"], dir.path(), None);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("unexpected argument"), "got: {stderr}");
}

/// `--top` is what bounds a `--format md` report, and the two analyzers
/// that used to reject it are the ones with the longest listings.
#[rstest]
#[case::coupling(&["analyze", "coupling", "src/lib.rs", "--format", "md", "--top", "1"], "top 1")]
#[case::wrapper(&["analyze", "wrapper", "src", "--format", "md", "--top", "1"], "not shown")]
fn top_bounds_the_markdown_report(#[case] args: &[&str], #[case] expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", "pub mod a;\npub mod b;\n");
    write_file(
        dir.path(),
        "src/a.rs",
        "pub fn helper() {}\npub fn one(x: &str) -> String { inner_one(x) }\n",
    );
    write_file(
        dir.path(),
        "src/b.rs",
        "pub fn two(x: &str) -> String { crate::a::helper(); inner_two(x) }\n\
         pub fn three(x: &str) -> String { inner_three(x) }\n",
    );

    let output = agent_lens(args, dir.path(), None);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains(expected), "got: {stdout}");
}

/// A failing hook still answers in the agent's own response schema and
/// exits 0, so the agent is told the lens broke instead of being handed
/// an empty stdout. The error is logged to stderr as well.
#[rstest]
#[case::claude_session_start(&["hook", "session-start", "summary"], "session-start")]
#[case::claude_pre_tool_use(&["hook", "pre-tool-use", "complexity"], "pre-tool-use")]
#[case::claude_post_tool_use(&["hook", "post-tool-use", "similarity"], "post-tool-use")]
#[case::codex_session_start(&["codex-hook", "session-start", "summary"], "codex session-start")]
#[case::codex_pre_tool_use(&["codex-hook", "pre-tool-use", "complexity"], "codex pre-tool-use")]
#[case::codex_post_tool_use(
    &["codex-hook", "post-tool-use", "similarity"],
    "codex post-tool-use",
)]
fn invalid_hook_payload_is_reported_in_the_hook_response(
    #[case] args: &[&str],
    #[case] event: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let output = agent_lens(args, dir.path(), Some("{}"));
    let stderr = String::from_utf8(output.stderr.clone()).unwrap();
    assert!(stderr.contains("hook failed"), "got: {stderr}");

    let json = stdout_json(&output);
    // Claude Code's tool-use events carry the report in `systemMessage`;
    // the SessionStart events and Codex's PostToolUse use
    // `hookSpecificOutput.additionalContext`.
    let report = json["systemMessage"]
        .as_str()
        .or_else(|| {
            json.pointer("/hookSpecificOutput/additionalContext")?
                .as_str()
        })
        .unwrap_or_else(|| panic!("no report field in {json}"));
    assert!(
        report.contains(&format!("agent-lens {event} hook failed")),
        "got: {report}",
    );
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

/// `--digest` replaces the stacked sections with the entity rollup:
/// one row per file with a drill-down command, corpus lines for the
/// module-shaped tools, and an explicit "nothing to report" list so a
/// quiet analyzer stays distinguishable from one that never ran.
#[test]
fn run_profile_digest_transposes_sections_into_entity_rows() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "src/lib.rs",
        "fn work(x: i32) -> i32 { x + 1 }\npub fn call_work(x: i32) -> i32 { work(x) }\n",
    );
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\nformat = \"md\"\ntools = [\"wrapper\", \"complexity\", \"cycles\"]\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "audit", "--digest"], dir.path(), None);
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("# Digest: audit"), "got: {stdout}");
    assert!(
        stdout.contains("- src/lib.rs — 1 forwarding wrapper (`call_work`)"),
        "got: {stdout}",
    );
    assert!(
        stdout.contains("detail: `agent-lens analyze wrapper src/lib.rs --format md`"),
        "got: {stdout}",
    );
    // The stacked per-tool sections are replaced, not prefixed.
    assert!(!stdout.contains("## wrapper"), "got: {stdout}");
    // Both quiet analyzers are named: neither file crosses the
    // complexity floor and there is no call cycle.
    assert!(
        stdout.contains("Nothing to report from: complexity, cycles."),
        "got: {stdout}",
    );
}

/// The digest is its own rendering, so combining it with `--format` is
/// a parse error rather than a silently ignored flag.
#[test]
fn run_profile_digest_rejects_an_explicit_format() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", BRANCHY_RS);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\ntools = [\"complexity\"]\n",
    )
    .unwrap();

    let output = agent_lens(
        &["run", "audit", "--digest", "--format", "json"],
        dir.path(),
        None,
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--digest"), "got: {stderr}");
    assert!(stderr.contains("--format"), "got: {stderr}");
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

/// The motivating case for a `path` array: the clone spans `internal`
/// and `cmd`, so a profile that can only name one of the two trees
/// cannot see it at all.
#[test]
fn run_profile_with_several_paths_finds_a_clone_spanning_them() {
    const BODY: &str = "
fn %NAME%(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}
";
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "internal/helper.rs",
        &BODY.replace("%NAME%", "a"),
    );
    write_file(dir.path(), "cmd/main.rs", &BODY.replace("%NAME%", "b"));
    // Not part of the corpus: a profile naming two trees must walk those
    // two and no others.
    write_file(dir.path(), "vendor/dep.rs", &BODY.replace("%NAME%", "c"));
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.backend]\npath = [\"internal\", \"cmd\"]\ntools = [\"similarity\"]\n\n\
         [profile.backend.similarity]\nthreshold = 0.5\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "backend"], dir.path(), None);
    let json = stdout_json(&output);
    let report = &json["results"][0]["report"];
    assert_eq!(report["unit_count"], 2, "got: {report}");
    assert_eq!(report["cluster_count"], 1, "got: {report}");
    let files: Vec<&str> = report["clusters"][0]["units"]
        .as_array()
        .unwrap()
        .iter()
        .map(|unit| unit["file"].as_str().unwrap())
        .collect();
    assert_eq!(
        files,
        ["cmd/main.rs", "internal/helper.rs"],
        "got: {report}"
    );
}

/// `coupling` and `context-span` grow one module graph from one entry
/// point, so the config rejects a wider corpus for them by name rather
/// than picking a path and running a report nobody asked for.
#[rstest]
#[case::coupling("coupling")]
#[case::context_span("context-span")]
#[case::communities("communities")]
fn run_profile_rejects_several_paths_for_a_single_root_tool(#[case] tool: &str) {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "internal/lib.rs", BRANCHY_RS);
    write_file(dir.path(), "cmd/main.rs", BRANCHY_RS);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        format!("[profile.backend]\npath = [\"internal\", \"cmd\"]\ntools = [\"{tool}\"]\n"),
    )
    .unwrap();

    let output = agent_lens(&["run", "backend"], dir.path(), None);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(tool), "got: {stderr}");
    assert!(stderr.contains("single"), "got: {stderr}");
}

/// A missing path is reported by name, so a reader of a multi-path
/// profile knows which entry to fix.
#[test]
fn run_profile_names_the_missing_path_of_a_multi_path_profile() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "internal/lib.rs", BRANCHY_RS);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.backend]\npath = [\"internal\", \"cmd\"]\ntools = [\"similarity\"]\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "backend"], dir.path(), None);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("cmd"), "got: {stderr}");
    assert!(!stderr.contains("\"internal\""), "got: {stderr}");
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
fn run_profile_drives_unreachable_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "src/main.rs",
        "fn main() { covered(); }\n\
         fn covered() -> usize { 1 }\n\
         fn never_reached() -> usize { 2 }\n",
    );
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.dead]\npath = \"src\"\ntools = [\"unreachable\"]\n\n\
         [profile.dead.unreachable]\ntop = 5\ntier = \"unknown\"\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "dead"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["results"][0]["tool"], "unreachable");
    let report = &json["results"][0]["report"];
    assert_eq!(report["summary"]["confirmed_count"], 1);
    assert_eq!(
        report["modules"][0]["findings"][0]["qualified_name"],
        "crate::never_reached",
    );
    assert_eq!(report["modules"][0]["findings"][0]["tier"], "confirmed");
}

#[test]
fn run_profile_drives_single_use_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "src/lib.rs",
        "fn helper() -> usize { 1 }\n\
         fn shared() -> usize { 2 }\n\
         pub fn caller() -> usize { helper() + shared() }\n\
         pub fn other() -> usize { shared() }\n",
    );
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.inline]\npath = \"src\"\ntools = [\"single-use\"]\n\n\
         [profile.inline.single-use]\nmax-loc = 10\nmax-cyclomatic = 3\ntop = 5\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "inline"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["results"][0]["tool"], "single-use");
    let report = &json["results"][0]["report"];
    assert_eq!(report["thresholds"]["max_loc"], 10);
    assert_eq!(report["thresholds"]["max_cyclomatic"], 3);
    let candidates = report["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1, "got {candidates:?}");
    assert_eq!(candidates[0]["qualified_name"], "crate::helper");
    assert_eq!(candidates[0]["caller"]["qualified_name"], "crate::caller");
}

#[test]
fn run_profile_drives_single_impl_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "src/lib.rs",
        "trait Store { fn get(&self) -> usize; }\n\
         struct Memory;\n\
         impl Store for Memory { fn get(&self) -> usize { 1 } }\n",
    );
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.abstractions]\npath = \"src\"\ntools = [\"single-impl\"]\n\n\
         [profile.abstractions.single-impl]\ntop = 5\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "abstractions"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["results"][0]["tool"], "single-impl");
    let report = &json["results"][0]["report"];
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "got {findings:?}");
    assert_eq!(findings[0]["display_name"], "Store");
    assert_eq!(
        findings[0]["production_implementors"],
        serde_json::json!(["Memory"]),
    );
}

#[test]
fn run_profile_drives_test_only_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    write_file(
        dir.path(),
        "src/lib.rs",
        "fn fixture() -> usize { 1 }\n\
         pub fn api() -> usize { 2 }\n\
         #[cfg(test)]\n\
         mod tests {\n\
             #[test]\n\
             fn t() { let v = crate::fixture(); assert_eq!(v, 1); }\n\
         }\n",
    );
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.seams]\npath = \"src\"\ntools = [\"test-only\"]\n\n\
         [profile.seams.test-only]\ntop = 5\n",
    )
    .unwrap();

    let output = agent_lens(&["run", "seams"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["results"][0]["tool"], "test-only");
    let report = &json["results"][0]["report"];
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "got {findings:?}");
    assert_eq!(findings[0]["qualified_name"], "crate::fixture");
    assert_eq!(findings[0]["kind"], "test_only");
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
fn baseline_create_snapshots_profile_metrics() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", BRANCHY_RS);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\ntools = [\"complexity\", \"cohesion\"]\n",
    )
    .unwrap();

    let output = agent_lens(&["baseline", "create", "audit"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["profile"], "audit");
    assert_eq!(json["target"], "src");
    assert_eq!(json["tools"][0]["tool"], "complexity");
    assert_eq!(json["tools"][0]["metrics"]["file_count"], 1);
    assert_eq!(json["tools"][0]["metrics"]["cyclomatic_max"], 2);
    assert_eq!(json["tools"][1]["tool"], "cohesion");
    // Outside a git tree the snapshot still stands; it just cannot say
    // which commit it describes.
    assert!(json.get("commit").is_none(), "got: {json}");
    assert!(json.get("skipped").is_none(), "got: {json}");
}

/// A profile's `format` shapes the report a reader gets from `run`; a
/// snapshot is built from structured fields either way.
#[test]
fn baseline_create_ignores_the_profiles_markdown_format() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", BRANCHY_RS);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\nformat = \"md\"\ntools = [\"complexity\"]\n",
    )
    .unwrap();

    let output = agent_lens(&["baseline", "create", "audit"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["tools"][0]["metrics"]["function_count"], 1);
}

#[test]
fn baseline_create_records_the_commit_and_repeats_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", BRANCHY_RS);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\ntools = [\"complexity\"]\n",
    )
    .unwrap();
    run_git(dir.path(), &["init", "-q", "-b", "main"]);
    run_git(dir.path(), &["add", "."]);
    run_git(dir.path(), &["commit", "-q", "-m", "init"]);

    let first = agent_lens(&["baseline", "create", "audit"], dir.path(), None);
    let second = agent_lens(&["baseline", "create", "audit"], dir.path(), None);
    let json = stdout_json(&first);
    assert_eq!(
        json["commit"].as_str().map(str::len),
        Some(40),
        "got: {json}"
    );
    // Nothing in a snapshot reads the clock, so re-running it against an
    // unchanged tree must not produce a diff.
    assert_eq!(first.stdout, second.stdout);
}

#[test]
fn baseline_create_lists_analyzers_it_cannot_summarize() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", BRANCHY_RS);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\ntools = [\"wrapper\", \"complexity\"]\n",
    )
    .unwrap();

    let output = agent_lens(&["baseline", "create", "audit"], dir.path(), None);
    let json = stdout_json(&output);
    assert_eq!(json["skipped"][0]["tool"], "wrapper");
    assert!(json["skipped"][0]["reason"].is_string(), "got: {json}");
    // The covered tools still make it into the snapshot.
    assert_eq!(json["tools"][0]["tool"], "complexity");
}

#[test]
fn baseline_create_writes_the_snapshot_to_out_path() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", BRANCHY_RS);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\ntools = [\"complexity\"]\n",
    )
    .unwrap();

    let output = agent_lens(
        &[
            "baseline",
            "create",
            "audit",
            "--out",
            "target/lens/baseline.json",
        ],
        dir.path(),
        None,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
    // The directory did not exist beforehand: a snapshot's natural home
    // is a build path nothing has created yet.
    let written = std::fs::read_to_string(dir.path().join("target/lens/baseline.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(json["tools"][0]["tool"], "complexity");
    assert!(written.ends_with("}\n"), "got: {written}");
}

#[test]
fn baseline_create_with_unknown_profile_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\ntools = [\"complexity\"]\n",
    )
    .unwrap();

    let output = agent_lens(&["baseline", "create", "missing"], dir.path(), None);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("agent-lens failed"), "got: {stderr}");
}

/// A function tangled enough to lift every complexity extreme above what
/// [`BRANCHY_RS`] scores, so swapping the two sources moves the snapshot
/// in a known direction.
const TANGLED_RS: &str = "\
fn branchy(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }

fn tangled(n: i32) -> i32 {
    let mut total = 0;
    for i in 0..n {
        if i % 2 == 0 {
            for j in 0..i {
                if j % 3 == 0 {
                    total += j;
                } else if j % 5 == 0 {
                    total -= j;
                }
            }
        }
    }
    total
}
";

/// A project with one profile and a stored snapshot of `source`, ready
/// for a `baseline compare` against whatever the test writes next.
fn project_with_snapshot(source: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", source);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\ntools = [\"complexity\"]\n",
    )
    .unwrap();
    let created = agent_lens(
        &["baseline", "create", "audit", "--out", "baseline.json"],
        dir.path(),
        None,
    );
    assert!(
        created.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&created.stderr),
    );
    dir
}

fn stored_metric(dir: &Path, metric: &str) -> serde_json::Value {
    let document = std::fs::read_to_string(dir.join("baseline.json")).unwrap();
    let json: serde_json::Value = serde_json::from_str(&document).unwrap();
    json["tools"][0]["metrics"][metric].clone()
}

#[test]
fn baseline_compare_against_an_unchanged_tree_exits_zero() {
    let dir = project_with_snapshot(BRANCHY_RS);

    let output = agent_lens(
        &["baseline", "compare", "audit", "baseline.json"],
        dir.path(),
        None,
    );
    let json = stdout_json(&output);
    assert_eq!(json["summary"]["regressed"], 0);
    assert!(json["summary"]["held"].as_u64().unwrap() > 0, "got: {json}");
}

#[test]
fn baseline_compare_exits_two_when_a_gated_metric_worsens() {
    let dir = project_with_snapshot(BRANCHY_RS);
    write_file(dir.path(), "src/lib.rs", TANGLED_RS);

    let output = agent_lens(
        &["baseline", "compare", "audit", "baseline.json"],
        dir.path(),
        None,
    );
    // Exit 2, not 1: "the code got worse" has to be distinguishable from
    // "the tool could not run".
    assert_eq!(output.status.code(), Some(2));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        json["summary"]["regressed"].as_u64().unwrap() > 0,
        "got: {json}",
    );
    let cognitive = json["tools"][0]["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|metric| metric["metric"] == "cognitive_max")
        .unwrap()
        .clone();
    assert_eq!(cognitive["verdict"], "regressed");
    assert_eq!(cognitive["direction"], "lower-is-better");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("metrics regressed against the baseline"),
        "got: {stderr}",
    );
}

/// Growth is not regression: the same edit that adds a function moves
/// `function_count` and `loc_total`, and neither may fail the check.
#[test]
fn baseline_compare_does_not_gate_on_a_growing_surface() {
    let dir = project_with_snapshot(BRANCHY_RS);
    write_file(
        dir.path(),
        "src/lib.rs",
        "fn branchy(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\nfn plain(n: i32) -> i32 { n }\n",
    );

    let output = agent_lens(
        &[
            "baseline",
            "compare",
            "audit",
            "baseline.json",
            "--format",
            "md",
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
    assert!(
        stdout.contains("_Nothing regressed against the baseline._"),
        "got: {stdout}",
    );
    assert!(
        stdout.contains("## Context moved (not gated)"),
        "got: {stdout}"
    );
    assert!(stdout.contains("function_count"), "got: {stdout}");
}

#[test]
fn baseline_compare_update_tightens_the_snapshot_on_an_improvement() {
    let dir = project_with_snapshot(TANGLED_RS);
    let before = stored_metric(dir.path(), "cognitive_max").as_u64().unwrap();
    write_file(dir.path(), "src/lib.rs", BRANCHY_RS);

    let output = agent_lens(
        &["baseline", "compare", "audit", "baseline.json", "--update"],
        dir.path(),
        None,
    );
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
    let after = stored_metric(dir.path(), "cognitive_max").as_u64().unwrap();
    assert!(after < before, "{after} should be below {before}");
}

/// The ratchet only turns one way: `--update` over a regression writes
/// the improvements back but keeps the stricter bar, and still fails.
#[test]
fn baseline_compare_update_never_loosens_a_regressed_metric() {
    let dir = project_with_snapshot(BRANCHY_RS);
    let before = stored_metric(dir.path(), "cognitive_max");
    write_file(dir.path(), "src/lib.rs", TANGLED_RS);

    let output = agent_lens(
        &["baseline", "compare", "audit", "baseline.json", "--update"],
        dir.path(),
        None,
    );
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(stored_metric(dir.path(), "cognitive_max"), before);
    // The surface metrics are not gated, so those do follow the run.
    assert_eq!(stored_metric(dir.path(), "function_count"), 2);
}

#[test]
fn baseline_compare_refuses_a_snapshot_of_another_profile() {
    let dir = project_with_snapshot(BRANCHY_RS);
    std::fs::write(
        dir.path().join("agent-lens.toml"),
        "[profile.audit]\npath = \"src\"\ntools = [\"complexity\"]\n\n\
         [profile.other]\npath = \"src\"\ntools = [\"complexity\"]\n",
    )
    .unwrap();

    let output = agent_lens(
        &["baseline", "compare", "other", "baseline.json"],
        dir.path(),
        None,
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("profile"), "got: {stderr}");
}

#[test]
fn baseline_compare_names_a_snapshot_it_cannot_read() {
    let dir = project_with_snapshot(BRANCHY_RS);

    let output = agent_lens(
        &["baseline", "compare", "audit", "missing.json"],
        dir.path(),
        None,
    );
    // A missing snapshot is a broken invocation, not a regression.
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("missing.json"), "got: {stderr}");
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

/// `analyze co-change` reads history and nothing else, so the binary is
/// the only place its wiring can be checked end to end: a missing
/// subcommand registration, a broken rename map, or a path space that
/// drifted from `hotspot`'s would each show up as a missing or
/// misspelled pair here.
#[test]
fn analyze_co_change_pairs_files_from_a_git_history() {
    let dir = tempfile::tempdir().unwrap();
    run_git(dir.path(), &["init", "-q", "-b", "main"]);
    run_git(dir.path(), &["config", "user.email", "test@example.com"]);
    run_git(dir.path(), &["config", "user.name", "Test"]);
    for i in 0..4 {
        write_file(
            dir.path(),
            "src/api.rs",
            &format!("pub fn api() -> u8 {{ {i} }}\n"),
        );
        // A non-source partner: the coupling no AST-based analyzer here
        // can see, and the reason this analyzer has no language matrix.
        write_file(dir.path(), "api.toml", &format!("version = {i}\n"));
        run_git(dir.path(), &["add", "-A"]);
        run_git(dir.path(), &["commit", "-q", "-m", &format!("bump {i}")]);
    }
    write_file(dir.path(), "src/lone.rs", "pub fn lone() {}\n");
    run_git(dir.path(), &["add", "-A"]);
    run_git(dir.path(), &["commit", "-q", "-m", "lone"]);

    let json = stdout_json(&agent_lens(
        &["analyze", "co-change", "."],
        dir.path(),
        None,
    ));
    let pairs = json["pairs"].as_array().unwrap();
    assert_eq!(pairs.len(), 1, "got {json}");
    assert_eq!(pairs[0]["a"], "api.toml", "got {json}");
    assert_eq!(pairs[0]["b"], "src/api.rs", "got {json}");
    assert_eq!(pairs[0]["cochanges"], 4, "got {json}");
    assert_eq!(json["commit_count"], 5, "got {json}");
    assert!(json.get("shallow_clone").is_none(), "got {json}");
}

/// `analyze change-entropy` reads history and the pending diff, so the
/// binary is the only place its wiring can be checked end to end. The
/// `--diff-only` half especially: a working-tree read that never reached
/// git would report a focused change for every edit, silently.
#[test]
fn analyze_change_entropy_scores_history_and_the_pending_change() {
    let dir = tempfile::tempdir().unwrap();
    run_git(dir.path(), &["init", "-q", "-b", "main"]);
    run_git(dir.path(), &["config", "user.email", "test@example.com"]);
    run_git(dir.path(), &["config", "user.name", "Test"]);
    let body: String = (0..10).map(|i| format!("// line {i}\n")).collect();
    for (day, path) in [(10, "src/a.rs"), (12, "src/b.rs"), (14, "api.toml")] {
        write_file(dir.path(), path, &body);
        run_git(dir.path(), &["add", "-A"]);
        run_git(
            dir.path(),
            &[
                "commit",
                "-q",
                "-m",
                path,
                &format!("--date=2026-08-{day}T12:00:00Z"),
            ],
        );
    }

    let history = stdout_json(&agent_lens(
        &["analyze", "change-entropy", ".", "--min-commits", "1"],
        dir.path(),
        None,
    ));
    assert_eq!(history["commit_count"], 3, "got {history}");
    let periods = history["periods"].as_array().unwrap();
    assert_eq!(periods.len(), 1, "got {history}");
    assert_eq!(periods[0]["period"], "2026-W33", "got {history}");
    // Three files, ten lines each: evenly spread, so maximal scatter.
    assert!(
        (periods[0]["entropy"].as_f64().unwrap_or(0.0) - 1.0).abs() < 1e-9,
        "got {history}",
    );

    write_file(dir.path(), "src/a.rs", "// only this one\n");
    let verdict = stdout_json(&agent_lens(
        &["analyze", "change-entropy", ".", "--diff-only"],
        dir.path(),
        None,
    ));
    assert_eq!(verdict["pending"]["files_touched"], 1, "got {verdict}");
    assert_eq!(verdict["pending"]["files"][0]["path"], "src/a.rs");
    assert_eq!(verdict["reference"]["commit_count"], 3, "got {verdict}");
}

/// `analyze hidden-coupling` joins two views the binary assembles from
/// different halves of the crate — a git history read and the language
/// backends' graphs — so the wiring is only really exercised end to end
/// here: a path space that drifted between them, or a missing subcommand
/// registration, would land the pair in the wrong bucket or in none.
#[test]
fn analyze_hidden_coupling_separates_the_undeclared_pair_from_the_declared_one() {
    let dir = tempfile::tempdir().unwrap();
    run_git(dir.path(), &["init", "-q", "-b", "main"]);
    run_git(dir.path(), &["config", "user.email", "test@example.com"]);
    run_git(dir.path(), &["config", "user.name", "Test"]);
    write_file(
        dir.path(),
        "src/lib.rs",
        "pub mod a;\npub mod b;\npub mod c;\npub mod d;\n",
    );
    for i in 0..4 {
        // `a` imports from `b`, and they move together: expected, so
        // neither bucket may claim them.
        write_file(
            dir.path(),
            "src/a.rs",
            &format!("use crate::b::work;\npub fn run() -> u8 {{ work() + {i} }}\n"),
        );
        write_file(
            dir.path(),
            "src/b.rs",
            &format!("pub fn work() -> u8 {{ {i} }}\n"),
        );
        run_git(dir.path(), &["add", "-A"]);
        run_git(
            dir.path(),
            &["commit", "-q", "-m", &format!("declared {i}")],
        );

        // `c` and `d` move together in their own commits with nothing
        // between them: hidden.
        write_file(
            dir.path(),
            "src/c.rs",
            &format!("pub fn c() -> u8 {{ {i} }}\n"),
        );
        write_file(
            dir.path(),
            "src/d.rs",
            &format!("pub fn d() -> u8 {{ {i} }}\n"),
        );
        run_git(dir.path(), &["add", "-A"]);
        run_git(dir.path(), &["commit", "-q", "-m", &format!("hidden {i}")]);
    }

    let json = stdout_json(&agent_lens(
        &["analyze", "hidden-coupling", "."],
        dir.path(),
        None,
    ));
    let hidden = json["hidden_coupling"].as_array().unwrap();
    assert_eq!(hidden.len(), 1, "got {json}");
    assert_eq!(hidden[0]["a"], "src/c.rs", "got {json}");
    assert_eq!(hidden[0]["b"], "src/d.rs", "got {json}");
    assert_eq!(hidden[0]["static"]["relation"], "no_path", "got {json}");
    assert_eq!(hidden[0]["cochanges"], 4, "got {json}");
    assert!(
        json["suspect_dependencies"].as_array().unwrap().is_empty(),
        "the declared pair co-changes, so it is expected: {json}",
    );
    assert!(
        json["static_view"]["file_count"].as_u64().unwrap() >= 5,
        "got {json}"
    );
}

/// `analyze communities` compares the detected clustering against the
/// declared one, so the binary is where its wiring can be checked end to
/// end: a missing subcommand registration, a declared partition that
/// drifted from the module tree, or a member fold that stopped folding
/// would each show up as a missing or wrongly-attributed row here.
#[test]
fn analyze_communities_names_a_module_wired_into_a_neighbour() {
    let dir = tempfile::tempdir().unwrap();
    write_file(dir.path(), "src/lib.rs", "pub mod a;\npub mod b;\n");
    write_file(dir.path(), "src/a/mod.rs", "pub mod one;\npub mod stray;\n");
    write_file(
        dir.path(),
        "src/a/one.rs",
        "pub struct One;\npub fn one() {}\n",
    );
    write_file(dir.path(), "src/b/mod.rs", "pub mod p;\npub mod q;\n");
    write_file(dir.path(), "src/b/p.rs", "pub struct P;\npub fn p() {}\n");
    write_file(
        dir.path(),
        "src/b/q.rs",
        "use crate::b::p::P;\npub fn q(_p: P) { crate::b::p::p(); }\n",
    );
    // Filed under `a`, wired entirely into `b`.
    write_file(
        dir.path(),
        "src/a/stray.rs",
        "use crate::b::p::P;\nuse crate::b::q::q;\npub fn stray(p: P) { q(p); crate::b::p::p(); }\n",
    );

    let json = stdout_json(&agent_lens(
        &["analyze", "communities", "src/lib.rs"],
        dir.path(),
        None,
    ));
    let misfiled = json["misfiled"].as_array().unwrap();
    let stray = misfiled
        .iter()
        .find(|m| m["member"] == "crate::a::stray")
        .unwrap_or_else(|| panic!("no stray row in {json}"));
    assert_eq!(stray["declared"], "crate::a", "got {json}");
    assert_eq!(stray["suggested"], "crate::b", "got {json}");
    assert!(
        json["modularity"]["detected"].as_f64().unwrap()
            >= json["modularity"]["declared"].as_f64().unwrap(),
        "got {json}",
    );
    // Inline `mod tests` blocks would inflate the member count; this
    // fixture has none, so the two counts agree.
    assert_eq!(json["node_count"], json["module_count"], "got {json}");
}

#[rstest]
#[case::index("## Command index")]
#[case::index_row("| `agent-lens analyze hotspot` |")]
#[case::risk_index_row("| `agent-lens analyze risk` |")]
#[case::co_change_index_row("| `agent-lens analyze co-change` |")]
#[case::change_entropy_index_row("| `agent-lens analyze change-entropy` |")]
#[case::communities_index_row("| `agent-lens analyze communities` |")]
#[case::hidden_coupling_index_row("| `agent-lens analyze hidden-coupling` |")]
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
