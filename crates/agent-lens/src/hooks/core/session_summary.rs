//! Engine-agnostic body of the SessionStart "context summary" hook.
//!
//! Both the Claude Code and Codex SessionStart handlers want to inject
//! the same payload — a hotspot ranking plus a coupling thumbnail of the
//! crate the agent is anchored at. The two protocols only differ in how
//! that body is shaped into a hook response, so the rendering itself
//! lives here and the agent-specific modules are thin adapters that wrap
//! [`render_summary`] in their respective output types.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use lens_domain::{CouplingReport, DependencyCycle, ModuleMetrics, PairCoupling, compute_report};
use lens_rust::{build_module_tree, extract_edges};
use tracing::warn;

use crate::analyze::{HotspotAnalyzer, HotspotError, OutputFormat, resolve_crate_root};

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
pub enum SessionSummaryError {
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

/// Render a hotspot + coupling summary for `cwd`, or return `None` when
/// neither section produces signal (cwd outside a git working tree and
/// not anchored at a Rust crate). The header is included so callers can
/// inject the body verbatim.
pub fn render_summary(cwd: &Path) -> Result<Option<String>, SessionSummaryError> {
    let mut sections: Vec<String> = Vec::new();
    if let Some(s) = render_hotspot_section(cwd)? {
        sections.push(s);
    }
    if let Some(s) = render_coupling_section(cwd)? {
        sections.push(s);
    }

    if sections.is_empty() {
        return Ok(None);
    }

    let mut body = String::from("# agent-lens session-start\n");
    for section in &sections {
        body.push('\n');
        body.push_str(section);
    }
    Ok(Some(body))
}

/// Run the hotspot analyzer against `cwd` and return a compact section
/// for the SessionStart payload, or `None` when there is nothing to
/// inject (cwd outside a git working tree, no Rust files, every file
/// has score 0). Soft failures are logged to stderr and treated as
/// "no section."
fn render_hotspot_section(cwd: &Path) -> Result<Option<String>, SessionSummaryError> {
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
fn render_coupling_section(cwd: &Path) -> Result<Option<String>, SessionSummaryError> {
    let root = match resolve_crate_root(cwd) {
        Ok(p) => p,
        Err(crate::analyze::CrateAnalyzerError::UnsupportedRoot { .. }) => return Ok(None),
        Err(e) => return Err(SessionSummaryError::Coupling(e)),
    };
    let modules = build_module_tree(&root)
        .map_err(|e| SessionSummaryError::Coupling(crate::analyze::CrateAnalyzerError::from(e)))?;
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
    use lens_domain::ModulePath;

    fn module(path: &str, fan_in: usize, fan_out: usize, ifc: u64) -> ModuleMetrics {
        ModuleMetrics {
            path: ModulePath::new(path),
            fan_in,
            fan_out,
            ifc,
            instability: None,
        }
    }

    fn report(modules: Vec<ModuleMetrics>, cycles: Vec<DependencyCycle>) -> CouplingReport {
        CouplingReport {
            modules,
            edges: Vec::new(),
            pairs: Vec::new(),
            cycles,
            number_of_couplings: 0,
        }
    }

    #[test]
    fn top_modules_by_ifc_orders_by_ifc_descending() {
        let mods = vec![
            module("crate::a", 1, 1, 1),
            module("crate::b", 2, 2, 16),
            module("crate::c", 1, 3, 9),
        ];
        let top = top_modules_by_ifc(&mods);
        let names: Vec<&str> = top.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(names, vec!["crate::b", "crate::c", "crate::a"]);
    }

    #[test]
    fn top_modules_by_ifc_drops_zero_ifc_entries() {
        // Mix of zero-IFC and non-zero modules. The zero entries must be
        // filtered out — surfacing them would crowd out genuine
        // bottlenecks. Mutating `> 0` to `== 0`, `< 0`, or `>= 0` flips
        // which set survives, so this test pins the boundary.
        let mods = vec![
            module("crate::leaf", 0, 1, 0),
            module("crate::root", 1, 0, 0),
            module("crate::hub", 2, 2, 16),
        ];
        let top = top_modules_by_ifc(&mods);
        let names: Vec<&str> = top.iter().map(|m| m.path.as_str()).collect();
        assert_eq!(names, vec!["crate::hub"]);
    }

    #[test]
    fn top_modules_by_ifc_returns_empty_when_all_zero() {
        let mods = vec![
            module("crate::leaf", 0, 1, 0),
            module("crate::root", 1, 0, 0),
        ];
        assert!(top_modules_by_ifc(&mods).is_empty());
    }

    #[test]
    fn format_cycle_lists_members_with_arrow() {
        let cycle = DependencyCycle {
            members: vec![
                ModulePath::new("crate::a"),
                ModulePath::new("crate::b"),
                ModulePath::new("crate::c"),
            ],
        };
        assert_eq!(
            format_cycle(&cycle),
            "3 module(s): crate::a → crate::b → crate::c",
        );
    }

    #[test]
    fn format_coupling_includes_top_modules_section_when_non_empty() {
        let r = report(vec![module("crate::hub", 2, 2, 16)], Vec::new());
        let out = format_coupling(&r);
        assert!(out.contains("Top modules by IFC:"), "got {out}");
        assert!(
            out.contains("crate::hub (fan_in=2, fan_out=2, ifc=16)"),
            "got {out}",
        );
    }

    #[test]
    fn format_coupling_omits_top_modules_section_when_only_zero_ifc() {
        let r = report(vec![module("crate::leaf", 0, 1, 0)], Vec::new());
        let out = format_coupling(&r);
        assert!(
            !out.contains("Top modules by IFC:"),
            "should skip empty section: {out}",
        );
    }

    #[test]
    fn format_coupling_includes_cycles_section_when_non_empty() {
        let cycle = DependencyCycle {
            members: vec![ModulePath::new("crate::a"), ModulePath::new("crate::b")],
        };
        let r = report(Vec::new(), vec![cycle]);
        let out = format_coupling(&r);
        assert!(out.contains("Dependency cycles:"), "got {out}");
        assert!(out.contains("crate::a → crate::b"), "got {out}");
    }

    #[test]
    fn format_coupling_omits_cycles_section_when_empty() {
        let r = report(Vec::new(), Vec::new());
        let out = format_coupling(&r);
        assert!(
            !out.contains("Dependency cycles:"),
            "should skip empty section: {out}",
        );
    }
}
