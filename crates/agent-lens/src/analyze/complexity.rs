//! `analyze complexity` — surface per-function complexity metrics.
//!
//! Accepts either a single source file or a directory. When the input is a
//! directory the analyzer walks it recursively (respecting `.gitignore`
//! via the `ignore` crate, the same one used by ripgrep), parses every
//! supported file, and groups findings per file. The top-level `summary`
//! aggregates metrics across the entire corpus; the markdown top-N table
//! likewise ranks across every file. Output is JSON by default; the
//! markdown mode emits a compact summary tuned for LLM context windows
//! rather than for humans, in line with the project's "agent-friendly lint"
//! ethos.

use std::fmt::Write as _;
use std::path::Path;

use lens_domain::FunctionComplexity;
use serde::Serialize;

use super::{
    AnalyzePathFilter, AnalyzerError, OutputFormat, SourceFile, SourceLang, changed_line_ranges,
    collect_source_files, format_optional_f64, overlaps_any, read_source,
};

/// Analyzer entry point. Stateless today; kept as a struct so per-run
/// configuration (filters, thresholds) can be added without breaking the
/// CLI surface.
#[derive(Debug, Default, Clone)]
pub struct ComplexityAnalyzer {
    diff_only: bool,
    top: Option<usize>,
    min_score: Option<u32>,
    path_filter: AnalyzePathFilter,
}

impl ComplexityAnalyzer {
    pub fn new() -> Self {
        Self {
            diff_only: false,
            top: None,
            min_score: None,
            path_filter: AnalyzePathFilter::new(),
        }
    }

    /// Restrict reports to functions intersecting an unstaged changed
    /// line in `git diff -U0`.
    pub fn with_diff_only(mut self, diff_only: bool) -> Self {
        self.diff_only = diff_only;
        self
    }

    /// Cap the markdown report's function ranking to the top-N entries.
    /// JSON output always carries the full list.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    /// Only include functions whose cognitive complexity is at least this
    /// score in the markdown ranking. JSON output always carries the full
    /// list.
    pub fn with_min_score(mut self, min_score: Option<u32>) -> Self {
        self.min_score = min_score;
        self
    }

    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_only_tests(only_tests);
        self
    }

    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_exclude_tests(exclude_tests);
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.path_filter = self.path_filter.with_exclude_patterns(exclude);
        self
    }

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let files = self.collect_file_reports(path)?;
        let report = Report::new(path, &files);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).map_err(AnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&report, self.top, self.min_score)),
        }
    }

    /// Resolve `path` to a list of per-file reports. Single-file inputs
    /// produce a one-element vec; directory inputs walk recursively,
    /// honouring `.gitignore`. Files with no functions are dropped so the
    /// output stays signal-dense.
    fn collect_file_reports(&self, path: &Path) -> Result<Vec<FileReport>, AnalyzerError> {
        let filter = self.path_filter.compile(path)?;
        let mut out = Vec::new();
        for source_file in collect_source_files(path, &filter)? {
            if let Some(report) = self.analyze_file(&source_file)? {
                out.push(report);
            }
        }
        Ok(out)
    }

    /// Analyze a single file. Returns `None` when the file has no
    /// functions (after filtering), so empty entries don't pollute the
    /// directory-mode report.
    fn analyze_file(&self, file: &SourceFile) -> Result<Option<FileReport>, AnalyzerError> {
        let (lang, source) = read_source(&file.path)?;
        let mut functions = extract_units(lang, &source).map_err(AnalyzerError::Parse)?;
        if self.diff_only {
            let changed = changed_line_ranges(&file.path);
            functions.retain(|f| overlaps_any(f.start_line, f.end_line, &changed));
        }
        if functions.is_empty() {
            return Ok(None);
        }
        Ok(Some(FileReport {
            file: file.display_path.clone(),
            functions,
        }))
    }
}

type BoxedError = Box<dyn std::error::Error + Send + Sync>;

fn extract_units(lang: SourceLang, source: &str) -> Result<Vec<FunctionComplexity>, BoxedError> {
    match lang {
        SourceLang::Rust => {
            lens_rust::extract_complexity_units(source).map_err(|e| Box::new(e) as BoxedError)
        }
        SourceLang::TypeScript(dialect) => lens_ts::extract_complexity_units(source, dialect)
            .map_err(|e| Box::new(e) as BoxedError),
        SourceLang::Python => {
            lens_py::extract_complexity_units(source).map_err(|e| Box::new(e) as BoxedError)
        }
        // Complexity for Go is not implemented yet; the language is only
        // wired up for similarity. Returning an empty unit list keeps
        // directory walks running across mixed-language repos.
        SourceLang::Go => {
            let _ = source;
            Ok(Vec::new())
        }
    }
}

/// Per-file slice of the report. Owns the display path so directory mode
/// can attach a path relative to the walk root without storing the original
/// `PathBuf`.
#[derive(Debug)]
struct FileReport {
    file: String,
    functions: Vec<FunctionComplexity>,
}

#[derive(Debug, Serialize)]
struct Report<'a> {
    /// Input path: a single source file, or the root directory walked.
    root: String,
    file_count: usize,
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
    cyclomatic_max: u32,
    cyclomatic_p95: u32,
    cyclomatic_median: u32,
    cognitive_max: u32,
    cognitive_p95: u32,
    cognitive_median: u32,
    max_nesting_max: u32,
    loc_total: usize,
    /// Lowest MI seen across the corpus, or `null` when no function has
    /// a defined MI (empty input or all-undefined Halstead).
    #[serde(skip_serializing_if = "Option::is_none")]
    maintainability_index_min: Option<f64>,
}

impl Summary {
    fn from_files(files: &[FileReport]) -> Self {
        let total: usize = files.iter().map(|f| f.functions.len()).sum();
        if total == 0 {
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
        let all = || files.iter().flat_map(|f| f.functions.iter());
        let mut cc: Vec<u32> = all().map(|f| f.cyclomatic).collect();
        let mut cog: Vec<u32> = all().map(|f| f.cognitive).collect();
        cc.sort_unstable();
        cog.sort_unstable();
        let nesting_max = all().map(|f| f.max_nesting).max().unwrap_or(0);
        let loc_total = all().map(FunctionComplexity::loc).sum();
        let mi_min = all()
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

const DEFAULT_TOP: usize = 5;

fn format_markdown(report: &Report<'_>, top: Option<usize>, min_score: Option<u32>) -> String {
    let mut out = format!(
        "# Complexity report: {} ({} file(s), {} function(s))\n",
        report.root, report.file_count, report.function_count,
    );
    if report.function_count == 0 {
        out.push_str("\n_No functions found._\n");
        return out;
    }
    render_summary(&mut out, &report.summary);
    render_top_functions(&mut out, &report.files, top, min_score);
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

/// One row of the top-N table. The view holds borrows into the per-file
/// reports plus the file the function came from, so directory-mode output
/// stays unambiguous.
struct TopRow<'a> {
    file: &'a str,
    func: &'a FunctionView<'a>,
}

fn render_top_functions(
    out: &mut String,
    files: &[FileView<'_>],
    top: Option<usize>,
    min_score: Option<u32>,
) {
    // Rank by cognitive first, then cyclomatic, then earliest line.
    //
    // Cyclomatic is dominated by `match` arms, so an exhaustive enum
    // dispatch (CC=30, cog=1) would otherwise drown out genuinely complex
    // logic (CC=8, cog=12). Cognitive penalises nesting and short-circuit
    // chains the way a human reader does, which is closer to the signal
    // the agent actually wants when picking what to read or refactor
    // first. Cyclomatic stays as the tiebreaker so ranking is still
    // deterministic when cognitive ties.
    let mut rows: Vec<TopRow<'_>> = files
        .iter()
        .flat_map(|fv| {
            fv.functions.iter().map(|func| TopRow {
                file: fv.file,
                func,
            })
        })
        .collect();
    if let Some(min_score) = min_score {
        rows.retain(|row| row.func.cognitive >= min_score);
    }
    rows.sort_by(|a, b| {
        b.func
            .cognitive
            .cmp(&a.func.cognitive)
            .then_with(|| b.func.cyclomatic.cmp(&a.func.cyclomatic))
            .then_with(|| a.func.start_line.cmp(&b.func.start_line))
            .then_with(|| a.file.cmp(b.file))
    });

    let limit = top.unwrap_or(DEFAULT_TOP);
    let suffix = min_score.map_or_else(String::new, |s| format!(", cognitive >= {s}"));
    let _ = writeln!(
        out,
        "\n## Top {limit} by complexity (cognitive, then cyclomatic{suffix})"
    );
    if rows.is_empty() {
        out.push_str("\n_No functions matched the complexity score filter._\n");
        return;
    }
    for row in rows.iter().take(limit) {
        let f = row.func;
        let _ = writeln!(
            out,
            "- {}:`{}` (L{}-{}): cc={}, cog={}, nest={}, loc={}, mi={}",
            row.file,
            f.name,
            f.start_line,
            f.end_line,
            f.cyclomatic,
            f.cognitive,
            f.max_nesting,
            f.loc,
            format_optional_f64(f.maintainability_index, 0),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};

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
        assert_eq!(parsed["file_count"], 1);
        // The branchy function has CC = 3 (1 + if + else if).
        let cc_max = parsed["summary"]["cyclomatic_max"].as_u64().unwrap();
        assert_eq!(cc_max, 3);
        let funcs = parsed["files"][0]["functions"].as_array().unwrap();
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
    fn markdown_top_and_min_score_filter_the_ranking() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn quiet() {}
fn branchy(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }
fn dispatch(n: i32) -> i32 {
    match n { 0 => 0, 1 => 1, 2 => 2, _ => 3 }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let md = ComplexityAnalyzer::new()
            .with_top(Some(1))
            .with_min_score(Some(2))
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Top 1 by complexity"));
        assert!(md.contains("cognitive >= 2"));
        assert!(md.contains("`branchy`"));
        assert!(!md.contains("`dispatch`"), "got: {md}");
        assert!(!md.contains("`quiet`"), "got: {md}");
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
        assert_eq!(parsed["files"][0]["functions"][0]["name"], "alpha");
    }

    #[test]
    fn directory_mode_aggregates_summary_across_files() {
        // Two files with one function each: the corpus-wide summary
        // should reflect the higher of the two complexities, and the
        // top-N markdown should rank across both files.
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "a.rs", "fn a() {}\n");
        write_file(
            dir.path(),
            "nested/b.rs",
            r#"
fn b(n: i32) -> i32 {
    if n > 0 { 1 } else if n < 0 { -1 } else { 0 }
}
"#,
        );

        let json = ComplexityAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["file_count"], 2);
        assert_eq!(parsed["function_count"], 2);
        // b: 1 + if + else if = 3
        assert_eq!(parsed["summary"]["cyclomatic_max"], 3);

        let md = ComplexityAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        // Top-N row carries the file path so cross-file ranking is
        // unambiguous.
        assert!(md.contains("nested/b.rs:`b`"));
    }

    #[test]
    fn directory_mode_skips_unsupported_extensions_and_gitignored_files() {
        // `.gitignore` should be honoured (the `ignore` walker is
        // gitignore-aware out of the box), and unsupported extensions
        // should be silently skipped.
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "a.rs", "fn a() {}\n");
        write_file(dir.path(), "ignored.rs", "fn b() {}\n");
        write_file(dir.path(), "notes.txt", "not a source file");
        write_file(dir.path(), ".gitignore", "ignored.rs\n");

        // The `ignore` crate honours .gitignore only inside a git repo
        // by default; bootstrap one so the test exercises the gitignore
        // path rather than just the extension filter.
        let status = std::process::Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());

        let json = ComplexityAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["file_count"], 1, "got {parsed}");
        assert_eq!(parsed["files"][0]["file"], "a.rs");
    }

    #[test]
    fn directory_mode_drops_files_without_functions() {
        // Two files: one with a function, one with only a comment. The
        // comment-only file should be dropped to keep the report
        // signal-dense.
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "with_fn.rs", "fn a() {}\n");
        write_file(dir.path(), "empty.rs", "// nothing\n");

        let json = ComplexityAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["file_count"], 1);
        assert_eq!(parsed["files"][0]["file"], "with_fn.rs");
    }

    #[test]
    fn path_filters_apply_to_directory_walks() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "fn prod() {}\n");
        write_file(dir.path(), "tests/lib_test.rs", "fn test_case() {}\n");
        write_file(dir.path(), "src/generated.rs", "fn generated() {}\n");

        let only_tests = ComplexityAnalyzer::new()
            .with_only_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&only_tests).unwrap();
        assert_eq!(parsed["file_count"], 1);
        assert_eq!(parsed["files"][0]["file"], "tests/lib_test.rs");

        let exclude_tests = ComplexityAnalyzer::new()
            .with_exclude_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exclude_tests).unwrap();
        let files: Vec<&str> = parsed["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["file"].as_str().unwrap())
            .collect();
        assert!(files.contains(&"src/lib.rs"));
        assert!(files.contains(&"src/generated.rs"));
        assert!(!files.contains(&"tests/lib_test.rs"));

        let exclude_generated = ComplexityAnalyzer::new()
            .with_exclude_patterns(vec!["generated.rs".to_owned()])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exclude_generated).unwrap();
        let files: Vec<&str> = parsed["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["file"].as_str().unwrap())
            .collect();
        assert!(!files.contains(&"src/generated.rs"));
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
