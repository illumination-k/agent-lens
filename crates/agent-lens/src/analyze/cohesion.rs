//! `analyze cohesion` — surface LCOM4 cohesion units for a Rust source file.
//!
//! Today the analyzer is single-file, single-language: pass it a `.rs` path
//! and it reports every `impl` block in the file along with the connected
//! components of its method-cohesion graph. Output is JSON by default; the
//! markdown mode emits a compact summary tuned for LLM context windows
//! rather than for humans, in line with the project's "agent-friendly lint"
//! ethos.

use std::fmt::Write as _;
use std::path::Path;

use lens_domain::{CohesionUnit, CohesionUnitKind};
use serde::Serialize;

use super::{AnalyzerError, LineRange, OutputFormat, SourceLang, changed_line_ranges, read_source};

/// Analyzer entry point. Stateless today; kept as a struct so per-run
/// configuration (filters, thresholds) can be added without breaking the
/// CLI surface.
#[derive(Debug, Default, Clone, Copy)]
pub struct CohesionAnalyzer {
    diff_only: bool,
}

impl CohesionAnalyzer {
    pub fn new() -> Self {
        Self { diff_only: false }
    }

    /// Restrict reports to cohesion units whose `impl` range intersects
    /// an unstaged changed line in `git diff -U0`.
    pub fn with_diff_only(mut self, diff_only: bool) -> Self {
        self.diff_only = diff_only;
        self
    }

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, AnalyzerError> {
        let (lang, source) = read_source(path)?;
        let mut units = match lang {
            SourceLang::Rust => lens_rust::extract_cohesion_units(&source)
                .map_err(|e| AnalyzerError::Parse(Box::new(e)))?,
            SourceLang::TypeScript => lens_ts::extract_cohesion_units(&source)
                .map_err(|e| AnalyzerError::Parse(Box::new(e)))?,
        };
        if self.diff_only {
            let changed = changed_line_ranges(path);
            units.retain(|u| overlaps_any(u.start_line, u.end_line, &changed));
        }
        let report = Report::new(path, &units);
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
    unit_count: usize,
    units: Vec<UnitView<'a>>,
}

impl<'a> Report<'a> {
    fn new(path: &Path, units: &'a [CohesionUnit]) -> Self {
        Self {
            file: path.display().to_string(),
            unit_count: units.len(),
            units: units.iter().map(UnitView::from).collect(),
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
        "# Cohesion report: {} ({} unit(s))\n",
        report.file, report.unit_count,
    );
    if report.units.is_empty() {
        out.push_str("\n_No `impl` blocks with instance methods._\n");
        return out;
    }
    for unit in &report.units {
        render_unit(&mut out, unit);
    }
    out
}

fn render_unit(out: &mut String, unit: &UnitView<'_>) {
    let header = match unit.trait_name {
        Some(t) => format!("impl {t} for {}", unit.type_name),
        None => format!("impl {}", unit.type_name),
    };
    let lcom96 = format_optional_f64(unit.lcom96, 2);
    // writeln! into a String cannot fail; the result is swallowed
    // deliberately rather than unwrapped to satisfy the workspace's
    // `unwrap_used` lint.
    let _ = writeln!(
        out,
        "\n## {header} (L{}-{}) — LCOM4 = {}, LCOM96 = {}, {} method(s)",
        unit.start_line, unit.end_line, unit.lcom4, lcom96, unit.method_count,
    );
    for component in &unit.components {
        let _ = writeln!(out, "- {{{}}}", component.join(", "));
    }
}

fn format_optional_f64(v: Option<f64>, precision: usize) -> String {
    match v {
        Some(x) => format!("{x:.precision$}"),
        None => "n/a".to_owned(),
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
        assert_eq!(parsed["units"][0]["lcom4"], 2);
        assert_eq!(parsed["units"][0]["type_name"], "Thing");
        assert_eq!(parsed["units"][0]["kind"], "inherent");
        // Two methods, two disjoint fields → LCOM96 = 1.0.
        let lcom96 = parsed["units"][0]["lcom96"].as_f64().unwrap();
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
        assert!(parsed["units"][0]["lcom96"].is_null());
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
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", "fn solo() {}\n");
        let md = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("No `impl` blocks"));
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
        assert_eq!(parsed["units"][0]["type_name"], "A");
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
