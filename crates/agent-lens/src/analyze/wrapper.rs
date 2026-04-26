//! `analyze wrapper` — surface thin forwarding wrappers in a Rust source
//! file.
//!
//! Today the analyzer is single-file, single-language: pass it a `.rs` path
//! and it reports every function whose body, after stripping a short chain
//! of trivial adapters (`?`, `.unwrap()`, `.into()`, …), is just a
//! forwarding call to another function. Output is JSON by default; the
//! markdown mode emits a compact summary tuned for LLM context windows
//! rather than for humans, in line with the project's "agent-friendly lint"
//! ethos.

use std::fmt::Write as _;
use std::path::Path;

use lens_domain::WrapperFinding;
use serde::Serialize;

use super::{AnalyzerError, LineRange, OutputFormat, SourceLang, changed_line_ranges, read_source};

/// Analyzer entry point. Stateless today; kept as a struct so per-run
/// configuration (filters, thresholds) can be added without breaking the
/// CLI surface.
#[derive(Debug, Default, Clone, Copy)]
pub struct WrapperAnalyzer {
    diff_only: bool,
}

impl WrapperAnalyzer {
    pub fn new() -> Self {
        Self { diff_only: false }
    }

    /// Restrict reports to wrappers intersecting an unstaged changed line
    /// in `git diff -U0`.
    pub fn with_diff_only(mut self, diff_only: bool) -> Self {
        self.diff_only = diff_only;
        self
    }

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let (lang, source) = read_source(path)?;
        let mut findings = run_wrappers(lang, &source)?;
        if self.diff_only {
            let changed = changed_line_ranges(path);
            findings.retain(|f| overlaps_any(f.start_line, f.end_line, &changed));
        }
        let report = Report::new(path, &findings);
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

fn run_wrappers(lang: SourceLang, source: &str) -> Result<Vec<WrapperFinding>, AnalyzerError> {
    match lang {
        SourceLang::Rust => {
            lens_rust::find_wrappers(source).map_err(|e| AnalyzerError::Parse(Box::new(e)))
        }
        SourceLang::TypeScript => {
            lens_ts::find_wrappers(source).map_err(|e| AnalyzerError::Parse(Box::new(e)))
        }
    }
}

#[derive(Debug, Serialize)]
struct Report<'a> {
    file: String,
    wrapper_count: usize,
    wrappers: Vec<WrapperView<'a>>,
}

impl<'a> Report<'a> {
    fn new(path: &Path, findings: &'a [WrapperFinding]) -> Self {
        Self {
            file: path.display().to_string(),
            wrapper_count: findings.len(),
            wrappers: findings.iter().map(WrapperView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct WrapperView<'a> {
    name: &'a str,
    start_line: usize,
    end_line: usize,
    callee: &'a str,
    adapters: &'a [String],
}

impl<'a> From<&'a WrapperFinding> for WrapperView<'a> {
    fn from(f: &'a WrapperFinding) -> Self {
        Self {
            name: f.name.as_str(),
            start_line: f.start_line,
            end_line: f.end_line,
            callee: f.callee.as_str(),
            adapters: &f.adapters,
        }
    }
}

fn format_markdown(report: &Report<'_>) -> String {
    let mut out = format!(
        "# Wrapper report: {} ({} wrapper(s))\n",
        report.file, report.wrapper_count,
    );
    if report.wrappers.is_empty() {
        out.push_str("\n_No thin forwarding wrappers found._\n");
        return out;
    }
    for w in &report.wrappers {
        // writeln! into a String cannot fail; the result is swallowed
        // deliberately rather than unwrapped to satisfy the workspace's
        // `unwrap_used` lint.
        if w.adapters.is_empty() {
            let _ = writeln!(
                out,
                "- `{}` (L{}-{}) -> {}",
                w.name, w.start_line, w.end_line, w.callee,
            );
        } else {
            let _ = writeln!(
                out,
                "- `{}` (L{}-{}) -> {} [via {}]",
                w.name,
                w.start_line,
                w.end_line,
                w.callee,
                w.adapters.join(""),
            );
        }
    }
    out
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
    fn json_report_lists_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn render(x: &str) -> String { internal_render(x) }
fn meaningful(x: i32) -> i32 { let y = x + 1; y * 2 }
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let json = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["wrapper_count"], 1);
        let wrappers = parsed["wrappers"].as_array().unwrap();
        assert_eq!(wrappers[0]["name"], "render");
        assert_eq!(wrappers[0]["callee"], "internal_render");
        let names: Vec<&str> = wrappers
            .iter()
            .map(|w| w["name"].as_str().unwrap())
            .collect();
        assert!(!names.contains(&"meaningful"));
    }

    #[test]
    fn json_report_includes_adapter_chain() {
        let dir = tempfile::tempdir().unwrap();
        let src = "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n";
        let file = write_file(dir.path(), "lib.rs", src);
        let json = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let adapters = parsed["wrappers"][0]["adapters"].as_array().unwrap();
        let joined: String = adapters
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>()
            .join("");
        assert!(joined.contains(".unwrap()"));
        assert!(joined.contains(".into()"));
    }

    #[test]
    fn markdown_report_lists_wrappers_and_adapter_chain() {
        let dir = tempfile::tempdir().unwrap();
        let src = "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n";
        let file = write_file(dir.path(), "lib.rs", src);
        let md = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Wrapper report"));
        assert!(md.contains("shim"));
        assert!(md.contains("compute"));
        assert!(md.contains("via"));
        assert!(md.contains(".unwrap()"));
        assert!(md.contains(".into()"));
    }

    #[test]
    fn empty_report_when_no_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
fn alpha(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs { total += *x; }
    total
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let md = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("No thin forwarding wrappers"));
    }

    #[test]
    fn unknown_extension_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "notes.txt", "hello");
        let err = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::UnsupportedExtension { .. }));
    }

    #[test]
    fn missing_file_surfaces_io_error() {
        let err = WrapperAnalyzer::new()
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
        let err = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::Parse(_)));
    }

    #[test]
    fn diff_only_filters_to_changed_wrapper() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
fn render(x: &str) -> String { internal_render(x) }
fn passthrough(x: i32) -> i32 { compute(x) }
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
fn render(x: &str) -> String { internal_render(x).into() }
fn passthrough(x: i32) -> i32 { compute(x) }
"#,
        );
        let json = WrapperAnalyzer::new()
            .with_diff_only(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["wrapper_count"], 1);
        assert_eq!(parsed["wrappers"][0]["name"], "render");
    }

    fn run_git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {}", dir.display());
    }
}
