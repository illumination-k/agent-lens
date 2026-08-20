//! The `agent-lens baseline` command: snapshotting a profile's
//! analyzers as a compact set of metrics, and comparing a later run
//! against that snapshot.
//!
//! The snapshot is the artifact the comparison reads to separate "this
//! change made things worse" from "this file was already like that",
//! which is what lets a repository adopt a check without first paying off
//! its existing debt. `compare` is that check; `--update` makes it a
//! ratchet.

use std::path::Path;
use std::process::ExitCode;

use agent_lens::analyze::OutputFormat;
use agent_lens::baseline::compare::{self, REGRESSION_EXIT_CODE};
use agent_lens::baseline::{
    Baseline, NO_SUMMARY_REASON, SCHEMA_VERSION, SkippedTool, ToolBaseline, head_commit, summarizer,
};
use tracing::{info, warn};

use super::args::{BaselineCommand, BaselineCompareArgs, BaselineCreateArgs};
use super::profile::ResolvedProfile;
use super::write_stdout_line;

pub(super) fn run_baseline(cmd: BaselineCommand) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match cmd {
        BaselineCommand::Create(args) => create_baseline(args).map(|()| ExitCode::SUCCESS),
        BaselineCommand::Compare(args) => compare_baseline(args),
    }
}

/// Run the profile's analyzers and reduce each report to its metrics.
///
/// The analyzers always run as JSON here, whatever the profile's
/// `format` says: that key shapes the *report* a human or agent reads,
/// while a snapshot is built by reading structured fields back out. A
/// profile can therefore serve `run` (markdown for the agent), `baseline
/// create`, and `baseline compare` without a second config.
fn snapshot(resolved: &ResolvedProfile) -> Result<Baseline, Box<dyn std::error::Error>> {
    // One analysis index per snapshot: the profile's analyzers walk the
    // same tree, so parses and assembled graphs are shared instead of
    // redone per tool.
    let _index = agent_lens::analyze::AnalysisIndexScope::activate();
    let mut tools = Vec::new();
    let mut skipped = Vec::new();
    for tool in resolved.tools() {
        // Asked before the analyzer runs: an uncovered tool's report
        // would only be discarded, and running the analyzers is the
        // expensive part of a snapshot.
        let Some(summarize) = summarizer(tool) else {
            warn!(
                profile = %resolved.name,
                tool = tool.as_str(),
                "{NO_SUMMARY_REASON}; not part of this snapshot",
            );
            skipped.push(SkippedTool {
                tool: tool.as_str().to_owned(),
                reason: NO_SUMMARY_REASON.to_owned(),
            });
            continue;
        };
        let report: serde_json::Value =
            serde_json::from_str(&resolved.run_tool(tool, OutputFormat::Json)?)?;
        tools.push(ToolBaseline {
            tool: tool.as_str().to_owned(),
            metrics: summarize(&report),
        });
    }

    Ok(Baseline {
        schema_version: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        profile: resolved.name.clone(),
        target: resolved.profile.path.display(),
        commit: head_commit(resolved.git_anchor()),
        tools,
        skipped,
    })
}

fn create_baseline(args: BaselineCreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resolved = ResolvedProfile::resolve(args.selector)?;
    let document = snapshot(&resolved)?.render()?;

    match args.out {
        Some(path) => write_out(&path, &document),
        None => write_stdout_line(&document),
    }
}

/// Compare a fresh snapshot against the stored one, and answer with an
/// exit status.
///
/// The report goes to stdout before the verdict is acted on: a failing
/// check whose output the caller never sees is a check nobody can fix.
/// [`REGRESSION_EXIT_CODE`] rather than a plain error keeps "the code got
/// worse" distinguishable from "the tool could not run" — the same reason
/// a test runner does not exit 1 for both a failing test and a missing
/// binary.
fn compare_baseline(args: BaselineCompareArgs) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let stored = read_snapshot(&args.snapshot)?;
    let resolved = ResolvedProfile::resolve(args.selector)?;
    let current = snapshot(&resolved)?;
    let comparison = compare::compare(&stored, &current)?;

    // Neither of these invalidates the comparison, and neither is
    // something the report itself can shout about, so they are warnings:
    // both change what the numbers mean without changing any of them.
    if stored.tool_version != current.tool_version {
        warn!(
            baseline = %stored.tool_version,
            current = %current.tool_version,
            "snapshot was produced by a different agent-lens version; metric definitions may differ",
        );
    }
    if stored.target != current.target {
        warn!(
            baseline = %stored.target,
            current = %current.target,
            "snapshot was taken against a different target path",
        );
    }

    write_stdout_line(&comparison.render(args.format)?)?;

    if args.update {
        // Tightened even when something regressed: the improvements this
        // run did make are still improvements, and the regressed metrics
        // keep their stored value, so the file cannot come out looser
        // than it went in.
        write_out(
            &args.snapshot,
            &compare::ratchet(&stored, &current).render()?,
        )?;
    }

    if comparison.regressed() {
        warn!(
            profile = %comparison.profile,
            regressed = comparison.summary.regressed,
            "metrics regressed against the baseline",
        );
        return Ok(ExitCode::from(REGRESSION_EXIT_CODE));
    }
    info!(profile = %comparison.profile, "no metric regressed against the baseline");
    Ok(ExitCode::SUCCESS)
}

/// Read a stored snapshot, naming the file in both failure modes — a
/// missing path and a document this build cannot parse are different
/// mistakes, and neither error says which file it was about on its own.
fn read_snapshot(path: &Path) -> Result<Baseline, Box<dyn std::error::Error>> {
    let document = std::fs::read_to_string(path)
        .map_err(|source| format!("failed to read baseline {}: {source}", path.display()))?;
    serde_json::from_str(&document)
        .map_err(|source| format!("failed to parse baseline {}: {source}", path.display()).into())
}

/// Write the snapshot to `path`, creating the directory it lives in.
///
/// The parent is created because the natural home for a snapshot is a
/// path like `target/agent-lens/baseline.json` that no earlier step made
/// — failing on a missing directory would only teach callers to prefix
/// every invocation with `mkdir -p`.
fn write_out(path: &Path, document: &str) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, document)?;
    info!(path = %path.display(), "wrote baseline");
    Ok(())
}
