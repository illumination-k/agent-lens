use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

#[derive(Debug, Clone, Default)]
pub struct AnalyzePathFilter {
    only_tests: bool,
    exclude_tests: bool,
    exclude: Vec<String>,
}

impl AnalyzePathFilter {
    pub fn new() -> Self {
        Self::default()
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

    pub fn compile(&self, root: &Path) -> Result<CompiledPathFilter, PathFilterError> {
        let base = if root.is_dir() {
            root.to_path_buf()
        } else {
            root.parent().unwrap_or_else(|| Path::new("")).to_path_buf()
        };
        let root_is_test = path_context_looks_like_test(root);
        let mut builder = GlobSetBuilder::new();
        for pattern in &self.exclude {
            add_exclude_pattern(&mut builder, pattern)?;
        }
        let exclude = builder
            .build()
            .map_err(|source| PathFilterError::InvalidExcludePattern {
                pattern: self.exclude.join(", "),
                source,
            })?;
        Ok(CompiledPathFilter {
            base,
            root_is_test,
            only_tests: self.only_tests,
            exclude_tests: self.exclude_tests,
            exclude,
            has_excludes: !self.exclude.is_empty(),
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PathFilterError {
    #[error("invalid --exclude pattern {pattern:?}: {source}")]
    InvalidExcludePattern {
        pattern: String,
        #[source]
        source: globset::Error,
    },
}

#[derive(Debug)]
pub struct CompiledPathFilter {
    base: PathBuf,
    root_is_test: bool,
    only_tests: bool,
    exclude_tests: bool,
    exclude: GlobSet,
    has_excludes: bool,
}

impl CompiledPathFilter {
    pub fn includes_path(&self, path: &Path) -> bool {
        let rel = super::relative_display_path(path, &self.base);
        self.includes_relative(&rel)
    }

    pub(crate) fn is_test_path(&self, path: &Path) -> bool {
        let rel = super::relative_display_path(path, &self.base);
        self.root_is_test || path_looks_like_test(&rel)
    }

    pub fn includes_relative(&self, rel: &str) -> bool {
        let is_test = self.root_is_test || path_looks_like_test(rel);
        if self.only_tests && !is_test {
            return false;
        }
        if self.exclude_tests && is_test {
            return false;
        }
        !self.has_excludes || !self.exclude.is_match(rel)
    }
}

fn add_exclude_pattern(builder: &mut GlobSetBuilder, pattern: &str) -> Result<(), PathFilterError> {
    let glob = Glob::new(pattern).map_err(|source| PathFilterError::InvalidExcludePattern {
        pattern: pattern.to_owned(),
        source,
    })?;
    builder.add(glob);

    if !pattern.contains('/') && !pattern.contains('\\') {
        let recursive = format!("**/{pattern}");
        let glob =
            Glob::new(&recursive).map_err(|source| PathFilterError::InvalidExcludePattern {
                pattern: pattern.to_owned(),
                source,
            })?;
        builder.add(glob);
    }
    Ok(())
}

fn path_context_looks_like_test(path: &Path) -> bool {
    let context = std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(cwd).ok())
        .unwrap_or(path);
    let display = context.to_string_lossy();
    path_looks_like_test(&display)
}

fn path_looks_like_test(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let segments: Vec<&str> = normalized
        .split('/')
        .filter_map(|segment| match segment {
            "" | "." => None,
            segment => Some(segment),
        })
        .collect();
    if segments
        .iter()
        .any(|segment| path_segment_looks_like_test_dir(segment))
    {
        return true;
    }

    let Some(file) = segments.last().copied() else {
        return false;
    };
    file_name_looks_like_test(file)
}

fn path_segment_looks_like_test_dir(segment: &str) -> bool {
    let segment = segment.to_ascii_lowercase();
    matches!(
        segment.as_str(),
        "test"
            | "tests"
            | "__test__"
            | "__tests__"
            | "spec"
            | "specs"
            | "__spec__"
            | "__specs__"
            | "e2e"
            | "integration_tests"
            | "integration-test"
            | "unit_tests"
            | "unit-test"
            | "testdata"
            | "testing"
    )
}

fn file_name_looks_like_test(file: &str) -> bool {
    let file = file.to_ascii_lowercase();
    if file == "conftest.py" {
        return true;
    }

    let stem = file
        .rsplit_once('.')
        .map_or(file.as_str(), |(stem, _)| stem);
    stem.split(['.', '_', '-'])
        .any(|part| matches!(part, "test" | "tests" | "spec" | "specs" | "e2e" | "cy"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("tests/api.rs")]
    #[case("src/__tests__/view.ts")]
    #[case("src/__specs__/view.ts")]
    #[case("src/testing/helpers.py")]
    #[case("pkg/testdata/input.rs")]
    #[case("src/foo.test.ts")]
    #[case("src/foo.tests.ts")]
    #[case("src/foo.spec.ts")]
    #[case("src/foo.e2e.ts")]
    #[case("src/foo.cy.ts")]
    #[case("pkg/conftest.py")]
    #[case("pkg/test_api.py")]
    #[case("src/foo_test.rs")]
    #[case("src/foo_tests.rs")]
    #[case("src/foo_spec.rs")]
    #[case("src/foo-test.rs")]
    #[case("src/foo_test.generated.rs")]
    fn test_path_detection_covers_common_file_conventions(#[case] path: &str) {
        assert!(path_looks_like_test(path), "{path} should look like a test");
    }

    #[rstest]
    #[case("src/testsupport.rs")]
    #[case("src/generated.rs")]
    #[case("src/latest.rs")]
    #[case("src/contest.rs")]
    fn test_path_detection_avoids_non_test_substrings(#[case] path: &str) {
        assert!(
            !path_looks_like_test(path),
            "{path} should not look like a test"
        );
    }

    #[test]
    fn compiled_path_filter_combines_test_modes_and_exclude_globs() {
        let root = Path::new("/repo");
        let include_all = AnalyzePathFilter::new().compile(root).unwrap();
        assert!(include_all.includes_relative("src/lib.rs"));

        let only_tests = AnalyzePathFilter::new()
            .with_only_tests(true)
            .compile(root)
            .unwrap();
        assert!(only_tests.includes_relative("tests/api.rs"));
        assert!(!only_tests.includes_relative("src/lib.rs"));

        let exclude_tests = AnalyzePathFilter::new()
            .with_exclude_tests(true)
            .compile(root)
            .unwrap();
        assert!(!exclude_tests.includes_relative("tests/api.rs"));
        assert!(exclude_tests.includes_relative("src/lib.rs"));

        let exclude_bare = AnalyzePathFilter::new()
            .with_exclude_patterns(vec!["generated.rs".to_owned()])
            .compile(root)
            .unwrap();
        assert!(!exclude_bare.includes_relative("src/generated.rs"));
        assert!(exclude_bare.includes_relative("src/handwritten.rs"));
    }

    #[test]
    fn compiled_path_filter_keeps_test_root_context() {
        let only_tests = AnalyzePathFilter::new()
            .with_only_tests(true)
            .compile(Path::new("tests"))
            .unwrap();
        assert!(only_tests.includes_relative("api.rs"));
        assert!(only_tests.includes_relative("nested/api.rs"));

        let exclude_tests = AnalyzePathFilter::new()
            .with_exclude_tests(true)
            .compile(Path::new("tests"))
            .unwrap();
        assert!(!exclude_tests.includes_relative("api.rs"));
        assert!(!exclude_tests.includes_relative("nested/api.rs"));
    }

    #[test]
    fn exclude_globs_with_slashes_are_not_promoted_to_any_depth() {
        let filter = AnalyzePathFilter::new()
            .with_exclude_patterns(vec!["generated/*.rs".to_owned()])
            .compile(Path::new("/repo"))
            .unwrap();
        assert!(!filter.includes_relative("generated/bindings.rs"));
        assert!(filter.includes_relative("src/generated/bindings.rs"));
    }
}
