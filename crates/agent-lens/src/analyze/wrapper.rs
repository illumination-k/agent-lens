//! `analyze wrapper` — surface thin forwarding wrappers in source files.
//!
//! Accepts either a single source file or a directory. When the input is a
//! directory the analyzer walks it recursively (respecting `.gitignore`
//! via the `ignore` crate, the same one used by ripgrep), parses every
//! supported file, and reports wrappers grouped by file. Output is JSON by
//! default; the markdown mode emits a compact summary tuned for LLM
//! context windows rather than for humans, in line with the project's
//! "agent-friendly lint" ethos.

use std::fmt::Write as _;
use std::path::Path;

use lens_domain::WrapperFinding;
use serde::Serialize;

use super::{
    AnalyzePathFilter, AnalyzerError, OutputFormat, SourceFile, SourceLang, changed_line_ranges,
    collect_source_files, overlaps_any, read_source,
};

/// Analyzer entry point. Stateless today; kept as a struct so per-run
/// configuration (filters, thresholds) can be added without breaking the
/// CLI surface.
#[derive(Debug, Default, Clone)]
pub struct WrapperAnalyzer {
    diff_only: bool,
    path_filter: AnalyzePathFilter,
}

impl WrapperAnalyzer {
    pub fn new() -> Self {
        Self {
            diff_only: false,
            path_filter: AnalyzePathFilter::new(),
        }
    }

    /// Restrict reports to wrappers intersecting an unstaged changed line
    /// in `git diff -U0`.
    pub fn with_diff_only(mut self, diff_only: bool) -> Self {
        self.diff_only = diff_only;
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
            OutputFormat::Md => Ok(format_markdown(&report)),
        }
    }

    /// Resolve `path` to a list of per-file reports. Single-file inputs
    /// produce a one-element vec; directory inputs walk recursively,
    /// honouring `.gitignore`. Files with no findings are dropped so the
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
    /// wrappers (after filtering), so empty entries don't pollute the
    /// directory-mode report.
    fn analyze_file(&self, file: &SourceFile) -> Result<Option<FileReport>, AnalyzerError> {
        let (lang, source) = read_source(&file.path)?;
        let mut findings = run_wrappers(lang, &source).map_err(AnalyzerError::Parse)?;
        if self.diff_only {
            let changed = changed_line_ranges(&file.path);
            findings.retain(|f| overlaps_any(f.start_line, f.end_line, &changed));
        }
        if findings.is_empty() {
            return Ok(None);
        }
        Ok(Some(FileReport {
            file: file.display_path.clone(),
            findings,
        }))
    }
}

type BoxedError = Box<dyn std::error::Error + Send + Sync>;

fn run_wrappers(lang: SourceLang, source: &str) -> Result<Vec<WrapperFinding>, BoxedError> {
    match lang {
        SourceLang::Rust => lens_rust::find_wrappers(source).map_err(|e| Box::new(e) as BoxedError),
        SourceLang::TypeScript(dialect) => {
            lens_ts::find_wrappers(source, dialect).map_err(|e| Box::new(e) as BoxedError)
        }
        SourceLang::Python => lens_py::find_wrappers(source).map_err(|e| Box::new(e) as BoxedError),
        // Wrapper detection for Go is not implemented yet; the language
        // is only wired up for similarity. Returning an empty finding
        // list keeps directory walks running across mixed-language
        // repos.
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
    findings: Vec<WrapperFinding>,
}

#[derive(Debug, Serialize)]
struct Report<'a> {
    /// Input path: a single source file, or the root directory walked.
    root: String,
    file_count: usize,
    wrapper_count: usize,
    files: Vec<FileView<'a>>,
}

impl<'a> Report<'a> {
    fn new(path: &Path, files: &'a [FileReport]) -> Self {
        let wrapper_count = files.iter().map(|f| f.findings.len()).sum();
        Self {
            root: path.display().to_string(),
            file_count: files.len(),
            wrapper_count,
            files: files.iter().map(FileView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct FileView<'a> {
    file: &'a str,
    wrapper_count: usize,
    wrappers: Vec<WrapperView<'a>>,
}

impl<'a> From<&'a FileReport> for FileView<'a> {
    fn from(f: &'a FileReport) -> Self {
        Self {
            file: f.file.as_str(),
            wrapper_count: f.findings.len(),
            wrappers: f.findings.iter().map(WrapperView::from).collect(),
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
        "# Wrapper report: {} ({} file(s), {} wrapper(s))\n",
        report.root, report.file_count, report.wrapper_count,
    );
    if report.wrapper_count == 0 {
        out.push_str("\n_No thin forwarding wrappers found._\n");
        return out;
    }
    for file in &report.files {
        // writeln! into a String cannot fail; the result is swallowed
        // deliberately rather than unwrapped to satisfy the workspace's
        // `unwrap_used` lint.
        let _ = writeln!(
            out,
            "\n## {} ({} wrapper(s))",
            file.file, file.wrapper_count
        );
        for w in &file.wrappers {
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
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
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
        assert_eq!(parsed["file_count"], 1);
        let files = parsed["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        let wrappers = files[0]["wrappers"].as_array().unwrap();
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
        let adapters = parsed["files"][0]["wrappers"][0]["adapters"]
            .as_array()
            .unwrap();
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
    fn python_wrapper_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let src = "
def render(x):
    return internal_render(x)

def meaningful(x):
    y = x + 1
    return y * 2
";
        let file = write_file(dir.path(), "lib.py", src);
        let json = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["wrapper_count"], 1);
        let wrapper = &parsed["files"][0]["wrappers"][0];
        assert_eq!(wrapper["name"], "render");
        assert_eq!(wrapper["callee"], "internal_render");
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
        assert_eq!(parsed["files"][0]["wrappers"][0]["name"], "render");
    }

    #[test]
    fn directory_mode_groups_wrappers_per_file() {
        // Two wrappers split across two files: only visible to the
        // analyzer once it walks the directory. The output shape is
        // grouped per file so the agent can attribute each finding.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.rs",
            "fn render(x: &str) -> String { internal_render(x) }\n",
        );
        write_file(
            dir.path(),
            "nested/b.rs",
            "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n",
        );
        // A file with no wrappers should not appear in the report at all.
        write_file(
            dir.path(),
            "noop.rs",
            r#"
fn meaningful(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs { total += *x; }
    total
}
"#,
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["wrapper_count"], 2);
        assert_eq!(parsed["file_count"], 2);
        let files = parsed["files"].as_array().unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f["file"].as_str().unwrap()).collect();
        assert!(paths.contains(&"a.rs"), "got {paths:?}");
        assert!(paths.contains(&"nested/b.rs"), "got {paths:?}");
        assert!(!paths.contains(&"noop.rs"), "got {paths:?}");
    }

    #[test]
    fn directory_mode_skips_unsupported_extensions_and_gitignored_files() {
        // `.gitignore` should be honoured (the `ignore` walker is
        // gitignore-aware out of the box), and unsupported extensions
        // should be silently skipped.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.rs",
            "fn render(x: &str) -> String { internal_render(x) }\n",
        );
        write_file(
            dir.path(),
            "ignored.rs",
            "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n",
        );
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

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["wrapper_count"], 1, "got {parsed}");
        assert_eq!(parsed["file_count"], 1, "got {parsed}");
        assert_eq!(parsed["files"][0]["file"], "a.rs");
    }

    #[test]
    fn directory_mode_markdown_renders_per_file_sections() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.rs",
            "fn render(x: &str) -> String { internal_render(x) }\n",
        );
        write_file(
            dir.path(),
            "nested/b.rs",
            "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n",
        );

        let md = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Wrapper report"));
        assert!(md.contains("2 file(s)"));
        assert!(md.contains("2 wrapper(s)"));
        assert!(md.contains("## a.rs"));
        assert!(md.contains("## nested/b.rs"));
        assert!(md.contains("render"));
        assert!(md.contains("shim"));
    }

    #[test]
    fn path_filters_apply_to_directory_walks() {
        let dir = tempfile::tempdir().unwrap();
        let wrapper = "fn render(x: &str) -> String { internal_render(x) }\n";
        write_file(dir.path(), "src/lib.rs", wrapper);
        write_file(dir.path(), "tests/lib_test.rs", wrapper);
        write_file(dir.path(), "src/generated.rs", wrapper);

        let only_tests = WrapperAnalyzer::new()
            .with_only_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&only_tests).unwrap();
        assert_eq!(parsed["file_count"], 1);
        assert_eq!(parsed["files"][0]["file"], "tests/lib_test.rs");

        let exclude_tests = WrapperAnalyzer::new()
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
        assert!(!files.contains(&"tests/lib_test.rs"));

        let exclude_generated = WrapperAnalyzer::new()
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
}
