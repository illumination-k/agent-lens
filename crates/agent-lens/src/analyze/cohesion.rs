//! `analyze cohesion` — surface LCOM4 cohesion units for a Rust source file.
//!
//! Today the analyzer is single-file, single-language: pass it a `.rs` path
//! and it reports every `impl` block in the file along with the connected
//! components of its method-cohesion graph. Output is JSON by default; the
//! markdown mode emits a compact summary tuned for LLM context windows
//! rather than for humans, in line with the project's "agent-friendly lint"
//! ethos.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use lens_domain::{CohesionUnit, CohesionUnitKind};
use serde::Serialize;

use super::OutputFormat;

/// Errors raised while running the cohesion analyzer.
#[derive(Debug)]
pub enum CohesionAnalyzerError {
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

impl std::fmt::Display for CohesionAnalyzerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::UnsupportedExtension { path } => write!(
                f,
                "unsupported file extension for cohesion analysis: {}",
                path.display()
            ),
            Self::Parse(e) => write!(f, "failed to parse source: {e}"),
            Self::Serialize(e) => write!(f, "failed to serialize report: {e}"),
        }
    }
}

impl std::error::Error for CohesionAnalyzerError {
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

    fn extract(self, source: &str) -> Result<Vec<CohesionUnit>, CohesionAnalyzerError> {
        match self {
            Self::Rust => lens_rust::extract_cohesion_units(source)
                .map_err(|e| CohesionAnalyzerError::Parse(Box::new(e))),
        }
    }
}

/// Analyzer entry point. Stateless today; kept as a struct so per-run
/// configuration (filters, thresholds) can be added without breaking the
/// CLI surface.
#[derive(Debug, Default, Clone, Copy)]
pub struct CohesionAnalyzer;

impl CohesionAnalyzer {
    pub fn new() -> Self {
        Self
    }

    /// Read `path`, analyze it, and produce a report in `format`.
    pub fn analyze(
        &self,
        path: &Path,
        format: OutputFormat,
    ) -> Result<String, CohesionAnalyzerError> {
        let language = path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(Language::from_extension)
            .ok_or_else(|| CohesionAnalyzerError::UnsupportedExtension {
                path: path.to_path_buf(),
            })?;

        let source = std::fs::read_to_string(path).map_err(|source| CohesionAnalyzerError::Io {
            path: path.to_path_buf(),
            source,
        })?;

        let units = language.extract(&source)?;
        let report = Report::new(path, &units);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&report).map_err(CohesionAnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&report)),
        }
    }
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
        let header = match unit.trait_name {
            Some(t) => format!("impl {t} for {}", unit.type_name),
            None => format!("impl {}", unit.type_name),
        };
        let lcom96 = match unit.lcom96 {
            Some(v) => format!("{v:.2}"),
            None => "n/a".to_owned(),
        };
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
        assert!(matches!(
            err,
            CohesionAnalyzerError::UnsupportedExtension { .. }
        ));
    }

    #[test]
    fn missing_file_surfaces_io_error() {
        let err = CohesionAnalyzer::new()
            .analyze(
                Path::new("/definitely/does/not/exist.rs"),
                OutputFormat::Json,
            )
            .unwrap_err();
        assert!(matches!(err, CohesionAnalyzerError::Io { .. }));
    }

    #[test]
    fn invalid_rust_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "broken.rs", "fn ??? {");
        let err = CohesionAnalyzer::new()
            .analyze(&file, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, CohesionAnalyzerError::Parse(_)));
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
}
