//! Glue between the CLI and the baseline subsystem.
//!
//! Each supported analyzer gets a thin pair of entry points:
//! `save_<analyzer>` (run analyzer → snapshot → write to disk) and
//! `check_<analyzer>` (run analyzer → snapshot → diff against the
//! baseline → exit-code-bearing report). The CLI is responsible for
//! parsing args and constructing the analyzer instance; the runner is
//! responsible for the snapshot dance and reporting.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::analyze::{CohesionAnalyzer, ComplexityAnalyzer, CouplingAnalyzer, HotspotAnalyzer};

use super::adapters::{self, build_ratchet_config};
use super::compare::{RatchetConfig, Regression, RegressionReport, compare, compare_cycles};
use super::storage::{default_baseline_path, load_snapshot, save_snapshot};
use super::{Baselinable, BaselineError, FailOn, Item, NewItemPolicy, Snapshot};

/// Process exit code mapping for `baseline check`. The numeric values
/// are deliberate: `0` clean, `1` operational error (matches our
/// general fail mode), `2` regressions found (distinct so CI can tell
/// "the tool blew up" from "the tool found something").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineExitCode {
    Clean,
    Regressions,
}

impl BaselineExitCode {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Clean => 0,
            Self::Regressions => 2,
        }
    }
}

/// Outcome of `baseline check`. The CLI prints `report` as JSON and
/// converts `exit_code` into a process exit status.
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub report: RegressionReport,
    pub exit_code: BaselineExitCode,
}

/// Top-level dispatch entry called from the CLI. Subcommand variants
/// live in the binary crate; this enum is just the shape the runner
/// needs to act on.
#[derive(Debug)]
pub enum BaselineCommand {
    Save(BaselineSave),
    Check(BaselineCheck),
}

#[derive(Debug)]
pub struct BaselineSave {
    pub kind: AnalyzerKind,
    pub target: PathBuf,
    pub out: Option<PathBuf>,
    pub args_record: serde_json::Value,
    pub analyzer_options: AnalyzerOptions,
}

#[derive(Debug)]
pub struct BaselineCheck {
    pub kind: AnalyzerKind,
    pub target: PathBuf,
    pub baseline: Option<PathBuf>,
    pub policy: NewItemPolicy,
    pub fail_on: FailOn,
    pub metrics_override: Vec<String>,
    pub args_record: serde_json::Value,
    pub analyzer_options: AnalyzerOptions,
}

/// Which analyzer to drive. Constrained to the v1 set; extending
/// `agent-lens baseline` to a new analyzer is a matter of adding a
/// variant here, an adapter under [`super::adapters`], and a CLI arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalyzerKind {
    Complexity,
    Cohesion,
    Coupling,
    Hotspot,
}

impl AnalyzerKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Complexity => adapters::complexity::ComplexityBaseline::ANALYZER_NAME,
            Self::Cohesion => adapters::cohesion::CohesionBaseline::ANALYZER_NAME,
            Self::Coupling => adapters::coupling::CouplingBaseline::ANALYZER_NAME,
            Self::Hotspot => adapters::hotspot::HotspotBaseline::ANALYZER_NAME,
        }
    }
}

/// Per-analyzer knob bag the CLI threads from clap args. Kept as one
/// flat struct because every supported analyzer ignores fields it
/// doesn't use, and a clap-generic dispatch in the binary is much
/// easier to write that way.
#[derive(Debug, Default, Clone)]
pub struct AnalyzerOptions {
    pub diff_only: bool,
    pub only_tests: bool,
    pub exclude_tests: bool,
    pub exclude: Vec<String>,
    pub since: Option<String>,
}

/// Result of `baseline save`. The CLI writes the path to a status
/// line so the user knows where the snapshot landed.
#[derive(Debug, Clone, Serialize)]
pub struct SaveOutcome {
    pub analyzer: String,
    pub path: PathBuf,
    pub item_count: usize,
}

/// Dispatch a parsed [`BaselineCommand`] into the right analyzer's
/// `save` / `check` path. Single entry point so the CLI keeps a thin
/// `match` arm and the analyzer-specific glue stays here.
pub fn run_baseline(cmd: BaselineCommand, cwd: &Path) -> Result<RunOutcome, BaselineError> {
    match cmd {
        BaselineCommand::Save(s) => run_save(s, cwd).map(RunOutcome::Save),
        BaselineCommand::Check(c) => run_check(c, cwd).map(RunOutcome::Check),
    }
}

#[derive(Debug)]
pub enum RunOutcome {
    Save(SaveOutcome),
    Check(CheckOutcome),
}

fn run_save(cmd: BaselineSave, cwd: &Path) -> Result<SaveOutcome, BaselineError> {
    let snap = build_snapshot(
        cmd.kind,
        &cmd.target,
        cmd.args_record,
        &cmd.analyzer_options,
    )?;
    let path = cmd
        .out
        .unwrap_or_else(|| default_baseline_path(cmd.kind.name(), cwd));
    save_snapshot(&snap, &path)?;
    Ok(SaveOutcome {
        analyzer: snap.analyzer,
        path,
        item_count: snap.items.len(),
    })
}

fn run_check(cmd: BaselineCheck, cwd: &Path) -> Result<CheckOutcome, BaselineError> {
    let baseline_path = cmd
        .baseline
        .clone()
        .unwrap_or_else(|| default_baseline_path(cmd.kind.name(), cwd));
    let baseline = load_snapshot(&baseline_path)?;
    if baseline.analyzer != cmd.kind.name() {
        return Err(BaselineError::AnalyzerMismatch {
            baseline: baseline.analyzer,
            requested: cmd.kind.name().to_owned(),
        });
    }
    let current = build_snapshot(
        cmd.kind,
        &cmd.target,
        cmd.args_record,
        &cmd.analyzer_options,
    )?;
    let cfg = ratchet_config_for(cmd.kind, cmd.metrics_override, cmd.policy)?;
    let baseline_path_str = baseline_path.display().to_string();
    let mut report = compare(&baseline, &current, &baseline_path_str, &cfg);
    let cycle_regs = if cmd.kind == AnalyzerKind::Coupling {
        compare_cycles(&baseline, &current)
    } else {
        Vec::new()
    };
    merge_extra_regressions(&mut report, cycle_regs);
    let exit_code = if report.has_failures(cmd.fail_on) {
        BaselineExitCode::Regressions
    } else {
        BaselineExitCode::Clean
    };
    Ok(CheckOutcome { report, exit_code })
}

fn merge_extra_regressions(report: &mut RegressionReport, extras: Vec<Regression>) {
    if extras.is_empty() {
        return;
    }
    report.regressions.extend(extras);
    report.summary.regressed = report.regressions.len();
}

fn ratchet_config_for(
    kind: AnalyzerKind,
    metrics: Vec<String>,
    policy: NewItemPolicy,
) -> Result<RatchetConfig, BaselineError> {
    match kind {
        AnalyzerKind::Complexity => {
            build_ratchet_config::<adapters::complexity::ComplexityBaseline>(metrics, policy)
        }
        AnalyzerKind::Cohesion => {
            build_ratchet_config::<adapters::cohesion::CohesionBaseline>(metrics, policy)
        }
        AnalyzerKind::Coupling => {
            build_ratchet_config::<adapters::coupling::CouplingBaseline>(metrics, policy)
        }
        AnalyzerKind::Hotspot => {
            build_ratchet_config::<adapters::hotspot::HotspotBaseline>(metrics, policy)
        }
    }
}

fn build_snapshot(
    kind: AnalyzerKind,
    target: &Path,
    args_record: serde_json::Value,
    opts: &AnalyzerOptions,
) -> Result<Snapshot, BaselineError> {
    match kind {
        AnalyzerKind::Complexity => build_complexity_snapshot(target, args_record, opts),
        AnalyzerKind::Cohesion => build_cohesion_snapshot(target, args_record, opts),
        AnalyzerKind::Coupling => build_coupling_snapshot(target, args_record, opts),
        AnalyzerKind::Hotspot => build_hotspot_snapshot(target, args_record, opts),
    }
}

fn build_complexity_snapshot(
    target: &Path,
    args_record: serde_json::Value,
    opts: &AnalyzerOptions,
) -> Result<Snapshot, BaselineError> {
    let analyzer = ComplexityAnalyzer::new()
        .with_diff_only(opts.diff_only)
        .with_only_tests(opts.only_tests)
        .with_exclude_tests(opts.exclude_tests)
        .with_exclude_patterns(opts.exclude.clone());
    let reports = analyzer
        .collect(target)
        .map_err(|e| BaselineError::Analyzer(e.to_string()))?;
    let items = adapters::complexity::to_items(&reports);
    Ok(Snapshot::new(
        adapters::complexity::ComplexityBaseline::ANALYZER_NAME,
        args_record,
        items,
        serde_json::Map::new(),
    ))
}

fn build_cohesion_snapshot(
    target: &Path,
    args_record: serde_json::Value,
    opts: &AnalyzerOptions,
) -> Result<Snapshot, BaselineError> {
    let analyzer = CohesionAnalyzer::new()
        .with_diff_only(opts.diff_only)
        .with_only_tests(opts.only_tests)
        .with_exclude_tests(opts.exclude_tests)
        .with_exclude_patterns(opts.exclude.clone());
    let reports = analyzer
        .collect(target)
        .map_err(|e| BaselineError::Analyzer(e.to_string()))?;
    let items = adapters::cohesion::to_items(&reports);
    Ok(Snapshot::new(
        adapters::cohesion::CohesionBaseline::ANALYZER_NAME,
        args_record,
        items,
        serde_json::Map::new(),
    ))
}

fn build_coupling_snapshot(
    target: &Path,
    args_record: serde_json::Value,
    opts: &AnalyzerOptions,
) -> Result<Snapshot, BaselineError> {
    let analyzer = CouplingAnalyzer::new()
        .with_only_tests(opts.only_tests)
        .with_exclude_tests(opts.exclude_tests)
        .with_exclude_patterns(opts.exclude.clone());
    let collection = analyzer
        .collect(target)
        .map_err(|e| BaselineError::Analyzer(e.to_string()))?;
    let items = adapters::coupling::to_items(&collection);
    let extras = adapters::coupling::to_extras(&collection);
    Ok(Snapshot::new(
        adapters::coupling::CouplingBaseline::ANALYZER_NAME,
        args_record,
        items,
        extras,
    ))
}

fn build_hotspot_snapshot(
    target: &Path,
    args_record: serde_json::Value,
    opts: &AnalyzerOptions,
) -> Result<Snapshot, BaselineError> {
    let analyzer = HotspotAnalyzer::new()
        .with_only_tests(opts.only_tests)
        .with_exclude_tests(opts.exclude_tests)
        .with_exclude_patterns(opts.exclude.clone())
        .with_since_opt(opts.since.clone());
    let collection = analyzer
        .collect(target)
        .map_err(|e| BaselineError::Analyzer(e.to_string()))?;
    let items = adapters::hotspot::to_items(&collection);
    Ok(Snapshot::new(
        adapters::hotspot::HotspotBaseline::ANALYZER_NAME,
        args_record,
        items,
        serde_json::Map::new(),
    ))
}

/// Convenience: render an [`Item`] as a stable display string for
/// non-JSON status lines. Currently only used by tests; kept here so
/// any future markdown formatter doesn't reach into adapter internals.
pub fn item_display(item: &Item) -> String {
    item.id
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;

    fn options() -> AnalyzerOptions {
        AnalyzerOptions::default()
    }

    #[test]
    fn run_save_writes_snapshot_to_default_path_under_cwd() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "lib.rs",
            "fn solo() { let x = 1; let _y = x; }\n",
        );
        let outcome = run_baseline(
            BaselineCommand::Save(BaselineSave {
                kind: AnalyzerKind::Complexity,
                target: dir.path().join("lib.rs"),
                out: None,
                args_record: serde_json::Value::Null,
                analyzer_options: options(),
            }),
            dir.path(),
        )
        .unwrap();
        let RunOutcome::Save(save) = outcome else {
            panic!("expected save outcome");
        };
        assert!(save.path.ends_with(".agent-lens/baseline/complexity.json"));
        assert!(save.path.exists());
        assert_eq!(save.item_count, 1);
        // Round-trip: reload and confirm it parses.
        let snap = load_snapshot(&save.path).unwrap();
        assert_eq!(snap.analyzer, "complexity");
    }

    #[test]
    fn run_check_clean_against_unchanged_source_returns_zero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            "fn solo() { let x = 1; let _y = x; }\n",
        );
        let baseline_path = dir.path().join("snap.json");
        run_baseline(
            BaselineCommand::Save(BaselineSave {
                kind: AnalyzerKind::Complexity,
                target: file.clone(),
                out: Some(baseline_path.clone()),
                args_record: serde_json::Value::Null,
                analyzer_options: options(),
            }),
            dir.path(),
        )
        .unwrap();
        let outcome = run_baseline(
            BaselineCommand::Check(BaselineCheck {
                kind: AnalyzerKind::Complexity,
                target: file,
                baseline: Some(baseline_path),
                policy: NewItemPolicy::Strict,
                fail_on: FailOn::Any,
                metrics_override: Vec::new(),
                args_record: serde_json::Value::Null,
                analyzer_options: options(),
            }),
            dir.path(),
        )
        .unwrap();
        let RunOutcome::Check(check) = outcome else {
            panic!("expected check outcome");
        };
        assert_eq!(check.exit_code, BaselineExitCode::Clean);
        assert!(check.report.regressions.is_empty());
    }

    #[test]
    fn run_check_flags_worsened_function_with_exit_two() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            "fn f(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\n",
        );
        let baseline_path = dir.path().join("snap.json");
        run_baseline(
            BaselineCommand::Save(BaselineSave {
                kind: AnalyzerKind::Complexity,
                target: file.clone(),
                out: Some(baseline_path.clone()),
                args_record: serde_json::Value::Null,
                analyzer_options: options(),
            }),
            dir.path(),
        )
        .unwrap();

        // Rewrite `f` with a deeper nest to bump cognitive.
        std::fs::write(
            &file,
            "fn f(n: i32) -> i32 { if n > 0 { if n > 5 { if n > 10 { 1 } else { 2 } } else { 3 } } else { 0 } }\n",
        )
        .unwrap();

        let outcome = run_baseline(
            BaselineCommand::Check(BaselineCheck {
                kind: AnalyzerKind::Complexity,
                target: file,
                baseline: Some(baseline_path),
                policy: NewItemPolicy::Strict,
                fail_on: FailOn::Any,
                metrics_override: Vec::new(),
                args_record: serde_json::Value::Null,
                analyzer_options: options(),
            }),
            dir.path(),
        )
        .unwrap();
        let RunOutcome::Check(check) = outcome else {
            panic!("expected check outcome");
        };
        assert_eq!(check.exit_code, BaselineExitCode::Regressions);
        assert!(!check.report.regressions.is_empty(), "regressions list");
        let reg = &check.report.regressions[0];
        assert_eq!(reg.id["name"], "f");
        assert!(
            reg.deltas
                .iter()
                .any(|d| d.metric == "cognitive" && d.delta > 0.0),
            "expected positive cognitive delta, got {:?}",
            reg.deltas
        );
    }

    #[test]
    fn run_check_rejects_analyzer_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", "fn solo() { }\n");
        let baseline_path = dir.path().join("snap.json");
        // Save a complexity baseline.
        run_baseline(
            BaselineCommand::Save(BaselineSave {
                kind: AnalyzerKind::Complexity,
                target: file.clone(),
                out: Some(baseline_path.clone()),
                args_record: serde_json::Value::Null,
                analyzer_options: options(),
            }),
            dir.path(),
        )
        .unwrap();
        // Check it as cohesion.
        let err = run_baseline(
            BaselineCommand::Check(BaselineCheck {
                kind: AnalyzerKind::Cohesion,
                target: file,
                baseline: Some(baseline_path),
                policy: NewItemPolicy::Strict,
                fail_on: FailOn::Any,
                metrics_override: Vec::new(),
                args_record: serde_json::Value::Null,
                analyzer_options: options(),
            }),
            dir.path(),
        )
        .unwrap_err();
        assert!(matches!(err, BaselineError::AnalyzerMismatch { .. }));
    }
}
