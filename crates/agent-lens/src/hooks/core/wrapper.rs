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
        let mut body = String::new();
        let mut total = 0usize;

        for src in sources {
            let findings = run_wrappers(src.lang, &src.source)?;
            if findings.is_empty() {
                continue;
            }
            total += findings.len();
            append_section(&mut body, &src.rel_path, &findings);
        }

        if total == 0 {
            return Ok(None);
        }

        let header = format!("agent-lens wrapper: {total} thin wrapper(s) detected\n");
        Ok(Some(format!("{header}{body}")))
    }
}

fn run_wrappers(lang: SourceLang, source: &str) -> Result<Vec<WrapperFinding>, HookError> {
    match lang {
        SourceLang::Rust => {
            lens_rust::find_wrappers(source).map_err(|e| HookError::Parse(Box::new(e)))
        }
        SourceLang::TypeScript => {
            lens_ts::find_wrappers(source).map_err(|e| HookError::Parse(Box::new(e)))
        }
        // No wrapper detection for Python yet. Returning an empty list
        // keeps the PostToolUse hook silent on `.py` edits instead of
        // erroring out, since the similarity hook still has work to do
        // for the same file.
        SourceLang::Python => Ok(Vec::new()),
    }
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
