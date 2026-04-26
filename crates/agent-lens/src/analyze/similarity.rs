//! `analyze similarity` — surface near-duplicate function pairs in a Rust
//! source file.
//!
//! Today the analyzer is single-file, single-language: pass it a `.rs` path
//! and it reports every pair of functions whose TSED similarity score is at
//! or above the configured threshold. Output is JSON by default; the
//! markdown mode emits a compact summary tuned for LLM context windows
//! rather than for humans, in line with the project's "agent-friendly lint"
//! ethos.

use std::fmt::Write as _;
use std::path::Path;

use lens_domain::{FunctionDef, LanguageParser, SimilarPair, TSEDOptions, find_similar_functions};
use lens_rust::RustParser;
use lens_ts::TypeScriptParser;
use serde::Serialize;

use super::{AnalyzerError, OutputFormat, SourceLang, read_source};

/// Default similarity threshold. Picked to match the cutoff used by the
/// PostToolUse `similarity` hook so the on-demand analyzer reports the
/// same pairs that show up in the hook's transcript message.
pub const DEFAULT_THRESHOLD: f64 = 0.85;

/// Analyzer entry point. Holds the threshold and TSED options so per-run
/// configuration can be threaded through `analyze` without changing the
/// CLI surface.
#[derive(Debug, Clone)]
pub struct SimilarityAnalyzer {
    threshold: f64,
    opts: TSEDOptions,
}

impl SimilarityAnalyzer {
    pub fn new() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            opts: TSEDOptions::default(),
        }
    }

    /// Override the similarity threshold. Callers passing a non-default
    /// value via `--threshold` go through here.
    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.threshold = threshold;
        self
    }

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let (lang, source) = read_source(path)?;
        let functions = extract_functions(lang, &source)?;
        let pairs = find_similar_functions(&functions, self.threshold, &self.opts);
        let report = Report::new(path, self.threshold, functions.len(), &pairs);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).map_err(AnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&report)),
        }
    }
}

impl Default for SimilarityAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

fn extract_functions(lang: SourceLang, source: &str) -> Result<Vec<FunctionDef>, AnalyzerError> {
    match lang {
        SourceLang::Rust => {
            let mut parser = RustParser::new();
            parser
                .extract_functions(source)
                .map_err(|e| AnalyzerError::Parse(Box::new(e)))
        }
        SourceLang::TypeScript => {
            let mut parser = TypeScriptParser::new();
            parser
                .extract_functions(source)
                .map_err(|e| AnalyzerError::Parse(Box::new(e)))
        }
    }
}

#[derive(Debug, Serialize)]
struct Report<'a> {
    file: String,
    function_count: usize,
    threshold: f64,
    pair_count: usize,
    pairs: Vec<PairView<'a>>,
}

impl<'a> Report<'a> {
    fn new(
        path: &Path,
        threshold: f64,
        function_count: usize,
        pairs: &'a [SimilarPair<'a>],
    ) -> Self {
        Self {
            file: path.display().to_string(),
            function_count,
            threshold,
            pair_count: pairs.len(),
            pairs: pairs.iter().map(PairView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct PairView<'a> {
    a: FunctionRef<'a>,
    b: FunctionRef<'a>,
    similarity: f64,
}

impl<'a> From<&SimilarPair<'a>> for PairView<'a> {
    fn from(pair: &SimilarPair<'a>) -> Self {
        Self {
            a: FunctionRef::from(pair.a),
            b: FunctionRef::from(pair.b),
            similarity: pair.similarity,
        }
    }
}

#[derive(Debug, Serialize)]
struct FunctionRef<'a> {
    name: &'a str,
    start_line: usize,
    end_line: usize,
}

impl<'a> From<&'a FunctionDef> for FunctionRef<'a> {
    fn from(f: &'a FunctionDef) -> Self {
        Self {
            name: f.name.as_str(),
            start_line: f.start_line,
            end_line: f.end_line,
        }
    }
}

fn format_markdown(report: &Report<'_>) -> String {
    let mut out = format!(
        "# Similarity report: {} ({} function(s), threshold {:.2})\n",
        report.file, report.function_count, report.threshold,
    );
    if report.pairs.is_empty() {
        out.push_str("\n_No similar function pairs at or above threshold._\n");
        return out;
    }
    let _ = writeln!(out, "\n## {} similar pair(s)", report.pair_count);
    for pair in &report.pairs {
        // writeln! into a String cannot fail; the result is swallowed
        // deliberately rather than unwrapped to satisfy the workspace's
        // `unwrap_used` lint.
        let _ = writeln!(
            out,
            "- `{}` (L{}-{}) <-> `{}` (L{}-{}): {:.0}% similar",
            pair.a.name,
            pair.a.start_line,
            pair.a.end_line,
            pair.b.name,
            pair.b.start_line,
            pair.b.end_line,
            pair.similarity * 100.0,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::io::Write;
    use std::path::PathBuf;

    /// Two near-identical function bodies — guaranteed to score above any
    /// modest threshold. Used by the report-rendering and
    /// threshold-suppression tests so a single source string drives both
    /// success-path checks.
    const PAIRED_FUNCTIONS: &str = r#"
fn alpha(x: i32) -> i32 { let y = x + 1; let z = y * 2; z }
fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
"#;

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn assert_json_pair_report(out: &str) {
        let parsed: serde_json::Value = serde_json::from_str(out).unwrap();
        assert_eq!(parsed["function_count"], 2);
        assert!(parsed["pair_count"].as_u64().unwrap() >= 1);
        let pairs = parsed["pairs"].as_array().unwrap();
        let names: Vec<&str> = pairs
            .iter()
            .flat_map(|p| {
                [
                    p["a"]["name"].as_str().unwrap(),
                    p["b"]["name"].as_str().unwrap(),
                ]
            })
            .collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    fn assert_markdown_pair_report(out: &str) {
        assert!(out.contains("Similarity report"));
        assert!(out.contains("similar pair"));
        assert!(out.contains("alpha"));
        assert!(out.contains("beta"));
        assert!(out.contains("% similar"));
    }

    /// Both output formats must surface the matched function names; only the
    /// shape of the rendered report differs.
    #[rstest]
    #[case::json(OutputFormat::Json, assert_json_pair_report)]
    #[case::markdown(OutputFormat::Md, assert_markdown_pair_report)]
    fn report_renders_paired_functions(
        #[case] format: OutputFormat,
        #[case] assert_report: fn(&str),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", PAIRED_FUNCTIONS);
        let out = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, format)
            .unwrap();
        assert_report(&out);
    }

    #[test]
    fn empty_report_when_no_pairs_above_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn alpha() -> i32 { 42 }
fn beta(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs {
        if *x > 0 {
            total += x;
        }
    }
    total
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let md = SimilarityAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("No similar function pairs"));
    }

    #[test]
    fn threshold_override_suppresses_all_pairs() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", PAIRED_FUNCTIONS);
        let json = SimilarityAnalyzer::new()
            .with_threshold(1.5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["pair_count"], 0);
    }

    #[test]
    fn typescript_file_is_analyzed() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
function alpha(xs: number[]): number {
    let total = 0;
    for (const x of xs) { total += x; }
    return total;
}
function beta(ys: number[]): number {
    let sum = 0;
    for (const y of ys) { sum += y; }
    return sum;
}
"#;
        let file = write_file(dir.path(), "lib.ts", src);
        let out = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(parsed["pair_count"].as_u64().unwrap() >= 1);
    }

    fn setup_unsupported_extension(dir: &Path) -> PathBuf {
        write_file(dir, "notes.txt", "hello")
    }

    fn setup_missing_file(_dir: &Path) -> PathBuf {
        PathBuf::from("/definitely/does/not/exist.rs")
    }

    fn setup_invalid_rust(dir: &Path) -> PathBuf {
        write_file(dir, "broken.rs", "fn ??? {")
    }

    /// All recoverable failure modes route through `AnalyzerError`. Rather
    /// than spinning up a dedicated test per variant, drive the same
    /// `analyze` call and assert on the matching enum arm.
    #[rstest]
    #[case::unsupported_extension(
        setup_unsupported_extension,
        |e: &AnalyzerError| matches!(e, AnalyzerError::UnsupportedExtension { .. }),
    )]
    #[case::missing_file(
        setup_missing_file,
        |e: &AnalyzerError| matches!(e, AnalyzerError::Io { .. }),
    )]
    #[case::parse_failure(
        setup_invalid_rust,
        |e: &AnalyzerError| matches!(e, AnalyzerError::Parse(_)),
    )]
    fn analyze_surfaces_error_variants(
        #[case] setup: fn(&Path) -> PathBuf,
        #[case] matches_expected: fn(&AnalyzerError) -> bool,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let path = setup(dir.path());
        let err = SimilarityAnalyzer::new()
            .analyze(&path, OutputFormat::Json)
            .unwrap_err();
        assert!(matches_expected(&err), "unexpected error variant: {err}");
    }
}
