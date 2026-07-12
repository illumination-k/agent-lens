//! `analyze error-shape` — surface per-function error-handling shape.
//!
//! Reports the signals behind the "excessive error branching"
//! anti-pattern family: how much of a function is error handling
//! (`error_loc_ratio`), how fragmented its `try` usage is
//! (`disjoint_try_count` / `single_stmt_try_count`), and how many of
//! its handlers only rethrow or log-and-rethrow. The analyzer reports
//! shape, not verdicts — a boundary function legitimately spends most
//! of its lines on error paths, so thresholding is left to the agent
//! reading the report.
//!
//! Accepts either a single source file or a directory. When the input
//! is a directory the analyzer walks it recursively (respecting
//! `.gitignore` via the `ignore` crate), parses every supported file,
//! and groups findings per file. Functions without any error-handling
//! construct are dropped so the report stays signal-dense; the
//! top-level `summary` still records how many functions were scanned.
//! Output is JSON by default; the markdown mode emits a compact
//! summary tuned for LLM context windows.

use std::fmt::Write as _;
use std::path::Path;

use lens_domain::FunctionErrorShape;
use serde::Serialize;

use super::runner::{FilterConfig, delegate_filter_builders, render_report};
use super::{AnalyzerError, OutputFormat, SourceFile, SourceLang, read_source};

/// Analyzer entry point. Stateless today; kept as a struct so per-run
/// configuration can be added without breaking the CLI surface.
#[derive(Debug, Default, Clone)]
pub struct ErrorShapeAnalyzer {
    filter: FilterConfig,
    top: Option<usize>,
}

impl ErrorShapeAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cap the markdown report's function ranking to the top-N entries.
    /// JSON output always carries the full list.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    delegate_filter_builders!(filter);

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let files = self
            .filter
            .collect_per_file(path, |sf| self.analyze_file(sf))?;
        let report = Report::new(path, &files);
        render_report(&report, format, || format_markdown(&report, self.top))
    }

    /// Analyze a single file. Returns `None` when no function in the
    /// file has any error handling (after filtering), so empty entries
    /// don't pollute the directory-mode report.
    fn analyze_file(&self, file: &SourceFile) -> Result<Option<FileReport>, AnalyzerError> {
        let (lang, source) = read_source(&file.path)?;
        let mut functions = extract_units(lang, &source).map_err(AnalyzerError::Parse)?;
        self.filter
            .retain_changed(&mut functions, &file.path, |f| (f.start_line, f.end_line));
        let scanned = functions.len();
        functions.retain(FunctionErrorShape::has_error_handling);
        if functions.is_empty() {
            return Ok(None);
        }
        Ok(Some(FileReport {
            file: file.display_path.clone(),
            scanned_function_count: scanned,
            functions,
        }))
    }
}

type BoxedError = Box<dyn std::error::Error + Send + Sync>;

fn extract_units(lang: SourceLang, source: &str) -> Result<Vec<FunctionErrorShape>, BoxedError> {
    super::dispatch_lens!(lang, source, extract_error_shapes)
}

/// Per-file slice of the report.
#[derive(Debug)]
struct FileReport {
    file: String,
    /// Functions parsed in the file, including those dropped for
    /// having no error handling.
    scanned_function_count: usize,
    functions: Vec<FunctionErrorShape>,
}

#[derive(Debug, Serialize)]
struct Report<'a> {
    /// Input path: a single source file, or the root directory walked.
    root: String,
    file_count: usize,
    /// Functions with at least one error-handling construct — the ones
    /// listed under `files`.
    function_count: usize,
    summary: Summary,
    files: Vec<FileView<'a>>,
}

impl<'a> Report<'a> {
    fn new(path: &Path, files: &'a [FileReport]) -> Self {
        let function_count = files.iter().map(|f| f.functions.len()).sum();
        Self {
            root: path.display().to_string(),
            file_count: files.len(),
            function_count,
            summary: Summary::from_files(files),
            files: files.iter().map(FileView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct FileView<'a> {
    file: &'a str,
    function_count: usize,
    functions: Vec<FunctionView<'a>>,
}

impl<'a> From<&'a FileReport> for FileView<'a> {
    fn from(f: &'a FileReport) -> Self {
        Self {
            file: f.file.as_str(),
            function_count: f.functions.len(),
            functions: f.functions.iter().map(FunctionView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct Summary {
    /// Functions parsed across the corpus, including those without any
    /// error handling (which the report drops).
    scanned_function_count: usize,
    error_loc_ratio_median: f64,
    error_loc_ratio_p95: f64,
    error_loc_ratio_max: f64,
    error_branch_max: u32,
    disjoint_try_max: u32,
    single_stmt_try_total: u32,
    rethrow_only_total: u32,
    log_and_rethrow_total: u32,
    /// Functions whose whole error path is propagation — candidate
    /// links in a wrap-at-every-layer chain.
    wrap_only_function_count: usize,
}

impl Summary {
    fn from_files(files: &[FileReport]) -> Self {
        let scanned = files.iter().map(|f| f.scanned_function_count).sum();
        let all = || files.iter().flat_map(|f| f.functions.iter());
        let mut ratios: Vec<f64> = all().map(FunctionErrorShape::error_loc_ratio).collect();
        ratios.sort_unstable_by(f64::total_cmp);
        Self {
            scanned_function_count: scanned,
            error_loc_ratio_median: percentile_f64(&ratios, 50),
            error_loc_ratio_p95: percentile_f64(&ratios, 95),
            error_loc_ratio_max: percentile_f64(&ratios, 100),
            error_branch_max: all().map(|f| f.error_branch_count).max().unwrap_or(0),
            disjoint_try_max: all().map(|f| f.disjoint_try_count).max().unwrap_or(0),
            single_stmt_try_total: all().map(|f| f.single_stmt_try_count).sum(),
            rethrow_only_total: all().map(|f| f.rethrow_only_handlers).sum(),
            log_and_rethrow_total: all().map(|f| f.log_and_rethrow_handlers).sum(),
            wrap_only_function_count: all().filter(|f| f.wrap_only_error_path).count(),
        }
    }
}

/// Percentile lookup over a pre-sorted slice. `p` is in `[0, 100]`.
///
/// Nearest-rank, mirroring the integer version in the complexity
/// analyzer: index = ceil(p/100 * n) - 1, clamped to `[0, n-1]`.
/// Returns `0.0` for an empty slice.
fn percentile_f64(sorted: &[f64], p: u32) -> f64 {
    if sorted.is_empty() {
        return 0.0;
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
    error_branch_count: u32,
    error_loc: usize,
    error_loc_ratio: f64,
    disjoint_try_count: u32,
    single_stmt_try_count: u32,
    rethrow_only_handlers: u32,
    log_and_rethrow_handlers: u32,
    wrap_only_error_path: bool,
}

impl<'a> From<&'a FunctionErrorShape> for FunctionView<'a> {
    fn from(f: &'a FunctionErrorShape) -> Self {
        Self {
            name: f.name.as_str(),
            start_line: f.start_line,
            end_line: f.end_line,
            loc: f.loc(),
            error_branch_count: f.error_branch_count,
            error_loc: f.error_loc,
            error_loc_ratio: f.error_loc_ratio(),
            disjoint_try_count: f.disjoint_try_count,
            single_stmt_try_count: f.single_stmt_try_count,
            rethrow_only_handlers: f.rethrow_only_handlers,
            log_and_rethrow_handlers: f.log_and_rethrow_handlers,
            wrap_only_error_path: f.wrap_only_error_path,
        }
    }
}

const DEFAULT_TOP: usize = 5;

fn format_markdown(report: &Report<'_>, top: Option<usize>) -> String {
    let mut out = format!(
        "# Error-shape report: {} ({} file(s), {} function(s) with error handling)\n",
        report.root, report.file_count, report.function_count,
    );
    if report.function_count == 0 {
        out.push_str("\n_No error-handling functions found._\n");
        return out;
    }
    render_summary(&mut out, &report.summary);
    render_top_functions(&mut out, &report.files, top);
    out
}

fn render_summary(out: &mut String, s: &Summary) {
    let _ = writeln!(
        out,
        "\n## Summary\n\
         - scanned_functions: {}\n\
         - error_loc_ratio: median={:.2}, p95={:.2}, max={:.2}\n\
         - error_branch_max: {}\n\
         - disjoint_try_max: {}\n\
         - single_stmt_try_total: {}\n\
         - rethrow_only_total: {}\n\
         - log_and_rethrow_total: {}\n\
         - wrap_only_functions: {}",
        s.scanned_function_count,
        s.error_loc_ratio_median,
        s.error_loc_ratio_p95,
        s.error_loc_ratio_max,
        s.error_branch_max,
        s.disjoint_try_max,
        s.single_stmt_try_total,
        s.rethrow_only_total,
        s.log_and_rethrow_total,
        s.wrap_only_function_count,
    );
}

/// One row of the top-N table.
struct TopRow<'a> {
    file: &'a str,
    func: &'a FunctionView<'a>,
}

fn render_top_functions(out: &mut String, files: &[FileView<'_>], top: Option<usize>) {
    // Rank by error-line ratio first — the "happy path is buried"
    // signal — then by try fragmentation, then branch count, then
    // position for determinism.
    let mut rows: Vec<TopRow<'_>> = files
        .iter()
        .flat_map(|fv| {
            fv.functions.iter().map(|func| TopRow {
                file: fv.file,
                func,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        b.func
            .error_loc_ratio
            .total_cmp(&a.func.error_loc_ratio)
            .then_with(|| b.func.disjoint_try_count.cmp(&a.func.disjoint_try_count))
            .then_with(|| b.func.error_branch_count.cmp(&a.func.error_branch_count))
            .then_with(|| a.func.start_line.cmp(&b.func.start_line))
            .then_with(|| a.file.cmp(b.file))
    });

    let limit = top.unwrap_or(DEFAULT_TOP);
    let _ = writeln!(out, "\n## Top {limit} by error_loc_ratio");
    for row in rows.iter().take(limit) {
        let f = row.func;
        let _ = writeln!(
            out,
            "- {}:`{}` (L{}-{}): ratio={:.2}, branches={}, tries={}, single_stmt_tries={}, rethrow_only={}, log_rethrow={}, wrap_only={}",
            row.file,
            f.name,
            f.start_line,
            f.end_line,
            f.error_loc_ratio,
            f.error_branch_count,
            f.disjoint_try_count,
            f.single_stmt_try_count,
            f.rethrow_only_handlers,
            f.log_and_rethrow_handlers,
            f.wrap_only_error_path,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};

    #[test]
    fn json_report_includes_shape_metrics_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn quiet() {}
fn propagates(r: Result<i32, String>) -> Result<i32, String> {
    match r {
        Ok(v) => Ok(v),
        Err(e) => Err(e),
    }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let json = ErrorShapeAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // `quiet` has no error handling and is dropped from the file
        // list, but still shows up in the scanned count.
        assert_eq!(parsed["function_count"], 1);
        assert_eq!(parsed["summary"]["scanned_function_count"], 2);
        assert_eq!(parsed["summary"]["rethrow_only_total"], 1);
        assert_eq!(parsed["summary"]["wrap_only_function_count"], 1);
        let func = &parsed["files"][0]["functions"][0];
        assert_eq!(func["name"], "propagates");
        assert_eq!(func["error_branch_count"], 1);
        assert_eq!(func["wrap_only_error_path"], true);
        assert!(func["error_loc_ratio"].as_f64().unwrap() > 0.0);
    }

    #[test]
    fn typescript_try_fragmentation_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
function f(): void {
    try {
        stepOne();
    } catch (e) {
        throw e;
    }
    try {
        stepTwo();
    } catch (e) {
        console.error(e);
        throw e;
    }
}
"#;
        let file = write_file(dir.path(), "app.ts", src);
        let json = ErrorShapeAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let func = &parsed["files"][0]["functions"][0];
        assert_eq!(func["disjoint_try_count"], 2);
        assert_eq!(func["single_stmt_try_count"], 2);
        assert_eq!(func["rethrow_only_handlers"], 1);
        assert_eq!(func["log_and_rethrow_handlers"], 1);
    }

    #[test]
    fn python_handlers_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let src = "
def f():
    try:
        return risky()
    except ValueError:
        raise
";
        let file = write_file(dir.path(), "app.py", src);
        let json = ErrorShapeAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let func = &parsed["files"][0]["functions"][0];
        assert_eq!(func["name"], "f");
        assert_eq!(func["rethrow_only_handlers"], 1);
        assert_eq!(func["wrap_only_error_path"], true);
    }

    #[test]
    fn go_error_checks_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
package p

func f() error {
    if err := step(); err != nil {
        return err
    }
    return nil
}
"#;
        let file = write_file(dir.path(), "main.go", src);
        let json = ErrorShapeAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let func = &parsed["files"][0]["functions"][0];
        assert_eq!(func["error_branch_count"], 1);
        assert_eq!(func["rethrow_only_handlers"], 1);
        assert_eq!(func["wrap_only_error_path"], true);
    }

    #[test]
    fn markdown_ranks_by_error_loc_ratio() {
        let dir = tempfile::tempdir().unwrap();
        // `heavy` is mostly error handling; `light` has one small
        // handler in a longer body.
        let src = r#"
fn heavy(r: Result<i32, String>) -> Result<i32, String> {
    match r {
        Ok(v) => Ok(v),
        Err(e) => {
            let msg = format!("failed: {e}");
            Err(msg)
        }
    }
}
fn light(r: Result<i32, String>) -> i32 {
    let a = 1;
    let b = 2;
    let c = a + b;
    let d = c * 2;
    match r {
        Ok(v) => v + d,
        Err(_) => 0,
    }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let md = ErrorShapeAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Error-shape report"));
        assert!(md.contains("Summary"));
        assert!(md.contains("Top 5 by error_loc_ratio"));
        let pos_heavy = md.find("`heavy`").unwrap();
        let pos_light = md.find("`light`").unwrap();
        assert!(
            pos_heavy < pos_light,
            "expected highest-ratio function listed first: {md}",
        );
    }

    #[test]
    fn markdown_top_caps_the_ranking() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn a(r: Result<i32, String>) -> Result<i32, String> {
    match r { Ok(v) => Ok(v), Err(e) => Err(e) }
}
fn b(r: Result<i32, String>) -> Result<i32, String> {
    match r { Ok(v) => Ok(v), Err(e) => Err(e) }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let md = ErrorShapeAnalyzer::new()
            .with_top(Some(1))
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Top 1 by error_loc_ratio"));
        assert_eq!(md.matches("lib.rs:`").count(), 1, "got: {md}");
    }

    #[test]
    fn functions_without_error_handling_produce_no_report() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", "fn quiet() { let _ = 1; }\n");
        let md = ErrorShapeAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("No error-handling functions found"));
    }

    #[test]
    fn unknown_extension_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "notes.txt", "hello");
        let err = ErrorShapeAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::UnsupportedExtension { .. }));
    }

    #[test]
    fn invalid_rust_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "broken.rs", "fn ??? {");
        let err = ErrorShapeAnalyzer::new()
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
fn alpha(r: Result<i32, ()>) -> Result<i32, ()> { r.map_err(|e| e) }
fn beta(r: Result<i32, ()>) -> Result<i32, ()> { r.map_err(|e| e) }
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
fn alpha(r: Result<i32, ()>) -> Result<i32, ()> { r.map_err(|e| { e }) }
fn beta(r: Result<i32, ()>) -> Result<i32, ()> { r.map_err(|e| e) }
"#,
        );
        let json = ErrorShapeAnalyzer::new()
            .with_diff_only(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["function_count"], 1);
        assert_eq!(parsed["files"][0]["functions"][0]["name"], "alpha");
    }

    #[test]
    fn directory_mode_aggregates_across_files() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "quiet.rs", "fn a() {}\n");
        write_file(
            dir.path(),
            "nested/b.rs",
            r#"
fn b(r: Result<i32, String>) -> Result<i32, String> {
    match r { Ok(v) => Ok(v), Err(e) => Err(e) }
}
"#,
        );

        let json = ErrorShapeAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // quiet.rs has no error-handling functions and is dropped.
        assert_eq!(parsed["file_count"], 1);
        assert_eq!(parsed["function_count"], 1);
        assert_eq!(parsed["files"][0]["file"], "nested/b.rs");

        let md = ErrorShapeAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("nested/b.rs:`b`"));
    }

    #[test]
    fn percentile_f64_picks_nearest_rank() {
        let sorted = [0.1, 0.2, 0.3, 0.4, 0.5];
        assert_eq!(percentile_f64(&sorted, 100), 0.5);
        assert_eq!(percentile_f64(&sorted, 95), 0.5);
        assert_eq!(percentile_f64(&sorted, 50), 0.3);
        assert_eq!(percentile_f64(&sorted, 0), 0.1);
    }

    #[test]
    fn percentile_f64_on_empty_slice_returns_zero() {
        assert_eq!(percentile_f64(&[], 50), 0.0);
    }
}
