//! `analyze complexity` — surface per-function complexity metrics for a
//! Rust source file.
//!
//! Today the analyzer is single-file, single-language: pass it a `.rs` path
//! and it reports every free function, inherent / trait method, and trait
//! default method along with Cyclomatic, Cognitive, Max Nesting, Halstead
//! Volume, and Maintainability Index. Output is JSON by default; the
//! markdown mode emits a compact summary tuned for LLM context windows
//! rather than for humans, in line with the project's "agent-friendly lint"
//! ethos.

use std::fmt::Write as _;
use std::path::Path;

use lens_domain::FunctionComplexity;
use serde::Serialize;

use super::{
    AnalyzerError, LineRange, OutputFormat, SourceLang, changed_line_ranges, format_optional_f64,
    read_source,
};

/// Analyzer entry point. Stateless today; kept as a struct so per-run
/// configuration (filters, thresholds) can be added without breaking the
/// CLI surface.
#[derive(Debug, Default, Clone, Copy)]
pub struct ComplexityAnalyzer {
    diff_only: bool,
}

impl ComplexityAnalyzer {
    pub fn new() -> Self {
        Self { diff_only: false }
    }

    /// Restrict reports to functions intersecting an unstaged changed
    /// line in `git diff -U0`.
    pub fn with_diff_only(mut self, diff_only: bool) -> Self {
        self.diff_only = diff_only;
        self
    }

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let (lang, source) = read_source(path)?;
        let mut functions = match lang {
            SourceLang::Rust => lens_rust::extract_complexity_units(&source)
                .map_err(|e| AnalyzerError::Parse(Box::new(e)))?,
            SourceLang::TypeScript(dialect) => lens_ts::extract_complexity_units(&source, dialect)
                .map_err(|e| AnalyzerError::Parse(Box::new(e)))?,
            SourceLang::Python => lens_py::extract_complexity_units(&source)
                .map_err(|e| AnalyzerError::Parse(Box::new(e)))?,
        };
        if self.diff_only {
            let changed = changed_line_ranges(path);
            functions.retain(|f| overlaps_any(f.start_line, f.end_line, &changed));
        }
        let report = Report::new(path, &functions);
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

#[derive(Debug, Serialize)]
struct Report<'a> {
    file: String,
    function_count: usize,
    summary: Summary,
    functions: Vec<FunctionView<'a>>,
}

impl<'a> Report<'a> {
    fn new(path: &Path, functions: &'a [FunctionComplexity]) -> Self {
        Self {
            file: path.display().to_string(),
            function_count: functions.len(),
            summary: Summary::from_functions(functions),
            functions: functions.iter().map(FunctionView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct Summary {
    cyclomatic_max: u32,
    cyclomatic_p95: u32,
    cyclomatic_median: u32,
    cognitive_max: u32,
    cognitive_p95: u32,
    cognitive_median: u32,
    max_nesting_max: u32,
    loc_total: usize,
    /// Lowest MI seen across the file's functions, or `null` when no
    /// function has a defined MI (empty file or all-undefined Halstead).
    #[serde(skip_serializing_if = "Option::is_none")]
    maintainability_index_min: Option<f64>,
}

impl Summary {
    fn from_functions(functions: &[FunctionComplexity]) -> Self {
        if functions.is_empty() {
            return Self {
                cyclomatic_max: 0,
                cyclomatic_p95: 0,
                cyclomatic_median: 0,
                cognitive_max: 0,
                cognitive_p95: 0,
                cognitive_median: 0,
                max_nesting_max: 0,
                loc_total: 0,
                maintainability_index_min: None,
            };
        }
        let mut cc: Vec<u32> = functions.iter().map(|f| f.cyclomatic).collect();
        let mut cog: Vec<u32> = functions.iter().map(|f| f.cognitive).collect();
        cc.sort_unstable();
        cog.sort_unstable();
        let nesting_max = functions.iter().map(|f| f.max_nesting).max().unwrap_or(0);
        let loc_total = functions.iter().map(FunctionComplexity::loc).sum();
        let mi_min = functions
            .iter()
            .filter_map(FunctionComplexity::maintainability_index)
            .fold(None::<f64>, |acc, x| Some(acc.map_or(x, |a| a.min(x))));

        Self {
            cyclomatic_max: percentile(&cc, 100),
            cyclomatic_p95: percentile(&cc, 95),
            cyclomatic_median: percentile(&cc, 50),
            cognitive_max: percentile(&cog, 100),
            cognitive_p95: percentile(&cog, 95),
            cognitive_median: percentile(&cog, 50),
            max_nesting_max: nesting_max,
            loc_total,
            maintainability_index_min: mi_min,
        }
    }
}

/// Percentile lookup over a pre-sorted slice. `p` is in `[0, 100]`.
///
/// Uses nearest-rank: index = ceil(p/100 * n) - 1, clamped to `[0, n-1]`.
/// Returns `0` for an empty slice (callers guard against this above).
fn percentile(sorted: &[u32], p: u32) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let p = p.min(100);
    let n = sorted.len();
    let idx = ((p as usize * n).div_ceil(100)).saturating_sub(1);
    sorted[idx]
}

#[derive(Debug, Serialize)]
struct FunctionView<'a> {
    name: &'a str,
    start_line: usize,
    end_line: usize,
    loc: usize,
    cyclomatic: u32,
    cognitive: u32,
    max_nesting: u32,
    halstead_volume: Option<f64>,
    maintainability_index: Option<f64>,
}

impl<'a> From<&'a FunctionComplexity> for FunctionView<'a> {
    fn from(f: &'a FunctionComplexity) -> Self {
        Self {
            name: f.name.as_str(),
            start_line: f.start_line,
            end_line: f.end_line,
            loc: f.loc(),
            cyclomatic: f.cyclomatic,
            cognitive: f.cognitive,
            max_nesting: f.max_nesting,
            halstead_volume: f.halstead_volume(),
            maintainability_index: f.maintainability_index(),
        }
    }
}

const TOP_N: usize = 5;

fn format_markdown(report: &Report<'_>) -> String {
    let mut out = format!(
        "# Complexity report: {} ({} function(s))\n",
        report.file, report.function_count,
    );
    if report.functions.is_empty() {
        out.push_str("\n_No functions found._\n");
        return out;
    }
    render_summary(&mut out, &report.summary);
    render_top_functions(&mut out, &report.functions);
    out
}

fn render_summary(out: &mut String, s: &Summary) {
    // writeln! into a String cannot fail; the result is swallowed
    // deliberately rather than unwrapped to satisfy the workspace's
    // `unwrap_used` lint.
    let _ = writeln!(
        out,
        "\n## Summary\n\
         - cyclomatic: median={}, p95={}, max={}\n\
         - cognitive: median={}, p95={}, max={}\n\
         - max_nesting: {}\n\
         - loc_total: {}\n\
         - maintainability_index_min: {}",
        s.cyclomatic_median,
        s.cyclomatic_p95,
        s.cyclomatic_max,
        s.cognitive_median,
        s.cognitive_p95,
        s.cognitive_max,
        s.max_nesting_max,
        s.loc_total,
        format_optional_f64(s.maintainability_index_min, 1),
    );
}

fn render_top_functions(out: &mut String, functions: &[FunctionView<'_>]) {
    // Rank by cognitive first, then cyclomatic, then earliest line.
    //
    // Cyclomatic is dominated by `match` arms, so an exhaustive enum
    // dispatch (CC=30, cog=1) would otherwise drown out genuinely complex
    // logic (CC=8, cog=12). Cognitive penalises nesting and short-circuit
    // chains the way a human reader does, which is closer to the signal
    // the agent actually wants when picking what to read or refactor
    // first. Cyclomatic stays as the tiebreaker so ranking is still
    // deterministic when cognitive ties.
    let mut indexed: Vec<&FunctionView<'_>> = functions.iter().collect();
    indexed.sort_by(|a, b| {
        b.cognitive
            .cmp(&a.cognitive)
            .then_with(|| b.cyclomatic.cmp(&a.cyclomatic))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });

    let _ = writeln!(
        out,
        "\n## Top {TOP_N} by complexity (cognitive, then cyclomatic)"
    );
    for fv in indexed.iter().take(TOP_N) {
        let _ = writeln!(
            out,
            "- `{}` (L{}-{}): cc={}, cog={}, nest={}, loc={}, mi={}",
            fv.name,
            fv.start_line,
            fv.end_line,
            fv.cyclomatic,
            fv.cognitive,
            fv.max_nesting,
            fv.loc,
            format_optional_f64(fv.maintainability_index, 0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn json_report_includes_function_metrics_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn simple() {}
fn branchy(n: i32) -> i32 {
    if n > 0 { 1 } else if n < 0 { -1 } else { 0 }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let json = ComplexityAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["function_count"], 2);
        // The branchy function has CC = 3 (1 + if + else if).
        let cc_max = parsed["summary"]["cyclomatic_max"].as_u64().unwrap();
        assert_eq!(cc_max, 3);
        // simple should land at CC=1, branchy at 3 in the function list.
        let funcs = parsed["functions"].as_array().unwrap();
        assert_eq!(funcs.len(), 2);
    }

    #[test]
    fn markdown_report_lists_top_functions_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        // `branchy` nests an if/else (cc=2, cog=2). `dispatch` is a flat
        // exhaustive match (cc=4, cog=1). Cyclomatic-first ordering would
        // surface `dispatch` even though it is humanly trivial, while
        // cognitive-first ordering correctly puts `branchy` ahead.
        let src = r#"
fn quiet() {}
fn branchy(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }
fn dispatch(n: i32) -> i32 {
    match n { 0 => 0, 1 => 1, 2 => 2, _ => 3 }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let md = ComplexityAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Complexity report"));
        assert!(md.contains("Summary"));
        assert!(md.contains("Top 5 by complexity (cognitive, then cyclomatic)"));
        // Cognitive-first: `branchy` (cog=2) outranks `dispatch` (cog=1)
        // even though `dispatch` has the higher cyclomatic.
        let pos_branchy = md.find("`branchy`").unwrap();
        let pos_dispatch = md.find("`dispatch`").unwrap();
        assert!(
            pos_branchy < pos_dispatch,
            "expected highest-cognitive function listed first",
        );
    }

    #[test]
    fn empty_file_produces_no_function_report() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", "// nothing here\n");
        let md = ComplexityAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("No functions found"));
    }

    #[test]
    fn unknown_extension_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "notes.txt", "hello");
        let err = ComplexityAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::UnsupportedExtension { .. }));
    }

    #[test]
    fn python_function_metrics_are_reported() {
        // `simple` returns a literal rather than `pass`-only so that
        // the lens-py stub filter (Protocol / abstract / `pass`-only)
        // doesn't drop it before complexity is measured. A `pass` body
        // would now read as a stub.
        let dir = tempfile::tempdir().unwrap();
        let src = "
def simple():
    return 0

def branchy(n):
    if n > 0:
        return 1
    elif n < 0:
        return -1
    else:
        return 0
";
        let file = write_file(dir.path(), "lib.py", src);
        let json = ComplexityAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["function_count"], 2);
        // base 1 + if + elif = 3
        assert_eq!(parsed["summary"]["cyclomatic_max"], 3);
    }

    #[test]
    fn missing_file_surfaces_io_error() {
        let err = ComplexityAnalyzer::new()
            .analyze(
                Path::new("/definitely/does/not/exist.rs"),
                OutputFormat::Json,
            )
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::Io { .. }));
    }

    #[test]
    fn invalid_rust_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "broken.rs", "fn ??? {");
        let err = ComplexityAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::Parse(_)));
    }

    #[test]
    fn diff_only_filters_to_changed_functions() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
fn alpha() -> i32 { 1 }
fn beta() -> i32 { 2 }
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
fn alpha() -> i32 { if true { 1 } else { 0 } }
fn beta() -> i32 { 2 }
"#,
        );
        let json = ComplexityAnalyzer::new()
            .with_diff_only(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["function_count"], 1);
        assert_eq!(parsed["functions"][0]["name"], "alpha");
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

    #[test]
    fn percentile_picks_nearest_rank() {
        let sorted = [1, 2, 3, 4, 5];
        assert_eq!(percentile(&sorted, 100), 5);
        assert_eq!(percentile(&sorted, 95), 5);
        assert_eq!(percentile(&sorted, 50), 3);
        assert_eq!(percentile(&sorted, 25), 2);
        assert_eq!(percentile(&sorted, 0), 1);
    }

    #[test]
    fn percentile_on_empty_slice_returns_zero() {
        assert_eq!(percentile(&[], 50), 0);
    }

    #[test]
    fn percentile_clamps_out_of_range_p_to_last() {
        // p > 100 should not panic and should saturate at the last element.
        let sorted = [1, 2, 3, 4, 5];
        assert_eq!(percentile(&sorted, 150), 5);
    }

    #[test]
    fn format_optional_f64_renders_some_with_precision() {
        assert_eq!(format_optional_f64(Some(1.234), 1), "1.2");
        assert_eq!(format_optional_f64(Some(1.234), 0), "1");
        assert_eq!(format_optional_f64(Some(1.0), 2), "1.00");
    }

    #[test]
    fn format_optional_f64_renders_none_as_n_a() {
        assert_eq!(format_optional_f64(None, 0), "n/a");
        assert_eq!(format_optional_f64(None, 3), "n/a");
    }
}
