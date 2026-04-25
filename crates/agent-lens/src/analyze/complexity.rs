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
use std::path::{Path, PathBuf};

use lens_domain::FunctionComplexity;
use serde::Serialize;

use super::OutputFormat;

/// Errors raised while running the complexity analyzer.
#[derive(Debug)]
pub enum ComplexityAnalyzerError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    UnsupportedExtension {
        path: PathBuf,
    },
    Parse(Box<dyn std::error::Error + Send + Sync>),
    Serialize(serde_json::Error),
}

impl std::fmt::Display for ComplexityAnalyzerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::UnsupportedExtension { path } => write!(
                f,
                "unsupported file extension for complexity analysis: {}",
                path.display()
            ),
            Self::Parse(e) => write!(f, "failed to parse source: {e}"),
            Self::Serialize(e) => write!(f, "failed to serialize report: {e}"),
        }
    }
}

impl std::error::Error for ComplexityAnalyzerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse(e) => Some(e.as_ref()),
            Self::Serialize(e) => Some(e),
            Self::UnsupportedExtension { .. } => None,
        }
    }
}

/// Languages the analyzer knows how to handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
}

impl Language {
    fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            _ => None,
        }
    }

    fn extract(self, source: &str) -> Result<Vec<FunctionComplexity>, ComplexityAnalyzerError> {
        match self {
            Self::Rust => lens_rust::extract_complexity_units(source)
                .map_err(|e| ComplexityAnalyzerError::Parse(Box::new(e))),
        }
    }
}

/// Analyzer entry point. Stateless today; kept as a struct so per-run
/// configuration (filters, thresholds) can be added without breaking the
/// CLI surface.
#[derive(Debug, Default, Clone, Copy)]
pub struct ComplexityAnalyzer;

impl ComplexityAnalyzer {
    pub fn new() -> Self {
        Self
    }

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(
        &self,
        path: &Path,
        format: OutputFormat,
    ) -> Result<String, ComplexityAnalyzerError> {
        let language = path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(Language::from_extension)
            .ok_or_else(|| ComplexityAnalyzerError::UnsupportedExtension {
                path: path.to_path_buf(),
            })?;

        let source =
            std::fs::read_to_string(path).map_err(|source| ComplexityAnalyzerError::Io {
                path: path.to_path_buf(),
                source,
            })?;

        let functions = language.extract(&source)?;
        let report = Report::new(path, &functions);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).map_err(ComplexityAnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&report)),
        }
    }
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
    let n = sorted.len();
    let idx = ((p as usize * n).div_ceil(100))
        .saturating_sub(1)
        .min(n - 1);
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
    let s = &report.summary;
    let mi_min = match s.maintainability_index_min {
        Some(v) => format!("{v:.1}"),
        None => "n/a".to_owned(),
    };
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
        mi_min,
    );

    // Worst N by cyclomatic, ties broken by cognitive then by line.
    let mut indexed: Vec<&FunctionView<'_>> = report.functions.iter().collect();
    indexed.sort_by(|a, b| {
        b.cyclomatic
            .cmp(&a.cyclomatic)
            .then_with(|| b.cognitive.cmp(&a.cognitive))
            .then_with(|| a.start_line.cmp(&b.start_line))
    });

    let _ = writeln!(out, "\n## Top {TOP_N} by cyclomatic");
    for fv in indexed.iter().take(TOP_N) {
        let mi = match fv.maintainability_index {
            Some(v) => format!("{v:.0}"),
            None => "n/a".to_owned(),
        };
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
            mi,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

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
        let src = r#"
fn a() {}
fn b(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }
fn c(n: i32) -> i32 {
    match n { 0 => 0, 1 => 1, 2 => 2, _ => 3 }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let md = ComplexityAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Complexity report"));
        assert!(md.contains("Summary"));
        assert!(md.contains("Top 5 by cyclomatic"));
        // `c` has the highest CC (4) and should appear before `b` (CC 2).
        let pos_c = md.find("`c`").unwrap();
        let pos_b = md.find("`b`").unwrap();
        assert!(pos_c < pos_b, "expected highest-CC function listed first");
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
        assert!(matches!(
            err,
            ComplexityAnalyzerError::UnsupportedExtension { .. }
        ));
    }

    #[test]
    fn missing_file_surfaces_io_error() {
        let err = ComplexityAnalyzer::new()
            .analyze(
                Path::new("/definitely/does/not/exist.rs"),
                OutputFormat::Json,
            )
            .unwrap_err();
        assert!(matches!(err, ComplexityAnalyzerError::Io { .. }));
    }

    #[test]
    fn invalid_rust_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "broken.rs", "fn ??? {");
        let err = ComplexityAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, ComplexityAnalyzerError::Parse(_)));
    }

    #[test]
    fn percentile_picks_nearest_rank() {
        let sorted = [1, 2, 3, 4, 5];
        assert_eq!(percentile(&sorted, 100), 5);
        assert_eq!(percentile(&sorted, 50), 3);
        assert_eq!(percentile(&sorted, 0), 1);
    }

    #[test]
    fn percentile_on_empty_slice_returns_zero() {
        assert_eq!(percentile(&[], 50), 0);
    }
}
