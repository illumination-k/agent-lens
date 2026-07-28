//! `analyze wrapper` — surface thin forwarding wrappers in source files.
//!
//! Accepts either a single source file or a directory. When the input is a
//! directory the analyzer walks it recursively (respecting `.gitignore`
//! via the `ignore` crate, the same one used by ripgrep), parses every
//! supported file, and reports wrappers grouped by file. Output is JSON by
//! default; the markdown mode emits a compact summary tuned for LLM
//! context windows rather than for humans, in line with the project's
//! "agent-friendly lint" ethos.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use lens_domain::{ReuseMetrics, WrapperFinding};
use serde::Serialize;

use super::runner::{
    FilterConfig, PerFileReport, PerFileShape, delegate_filter_builders, render_report,
};
use super::{AnalyzerError, OutputFormat, SourceFile, SourceLang, read_source};

/// Analyzer entry point. Stateless today; kept as a struct so per-run
/// configuration (filters, thresholds) can be added without breaking the
/// CLI surface.
#[derive(Debug, Default, Clone)]
pub struct WrapperAnalyzer {
    filter: FilterConfig,
}

impl WrapperAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    delegate_filter_builders!(filter);

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let files = self.collect_file_reports(path)?;
        let report = build_report(path, &files);
        render_report(&report, format, || format_markdown(&report))
    }

    /// Resolve `path` to a list of per-file reports. Single-file inputs
    /// produce a one-element vec; directory inputs walk recursively,
    /// honouring `.gitignore`. Files with no findings are dropped so the
    /// output stays signal-dense.
    fn collect_file_reports(&self, path: &Path) -> Result<Vec<FileReport>, AnalyzerError> {
        // Pass 1: produce wrapper findings AND a call-site index for
        // every supported source file. Splitting it from the metric
        // rollup lets reuse metrics see calls in files that themselves
        // contain no wrappers.
        let mut per_files = self
            .filter
            .collect_per_file(path, |sf| self.scan_file(sf))?;
        // Reuse metrics are workspace-wide by construction. A
        // single-file input only sees calls inside that one file, so
        // every cross-file rollup would trivially be 0. To avoid
        // emitting that misleading "0 sites" signal we leave reuse at
        // `None` in single-file mode and only annotate when the input
        // path is a directory.
        if path.is_dir() {
            annotate_reuse(&mut per_files);
        }
        Ok(per_files
            .into_iter()
            .filter_map(PerFile::into_report)
            .collect())
    }

    /// Walk a single file, returning the per-file slice (wrappers +
    /// call sites) used by `collect_file_reports`. Files with neither
    /// a wrapper nor a call site are dropped at the next stage.
    fn scan_file(&self, file: &SourceFile) -> Result<Option<PerFile>, AnalyzerError> {
        let (lang, source) = read_source(&file.path)?;
        let mut findings = run_wrappers(lang, &source).map_err(AnalyzerError::Parse)?;
        self.filter
            .retain_changed(&mut findings, &file.path, |f| (f.start_line, f.end_line));
        // Call references seed the reuse-metrics pass. Every supported
        // language exposes a call index (Rust via `extract_call_sites`,
        // the rest via `extract_call_shapes_with_module`), so wrappers
        // in any language now get reuse metrics in directory mode —
        // previously this was a Rust-only signal.
        let calls = extract_calls(lang, &source).map_err(AnalyzerError::Parse)?;
        if findings.is_empty() && calls.is_empty() {
            return Ok(None);
        }
        Ok(Some(PerFile {
            file: file.display_path.clone(),
            findings,
            calls,
        }))
    }
}

/// Pass-1 row: a file that may carry wrappers, call references, or both.
/// Files with neither are dropped before the row is built.
struct PerFile {
    file: String,
    findings: Vec<WrapperFinding>,
    calls: Vec<CallRef>,
}

/// Language-neutral projection of a call site: the bare callee name
/// (last path segment) and the identity of the function the call is
/// written inside. Both are keyed the same way regardless of source
/// language so the reuse rollup can merge call indices across a mixed
/// directory.
struct CallRef {
    /// Last path segment of the callee (`foo` for `a::b::foo()`), or
    /// `None` when the callee isn't a plain named path.
    callee: Option<String>,
    /// Identity of the enclosing function, in the same qualified form
    /// the wrapper detector uses for `WrapperFinding::name` (`Owner::m`
    /// or a bare name). `None` for calls at module scope.
    caller: Option<String>,
}

/// Extract the call index for `source` as language-neutral [`CallRef`]s.
///
/// Rust keeps its dedicated `extract_call_sites` visitor (whose
/// `caller_name` is already the bare-qualified form the wrapper detector
/// uses). The other adapters go through `extract_call_shapes_with_module`
/// with an empty module prefix, which makes `caller_qualified_name`
/// collapse to the same `Owner::method` / bare-name shape as their
/// wrapper findings — so the self-reference filter and unique-caller
/// counting line up across languages.
fn extract_calls(lang: SourceLang, source: &str) -> Result<Vec<CallRef>, BoxedError> {
    match lang {
        SourceLang::Rust => {
            let sites = lens_rust::extract_call_sites(source).map_err(boxed)?;
            Ok(sites
                .into_iter()
                .map(|s| CallRef {
                    callee: s.callee_name,
                    caller: s.caller_name,
                })
                .collect())
        }
        SourceLang::TypeScript(dialect) => {
            let shapes =
                lens_ts::extract_call_shapes_with_module(source, dialect, "").map_err(boxed)?;
            Ok(shapes.into_iter().map(CallRef::from_shape).collect())
        }
        SourceLang::Python => {
            let shapes = lens_py::extract_call_shapes_with_module(source, "").map_err(boxed)?;
            Ok(shapes.into_iter().map(CallRef::from_shape).collect())
        }
        SourceLang::Go => {
            let shapes = lens_golang::extract_call_shapes_with_module(source, "").map_err(boxed)?;
            Ok(shapes.into_iter().map(CallRef::from_shape).collect())
        }
    }
}

impl CallRef {
    fn from_shape(shape: lens_domain::CallShape) -> Self {
        Self {
            callee: shape.callee_name().map(str::to_owned),
            caller: shape.caller_qualified_name().map(str::to_owned),
        }
    }
}

fn boxed(e: impl std::error::Error + Send + Sync + 'static) -> BoxedError {
    Box::new(e) as BoxedError
}

impl PerFile {
    /// Drop the call-site auxiliary data and convert into the
    /// presentation-side [`FileReport`]. Files whose findings list ended
    /// up empty (only call sites, no wrappers) are filtered out — the
    /// report exists to surface wrappers, not raw call indices.
    fn into_report(self) -> Option<FileReport> {
        if self.findings.is_empty() {
            return None;
        }
        Some(FileReport {
            file: self.file,
            findings: self.findings,
        })
    }
}

/// Walk every wrapper finding across `per_files` and populate its
/// [`ReuseMetrics`] from the merged call-site index. Every supported
/// language contributes a call index, so all findings are annotated in
/// directory mode.
fn annotate_reuse(per_files: &mut [PerFile]) {
    // callee_name -> Vec<(file_index, caller_name)>. Owned keys so the
    // index doesn't keep `per_files` borrowed when we re-walk to write
    // back the metrics.
    let mut index: HashMap<String, Vec<(usize, Option<String>)>> = HashMap::new();
    for (idx, per) in per_files.iter().enumerate() {
        for site in &per.calls {
            let Some(name) = site.callee.as_deref() else {
                continue;
            };
            index
                .entry(name.to_owned())
                .or_default()
                .push((idx, site.caller.clone()));
        }
    }
    let file_paths: Vec<String> = per_files.iter().map(|p| p.file.clone()).collect();
    for (idx, per) in per_files.iter_mut().enumerate() {
        let host_file = file_paths[idx].as_str();
        for finding in &mut per.findings {
            let last_segment = name_last_segment(&finding.name);
            let buckets = index.get(last_segment).cloned().unwrap_or_default();
            // Drop self-references: a call to the wrapper from
            // *inside* the wrapper itself doesn't represent reuse,
            // it's the wrapper's own body. (Recursion on a trivial
            // forwarder is unusual but possible.)
            let buckets: Vec<_> = buckets
                .into_iter()
                .filter(|(_, caller)| caller.as_deref() != Some(finding.name.as_str()))
                .collect();
            let call_sites = buckets.len();
            let same_file_only = buckets.iter().all(|(file_idx, _)| {
                file_paths.get(*file_idx).map(String::as_str) == Some(host_file)
            });
            // Distinct callers: pair each call site with `(file, caller)`.
            // Buckets with `caller = None` still count as one
            // anonymous caller per file (top-level references in a
            // `const` initialiser, etc.), so a wrapper used only at
            // module scope doesn't mis-report 0 callers.
            let callers: std::collections::HashSet<(usize, Option<String>)> =
                buckets.iter().cloned().collect();
            finding.reuse = Some(ReuseMetrics {
                call_sites,
                unique_callers: callers.len(),
                same_file_only,
            });
        }
    }
}

/// Strip qualifier prefixes from a wrapper's `name` to get the bare
/// last segment that appears at every call site (`Service::handle` →
/// `handle`).
fn name_last_segment(name: &str) -> &str {
    name.rsplit_once("::").map_or(name, |(_, last)| last)
}

type BoxedError = Box<dyn std::error::Error + Send + Sync>;

fn run_wrappers(lang: SourceLang, source: &str) -> Result<Vec<WrapperFinding>, BoxedError> {
    super::dispatch_lens!(lang, source, find_wrappers)
}

/// Per-file slice of the report. Owns the display path so directory mode
/// can attach a path relative to the walk root without storing the original
/// `PathBuf`.
#[derive(Debug)]
struct FileReport {
    file: String,
    findings: Vec<WrapperFinding>,
}

/// Names wrapper's two report fields for the shared per-file report.
#[derive(Debug)]
struct WrapperShape;

impl PerFileShape for WrapperShape {
    const COUNT_FIELD: &'static str = "wrapper_count";
    const ITEMS_FIELD: &'static str = "wrappers";
}

type Report<'a> = PerFileReport<'a, WrapperShape, WrapperView<'a>>;

fn build_report<'a>(path: &Path, files: &'a [FileReport]) -> Report<'a> {
    let views = files
        .iter()
        .map(|f| {
            super::runner::FileView::new(
                f.file.as_str(),
                f.findings.iter().map(WrapperView::from).collect(),
            )
        })
        .collect();
    PerFileReport::new(path, views)
}

#[derive(Debug, Serialize)]
struct WrapperView<'a> {
    name: &'a str,
    start_line: usize,
    end_line: usize,
    callee: &'a str,
    adapters: &'a [String],
    statement_count: usize,
    /// Workspace-wide reuse metrics. `null` when the finding came from
    /// a single-file run (the call-site universe was not enumerated).
    #[serde(skip_serializing_if = "Option::is_none")]
    reuse: Option<ReuseView>,
}

/// JSON-facing mirror of [`ReuseMetrics`]. Defined locally so the
/// output schema stays under this analyzer's control even if the
/// domain type grows fields.
#[derive(Debug, Serialize)]
struct ReuseView {
    call_sites: usize,
    unique_callers: usize,
    same_file_only: bool,
}

impl<'a> From<&'a WrapperFinding> for WrapperView<'a> {
    fn from(f: &'a WrapperFinding) -> Self {
        Self {
            name: f.name.as_str(),
            start_line: f.start_line,
            end_line: f.end_line,
            callee: f.callee.as_str(),
            adapters: &f.adapters,
            statement_count: f.statement_count,
            reuse: f.reuse.as_ref().map(|r| ReuseView {
                call_sites: r.call_sites,
                unique_callers: r.unique_callers,
                same_file_only: r.same_file_only,
            }),
        }
    }
}

fn format_markdown(report: &Report<'_>) -> String {
    let mut out = format!(
        "# Wrapper report: {} ({} file(s), {} wrapper(s))\n",
        report.root(),
        report.file_count(),
        report.item_count(),
    );
    if report.item_count() == 0 {
        out.push_str("\n_No thin forwarding wrappers found._\n");
        return out;
    }
    for file in report.files() {
        // writeln! into a String cannot fail; the result is swallowed
        // deliberately rather than unwrapped to satisfy the workspace's
        // `unwrap_used` lint.
        let _ = writeln!(out, "\n## {} ({} wrapper(s))", file.file(), file.count());
        for w in file.items() {
            // Body shape: callee chain plus optional adapter suffix.
            let body = if w.adapters.is_empty() {
                format!("-> {}", w.callee)
            } else {
                format!("-> {} [via {}]", w.callee, w.adapters.join(""))
            };
            // Reuse suffix: only attached when the finding had reuse
            // metrics (directory mode). Kept terse so the line stays
            // scannable at agent-context density.
            let suffix = match &w.reuse {
                Some(r) => format!(
                    "  \u{2022} {} site(s), {} caller(s), {}",
                    r.call_sites,
                    r.unique_callers,
                    if r.same_file_only {
                        "same-file"
                    } else {
                        "cross-file"
                    },
                ),
                None => String::new(),
            };
            let _ = writeln!(
                out,
                "- `{}` (L{}-{}) {}{}",
                w.name, w.start_line, w.end_line, body, suffix,
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};

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
    fn missing_file_surfaces_not_found_error() {
        let err = WrapperAnalyzer::new()
            .analyze(
                Path::new("/definitely/does/not/exist.rs"),
                OutputFormat::Json,
            )
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::PathNotFound { .. }), "{err:?}");
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

    #[test]
    fn directory_mode_populates_reuse_metrics_across_files() {
        // Two files: `wrap.rs` defines `render` (a wrapper). `caller.rs`
        // calls it from `consumer`. The wrapper itself is not called
        // from inside `wrap.rs`, so reuse spans one cross-file caller.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "wrap.rs",
            "fn render(x: &str) -> String { internal_render(x) }\n",
        );
        write_file(
            dir.path(),
            "caller.rs",
            "fn consumer() { let _ = render(\"hi\"); }\n",
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let wrappers = parsed["files"][0]["wrappers"].as_array().unwrap();
        let render = wrappers
            .iter()
            .find(|w| w["name"] == "render")
            .expect("render wrapper missing");
        assert_eq!(render["statement_count"], 1);
        let reuse = &render["reuse"];
        assert_eq!(reuse["call_sites"], 1);
        assert_eq!(reuse["unique_callers"], 1);
        assert_eq!(reuse["same_file_only"], false);
    }

    #[test]
    fn directory_mode_populates_reuse_metrics_for_python() {
        // The reuse pass is no longer Rust-only: a Python wrapper
        // called from another file must report cross-file reuse.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "wrap.py",
            "def render(x):\n    return internal_render(x)\n",
        );
        write_file(
            dir.path(),
            "caller.py",
            "def consumer():\n    return render(\"hi\")\n",
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let render = parsed["files"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|f| f["wrappers"].as_array().unwrap())
            .find(|w| w["name"] == "render")
            .expect("render wrapper missing");
        let reuse = &render["reuse"];
        assert_eq!(reuse["call_sites"], 1, "got {parsed}");
        assert_eq!(reuse["unique_callers"], 1, "got {parsed}");
        assert_eq!(reuse["same_file_only"], false, "got {parsed}");
    }

    #[test]
    fn directory_mode_populates_reuse_metrics_for_go() {
        // Same cross-language reuse guarantee for Go.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "wrap.go",
            "package p\n\nfunc Render(x int) int { return internalRender(x) }\n",
        );
        write_file(
            dir.path(),
            "caller.go",
            "package p\n\nfunc consumer() int { return Render(1) }\n",
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let render = parsed["files"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|f| f["wrappers"].as_array().unwrap())
            .find(|w| w["name"] == "Render")
            .expect("Render wrapper missing");
        let reuse = &render["reuse"];
        assert_eq!(reuse["call_sites"], 1, "got {parsed}");
        assert_eq!(reuse["unique_callers"], 1, "got {parsed}");
        assert_eq!(reuse["same_file_only"], false, "got {parsed}");
    }

    #[test]
    fn directory_mode_marks_same_file_only_when_caller_is_local() {
        // A wrapper used only inside its own file: `same_file_only` is
        // true and the caller count is 1. This is the canonical "low
        // reuse, low blast radius" finding the agent should treat as
        // safe to inline.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "lib.rs",
            r#"
fn render(x: &str) -> String { internal_render(x) }
fn consumer() { let _ = render("hi"); }
"#,
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let render = parsed["files"][0]["wrappers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["name"] == "render")
            .expect("render wrapper missing");
        let reuse = &render["reuse"];
        assert_eq!(reuse["call_sites"], 1);
        assert_eq!(reuse["unique_callers"], 1);
        assert_eq!(reuse["same_file_only"], true);
    }

    #[test]
    fn directory_mode_zero_call_sites_for_unused_wrapper() {
        // A wrapper that nothing else in the tree calls: `call_sites`
        // is 0. `same_file_only` is `true` by convention (the empty
        // call set trivially satisfies "all calls are local"), and the
        // agent reads it together with the count rather than in
        // isolation.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "wrap.rs",
            "fn unused(x: &str) -> String { internal_render(x) }\n",
        );

        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let reuse = &parsed["files"][0]["wrappers"][0]["reuse"];
        assert_eq!(reuse["call_sites"], 0);
        assert_eq!(reuse["unique_callers"], 0);
        assert_eq!(reuse["same_file_only"], true);
    }

    #[test]
    fn single_file_mode_leaves_reuse_unset() {
        // With a single source file as the input there is no
        // workspace to enumerate calls across, so `reuse` is omitted
        // from the JSON entirely (the field is `Option` and skipped
        // when None).
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            "fn render(x: &str) -> String { internal_render(x) }\n",
        );
        let json = WrapperAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let render = &parsed["files"][0]["wrappers"][0];
        assert_eq!(render["statement_count"], 1);
        assert!(render.get("reuse").is_none_or(|v| v.is_null()));
    }

    #[test]
    fn directory_mode_excludes_self_recursive_calls_from_reuse() {
        // A pathological wrapper that recurses on itself shouldn't
        // double-count its own body as reuse. The recursive call is
        // dropped from the bucket so call_sites stays 0.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "wrap.rs",
            "fn render(x: &str) -> String { render(x) }\n",
        );
        let json = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let reuse = &parsed["files"][0]["wrappers"][0]["reuse"];
        assert_eq!(reuse["call_sites"], 0);
    }

    #[test]
    fn markdown_directory_report_renders_reuse_suffix() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "wrap.rs",
            "fn render(x: &str) -> String { internal_render(x) }\n",
        );
        write_file(
            dir.path(),
            "caller.rs",
            "fn consumer() { let _ = render(\"hi\"); }\n",
        );

        let md = WrapperAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        // The reuse rollup is rendered as a terse trailing chip after
        // the body so the line stays scannable. Format details (bullet
        // glyph, exact wording) may shift; check for the count and the
        // cross-file marker.
        assert!(md.contains("1 site"), "missing reuse count: {md}");
        assert!(md.contains("cross-file"), "missing locality: {md}");
    }
}
