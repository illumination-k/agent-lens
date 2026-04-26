//! `analyze dead-pub` — find publicly-exposed items that no other module
//! in the crate references.
//!
//! Walks a Rust crate from a `.rs` root, builds the module tree, joins
//! the [`pub` item list][lens_rust::extract_public_items] against the
//! cross-module reference graph from
//! [`lens_rust::extract_edges`], and reports the items the rest of the
//! crate ignores. Items at the crate root (`crate::Foo`) are kept in
//! the report but flagged with `at_crate_root: true` — for library
//! crates those may still be consumed externally and are not always
//! safe to remove.
//!
//! Limitations carried over from the underlying extractors:
//!
//! * Single-crate. References from sibling crates inside the same
//!   workspace are invisible. Treat the report as a candidate list, not
//!   a delete order.
//! * `#[path = "…"]` attributes on `mod` declarations are not honoured.
//! * Macro-generated items are invisible to `syn`.
//! * Inherent methods (`impl Foo { pub fn bar() }`) and trait
//!   associated items are not tracked individually; their liveness
//!   rides on the enclosing `struct` / `enum` / `trait`.
//! * Method calls dispatched on a value (`x.foo()`) cannot be resolved
//!   to a path and therefore cannot keep their target alive on their
//!   own. The enclosing type is still marked alive whenever it is
//!   named, which keeps the false-positive rate low in practice.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use lens_domain::{CouplingEdge, PublicItem, find_dead_pub_items};
use lens_rust::{
    CouplingError as RustCouplingError, build_module_tree, extract_edges, extract_public_items,
};
use serde::Serialize;

use super::{OutputFormat, SourceLang};

/// Errors raised while running the dead-`pub` analyzer.
#[derive(Debug, thiserror::Error)]
pub enum DeadPubAnalyzerError {
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path:?}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The provided path exists but isn't a `.rs` file or a directory
    /// containing a recognisable crate root.
    #[error(
        "no usable Rust crate root found at {path:?}; pass a .rs file or a directory containing src/lib.rs or src/main.rs"
    )]
    UnsupportedRoot { path: PathBuf },
    /// `mod foo;` was declared in a parent file but neither `foo.rs`
    /// nor `foo/mod.rs` could be found.
    #[error(
        "module `{parent}::{name}` declared but neither {name}.rs nor {name}/mod.rs found in {near:?}"
    )]
    MissingMod {
        parent: String,
        name: String,
        near: PathBuf,
    },
    #[error("failed to serialize report: {0}")]
    Serialize(#[from] serde_json::Error),
}

impl From<RustCouplingError> for DeadPubAnalyzerError {
    fn from(value: RustCouplingError) -> Self {
        match value {
            RustCouplingError::Io { path, source } => Self::Io { path, source },
            RustCouplingError::Parse { path, source } => Self::Parse {
                path,
                source: Box::new(source),
            },
            RustCouplingError::MissingMod { parent, name, near } => {
                Self::MissingMod { parent, name, near }
            }
        }
    }
}

/// Stateless analyzer entry point. Configuration knobs (filtering,
/// output shaping) can be added without breaking the CLI surface.
#[derive(Debug, Default, Clone, Copy)]
pub struct DeadPubAnalyzer {
    include_crate_root: bool,
}

impl DeadPubAnalyzer {
    pub fn new() -> Self {
        Self {
            include_crate_root: true,
        }
    }

    /// Drop items declared directly in the crate root from the report.
    ///
    /// For library crates the crate-root `pub` items are the external
    /// API and the analyzer cannot see consumers outside the crate;
    /// callers that already know the crate is a library and want a
    /// noise-free report can opt out of those entries.
    pub fn with_include_crate_root(mut self, include: bool) -> Self {
        self.include_crate_root = include;
        self
    }

    /// Resolve `path`, build the crate's module tree, and report the
    /// `pub` items no other module references.
    pub fn analyze(
        &self,
        path: &Path,
        format: OutputFormat,
    ) -> Result<String, DeadPubAnalyzerError> {
        let root = resolve_crate_root(path)?;
        let modules = build_module_tree(&root)?;
        let edges: Vec<CouplingEdge> = extract_edges(&modules);
        let items: Vec<PublicItem> = extract_public_items(&modules);
        let total_pub_items = items.len();
        let mut dead = find_dead_pub_items(items, &edges);
        if !self.include_crate_root {
            dead.retain(|item| !item.at_crate_root());
        }
        let view = ReportView::new(&root, total_pub_items, &dead);
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&view).map_err(DeadPubAnalyzerError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&view)),
        }
    }
}

/// Map a user-provided path to a single `.rs` crate root.
///
/// Same shape as the coupling analyzer: a `.rs` file is taken as-is, a
/// directory is probed for `src/lib.rs` then `src/main.rs`.
fn resolve_crate_root(path: &Path) -> Result<PathBuf, DeadPubAnalyzerError> {
    let meta = std::fs::metadata(path).map_err(|source| DeadPubAnalyzerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.is_file() {
        if SourceLang::from_path(path) == Some(SourceLang::Rust) {
            return Ok(path.to_path_buf());
        }
        return Err(DeadPubAnalyzerError::UnsupportedRoot {
            path: path.to_path_buf(),
        });
    }
    if meta.is_dir() {
        for candidate in ["src/lib.rs", "src/main.rs"] {
            let probe = path.join(candidate);
            if probe.is_file() {
                return Ok(probe);
            }
        }
    }
    Err(DeadPubAnalyzerError::UnsupportedRoot {
        path: path.to_path_buf(),
    })
}

#[derive(Debug, Serialize)]
struct ReportView<'a> {
    crate_root: String,
    total_pub_items: usize,
    dead_count: usize,
    dead: Vec<DeadView<'a>>,
}

impl<'a> ReportView<'a> {
    fn new(root: &Path, total_pub_items: usize, dead: &'a [PublicItem]) -> Self {
        Self {
            crate_root: root.display().to_string(),
            total_pub_items,
            dead_count: dead.len(),
            dead: dead.iter().map(DeadView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct DeadView<'a> {
    module: &'a str,
    name: &'a str,
    kind: &'static str,
    file: String,
    start_line: usize,
    end_line: usize,
    at_crate_root: bool,
}

impl<'a> From<&'a PublicItem> for DeadView<'a> {
    fn from(item: &'a PublicItem) -> Self {
        Self {
            module: item.module.as_str(),
            name: item.name.as_str(),
            kind: item.kind.as_str(),
            file: item.file.display().to_string(),
            start_line: item.start_line,
            end_line: item.end_line,
            at_crate_root: item.at_crate_root(),
        }
    }
}

const TOP_KINDS_LIMIT: usize = 5;

fn format_markdown(view: &ReportView<'_>) -> String {
    let mut out = format!(
        "# Dead pub report: {} ({} of {} pub item(s))\n",
        view.crate_root, view.dead_count, view.total_pub_items,
    );
    if view.dead.is_empty() {
        out.push_str("\n_Every pub item is referenced from another module._\n");
        return out;
    }
    render_kind_breakdown(&mut out, &view.dead);
    render_dead_table(&mut out, &view.dead);
    out
}

fn render_kind_breakdown(out: &mut String, dead: &[DeadView<'_>]) {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for item in dead {
        if let Some(entry) = counts.iter_mut().find(|(k, _)| *k == item.kind) {
            entry.1 += 1;
        } else {
            counts.push((item.kind, 1));
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    let _ = writeln!(out, "\n## By kind");
    for (kind, count) in counts.iter().take(TOP_KINDS_LIMIT) {
        let _ = writeln!(out, "- {kind}: {count}");
    }
}

fn render_dead_table(out: &mut String, dead: &[DeadView<'_>]) {
    let _ = writeln!(out, "\n## Unreferenced pub items\n");
    let _ = writeln!(out, "| module | name | kind | location | crate_root |");
    let _ = writeln!(out, "| --- | --- | --- | --- | :---: |");
    for item in dead {
        let _ = writeln!(
            out,
            "| {} | `{}` | {} | {}:{} | {} |",
            item.module,
            item.name,
            item.kind,
            item.file,
            item.start_line,
            if item.at_crate_root { "yes" } else { "no" },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn small_crate(dir: &Path) -> PathBuf {
        // Layout:
        //   lib.rs declares mod a; mod b;
        //   a.rs has pub fn live_helper, pub fn dead_helper, pub struct Foo
        //   b.rs uses crate::a::Foo and calls crate::a::live_helper()
        // → dead_helper is dead; Foo and live_helper are alive.
        let lib = write_file(dir, "lib.rs", "pub mod a;\npub mod b;\n");
        write_file(
            dir,
            "a.rs",
            r#"
            pub fn live_helper() {}
            pub fn dead_helper() {}
            pub struct Foo;
            "#,
        );
        write_file(
            dir,
            "b.rs",
            r#"
            use crate::a::Foo;
            fn _x(_f: Foo) { crate::a::live_helper(); }
            "#,
        );
        lib
    }

    #[test]
    fn json_report_lists_only_unreferenced_items() {
        let dir = tempfile::tempdir().unwrap();
        let lib = small_crate(dir.path());
        let json = DeadPubAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let dead = parsed["dead"].as_array().unwrap();
        let names: Vec<&str> = dead.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"dead_helper"), "got {names:?}");
        assert!(!names.contains(&"live_helper"), "got {names:?}");
        assert!(!names.contains(&"Foo"), "got {names:?}");
    }

    #[test]
    fn json_report_carries_total_and_dead_counts() {
        let dir = tempfile::tempdir().unwrap();
        let lib = small_crate(dir.path());
        let json = DeadPubAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // 3 pub items: live_helper, dead_helper, Foo.
        assert_eq!(parsed["total_pub_items"], 3);
        // Only dead_helper survives the filter.
        assert_eq!(parsed["dead_count"], 1);
    }

    #[test]
    fn json_report_marks_crate_root_items() {
        let dir = tempfile::tempdir().unwrap();
        // A pub item only in the crate root — no other module refers to it.
        let lib = write_file(dir.path(), "lib.rs", "pub fn root_dead() {}\n");
        let json = DeadPubAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let dead = parsed["dead"].as_array().unwrap();
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0]["at_crate_root"], true);
        assert_eq!(dead[0]["module"], "crate");
    }

    #[test]
    fn excluding_crate_root_drops_top_level_items() {
        let dir = tempfile::tempdir().unwrap();
        let lib = small_crate(dir.path());
        // No crate-root pub items in `small_crate`, so the filter is a
        // no-op for the dead set, but exercising the flag path keeps
        // the public API documented as intended.
        let json = DeadPubAnalyzer::new()
            .with_include_crate_root(false)
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let dead = parsed["dead"].as_array().unwrap();
        for entry in dead {
            assert_ne!(entry["module"], "crate");
        }
    }

    #[test]
    fn excluding_crate_root_with_only_root_items_yields_empty_report() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "pub fn root_only() {}\n");
        let json = DeadPubAnalyzer::new()
            .with_include_crate_root(false)
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["dead_count"], 0);
    }

    #[test]
    fn glob_use_keeps_every_item_in_target_alive() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "pub mod a;\npub mod b;\n");
        write_file(
            dir.path(),
            "a.rs",
            "pub fn alpha() {}\npub fn beta() {}\npub fn gamma() {}\n",
        );
        write_file(dir.path(), "b.rs", "use crate::a::*;\n");
        let json = DeadPubAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // Every item in crate::a should be considered live thanks to
        // the glob import in crate::b.
        let dead = parsed["dead"].as_array().unwrap();
        let names: Vec<&str> = dead.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert!(!names.contains(&"alpha"));
        assert!(!names.contains(&"beta"));
        assert!(!names.contains(&"gamma"));
    }

    #[test]
    fn associated_call_keeps_enclosing_struct_alive() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "pub mod a;\npub mod b;\n");
        write_file(
            dir.path(),
            "a.rs",
            "pub struct Foo;\nimpl Foo { pub fn make() -> Self { Self } }\n",
        );
        write_file(
            dir.path(),
            "b.rs",
            "fn _x() { let _ = crate::a::Foo::make(); }\n",
        );
        let json = DeadPubAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let dead = parsed["dead"].as_array().unwrap();
        let names: Vec<&str> = dead.iter().map(|d| d["name"].as_str().unwrap()).collect();
        assert!(!names.contains(&"Foo"), "got {names:?}");
    }

    #[test]
    fn markdown_report_contains_kind_breakdown_and_table() {
        let dir = tempfile::tempdir().unwrap();
        let lib = small_crate(dir.path());
        let md = DeadPubAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("# Dead pub report:"));
        assert!(md.contains("## By kind"));
        assert!(md.contains("## Unreferenced pub items"));
        assert!(md.contains("dead_helper"));
        assert!(!md.contains("live_helper"));
    }

    #[test]
    fn markdown_report_celebrates_a_clean_crate() {
        let dir = tempfile::tempdir().unwrap();
        // A crate where the only pub item is consumed by another module.
        let lib = write_file(dir.path(), "lib.rs", "pub mod a;\npub mod b;\n");
        write_file(dir.path(), "a.rs", "pub fn helper() {}\n");
        write_file(dir.path(), "b.rs", "fn _x() { crate::a::helper(); }\n");
        let md = DeadPubAnalyzer::new()
            .analyze(&lib, OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Every pub item is referenced"));
    }

    #[test]
    fn directory_root_detects_src_lib_rs() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "pub fn solo() {}\n");
        let json = DeadPubAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["total_pub_items"], 1);
    }

    #[test]
    fn directory_root_falls_back_to_src_main_rs() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/main.rs", "fn main() {}\n");
        let json = DeadPubAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["total_pub_items"], 0);
    }

    #[test]
    fn unsupported_extension_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let f = write_file(dir.path(), "notes.txt", "hello");
        let err = DeadPubAnalyzer::new()
            .analyze(&f, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, DeadPubAnalyzerError::UnsupportedRoot { .. }));
    }

    #[test]
    fn missing_path_surfaces_io_error() {
        let err = DeadPubAnalyzer::new()
            .analyze(
                Path::new("/definitely/does/not/exist.rs"),
                OutputFormat::Json,
            )
            .unwrap_err();
        assert!(matches!(err, DeadPubAnalyzerError::Io { .. }));
    }

    #[test]
    fn missing_mod_file_surfaces_missing_mod_error() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "mod ghost;\n");
        let err = DeadPubAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, DeadPubAnalyzerError::MissingMod { .. }));
    }

    #[test]
    fn invalid_rust_surfaces_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", "fn ??? {");
        let err = DeadPubAnalyzer::new()
            .analyze(&lib, OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, DeadPubAnalyzerError::Parse { .. }));
    }
}
