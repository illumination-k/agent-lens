use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
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
        self.is_test_relative(&super::relative_display_path(path, &self.base))
    }

    /// Whether an already-relative path looks like a test, under the
    /// same rules [`Self::includes_relative`] gates on.
    ///
    /// The analyzers that work in git's repo-relative path space never
    /// hold a `Path` to ask about, and re-deriving "is this a test?"
    /// from the file-name conventions would put a second, driftable copy
    /// of that judgement next to this one.
    pub(crate) fn is_test_relative(&self, rel: &str) -> bool {
        self.root_is_test || path_looks_like_test(rel)
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

/// The walk-time view of this filter, for the adapters that discover
/// modules by walking a directory rather than by following declarations.
///
/// Same decision as [`CompiledPathFilter::includes_path`], reached before
/// the file is opened instead of after the module tree is built.
impl lens_domain::SourceFilter for CompiledPathFilter {
    fn includes(&self, path: &Path) -> bool {
        self.includes_path(path)
    }
}

fn add_exclude_pattern(builder: &mut GlobSetBuilder, pattern: &str) -> Result<(), PathFilterError> {
    for glob in exclude_globs(pattern) {
        let glob = Glob::new(&glob).map_err(|source| PathFilterError::InvalidExcludePattern {
            pattern: pattern.to_owned(),
            source,
        })?;
        builder.add(glob);
    }
    Ok(())
}

/// The globs one `--exclude` pattern stands for.
///
/// A pattern names a file *or* a directory, the way a `.gitignore` line
/// does, so each one also excludes everything below a directory it
/// matches: `vendor` drops `vendor/a.py` as well as a file called
/// `vendor`. Without that the only spelling that prunes a directory is
/// `vendor/**`, and a bare name silently keeps reading the tree it
/// appears to exclude (issue #493).
///
/// * A bare pattern (no separator) matches a path component at any
///   depth: the file `src/generated.rs` for `generated.rs`, and the
///   directory `a/vendor/` for `vendor`.
/// * A pattern containing a separator stays anchored at the filter's
///   base.
/// * A trailing `/` (`vendor/`) restricts the pattern to directories:
///   it matches what lies below one, never a file of that name.
fn exclude_globs(pattern: &str) -> Vec<String> {
    let name = pattern.trim_end_matches('/');
    if name.is_empty() {
        return vec![pattern.to_owned()];
    }
    let directory_only = name.len() != pattern.len();
    let bare = !name.contains('/') && !name.contains('\\');
    let any_depth = if bare { "**/" } else { "" };
    let below = format!("{any_depth}{name}/**");
    if directory_only {
        return vec![below];
    }

    let mut globs = vec![name.to_owned()];
    if bare {
        globs.push(format!("**/{name}"));
    }
    // `x/**` already spells "everything below"; another `/**` on top
    // would add nothing.
    if !name.ends_with("**") {
        globs.push(below);
    }
    globs
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
    use proptest::prelude::*;
    use rstest::rstest;

    fn excluding(pattern: &str) -> CompiledPathFilter {
        AnalyzePathFilter::new()
            .with_exclude_patterns(vec![pattern.to_owned()])
            .compile(Path::new("/repo"))
            .unwrap()
    }

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

    /// The table from issue #493: every spelling a reader would try for
    /// "leave `vendor/` out" has to keep the file below it from being
    /// read. The bare name and the trailing-slash form used to match
    /// file names only, so the walk still opened `vendor/…`.
    #[rstest]
    #[case::bare_directory_name("vendor")]
    #[case::trailing_slash("vendor/")]
    #[case::explicit_recursive("vendor/**")]
    #[case::any_depth_recursive("**/vendor/**")]
    #[case::single_star("vendor/*")]
    #[case::bare_file_name("bad_encoding.py")]
    fn every_spelling_of_a_directory_exclude_drops_the_file_below_it(#[case] pattern: &str) {
        let filter = excluding(pattern);
        assert!(!filter.includes_relative("vendor/bad_encoding.py"));
        assert!(filter.includes_relative("a.py"), "the sibling stays");
    }

    /// A bare name is a path component at any depth, whether the
    /// component is the directory or a file inside a nested one.
    #[rstest]
    #[case("vendor", "pkg/sub/vendor/x.py")]
    #[case("vendor", "pkg/vendor/deep/er/x.py")]
    #[case("vendor/", "pkg/sub/vendor/x.py")]
    #[case("vendor/", "pkg/vendor/deep/er/x.py")]
    #[case(".venv", ".venv/lib/python3.14/site-packages/joblib/test/enc.py")]
    #[case("node_*", "web/node_modules/pkg/index.js")]
    fn a_bare_directory_name_prunes_at_any_depth(#[case] pattern: &str, #[case] path: &str) {
        assert!(!excluding(pattern).includes_relative(path));
    }

    /// Components are matched whole: a name that merely contains the
    /// pattern, as a directory or as a file stem, is not the pattern.
    #[rstest]
    #[case("vendor", "vendored/x.py")]
    #[case("vendor", "src/myvendor/x.py")]
    #[case("vendor", "src/vendor.py")]
    #[case("vendor/", "src/vendor.py")]
    #[case("vendor/", "vendored/x.py")]
    fn a_bare_name_does_not_match_a_longer_component(#[case] pattern: &str, #[case] path: &str) {
        assert!(excluding(pattern).includes_relative(path));
    }

    /// The documented file behaviour is unchanged: a bare name still
    /// matches a file of that name at the base and at any depth.
    #[rstest]
    #[case("generated.rs", "generated.rs")]
    #[case("generated.rs", "src/generated.rs")]
    #[case("generated.rs", "a/b/c/generated.rs")]
    #[case("*.gen.rs", "src/api.gen.rs")]
    #[case("Makefile", "tools/Makefile")]
    fn a_bare_name_still_matches_files(#[case] pattern: &str, #[case] path: &str) {
        assert!(!excluding(pattern).includes_relative(path));
    }

    /// A trailing `/` says "directory": it prunes what is below one and
    /// leaves a file that happens to carry the name alone, where the
    /// plain name takes both.
    #[test]
    fn a_trailing_slash_matches_directories_only() {
        assert!(excluding("build/").includes_relative("tools/build"));
        assert!(!excluding("build/").includes_relative("tools/build/out.rs"));
        assert!(!excluding("build").includes_relative("tools/build"));
        assert!(!excluding("build").includes_relative("tools/build/out.rs"));
    }

    /// A pattern with a separator stays anchored at the base, and — like
    /// a bare one — covers the directory it names as well as a file.
    #[rstest]
    #[case("pkg/vendor")]
    #[case("pkg/vendor/")]
    #[case("pkg/vendor/**")]
    fn an_anchored_directory_pattern_prunes_only_from_the_base(#[case] pattern: &str) {
        let filter = excluding(pattern);
        assert!(!filter.includes_relative("pkg/vendor/x.py"));
        assert!(!filter.includes_relative("pkg/vendor/deep/x.py"));
        assert!(filter.includes_relative("other/pkg/vendor/x.py"));
        assert!(filter.includes_relative("pkg/vendored/x.py"));
    }

    #[test]
    fn an_anchored_file_pattern_is_not_promoted_to_any_depth() {
        let filter = excluding("src/generated.rs");
        assert!(!filter.includes_relative("src/generated.rs"));
        assert!(filter.includes_relative("crates/a/src/generated.rs"));
        assert!(filter.includes_relative("src/generated.rs.bak"));
    }

    /// The exact glob set each spelling stands for. Pinned because the
    /// behavioural cases above cannot tell a redundant glob from a
    /// missing one, and every glob is matched against every walked path.
    #[rstest]
    #[case("vendor", &["vendor", "**/vendor", "**/vendor/**"])]
    #[case("vendor/", &["**/vendor/**"])]
    #[case("vendor//", &["**/vendor/**"])]
    #[case("pkg/vendor", &["pkg/vendor", "pkg/vendor/**"])]
    #[case("pkg/vendor/", &["pkg/vendor/**"])]
    #[case("vendor/**", &["vendor/**"])]
    #[case("vendor/**/", &["vendor/**/**"])]
    #[case("**/vendor/**", &["**/vendor/**"])]
    #[case("vendor/*", &["vendor/*", "vendor/*/**"])]
    #[case("**", &["**", "**/**"])]
    #[case("pkg\\vendor", &["pkg\\vendor", "pkg\\vendor/**"])]
    #[case("/", &["/"])]
    #[case("", &[""])]
    fn exclude_patterns_expand_to_file_and_directory_globs(
        #[case] pattern: &str,
        #[case] expected: &[&str],
    ) {
        assert_eq!(exclude_globs(pattern), expected);
    }

    #[test]
    fn an_invalid_exclude_pattern_is_reported_as_the_user_wrote_it() {
        let err = AnalyzePathFilter::new()
            .with_exclude_patterns(vec!["vendor[".to_owned()])
            .compile(Path::new("/repo"))
            .unwrap_err();
        let PathFilterError::InvalidExcludePattern { pattern, .. } = err;
        assert_eq!(pattern, "vendor[");
    }

    /// Path components drawn from a tiny alphabet, so a generated path
    /// contains the excluded name often enough to exercise both answers.
    fn component() -> impl Strategy<Value = String> {
        "[ab]{1,2}(\\.[ab])?"
    }

    fn components() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(component(), 1..6)
    }

    proptest! {
        /// A bare pattern excludes a path exactly when one of the path's
        /// components is that name — the directory at any depth, or the
        /// file itself — and never on a partial component match.
        #[test]
        fn a_bare_exclude_matches_exactly_whole_components(
            name in component(),
            path in components(),
        ) {
            let excluded = !excluding(&name).includes_relative(&path.join("/"));
            prop_assert_eq!(excluded, path.contains(&name));
        }

        /// With a trailing `/` the name must be a directory: any
        /// component but the last.
        #[test]
        fn a_trailing_slash_exclude_matches_exactly_directory_components(
            name in component(),
            path in components(),
        ) {
            let excluded = !excluding(&format!("{name}/")).includes_relative(&path.join("/"));
            let directories = &path[..path.len() - 1];
            prop_assert_eq!(excluded, directories.contains(&name));
        }

        /// An anchored pattern excludes a path exactly when the pattern
        /// is a whole-component prefix of it, counted from the base.
        #[test]
        fn an_anchored_exclude_matches_exactly_a_component_prefix(
            pattern in prop::collection::vec(component(), 2..4),
            path in components(),
        ) {
            let excluded = !excluding(&pattern.join("/")).includes_relative(&path.join("/"));
            prop_assert_eq!(excluded, path.starts_with(&pattern));
        }
    }
}
