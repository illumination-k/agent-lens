//! Engine-agnostic similarity analysis for PostToolUse hooks.
//!
//! The hook adapters call [`SimilarityCore::run`] with the files the agent
//! just touched and get back a fully-formatted report — or `None` when no
//! pair scored above the threshold. Each agent then wraps that string in
//! the engine-specific output envelope.

use std::fmt::Write as _;

use lens_domain::{FunctionDef, LanguageParser, SimilarPair, TSEDOptions, find_similar_functions};
use lens_rust::RustParser;

use crate::analyze::SourceLang;
use crate::hooks::core::{EditedSource, HookError};

/// Default similarity threshold. Picked to match the cutoff used in the
/// existing similarity tests and to avoid flooding the transcript with
/// near-misses.
pub const DEFAULT_THRESHOLD: f64 = 0.85;

/// Configuration plus runner for the similarity hook.
#[derive(Debug, Clone)]
pub(crate) struct SimilarityCore {
    threshold: f64,
    opts: TSEDOptions,
}

impl SimilarityCore {
    pub fn new() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            opts: TSEDOptions::default(),
        }
    }

    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.threshold = threshold;
        self
    }

    /// Analyse every source and produce a single report.
    ///
    /// Returns `Ok(None)` when no file produced any pair, so callers can
    /// treat "no findings" as "no message" without inspecting the report
    /// string.
    pub fn run(&self, sources: &[EditedSource]) -> Result<Option<String>, HookError> {
        let mut body = String::new();
        let mut total = 0usize;

        for src in sources {
            let funcs = extract_functions(src.lang, &src.source)?;
            let pairs = find_similar_functions(&funcs, self.threshold, &self.opts);
            if pairs.is_empty() {
                continue;
            }
            total += pairs.len();
            append_section(&mut body, &src.rel_path, &pairs);
        }

        if total == 0 {
            return Ok(None);
        }

        let header = format!("agent-lens similarity: {total} similar function pair(s) detected\n");
        Ok(Some(format!("{header}{body}")))
    }
}

fn extract_functions(lang: SourceLang, source: &str) -> Result<Vec<FunctionDef>, HookError> {
    match lang {
        SourceLang::Rust => {
            let mut parser = RustParser::new();
            parser
                .extract_functions(source)
                .map_err(|e| HookError::Parse(Box::new(e)))
        }
        SourceLang::TypeScript => {
            let mut parser = lens_ts::TypeScriptParser::new();
            <lens_ts::TypeScriptParser as lens_domain::LanguageParser>::extract_functions(
                &mut parser,
                source,
            )
            .map_err(|e| HookError::Parse(Box::new(e)))
        }
    }
}

fn append_section(out: &mut String, file_path: &str, pairs: &[SimilarPair<'_>]) {
    // writeln! into a String cannot fail; the result is swallowed
    // deliberately rather than unwrapped to satisfy the workspace's
    // `unwrap_used` lint.
    let _ = writeln!(out, "{file_path}:");
    for pair in pairs {
        let _ = writeln!(
            out,
            "- {} (L{}-{}) <-> {} (L{}-{}): {:.0}% similar",
            pair.a.name,
            pair.a.start_line,
            pair.a.end_line,
            pair.b.name,
            pair.b.start_line,
            pair.b.end_line,
            pair.similarity * 100.0,
        );
    }
}
