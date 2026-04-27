//! `analyze cohesion` — surface LCOM4 cohesion units.
//!
//! Accepts either a single source file or a directory. When the input is a
//! directory the analyzer walks it recursively (respecting `.gitignore`
//! via the `ignore` crate, the same one used by ripgrep), parses every
//! supported file, and groups findings per file. Output is JSON by default;
//! the markdown mode emits a compact summary tuned for LLM context windows
//! rather than for humans, in line with the project's "agent-friendly lint"
//! ethos.

use std::fmt::Write as _;
use std::path::Path;

use ignore::WalkBuilder;
use lens_domain::{CohesionUnit, CohesionUnitKind};
use serde::Serialize;

use super::{
    AnalyzePathFilter, AnalyzerError, CompiledPathFilter, LineRange, OutputFormat, SourceLang,
    changed_line_ranges, format_optional_f64, read_source,
};

/// Analyzer entry point. Stateless today; kept as a struct so per-run
/// configuration (filters, thresholds) can be added without breaking the
/// CLI surface.
#[derive(Debug, Default, Clone)]
pub struct CohesionAnalyzer {
    diff_only: bool,
    path_filter: AnalyzePathFilter,
}

impl CohesionAnalyzer {
    pub fn new() -> Self {
        Self {
            diff_only: false,
            path_filter: AnalyzePathFilter::new(),
        }
    }

    /// Restrict reports to cohesion units whose `impl` range intersects
    /// an unstaged changed line in `git diff -U0`.
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
    /// honouring `.gitignore`. Files with no units are dropped so the
    /// output stays signal-dense.
    fn collect_file_reports(&self, path: &Path) -> Result<Vec<FileReport>, AnalyzerError> {
        let filter = self.path_filter.compile(path)?;
        if path.is_dir() {
            self.collect_directory(path, &filter)
        } else if filter.includes_path(path) {
            Ok(self.analyze_file(path, None)?.into_iter().collect())
        } else {
            Ok(Vec::new())
        }
    }

    fn collect_directory(
        &self,
        root: &Path,
        filter: &CompiledPathFilter,
    ) -> Result<Vec<FileReport>, AnalyzerError> {
        let mut out = Vec::new();
        for entry in WalkBuilder::new(root).build() {
            let entry = entry.map_err(|e| AnalyzerError::Io {
                path: root.to_path_buf(),
                source: std::io::Error::other(e),
            })?;
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let p = entry.path();
            if !filter.includes_path(p) {
                continue;
            }
            if SourceLang::from_path(p).is_none() {
                continue;
            }
            // Display paths relative to the walk root so per-file entries
            // stay readable when the analyzer is pointed at a deep
            // directory.
            let rel = p
                .strip_prefix(root)
                .unwrap_or(p)
                .display()
                .to_string()
                .replace('\\', "/");
            if let Some(report) = self.analyze_file(p, Some(rel))? {
                out.push(report);
            }
        }
        Ok(out)
    }

    /// Analyze a single file. Returns `None` when the file has no
    /// units (after filtering), so empty entries don't pollute the
    /// directory-mode report.
    fn analyze_file(
        &self,
        path: &Path,
        rel_override: Option<String>,
    ) -> Result<Option<FileReport>, AnalyzerError> {
        let (lang, source) = read_source(path)?;
        let mut units = extract_units(lang, &source).map_err(AnalyzerError::Parse)?;
        if self.diff_only {
            let changed = changed_line_ranges(path);
            units.retain(|u| overlaps_any(u.start_line, u.end_line, &changed));
        }
        if units.is_empty() {
            return Ok(None);
        }
        let display_path = rel_override.unwrap_or_else(|| path.display().to_string());
        Ok(Some(FileReport {
            file: display_path,
            units,
        }))
    }
}

fn overlaps_any(start: usize, end: usize, ranges: &[LineRange]) -> bool {
    ranges.iter().any(|r| r.overlaps(start, end))
}

type BoxedError = Box<dyn std::error::Error + Send + Sync>;

fn extract_units(lang: SourceLang, source: &str) -> Result<Vec<CohesionUnit>, BoxedError> {
    match lang {
        SourceLang::Rust => {
            lens_rust::extract_cohesion_units(source).map_err(|e| Box::new(e) as BoxedError)
        }
        SourceLang::TypeScript(dialect) => {
            lens_ts::extract_cohesion_units(source, dialect).map_err(|e| Box::new(e) as BoxedError)
        }
        SourceLang::Python => {
            lens_py::extract_cohesion_units(source).map_err(|e| Box::new(e) as BoxedError)
        }
    }
}

/// Per-file slice of the report. Owns the display path so directory mode
/// can attach a path relative to the walk root without storing the original
/// `PathBuf`.
#[derive(Debug)]
struct FileReport {
    file: String,
    units: Vec<CohesionUnit>,
}

#[derive(Debug, Serialize)]
struct Report<'a> {
    /// Input path: a single source file, or the root directory walked.
    root: String,
    file_count: usize,
    unit_count: usize,
    files: Vec<FileView<'a>>,
}

impl<'a> Report<'a> {
    fn new(path: &Path, files: &'a [FileReport]) -> Self {
        let unit_count = files.iter().map(|f| f.units.len()).sum();
        Self {
            root: path.display().to_string(),
            file_count: files.len(),
            unit_count,
            files: files.iter().map(FileView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct FileView<'a> {
    file: &'a str,
    unit_count: usize,
    units: Vec<UnitView<'a>>,
}

impl<'a> From<&'a FileReport> for FileView<'a> {
    fn from(f: &'a FileReport) -> Self {
        Self {
            file: f.file.as_str(),
            unit_count: f.units.len(),
            units: f.units.iter().map(UnitView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct UnitView<'a> {
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    trait_name: Option<&'a str>,
    type_name: &'a str,
    start_line: usize,
    end_line: usize,
    method_count: usize,
    lcom4: usize,
    /// Henderson-Sellers' LCOM\*. Serialised as JSON `null` when the
    /// metric is undefined for the unit (single method or no fields).
    lcom96: Option<f64>,
    components: Vec<Vec<&'a str>>,
    methods: Vec<MethodView<'a>>,
}

impl<'a> From<&'a CohesionUnit> for UnitView<'a> {
    fn from(unit: &'a CohesionUnit) -> Self {
        let (kind, trait_name) = match &unit.kind {
            CohesionUnitKind::Inherent => ("inherent", None),
            CohesionUnitKind::Trait { trait_name } => ("trait", Some(trait_name.as_str())),
            CohesionUnitKind::Module => ("module", None),
        };
        let components: Vec<Vec<&str>> = unit
            .components
            .iter()
            .map(|c| {
                c.iter()
                    .map(|&i| unit.methods[i].name.as_str())
                    .collect::<Vec<_>>()
            })
            .collect();
        let methods = unit.methods.iter().map(MethodView::from).collect();
        Self {
            kind,
            trait_name,
            type_name: unit.type_name.as_str(),
            start_line: unit.start_line,
            end_line: unit.end_line,
            method_count: unit.methods.len(),
            lcom4: unit.lcom4(),
            lcom96: unit.lcom96,
            components,
            methods,
        }
    }
}

#[derive(Debug, Serialize)]
struct MethodView<'a> {
    name: &'a str,
    start_line: usize,
    end_line: usize,
    fields: &'a [String],
    calls: &'a [String],
}

impl<'a> From<&'a lens_domain::MethodCohesion> for MethodView<'a> {
    fn from(m: &'a lens_domain::MethodCohesion) -> Self {
        Self {
            name: m.name.as_str(),
            start_line: m.start_line,
            end_line: m.end_line,
            fields: &m.fields,
            calls: &m.calls,
        }
    }
}

fn format_markdown(report: &Report<'_>) -> String {
    let mut out = format!(
        "# Cohesion report: {} ({} file(s), {} unit(s))\n",
        report.root, report.file_count, report.unit_count,
    );
    if report.unit_count == 0 {
        out.push_str("\n_No cohesion units (no `impl` block / class / module-level functions)._\n");
        return out;
    }
    for file in &report.files {
        let _ = writeln!(out, "\n## {} ({} unit(s))", file.file, file.unit_count);
        for unit in &file.units {
            render_unit(&mut out, unit);
        }
    }
    out
}

fn render_unit(out: &mut String, unit: &UnitView<'_>) {
    let header = match (unit.kind, unit.trait_name) {
        ("module", _) => format!("module {}", unit.type_name),
        (_, Some(t)) => format!("impl {t} for {}", unit.type_name),
        _ => format!("impl {}", unit.type_name),
    };
    let lcom96 = format_optional_f64(unit.lcom96, 2);
    // writeln! into a String cannot fail; the result is swallowed
    // deliberately rather than unwrapped to satisfy the workspace's
    // `unwrap_used` lint.
    let _ = writeln!(
        out,
        "- {header} (L{}-{}) — LCOM4 = {}, LCOM96 = {}, {} method(s)",
        unit.start_line, unit.end_line, unit.lcom4, lcom96, unit.method_count,
    );
    for component in &unit.components {
        let _ = writeln!(out, "  - {{{}}}", component.join(", "));
    }
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
    fn json_report_includes_components_lcom4_and_lcom96() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
struct Thing { a: i32, b: i32 }
impl Thing {
    fn ga(&self) -> i32 { self.a }
    fn gb(&self) -> i32 { self.b }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let json = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["unit_count"], 1);
        assert_eq!(parsed["file_count"], 1);
        let units = parsed["files"][0]["units"].as_array().unwrap();
        assert_eq!(units[0]["lcom4"], 2);
        assert_eq!(units[0]["type_name"], "Thing");
        assert_eq!(units[0]["kind"], "inherent");
        // Two methods, two disjoint fields → LCOM96 = 1.0.
        let lcom96 = units[0]["lcom96"].as_f64().unwrap();
        assert!((lcom96 - 1.0).abs() < 1e-9, "got {lcom96}");
    }

    #[test]
    fn json_report_emits_null_lcom96_when_undefined() {
        let dir = tempfile::tempdir().unwrap();
        // Single instance method → LCOM96 is undefined.
        let src = r#"
struct Foo { n: i32 }
impl Foo {
    fn get(&self) -> i32 { self.n }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let json = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["files"][0]["units"][0]["lcom96"].is_null());
    }

    #[test]
    fn markdown_report_lists_each_component_with_both_scores() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
struct Thing { a: i32, b: i32 }
impl Thing {
    fn ga(&self) -> i32 { self.a }
    fn sa(&mut self, v: i32) { self.a = v; }
    fn gb(&self) -> i32 { self.b }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let md = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("impl Thing"));
        assert!(md.contains("LCOM4 = 2"));
        assert!(md.contains("LCOM96 = "));
        assert!(md.contains("ga"));
        assert!(md.contains("gb"));
    }

    #[test]
    fn markdown_report_shows_na_when_lcom96_undefined() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
struct Foo { n: i32 }
impl Foo {
    fn get(&self) -> i32 { self.n }
}
"#;
        let file = write_file(dir.path(), "lib.rs", src);
        let md = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("LCOM96 = n/a"));
    }

    #[test]
    fn unknown_extension_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "notes.txt", "hello");
        let err = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::UnsupportedExtension { .. }));
    }

    #[test]
    fn python_class_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        let src = "
class Counter:
    def inc(self):
        self.n += 1
    def get(self):
        return self.n
";
        let file = write_file(dir.path(), "lib.py", src);
        let json = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["unit_count"], 1);
        assert_eq!(parsed["files"][0]["units"][0]["type_name"], "Counter");
        assert_eq!(parsed["files"][0]["units"][0]["lcom4"], 1);
    }

    #[test]
    fn python_module_unit_is_emitted_alongside_classes() {
        // A file with both a class and module-level functions should
        // produce two units: the class and the module. Without
        // module-level cohesion the agent would only see the class
        // and miss e.g. split-personality file scope.
        let dir = tempfile::tempdir().unwrap();
        let src = "
counter = 0

def bump():
    global counter
    counter += 1

def get():
    return counter

class Other:
    def m(self):
        return self.x
";
        let file = write_file(dir.path(), "lib.py", src);
        let json = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["file_count"], 1);
        assert_eq!(parsed["unit_count"], 2);
        let kinds: Vec<String> = parsed["files"][0]["units"]
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["kind"].as_str().unwrap().to_owned())
            .collect();
        assert!(kinds.contains(&"inherent".to_owned()));
        assert!(kinds.contains(&"module".to_owned()));
    }

    #[test]
    fn typescript_module_unit_is_emitted() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
let counter = 0;

export function bump(): void { counter += 1; }
export function get(): number { return counter; }
"#;
        let file = write_file(dir.path(), "lib.ts", src);
        let json = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["unit_count"], 1);
        assert_eq!(parsed["files"][0]["units"][0]["kind"], "module");
        assert_eq!(parsed["files"][0]["units"][0]["lcom4"], 1);
    }

    #[test]
    fn markdown_renders_module_units_with_module_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let src = r#"
let a = 0;
let b = 0;

export function ga(): number { return a; }
export function gb(): number { return b; }
"#;
        let file = write_file(dir.path(), "lib.ts", src);
        let md = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        // Module units render with `module <name>` (rather than
        // `impl <name>`) so the agent can tell the granularity apart.
        assert!(md.contains("module <module>"), "got: {md}");
        assert!(md.contains("LCOM4 = 2"));
    }

    #[test]
    fn missing_file_surfaces_io_error() {
        let err = CohesionAnalyzer::new()
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
        let err = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::Parse(_)));
    }

    #[test]
    fn empty_report_for_files_without_impls() {
        // A file with no `impl` block, no class, and no top-level
        // function produces an empty cohesion report.
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", "const X: i32 = 1;\n");
        let md = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("No cohesion units"));
    }

    #[test]
    fn diff_only_filters_to_changed_impl_block() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
struct A { x: i32 }
impl A { fn get(&self) -> i32 { self.x } }
struct B { y: i32 }
impl B { fn get(&self) -> i32 { self.y } }
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
struct A { x: i32 }
impl A { fn get(&self) -> i32 { self.x + 1 } }
struct B { y: i32 }
impl B { fn get(&self) -> i32 { self.y } }
"#,
        );
        let json = CohesionAnalyzer::new()
            .with_diff_only(true)
            .analyze(&file, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["unit_count"], 1);
        assert_eq!(parsed["files"][0]["units"][0]["type_name"], "A");
    }

    #[test]
    fn directory_mode_groups_units_per_file() {
        // Two files with one `impl` each — the analyzer should walk the
        // directory and surface both, grouped per file.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "a.rs",
            r#"
struct A { x: i32, y: i32 }
impl A {
    fn gx(&self) -> i32 { self.x }
    fn gy(&self) -> i32 { self.y }
}
"#,
        );
        write_file(
            dir.path(),
            "nested/b.rs",
            r#"
struct B { z: i32 }
impl B { fn gz(&self) -> i32 { self.z } }
"#,
        );

        let json = CohesionAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["file_count"], 2);
        assert_eq!(parsed["unit_count"], 2);
        let files = parsed["files"].as_array().unwrap();
        let names: Vec<&str> = files.iter().map(|f| f["file"].as_str().unwrap()).collect();
        assert!(names.contains(&"a.rs"));
        assert!(names.contains(&"nested/b.rs"));
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
            r#"
struct A { x: i32 }
impl A { fn gx(&self) -> i32 { self.x } }
"#,
        );
        write_file(
            dir.path(),
            "ignored.rs",
            r#"
struct B { y: i32 }
impl B { fn gy(&self) -> i32 { self.y } }
"#,
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

        let json = CohesionAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["file_count"], 1, "got {parsed}");
        assert_eq!(parsed["files"][0]["file"], "a.rs");
    }

    #[test]
    fn directory_mode_drops_files_without_units() {
        // Two files: one with an `impl`, one with only a `const` and no
        // functions of any kind. The latter should be dropped to keep
        // the report signal-dense.
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "with_impl.rs",
            r#"
struct A { x: i32 }
impl A { fn gx(&self) -> i32 { self.x } }
"#,
        );
        write_file(dir.path(), "no_units.rs", "const X: i32 = 1;\n");

        let json = CohesionAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["file_count"], 1);
        assert_eq!(parsed["files"][0]["file"], "with_impl.rs");
    }

    #[test]
    fn path_filters_apply_to_directory_walks() {
        let dir = tempfile::tempdir().unwrap();
        let unit = "struct A { x: i32 }\nimpl A { fn gx(&self) -> i32 { self.x } }\n";
        write_file(dir.path(), "src/lib.rs", unit);
        write_file(dir.path(), "tests/lib_test.rs", unit);
        write_file(dir.path(), "src/generated.rs", unit);

        let only_tests = CohesionAnalyzer::new()
            .with_only_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&only_tests).unwrap();
        assert_eq!(parsed["file_count"], 1);
        assert_eq!(parsed["files"][0]["file"], "tests/lib_test.rs");

        let exclude_tests = CohesionAnalyzer::new()
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

        let exclude_generated = CohesionAnalyzer::new()
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
