//! Engine-agnostic thin-wrapper detection for PostToolUse hooks.
//!
//! Mirrors [`super::similarity`]: the hook adapters call
//! [`WrapperCore::run`] with the files the agent just touched and get
//! back a fully-formatted report (or `None` if nothing was flagged).

use std::fmt::Write as _;

use lens_domain::WrapperFinding;

use crate::analyze::SourceLang;
use crate::hooks::core::{EditedSource, HookError};

/// Runner for the thin-wrapper detection hook. No knobs today; the type
/// exists so the call shape matches `SimilarityCore`.
#[derive(Debug, Clone, Default)]
pub struct WrapperCore;

impl WrapperCore {
    pub fn new() -> Self {
        Self
    }

    pub fn run(&self, sources: &[EditedSource]) -> Result<Option<String>, HookError> {
        crate::hooks::core::run_source_report(
            sources,
            |total| format!("agent-lens wrapper: {total} thin wrapper(s) detected\n"),
            |src, body| {
                let findings = run_wrappers(src.lang, &src.source)?;
                if !findings.is_empty() {
                    append_section(body, &src.rel_path, &findings);
                }
                Ok(findings.len())
            },
        )
    }
}

fn run_wrappers(lang: SourceLang, source: &str) -> Result<Vec<WrapperFinding>, HookError> {
    crate::analyze::dispatch_lens!(lang, source, find_wrappers).map_err(HookError::Parse)
}

fn append_section(out: &mut String, file_path: &str, findings: &[WrapperFinding]) {
    let _ = writeln!(out, "{file_path}:");
    for finding in findings {
        if finding.adapters.is_empty() {
            let _ = writeln!(
                out,
                "- {} (L{}-{}) -> {}",
                finding.name, finding.start_line, finding.end_line, finding.callee,
            );
        } else {
            let _ = writeln!(
                out,
                "- {} (L{}-{}) -> {} [via {}]",
                finding.name,
                finding.start_line,
                finding.end_line,
                finding.callee,
                finding.adapters.join(""),
            );
        }
    }
}
