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
use lens_rust::{RustParser, extract_functions_excluding_tests};
use serde::Serialize;

use super::{AnalyzerError, LineRange, OutputFormat, SourceLang, changed_line_ranges, read_source};

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
    diff_only: bool,
    exclude_tests: bool,
}

/// Generate `pub fn $name(mut self, $field: $ty) -> Self { self.$field = $field; self }`,
/// forwarding any `///` docs through `$attr`. Used to keep the trio of
/// `SimilarityAnalyzer::with_*` setters from drifting out of shape.
macro_rules! with_setter {
    ($(#[$attr:meta])* fn $name:ident, $field:ident: $ty:ty) => {
        $(#[$attr])*
        pub fn $name(mut self, $field: $ty) -> Self {
            self.$field = $field;
            self
        }
    };
}

impl SimilarityAnalyzer {
    pub fn new() -> Self {
        Self {
            threshold: DEFAULT_THRESHOLD,
            opts: TSEDOptions::default(),
            diff_only: false,
            exclude_tests: false,
        }
    }

    with_setter! {
        /// Override the similarity threshold. Callers passing a non-default
        /// value via `--threshold` go through here.
        fn with_threshold, threshold: f64
    }

    with_setter! {
        /// Restrict reports to pairs where at least one function intersects
        /// an unstaged changed line in `git diff -U0`.
        fn with_diff_only, diff_only: bool
    }

    with_setter! {
        /// Drop test scaffolding before computing similarity. Filters out
        /// `#[test]` / `#[rstest]` / `#[<runner>::test]` functions and items
        /// inside `#[cfg(test)] mod` blocks. Table-driven tests otherwise
        /// dominate the noise floor with parallel-but-distinct fixtures
        /// that aren't refactor candidates.
        fn with_exclude_tests, exclude_tests: bool
    }

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let (lang, source) = read_source(path)?;
        let functions = extract_functions(lang, &source, self.exclude_tests)?;
        let changed = if self.diff_only {
            changed_line_ranges(path)
        } else {
            Vec::new()
        };
        let pairs = if self.diff_only {
            find_similar_functions(&functions, self.threshold, &self.opts)
                .into_iter()
                .filter(|pair| {
                    overlaps_any(pair.a.start_line, pair.a.end_line, &changed)
                        || overlaps_any(pair.b.start_line, pair.b.end_line, &changed)
                })
                .collect()
        } else {
            find_similar_functions(&functions, self.threshold, &self.opts)
        };
        let report = Report::new(path, self.threshold, functions.len(), &pairs);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).map_err(AnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&report)),
        }
    }
}

fn overlaps_any(start: usize, end: usize, ranges: &[LineRange]) -> bool {
    ranges.iter().any(|r| r.overlaps(start, end))
}

impl Default for SimilarityAnalyzer {
    fn default() -> Self {
        Self::new()
    }
}

fn extract_functions(
    lang: SourceLang,
    source: &str,
    exclude_tests: bool,
) -> Result<Vec<FunctionDef>, AnalyzerError> {
    match lang {
        SourceLang::Rust => {
            if exclude_tests {
                extract_functions_excluding_tests(source)
                    .map_err(|e| AnalyzerError::Parse(Box::new(e)))
            } else {
                let mut parser = RustParser::new();
                parser
                    .extract_functions(source)
                    .map_err(|e| AnalyzerError::Parse(Box::new(e)))
            }
        }
        SourceLang::TypeScript => {
            // `exclude_tests` has no concrete meaning yet for TS — there
            // is no `#[test]` attribute equivalent. Fall through to the
            // standard parser; file-pattern based filtering can be
            // wired later if it proves useful.
            let mut parser = lens_ts::TypeScriptParser::new();
            <lens_ts::TypeScriptParser as lens_domain::LanguageParser>::extract_functions(
                &mut parser,
                source,
            )
            .map_err(|e| AnalyzerError::Parse(Box::new(e)))
        }
        SourceLang::Python => {
            if exclude_tests {
                lens_py::extract_functions_excluding_tests(source)
                    .map_err(|e| AnalyzerError::Parse(Box::new(e)))
            } else {
                let mut parser = lens_py::PythonParser::new();
                parser
                    .extract_functions(source)
                    .map_err(|e| AnalyzerError::Parse(Box::new(e)))
            }
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
    fn exclude_tests_drops_test_module_pairs_from_report() {
        // Two parallel `#[test]` fixtures alongside a single
        // production function. Without `--exclude-tests` the two test
        // bodies form a similar pair; with it they're filtered before
        // similarity is computed and `pair_count` falls to zero.
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn production(x: i32) -> i32 { x + 1 }

#[cfg(test)]
mod tests {
    fn alpha() -> i32 { 1 + 2 }
    fn beta()  -> i32 { 1 + 2 }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);

        let with_tests = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&with_tests).unwrap();
        assert!(parsed["pair_count"].as_u64().unwrap() >= 1);

        let without_tests = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_exclude_tests(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&without_tests).unwrap();
        assert_eq!(parsed["pair_count"], 0);
        assert_eq!(parsed["function_count"], 1);
    }

    #[test]
    fn report_renders_paired_python_functions() {
        // Two structurally identical Python functions — guaranteed to
        // score above the 0.5 threshold and exercise the lens-py
        // dispatch added alongside the Rust / TS arms.
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
def alpha(xs):
    total = 0
    for x in xs:
        total += x
    return total

def beta(ys):
    sum_ = 0
    for y in ys:
        sum_ += y
    return sum_
"#;
        let file = write_file(dir.path(), "lib.py", src);
        let json = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["function_count"], 2);
        assert!(parsed["pair_count"].as_u64().unwrap() >= 1);
        let names: Vec<&str> = parsed["pairs"]
            .as_array()
            .unwrap()
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

    #[test]
    fn exclude_tests_drops_python_test_functions_from_report() {
        // pytest-style `test_*` functions form a parallel pair next to
        // a single production function; `--exclude-tests` should drop
        // them via `lens_py::extract_functions_excluding_tests`.
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
def production(xs):
    total = 0
    for x in xs:
        total += x
    return total

def test_alpha():
    assert 1 + 1 == 2

def test_beta():
    assert 1 + 1 == 2
"#;
        let file = write_file(dir.path(), "lib.py", src);

        let with_tests = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&with_tests).unwrap();
        assert!(parsed["pair_count"].as_u64().unwrap() >= 1);

        let without_tests = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_exclude_tests(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&without_tests).unwrap();
        assert_eq!(parsed["pair_count"], 0);
        assert_eq!(parsed["function_count"], 1);
    }

    #[test]
    fn diff_only_filters_to_pairs_touching_changed_lines() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
fn alpha(x: i32) -> i32 { let y = x + 1; let z = y * 2; z }
fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
"#,
        );
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        run_git(dir.path(), &["add", "lib.rs"]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        write_file(
            dir.path(),
            "lib.rs",
            r#"
fn alpha(x: i32) -> i32 { let y = x + 10; let z = y * 2; z }
fn beta(x: i32)  -> i32 { let y = x + 1; let z = y * 2; z }
"#,
        );

        let json = SimilarityAnalyzer::new()
            .with_threshold(0.5)
            .with_diff_only(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["pair_count"], 1);
    }

    fn run_git(dir: &Path, args: &[&str]) {
        // Mirror the hardened helper in `hotspot.rs`: disable commit /
        // tag signing so the test never asks the host's signing setup
        // to participate. Without this, sandboxes that have a global
        // `commit.gpgsign=true` (and a signing helper that talks to a
        // service which can fail) make the test brittle.
        let status = std::process::Command::new("git")
            .arg("-c")
            .arg("commit.gpgsign=false")
            .arg("-c")
            .arg("tag.gpgsign=false")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
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
