//! Engine-agnostic similarity analysis for PostToolUse hooks.
//!
//! The hook adapters call [`SimilarityCore::run`] with the files the agent
//! just touched and get back a fully-formatted report — or `None` when no
//! pair scored above the threshold. Each agent then wraps that string in
//! the engine-specific output envelope.

use std::fmt::Write as _;

use lens_domain::{
    FunctionDef, LanguageParser, SimilarCluster, TSEDOptions, cluster_similar_pairs,
    find_similar_pair_indices,
};
use lens_rust::RustParser;

use crate::analyze::SourceLang;
use crate::hooks::core::{EditedSource, HookError};

/// Default similarity threshold. Picked to match the cutoff used in the
/// existing similarity tests and to avoid flooding the transcript with
/// near-misses.
pub const DEFAULT_THRESHOLD: f64 = 0.85;

/// Configuration plus runner for the similarity hook.
#[derive(Debug, Clone)]
pub struct SimilarityCore {
    threshold: f64,
    opts: TSEDOptions,
}

impl Default for SimilarityCore {
    fn default() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            opts: TSEDOptions::default(),
        }
    }
}

impl SimilarityCore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.threshold = threshold;
        self
    }

    /// Analyse every source and produce a single report.
    ///
    /// Returns `Ok(None)` when no file produced any cluster, so callers can
    /// treat "no findings" as "no message" without inspecting the report
    /// string.
    pub fn run(&self, sources: &[EditedSource]) -> Result<Option<String>, HookError> {
        let mut body = String::new();
        let mut total_clusters = 0usize;

        for src in sources {
            let funcs = extract_functions(src.lang, &src.source)?;
            let pair_indices = find_similar_pair_indices(&funcs, self.threshold, &self.opts);
            let clusters = cluster_similar_pairs(&pair_indices, self.threshold);
            if clusters.is_empty() {
                continue;
            }
            total_clusters += clusters.len();
            append_section(&mut body, &src.rel_path, &funcs, &clusters);
        }

        if total_clusters == 0 {
            return Ok(None);
        }

        let header = format!(
            "agent-lens similarity: {total_clusters} similar function cluster(s) detected\n",
        );
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
        SourceLang::TypeScript(dialect) => {
            let mut parser = lens_ts::TypeScriptParser::with_dialect(dialect);
            <lens_ts::TypeScriptParser as lens_domain::LanguageParser>::extract_functions(
                &mut parser,
                source,
            )
            .map_err(|e| HookError::Parse(Box::new(e)))
        }
        SourceLang::Python => {
            let mut parser = lens_py::PythonParser::new();
            parser
                .extract_functions(source)
                .map_err(|e| HookError::Parse(Box::new(e)))
        }
    }
}

fn append_section(
    out: &mut String,
    file_path: &str,
    funcs: &[FunctionDef],
    clusters: &[SimilarCluster],
) {
    // writeln! into a String cannot fail; the result is swallowed
    // deliberately rather than unwrapped to satisfy the workspace's
    // `unwrap_used` lint.
    let _ = writeln!(out, "{file_path}:");
    for cluster in clusters {
        let _ = writeln!(
            out,
            "- {} functions, similarity {:.0}–{:.0}%:",
            cluster.members.len(),
            cluster.min_similarity * 100.0,
            cluster.max_similarity * 100.0,
        );
        for idx in &cluster.members {
            let Some(f) = funcs.get(*idx) else { continue };
            let _ = writeln!(out, "  - {} (L{}-{})", f.name, f.start_line, f.end_line);
        }
    }
}
