//! Codex `SessionStart` hook handler.
//!
//! Runs once per session and injects a one-shot context summary into
//! Codex via `additionalContext`: the highest churn × complexity files
//! (hotspot) and a thumbnail of the crate's coupling graph (top
//! Fan-In/Fan-Out modules, dependency cycles, most coupled pairs).
//!
//! The point is an "onboarding sketch" — what the agent should know
//! about this codebase before it starts touching files. Both halves
//! are best-effort: a session that starts outside a git working tree
//! gets a report without the hotspot section, and a session that isn't
//! anchored at a Rust crate gets one without the coupling section. If
//! neither half produces signal, the hook stays silent and falls
//! through to a default no-op response.
//!
//! Codex is the only one of the two agents that ships a `SessionStart`
//! event today (Claude Code surfaces this affordance through its own
//! `SessionStart` since v1.0.43; we'll wire that in as a parallel
//! handler when the schema lands in `agent-hooks::claude_code`).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent_hooks::Hook;
use agent_hooks::codex::{SessionStartHookSpecificOutput, SessionStartInput, SessionStartOutput};
use lens_domain::{CouplingReport, DependencyCycle, ModuleMetrics, PairCoupling, compute_report};
use lens_rust::{build_module_tree, extract_edges};
use tracing::warn;

use crate::analyze::{HotspotAnalyzer, HotspotError, OutputFormat, resolve_crate_root};

const HOOK_EVENT_NAME: &str = "SessionStart";

/// How many hotspot rows to include in the injected report.
const HOTSPOT_TOP: usize = 5;
/// How many module / pair rows to include in the coupling thumbnail.
const COUPLING_TOP: usize = 5;

/// Errors raised while rendering a SessionStart summary.
///
/// Keeps the surface small: anything fatal (a clap-level wiring bug,
/// say) bubbles up; soft failures like "not inside a git repo" or
/// "directory has no Cargo crate root" are dropped to a `tracing::warn`
/// inside the renderers and the affected section is omitted.
#[derive(Debug, thiserror::Error)]
pub enum SessionStartError {
    #[error("hotspot analyzer failed: {0}")]
    Hotspot(#[from] HotspotError),
    #[error("coupling analyzer failed: {0}")]
    Coupling(#[source] crate::analyze::CrateAnalyzerError),
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Codex SessionStart handler that emits a hotspot + coupling summary.
#[derive(Debug, Default, Clone, Copy)]
pub struct SummaryHook;

impl SummaryHook {
    pub fn new() -> Self {
        Self
    }
}

impl Hook for SummaryHook {
    type Input = SessionStartInput;
    type Output = SessionStartOutput;
    type Error = SessionStartError;

    fn handle(&self, input: Self::Input) -> Result<Self::Output, Self::Error> {
        let cwd = &input.context.cwd;

        let mut sections: Vec<String> = Vec::new();
        if let Some(s) = render_hotspot_section(cwd)? {
            sections.push(s);
        }
        if let Some(s) = render_coupling_section(cwd)? {
            sections.push(s);
        }

        if sections.is_empty() {
            return Ok(SessionStartOutput::default());
        }

        let mut body = String::from("# agent-lens session-start\n");
        for section in &sections {
            body.push('\n');
            body.push_str(section);
        }

        Ok(SessionStartOutput {
            hook_specific_output: Some(SessionStartHookSpecificOutput {
                hook_event_name: HOOK_EVENT_NAME.to_owned(),
                additional_context: Some(body),
            }),
            ..SessionStartOutput::default()
        })
    }
}

/// Run the hotspot analyzer against `cwd` and return a compact section
/// for the SessionStart payload, or `None` when there is nothing to
/// inject (cwd outside a git working tree, no Rust files, every file
/// has score 0). Soft failures are logged to stderr and treated as
/// "no section."
fn render_hotspot_section(cwd: &Path) -> Result<Option<String>, SessionStartError> {
    let json = match HotspotAnalyzer::new()
        .with_top(Some(HOTSPOT_TOP))
        .analyze(cwd, OutputFormat::Json)
    {
        Ok(s) => s,
        Err(HotspotError::NotInGitRepo { .. }) => return Ok(None),
        Err(e) => {
            warn!(cwd = %cwd.display(), error = %e, "skipping hotspot section");
            return Ok(None);
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&json) {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "hotspot analyzer returned non-JSON; skipping");
            return Ok(None);
        }
    };
    let files = parsed.get("files").and_then(|v| v.as_array());
    let Some(files) = files else {
        return Ok(None);
    };
    let mut rows: Vec<HotspotRow> = Vec::new();
    for f in files.iter().take(HOTSPOT_TOP) {
        let Some(row) = HotspotRow::from_value(f) else {
            continue;
        };
        // Files with both 0 churn and 0 cognitive complexity are noise;
        // there is nothing for the agent to act on.
        if row.score == 0 {
            continue;
        }
        rows.push(row);
    }
    if rows.is_empty() {
        return Ok(None);
    }

    let mut out = String::from("## Hotspots (commits × cognitive_max)\n");
    for row in &rows {
        let _ = writeln!(
            out,
            "- {} (score={}, commits={}, cog={})",
            row.path, row.score, row.commits, row.cognitive_max,
        );
    }
    Ok(Some(out))
}

struct HotspotRow {
    path: String,
    score: u64,
    commits: u64,
    cognitive_max: u64,
}

impl HotspotRow {
    fn from_value(v: &serde_json::Value) -> Option<Self> {
        Some(Self {
            path: v.get("path")?.as_str()?.to_owned(),
            score: v.get("score")?.as_u64()?,
            commits: v.get("commits")?.as_u64()?,
            cognitive_max: v.get("cognitive_max")?.as_u64()?,
        })
    }
}

/// Build the crate's coupling graph from `cwd` and return a compact
/// section, or `None` when `cwd` isn't anchored at a Rust crate (no
/// `src/lib.rs` or `src/main.rs`) — that path is "not for us" rather
/// than an error worth surfacing.
fn render_coupling_section(cwd: &Path) -> Result<Option<String>, SessionStartError> {
    let root = match resolve_crate_root(cwd) {
        Ok(p) => p,
        Err(crate::analyze::CrateAnalyzerError::UnsupportedRoot { .. }) => return Ok(None),
        Err(e) => return Err(SessionStartError::Coupling(e)),
    };
    let modules = build_module_tree(&root)
        .map_err(|e| SessionStartError::Coupling(crate::analyze::CrateAnalyzerError::from(e)))?;
    let edges = extract_edges(&modules);
    let module_paths: Vec<lens_domain::ModulePath> = modules.into_iter().map(|m| m.path).collect();
    let report = compute_report(&module_paths, edges);

    if report.modules.is_empty() {
        return Ok(None);
    }

    Ok(Some(format_coupling(&report)))
}

fn format_coupling(report: &CouplingReport) -> String {
    let mut out = format!(
        "## Coupling ({} module(s), {} edge(s), {} cycle(s))\n",
        report.modules.len(),
        report.number_of_couplings,
        report.cycles.len(),
    );

    let top_modules = top_modules_by_ifc(&report.modules);
    if !top_modules.is_empty() {
        let _ = writeln!(out, "\nTop modules by IFC:");
        for m in &top_modules {
            let _ = writeln!(
                out,
                "- {} (fan_in={}, fan_out={}, ifc={})",
                m.path.as_str(),
                m.fan_in,
                m.fan_out,
                m.ifc,
            );
        }
    }

    if !report.cycles.is_empty() {
        let _ = writeln!(out, "\nDependency cycles:");
        for cycle in &report.cycles {
            let _ = writeln!(out, "- {}", format_cycle(cycle));
        }
    }

    let pairs: Vec<&PairCoupling> = report.pairs.iter().take(COUPLING_TOP).collect();
    if !pairs.is_empty() {
        let _ = writeln!(out, "\nTop coupled pairs:");
        for p in &pairs {
            let _ = writeln!(
                out,
                "- {} ↔ {} ({} shared symbol(s))",
                p.a.as_str(),
                p.b.as_str(),
                p.shared_symbols,
            );
        }
    }

    out
}

fn top_modules_by_ifc(modules: &[ModuleMetrics]) -> Vec<&ModuleMetrics> {
    let mut sorted: Vec<&ModuleMetrics> = modules.iter().collect();
    sorted.sort_by(|a, b| {
        b.ifc
            .cmp(&a.ifc)
            .then_with(|| b.fan_in.cmp(&a.fan_in))
            .then_with(|| b.fan_out.cmp(&a.fan_out))
            .then_with(|| a.path.as_str().cmp(b.path.as_str()))
    });
    // Drop modules with ifc=0 from the head: they carry no signal, and
    // surfacing them above the fold would push genuine bottlenecks off
    // the visible window.
    sorted.retain(|m| m.ifc > 0);
    sorted.truncate(COUPLING_TOP);
    sorted
}

fn format_cycle(cycle: &DependencyCycle) -> String {
    let names: Vec<&str> = cycle
        .members
        .iter()
        .map(lens_domain::ModulePath::as_str)
        .collect();
    format!("{} module(s): {}", cycle.members.len(), names.join(" → "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks::codex::{HookContext, SessionStartSource};
    use std::io::Write;
    use std::process::Command;

    fn ctx(cwd: PathBuf) -> HookContext {
        HookContext {
            session_id: "sess".into(),
            transcript_path: None,
            cwd,
            model: "gpt-5".into(),
        }
    }

    fn input(cwd: PathBuf) -> SessionStartInput {
        SessionStartInput {
            context: ctx(cwd),
            source: SessionStartSource::Startup,
        }
    }

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
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

    /// Produce a minimal repo with a `src/` crate and two commits so
    /// hotspot has churn to rank by.
    fn init_repo_with_crate(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        write_file(dir, "src/lib.rs", "pub mod a;\npub mod b;\n");
        write_file(
            dir,
            "src/a.rs",
            "use crate::b::Bar;\npub struct Foo;\nfn _x(_b: Bar) {}\n",
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
        // Touch b.rs again so its churn dominates a.rs.
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

    #[test]
    fn no_op_when_cwd_has_neither_repo_nor_crate() {
        let dir = tempfile::tempdir().unwrap();
        let out = SummaryHook::new()
            .handle(input(dir.path().to_path_buf()))
            .unwrap();
        assert_eq!(out, SessionStartOutput::default());
    }

    #[test]
    fn injects_hotspot_and_coupling_sections() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_crate(dir.path());

        let out = SummaryHook::new()
            .handle(input(dir.path().to_path_buf()))
            .unwrap();
        let extra = out
            .hook_specific_output
            .expect("expected hook_specific_output");
        assert_eq!(extra.hook_event_name, "SessionStart");
        let body = extra
            .additional_context
            .expect("expected additionalContext");

        assert!(body.starts_with("# agent-lens session-start"), "got {body}");
        assert!(
            body.contains("## Hotspots"),
            "should include hotspot: {body}"
        );
        assert!(
            body.contains("src/b.rs"),
            "should mention churn target: {body}"
        );
        assert!(
            body.contains("## Coupling"),
            "should include coupling: {body}"
        );
        assert!(body.contains("crate::a"), "should mention modules: {body}");
        assert!(body.contains("crate::b"), "should mention modules: {body}");
    }

    #[test]
    fn coupling_only_when_no_git_repo() {
        // A bare crate that isn't checked into git: hotspot section
        // is skipped, coupling stays.
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "pub mod a;\n");
        write_file(dir.path(), "src/a.rs", "pub fn solo() {}\n");

        let out = SummaryHook::new()
            .handle(input(dir.path().to_path_buf()))
            .unwrap();
        let body = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("expected additionalContext");
        assert!(body.contains("## Coupling"));
        assert!(!body.contains("## Hotspots"), "should skip hotspot: {body}");
    }

    #[test]
    fn hotspot_only_when_no_crate_root() {
        // A git repo with .rs files but no recognisable crate root
        // (no src/lib.rs or src/main.rs at the top level).
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(
            dir.path(),
            "loose.rs",
            r#"
pub fn nest(n: i32) -> i32 {
    if n > 0 { if n > 1 { if n > 2 { if n > 3 { return n; } } } }
    0
}
"#,
        );
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let out = SummaryHook::new()
            .handle(input(dir.path().to_path_buf()))
            .unwrap();
        let body = out
            .hook_specific_output
            .and_then(|h| h.additional_context)
            .expect("expected additionalContext");
        assert!(body.contains("## Hotspots"));
        assert!(
            !body.contains("## Coupling"),
            "should skip coupling: {body}"
        );
    }
}
