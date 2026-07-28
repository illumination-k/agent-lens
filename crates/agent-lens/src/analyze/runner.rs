//! Shared scaffolding for per-file analyzers.
//!
//! Cohesion, complexity, and wrapper all expose the same builder surface
//! (`with_diff_only`, `with_only_tests`, `with_exclude_tests`,
//! `with_exclude_patterns`), the same Json/Md format dispatch, and the
//! same per-file walk skeleton. This module factors those bits out so
//! each analyzer keeps only its own per-language extraction and report
//! shape.

use std::marker::PhantomData;
use std::path::Path;

use serde::Serialize;
use serde::ser::{SerializeStruct, Serializer};

use super::{
    AnalyzePathFilter, AnalyzerError, OutputFormat, SourceFile, changed_line_ranges,
    collect_source_files, overlaps_any,
};

/// Filter knobs every per-file analyzer shares: an unstaged-diff gate
/// plus the path-filter inputs (only-tests / exclude-tests / glob
/// excludes).
///
/// Holds the path-filter state directly rather than wrapping an
/// [`AnalyzePathFilter`]: the underlying type is a pure value object
/// built from these same flags, so a forwarding wrapper would just
/// rename the same setters one extra time. Helpers that need the
/// `AnalyzePathFilter` shape (`compile()`, the per-file walk) get one
/// on demand from [`Self::path_filter`].
#[derive(Debug, Default, Clone)]
pub(super) struct FilterConfig {
    diff_only: bool,
    only_tests: bool,
    exclude_tests: bool,
    exclude: Vec<String>,
}

impl FilterConfig {
    pub fn with_diff_only(mut self, diff_only: bool) -> Self {
        self.diff_only = diff_only;
        self
    }

    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.only_tests = only_tests;
        self
    }

    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.exclude_tests = exclude_tests;
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.exclude = exclude;
        self
    }

    pub fn diff_only(&self) -> bool {
        self.diff_only
    }

    /// Build a fresh [`AnalyzePathFilter`] reflecting the current
    /// state. Cheap to call (a few bool copies plus a `Vec` clone) and
    /// kept as a factory rather than a borrowed field so the config
    /// struct stays the single source of truth.
    pub fn path_filter(&self) -> AnalyzePathFilter {
        AnalyzePathFilter::new()
            .with_only_tests(self.only_tests)
            .with_exclude_tests(self.exclude_tests)
            .with_exclude_patterns(self.exclude.clone())
    }

    /// Compile the path filter against `path` and walk it (single file
    /// or directory, respecting `.gitignore`), returning every
    /// supported source file the analyzer should inspect.
    fn collect_source_files(&self, path: &Path) -> Result<Vec<SourceFile>, AnalyzerError> {
        let filter = self.path_filter().compile(path)?;
        collect_source_files(path, &filter)
    }

    /// Walk `path` and run `analyze_one` on every supported source
    /// file. Files for which `analyze_one` returns `Ok(None)` are
    /// dropped so directory-mode reports stay signal-dense.
    pub fn collect_per_file<R>(
        &self,
        path: &Path,
        mut analyze_one: impl FnMut(&SourceFile) -> Result<Option<R>, AnalyzerError>,
    ) -> Result<Vec<R>, AnalyzerError> {
        let mut out = Vec::new();
        for source_file in self.collect_source_files(path)? {
            if let Some(report) = analyze_one(&source_file)? {
                out.push(report);
            }
        }
        Ok(out)
    }

    /// When `diff_only` is set, retain only items whose `[start, end]`
    /// line range overlaps an unstaged hunk in `git diff -U0` for
    /// `path`. No-op otherwise so callers can call this unconditionally.
    pub fn retain_changed<T>(
        &self,
        items: &mut Vec<T>,
        path: &Path,
        range: impl Fn(&T) -> (usize, usize),
    ) {
        if !self.diff_only {
            return;
        }
        let changed = changed_line_ranges(path);
        items.retain(|item| {
            let (s, e) = range(item);
            overlaps_any(s, e, &changed)
        });
    }
}

/// Render a serializable report as JSON or markdown, deferring the
/// markdown formatter to a closure so each analyzer keeps its own
/// presentation logic. Centralising the match here means the JSON
/// pretty-printer and the `AnalyzerError::Serialize` mapping live in
/// one place.
pub(super) fn render_report<R: Serialize>(
    report: &R,
    format: OutputFormat,
    md: impl FnOnce() -> String,
) -> Result<String, AnalyzerError> {
    match format {
        OutputFormat::Json => {
            serde_json::to_string_pretty(report).map_err(AnalyzerError::Serialize)
        }
        OutputFormat::Md => Ok(md()),
    }
}

/// Names the two report fields that differ between otherwise-identical
/// per-file analyzers: cohesion counts `unit_count` / `units`,
/// complexity `function_count` / `functions`, wrapper `wrapper_count` /
/// `wrappers`. Implemented by a zero-sized marker per analyzer so
/// [`PerFileReport`] can emit each analyzer's established JSON shape
/// without a bespoke struct per analyzer.
pub(super) trait PerFileShape {
    /// Field name for the item count, at both report and file level.
    const COUNT_FIELD: &'static str;
    /// Field name for the per-file item list.
    const ITEMS_FIELD: &'static str;
}

/// One file's slice of a per-file report: the display path plus that
/// file's item views. The count is derived from `items` on serialisation
/// rather than stored, so the two can never drift apart.
#[derive(Debug)]
pub(super) struct FileView<'a, S, V> {
    file: &'a str,
    items: Vec<V>,
    shape: PhantomData<S>,
}

impl<'a, S, V> FileView<'a, S, V> {
    pub fn new(file: &'a str, items: Vec<V>) -> Self {
        Self {
            file,
            items,
            shape: PhantomData,
        }
    }

    pub fn file(&self) -> &'a str {
        self.file
    }

    pub fn items(&self) -> &[V] {
        &self.items
    }

    pub fn count(&self) -> usize {
        self.items.len()
    }
}

impl<S: PerFileShape, V: Serialize> Serialize for FileView<'_, S, V> {
    fn serialize<Z: Serializer>(&self, serializer: Z) -> Result<Z::Ok, Z::Error> {
        let mut state = serializer.serialize_struct("FileView", 3)?;
        state.serialize_field("file", self.file)?;
        state.serialize_field(S::COUNT_FIELD, &self.items.len())?;
        state.serialize_field(S::ITEMS_FIELD, &self.items)?;
        state.end()
    }
}

/// The report shape every per-file analyzer emits: the walked root, a
/// file count, a total item count, an optional corpus-wide summary, and
/// the per-file breakdown.
///
/// `X` carries the analyzer-specific summary block (only complexity has
/// one today) and defaults to `()`, which is never serialised because
/// summary-less reports leave it `None`.
#[derive(Debug)]
pub(super) struct PerFileReport<'a, S, V, X = ()> {
    root: String,
    files: Vec<FileView<'a, S, V>>,
    summary: Option<X>,
}

impl<'a, S, V, X> PerFileReport<'a, S, V, X> {
    /// Build a report with no corpus summary.
    pub fn new(root: &Path, files: Vec<FileView<'a, S, V>>) -> Self {
        Self {
            root: root.display().to_string(),
            files,
            summary: None,
        }
    }

    /// Attach a corpus-wide summary block, serialised between the item
    /// count and the per-file breakdown.
    pub fn with_summary(mut self, summary: X) -> Self {
        self.summary = Some(summary);
        self
    }

    pub fn root(&self) -> &str {
        &self.root
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Total items across every file — the value serialised under the
    /// shape's count field.
    pub fn item_count(&self) -> usize {
        self.files.iter().map(FileView::count).sum()
    }

    pub fn files(&self) -> &[FileView<'a, S, V>] {
        &self.files
    }

    pub fn summary(&self) -> Option<&X> {
        self.summary.as_ref()
    }
}

impl<S: PerFileShape, V: Serialize, X: Serialize> Serialize for PerFileReport<'_, S, V, X> {
    fn serialize<Z: Serializer>(&self, serializer: Z) -> Result<Z::Ok, Z::Error> {
        let len = 4 + usize::from(self.summary.is_some());
        let mut state = serializer.serialize_struct("Report", len)?;
        state.serialize_field("root", &self.root)?;
        state.serialize_field("file_count", &self.files.len())?;
        state.serialize_field(S::COUNT_FIELD, &self.item_count())?;
        if let Some(summary) = &self.summary {
            state.serialize_field("summary", summary)?;
        }
        state.serialize_field("files", &self.files)?;
        state.end()
    }
}

/// Generate the four standard filter-builder methods on an analyzer
/// struct, forwarding to a [`FilterConfig`]-typed field. Keeps each
/// analyzer's public builder API exactly as it was while removing the
/// per-analyzer boilerplate.
macro_rules! delegate_filter_builders {
    ($field:ident) => {
        pub fn with_diff_only(mut self, diff_only: bool) -> Self {
            self.$field = self.$field.with_diff_only(diff_only);
            self
        }

        pub fn with_only_tests(mut self, only_tests: bool) -> Self {
            self.$field = self.$field.with_only_tests(only_tests);
            self
        }

        pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
            self.$field = self.$field.with_exclude_tests(exclude_tests);
            self
        }

        pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
            self.$field = self.$field.with_exclude_patterns(exclude);
            self
        }
    };
}

pub(super) use delegate_filter_builders;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};
    use std::path::PathBuf;

    #[test]
    fn collect_per_file_drops_none_results() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "keep.rs", "fn a() {}\n");
        write_file(dir.path(), "drop.rs", "fn b() {}\n");

        let cfg = FilterConfig::default();
        let out: Vec<String> = cfg
            .collect_per_file(dir.path(), |sf| {
                if sf.display_path.ends_with("drop.rs") {
                    Ok(None)
                } else {
                    Ok(Some(sf.display_path.clone()))
                }
            })
            .unwrap();
        assert_eq!(out, vec!["keep.rs".to_owned()]);
    }

    #[test]
    fn collect_per_file_propagates_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "a.rs", "fn a() {}\n");

        let cfg = FilterConfig::default();
        let err = cfg
            .collect_per_file::<()>(dir.path(), |_| {
                Err(AnalyzerError::UnsupportedExtension {
                    path: PathBuf::from("synthetic"),
                })
            })
            .unwrap_err();
        assert!(matches!(err, AnalyzerError::UnsupportedExtension { .. }));
    }

    #[test]
    fn retain_changed_no_op_when_diff_only_off() {
        // With diff_only off, the helper should not even consult git;
        // any input list is preserved unchanged.
        let cfg = FilterConfig::default();
        let mut items = vec![(1usize, 5usize), (10, 12)];
        cfg.retain_changed(&mut items, Path::new("/does/not/matter.rs"), |&(s, e)| {
            (s, e)
        });
        assert_eq!(items, vec![(1, 5), (10, 12)]);
    }

    #[test]
    fn retain_changed_filters_to_overlapping_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n",
        );
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        run_git(dir.path(), &["add", "lib.rs"]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        // Mutate only line 2 (beta).
        write_file(
            dir.path(),
            "lib.rs",
            "fn alpha() {}\nfn beta() -> i32 { 1 }\nfn gamma() {}\n",
        );
        let cfg = FilterConfig::default().with_diff_only(true);
        let mut items = vec![("alpha", 1, 1), ("beta", 2, 2), ("gamma", 3, 3)];
        cfg.retain_changed(&mut items, &file, |&(_, s, e)| (s, e));
        let names: Vec<&str> = items.iter().map(|(n, _, _)| *n).collect();
        assert_eq!(names, vec!["beta"]);
    }

    #[test]
    fn diff_only_accessor_reflects_builder_state() {
        // `diff_only()` is read by similarity to short-circuit its
        // pair-level diff filter; the other path-filter knobs flow into
        // `path_filter()` and are exercised end-to-end by the analyzer
        // test suites. Pin `diff_only` directly so the accessor can't
        // silently invert.
        assert!(FilterConfig::default().with_diff_only(true).diff_only());
        assert!(!FilterConfig::default().diff_only());
    }

    #[test]
    fn render_report_json_serializes_pretty() {
        #[derive(serde::Serialize)]
        struct Sample {
            n: i32,
        }
        let s = render_report(&Sample { n: 7 }, OutputFormat::Json, || {
            "should not be called".to_owned()
        })
        .unwrap();
        assert!(s.contains("\"n\": 7"));
    }

    #[test]
    fn render_report_md_invokes_closure() {
        #[derive(serde::Serialize)]
        struct Sample;
        let s = render_report(&Sample, OutputFormat::Md, || "hello md".to_owned()).unwrap();
        assert_eq!(s, "hello md");
    }

    /// Stand-in for a real analyzer's shape marker. The names differ
    /// from every production shape so a test asserting on them can't
    /// pass by accident against the wrong constant.
    struct TestShape;

    impl PerFileShape for TestShape {
        const COUNT_FIELD: &'static str = "widget_count";
        const ITEMS_FIELD: &'static str = "widgets";
    }

    type TestReport<'a, X = ()> = PerFileReport<'a, TestShape, &'static str, X>;

    fn sample_files<'a>() -> Vec<FileView<'a, TestShape, &'static str>> {
        vec![
            FileView::new("a.rs", vec!["one", "two"]),
            FileView::new("b.rs", vec!["three"]),
        ]
    }

    fn to_json<S: Serialize>(value: &S) -> serde_json::Value {
        serde_json::to_value(value).unwrap()
    }

    #[test]
    fn per_file_report_serializes_under_the_shape_field_names() {
        let report: TestReport<'_> = TestReport::new(Path::new("/src"), sample_files());
        let json = to_json(&report);

        assert_eq!(json["root"], "/src");
        assert_eq!(json["file_count"], 2);
        assert_eq!(json["widget_count"], 3);
        assert_eq!(json["files"][0]["file"], "a.rs");
        assert_eq!(json["files"][0]["widget_count"], 2);
        assert_eq!(
            json["files"][0]["widgets"],
            serde_json::json!(["one", "two"])
        );
        assert_eq!(json["files"][1]["widget_count"], 1);
    }

    #[test]
    fn per_file_report_omits_summary_when_absent() {
        let report: TestReport<'_> = TestReport::new(Path::new("/src"), sample_files());
        let json = to_json(&report);
        assert!(report.summary().is_none());
        assert!(
            json.get("summary").is_none(),
            "summary-less report must not emit the key: {json}"
        );
    }

    #[test]
    fn per_file_report_emits_attached_summary() {
        let report = TestReport::new(Path::new("/src"), sample_files()).with_summary("corpus-wide");
        let json = to_json(&report);
        assert_eq!(report.summary().copied(), Some("corpus-wide"));
        assert_eq!(json["summary"], "corpus-wide");
        // The summary sits between the count and the per-file
        // breakdown, so both neighbours must survive its insertion.
        assert_eq!(json["widget_count"], 3);
        assert_eq!(json["files"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn per_file_report_counts_are_derived_from_the_files() {
        let report: TestReport<'_> = TestReport::new(Path::new("/src"), sample_files());
        assert_eq!(report.file_count(), 2);
        assert_eq!(report.item_count(), 3);
        assert_eq!(report.root(), "/src");
        assert_eq!(report.files().len(), 2);
    }

    #[test]
    fn empty_report_counts_zero() {
        let report: TestReport<'_> = TestReport::new(Path::new("/src"), Vec::new());
        let json = to_json(&report);
        assert_eq!(report.item_count(), 0);
        assert_eq!(json["file_count"], 0);
        assert_eq!(json["widget_count"], 0);
        assert_eq!(json["files"], serde_json::json!([]));
    }

    #[test]
    fn file_view_accessors_expose_path_and_items() {
        let view: FileView<'_, TestShape, &'static str> = FileView::new("a.rs", vec!["one", "two"]);
        assert_eq!(view.file(), "a.rs");
        assert_eq!(view.items(), ["one", "two"]);
        assert_eq!(view.count(), 2);
    }
}
