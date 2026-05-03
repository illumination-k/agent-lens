//! Shared scaffolding for per-file analyzers.
//!
//! Cohesion, complexity, and wrapper all expose the same builder surface
//! (`with_diff_only`, `with_only_tests`, `with_exclude_tests`,
//! `with_exclude_patterns`), the same Json/Md format dispatch, and the
//! same per-file walk skeleton. This module factors those bits out so
//! each analyzer keeps only its own per-language extraction and report
//! shape.

use std::path::Path;

use serde::Serialize;

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

    pub fn only_tests(&self) -> bool {
        self.only_tests
    }

    pub fn exclude_tests(&self) -> bool {
        self.exclude_tests
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
    pub fn collect_source_files(&self, path: &Path) -> Result<Vec<SourceFile>, AnalyzerError> {
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
    fn accessors_reflect_builder_state() {
        // Lock down the four accessors against silent regressions: each
        // builder must propagate to the matching accessor, otherwise
        // analyzers (e.g. similarity) that read the flags out for
        // function-level filtering would silently disagree with the
        // path-level walk.
        let cfg = FilterConfig::default()
            .with_diff_only(true)
            .with_only_tests(true)
            .with_exclude_patterns(vec!["gen.rs".to_owned()]);
        assert!(cfg.diff_only(), "with_diff_only -> diff_only");
        assert!(cfg.only_tests(), "with_only_tests -> only_tests");
        assert!(!cfg.exclude_tests(), "default exclude_tests stays false");

        let cfg = FilterConfig::default().with_exclude_tests(true);
        assert!(cfg.exclude_tests(), "with_exclude_tests -> exclude_tests");
        assert!(!cfg.only_tests());
        assert!(!cfg.diff_only());
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
}
