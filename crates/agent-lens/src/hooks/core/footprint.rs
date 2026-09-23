//! Engine-agnostic footprint report for PostToolUse hooks.
//!
//! Runs `analyze footprint` over the working-tree diff of the session's
//! directory after every edit, and reports only what the edit that just
//! happened is implicated in: flagged rows (outside the change's impact
//! closure, complexity increases, new wrappers, added functions nothing
//! calls) in the files the tool just wrote. The whole-diff size line
//! rides along as context. Rows in other files were reported when those
//! files were edited, so repeating them on every edit would only bury
//! the new one.

use std::path::{Path, PathBuf};

use crate::analyze::{AnalyzeRoots, FootprintAnalyzer, FootprintError};
use crate::hooks::core::{EditedSource, HookError};

/// Rows per list. The hook injects a nudge; `analyze footprint` is the
/// full picture.
const TOP: usize = 5;

/// Runner for the footprint hook.
#[derive(Debug, Clone, Default)]
pub struct FootprintCore;

impl FootprintCore {
    /// Measure the diff under `cwd` and report the flags that land in
    /// `sources`, or `None` when there are none — including when `cwd`
    /// is not in a git working tree, where there is no diff to measure.
    pub fn run(&self, cwd: &Path, sources: &[EditedSource]) -> Result<Option<String>, HookError> {
        if sources.is_empty() {
            return Ok(None);
        }
        let roots = AnalyzeRoots::from(cwd);
        let mut report = match FootprintAnalyzer::new().measure(&roots) {
            Ok(report) => report,
            Err(FootprintError::NotInGitRepo { .. }) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let edited: Vec<PathBuf> = sources
            .iter()
            .filter_map(|src| canonical(&cwd.join(&src.rel_path)))
            .collect();
        report.retain_flags_in(|file| {
            canonical(&cwd.join(file)).is_some_and(|abs| edited.contains(&abs))
        });
        if !report.has_flags() {
            return Ok(None);
        }
        // The analyzer's title names the absolute root; the session
        // already knows where it is.
        let body = report.render_markdown(TOP);
        let body = body
            .split_once('\n')
            .map_or(body.as_str(), |(_, rest)| rest);
        Ok(Some(format!(
            "# agent-lens footprint: flags in the file(s) just edited{body}"
        )))
    }
}

fn canonical(path: &Path) -> Option<PathBuf> {
    path.canonicalize().ok()
}
