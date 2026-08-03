//! The `agent-lens baseline` command: snapshotting a profile's
//! analyzers as a compact set of metrics.
//!
//! Creation only, for now. The snapshot is the artifact a later
//! comparison reads to separate "this change made things worse" from
//! "this file was already like that", which is what lets a repository
//! adopt a check without first paying off its existing debt.

use std::path::Path;

use agent_lens::analyze::OutputFormat;
use agent_lens::baseline::{
    Baseline, NO_SUMMARY_REASON, SCHEMA_VERSION, SkippedTool, ToolBaseline, head_commit, summarizer,
};
use tracing::{info, warn};

use super::args::{BaselineCommand, BaselineCreateArgs};
use super::profile::ResolvedProfile;
use super::write_stdout_line;

pub(super) fn run_baseline(cmd: BaselineCommand) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        BaselineCommand::Create(args) => create_baseline(args),
    }
}

/// Run the profile's analyzers and reduce each report to its metrics.
///
/// The analyzers always run as JSON here, whatever the profile's
/// `format` says: that key shapes the *report* a human or agent reads,
/// while a snapshot is built by reading structured fields back out. A
/// profile can therefore serve both `run` (markdown for the agent) and
/// `baseline create` (metrics for the ratchet) without a second config.
fn create_baseline(args: BaselineCreateArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resolved = ResolvedProfile::resolve(args.selector)?;

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

    let baseline = Baseline {
        schema_version: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        profile: resolved.name,
        target: resolved.profile.path.display().to_string(),
        commit: head_commit(&resolved.target),
        tools,
        skipped,
    };
    let document = baseline.render()?;

    match args.out {
        Some(path) => write_out(&path, &document),
        None => write_stdout_line(&document),
    }
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
