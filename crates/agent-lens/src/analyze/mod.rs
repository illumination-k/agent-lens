//! On-demand analyzers that emit LLM-friendly context.
//!
//! Each submodule is one analyzer (cohesion, complexity, coupling,
//! similarity, wrapper, hotspot, …) and is wired to a clap subcommand so
//! typos surface at parse time. Output is always written to stdout as JSON
//! by default; analyzers can opt in to a `--format md` mode for a more
//! compact human-readable summary.

pub mod cohesion;
pub mod complexity;
pub mod context_span;
pub mod coupling;
pub mod hotspot;
pub mod similarity;
pub mod wrapper;

use std::path::{Path, PathBuf};
use std::process::Command;

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

pub use cohesion::CohesionAnalyzer;
pub use complexity::ComplexityAnalyzer;
pub use context_span::ContextSpanAnalyzer;
pub use coupling::CouplingAnalyzer;
pub use hotspot::{HotspotAnalyzer, HotspotError};

/// Backward-compatible alias for the unified [`CrateAnalyzerError`].
pub type CouplingAnalyzerError = CrateAnalyzerError;
/// Backward-compatible alias for the unified [`CrateAnalyzerError`].
pub type ContextSpanAnalyzerError = CrateAnalyzerError;
pub use similarity::{
    DEFAULT_MIN_LINES as DEFAULT_SIMILARITY_MIN_LINES,
    DEFAULT_THRESHOLD as DEFAULT_SIMILARITY_THRESHOLD, SimilarityAnalyzer,
};
pub use wrapper::WrapperAnalyzer;

/// Output format shared across analyzers.
///
/// Lives at the module root so every analyzer's `--format` flag
/// resolves to the same enum, both in clap and in the public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Json,
    Md,
}

/// Source languages the analyzers know how to handle.
///
/// Centralising the extension → language mapping keeps every analyzer in
/// sync when a new language gets wired up; analyzers only need to add a
/// `match` arm for the new variant. The TypeScript variant carries a
/// [`lens_ts::Dialect`] so the same dispatch covers `.ts` / `.tsx` /
/// `.jsx` / `.js` / `.mjs` / `.cjs` without re-deriving it at every call
/// site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLang {
    Rust,
    TypeScript(lens_ts::Dialect),
    Python,
}

#[derive(Debug)]
pub(crate) struct SourceFile {
    pub path: PathBuf,
    pub display_path: String,
}

impl SourceLang {
    pub fn from_extension(ext: &str) -> Option<Self> {
        if ext == "rs" {
            return Some(Self::Rust);
        }
        if ext == "py" {
            return Some(Self::Python);
        }
        lens_ts::Dialect::from_extension(ext).map(Self::TypeScript)
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }
}

/// Path-level filters shared by every on-demand analyzer.
///
/// `only_tests` is intentionally path-based so it can apply uniformly to
/// function analyzers, file-level hotspot scoring, and Rust crate graph
/// analyzers. Language-specific "test function" filtering remains a
/// separate analyzer option where it exists. The compiled filter keeps the
/// input root context so `agent-lens analyze ... tests --only-tests` treats
/// every file below that root as test code even after paths are made
/// relative to the root.
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
        let rel = relative_display_path(path, &self.base);
        self.includes_relative(&rel)
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

pub(crate) fn collect_source_files(
    path: &Path,
    filter: &CompiledPathFilter,
) -> Result<Vec<SourceFile>, AnalyzerError> {
    if path.is_dir() {
        collect_directory_source_files(path, filter)
    } else if filter.includes_path(path) {
        Ok(vec![SourceFile {
            path: path.to_path_buf(),
            display_path: path.display().to_string(),
        }])
    } else {
        Ok(Vec::new())
    }
}

fn collect_directory_source_files(
    root: &Path,
    filter: &CompiledPathFilter,
) -> Result<Vec<SourceFile>, AnalyzerError> {
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
        if !filter.includes_path(p) || SourceLang::from_path(p).is_none() {
            continue;
        }
        out.push(SourceFile {
            path: p.to_path_buf(),
            display_path: relative_display_path(p, root),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

pub(crate) fn overlaps_any(start: usize, end: usize, ranges: &[LineRange]) -> bool {
    ranges.iter().any(|r| r.overlaps(start, end))
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

fn relative_display_path(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
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

/// Errors common to single-file analyzers (cohesion, complexity).
///
/// Coupling and context-span carry extra variants (`UnsupportedRoot`,
/// `MissingMod`) and use the dedicated [`CrateAnalyzerError`] below
/// instead.
#[derive(Debug, thiserror::Error)]
pub enum AnalyzerError {
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("unsupported file extension: {path:?}")]
    UnsupportedExtension { path: PathBuf },
    #[error("failed to parse source: {0}")]
    Parse(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("failed to serialize report: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error(transparent)]
    PathFilter(#[from] PathFilterError),
}

/// Errors raised by analyzers that walk a Rust crate from a `.rs` root
/// (coupling, context-span, …).
///
/// Both analyzers reach into the same `lens_rust` extractor and surface
/// the same handful of failure modes; they used to duplicate this enum.
#[derive(Debug, thiserror::Error)]
pub enum CrateAnalyzerError {
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
    /// `mod foo;` was declared in a parent file but neither `foo.rs` nor
    /// `foo/mod.rs` could be found.
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
    #[error(transparent)]
    PathFilter(#[from] PathFilterError),
}

impl From<lens_rust::CouplingError> for CrateAnalyzerError {
    fn from(value: lens_rust::CouplingError) -> Self {
        match value {
            lens_rust::CouplingError::Io { path, source } => Self::Io { path, source },
            lens_rust::CouplingError::Parse { path, source } => Self::Parse {
                path,
                source: Box::new(source),
            },
            lens_rust::CouplingError::MissingMod { parent, name, near } => {
                Self::MissingMod { parent, name, near }
            }
        }
    }
}

/// Resolve `path` to a Rust crate root.
///
/// Accepts:
/// 1. A `.rs` file → returned as-is.
/// 2. A directory containing `src/lib.rs` → that file.
/// 3. A directory containing `src/main.rs` → that file.
///
/// Anything else surfaces [`CrateAnalyzerError::UnsupportedRoot`].
pub fn resolve_crate_root(path: &Path) -> Result<PathBuf, CrateAnalyzerError> {
    let meta = std::fs::metadata(path).map_err(|source| CrateAnalyzerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.is_file() && SourceLang::from_path(path) == Some(SourceLang::Rust) {
        return Ok(path.to_path_buf());
    }
    if meta.is_dir()
        && let Some(probe) = first_existing_file(path, &["src/lib.rs", "src/main.rs"])
    {
        return Ok(probe);
    }
    Err(CrateAnalyzerError::UnsupportedRoot {
        path: path.to_path_buf(),
    })
}

fn first_existing_file(root: &Path, candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|c| root.join(c))
        .find(|p| p.is_file())
}

/// Format an `Option<f64>` for markdown reports: `Some(x)` becomes
/// `"{x:.precision$}"`, `None` becomes the literal `"n/a"`.
pub(crate) fn format_optional_f64(v: Option<f64>, precision: usize) -> String {
    match v {
        Some(x) => format!("{x:.precision$}"),
        None => "n/a".to_owned(),
    }
}

/// Detect the source language from `path` and read it into memory.
///
/// Returns the matched [`SourceLang`] alongside the file contents so
/// callers can dispatch on the language without re-parsing the path.
pub fn read_source(path: &Path) -> Result<(SourceLang, String), AnalyzerError> {
    let lang = SourceLang::from_path(path).ok_or_else(|| AnalyzerError::UnsupportedExtension {
        path: path.to_path_buf(),
    })?;
    let source = std::fs::read_to_string(path).map_err(|source| AnalyzerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok((lang, source))
}

/// 1-based inclusive line range extracted from a unified diff hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl LineRange {
    pub fn overlaps(self, start: usize, end: usize) -> bool {
        self.start <= end && start <= self.end
    }
}

/// Return changed line ranges for `path` from `git diff -U0`.
///
/// The ranges come from the "new file" side of each hunk (`+start,count`)
/// and are 1-based inclusive. When git is unavailable, the path is outside
/// a repo, or there are no unstaged edits for the file, this returns an
/// empty list.
pub fn changed_line_ranges(path: &Path) -> Vec<LineRange> {
    let (cwd, path_arg) = diff_invocation(path);
    let output = Command::new("git")
        .args(["diff", "--no-ext-diff", "--unified=0", "--"])
        .arg(path_arg)
        .current_dir(cwd)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(stdout) = String::from_utf8(output.stdout) else {
        return Vec::new();
    };
    parse_unified_zero_hunks(&stdout)
}

fn diff_invocation(path: &Path) -> (&Path, &Path) {
    if path.is_absolute() {
        let cwd = path.parent().unwrap_or(path);
        let arg = path.file_name().map_or(path, Path::new);
        (cwd, arg)
    } else {
        (Path::new("."), path)
    }
}

fn parse_unified_zero_hunks(diff: &str) -> Vec<LineRange> {
    let mut out = Vec::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("@@") else {
            continue;
        };
        let Some(header) = rest.split("@@").next() else {
            continue;
        };
        let Some(plus) = header.split_whitespace().find(|part| part.starts_with('+')) else {
            continue;
        };
        let coords = plus.trim_start_matches('+');
        let mut parts = coords.split(',');
        let Some(start) = parts.next().and_then(|x| x.parse::<usize>().ok()) else {
            continue;
        };
        let count = parts
            .next()
            .and_then(|x| x.parse::<usize>().ok())
            .unwrap_or(1);
        if count == 0 {
            continue;
        }
        out.push(LineRange {
            start,
            end: start.saturating_add(count.saturating_sub(1)),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::error::Error as _;
    use std::io;
    use std::io::Write;

    #[test]
    fn source_lang_from_extension_covers_ts_family() {
        // Each TS / JS extension must round-trip through the dialect
        // mapping, otherwise analyzers would silently reject the file
        // with `UnsupportedExtension` before reaching the parser.
        for (ext, expected) in [
            ("ts", lens_ts::Dialect::Ts),
            ("tsx", lens_ts::Dialect::Tsx),
            ("mts", lens_ts::Dialect::Mts),
            ("cts", lens_ts::Dialect::Cts),
            ("js", lens_ts::Dialect::Js),
            ("jsx", lens_ts::Dialect::Jsx),
            ("mjs", lens_ts::Dialect::Mjs),
            ("cjs", lens_ts::Dialect::Cjs),
        ] {
            assert_eq!(
                SourceLang::from_extension(ext),
                Some(SourceLang::TypeScript(expected)),
                "extension {ext} should map to dialect {expected:?}",
            );
        }
    }

    #[test]
    fn source_lang_from_extension_keeps_other_languages() {
        assert_eq!(SourceLang::from_extension("rs"), Some(SourceLang::Rust));
        assert_eq!(SourceLang::from_extension("py"), Some(SourceLang::Python));
        assert_eq!(SourceLang::from_extension("md"), None);
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

    #[test]
    fn analyzer_error_io_display_includes_path_and_source() {
        let err = AnalyzerError::Io {
            path: PathBuf::from("/tmp/nope.rs"),
            source: io::Error::new(io::ErrorKind::NotFound, "missing"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/nope.rs"), "got {msg}");
        assert!(msg.contains("missing"), "got {msg}");
        assert!(msg.starts_with("failed to read"), "got {msg}");
    }

    #[test]
    fn analyzer_error_unsupported_extension_display_includes_path() {
        let err = AnalyzerError::UnsupportedExtension {
            path: PathBuf::from("/tmp/file.txt"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/file.txt"), "got {msg}");
        assert!(msg.contains("unsupported file extension"), "got {msg}");
    }

    #[test]
    fn analyzer_error_parse_display_includes_inner() {
        let err = AnalyzerError::Parse(Box::<dyn std::error::Error + Send + Sync>::from(
            "boom".to_owned(),
        ));
        let msg = err.to_string();
        assert!(msg.contains("boom"), "got {msg}");
        assert!(msg.contains("parse"), "got {msg}");
    }

    #[test]
    fn analyzer_error_serialize_display_includes_inner() {
        // Force a serde_json error by parsing invalid JSON.
        let serde_err = serde_json::from_str::<serde_json::Value>("{invalid").unwrap_err();
        let err = AnalyzerError::Serialize(serde_err);
        let msg = err.to_string();
        assert!(msg.contains("serialize"), "got {msg}");
    }

    #[test]
    fn analyzer_error_io_source_is_present() {
        let err = AnalyzerError::Io {
            path: PathBuf::from("/tmp/x"),
            source: io::Error::new(io::ErrorKind::PermissionDenied, "denied"),
        };
        assert!(err.source().is_some());
    }

    #[test]
    fn analyzer_error_parse_source_is_present() {
        let err = AnalyzerError::Parse(Box::<dyn std::error::Error + Send + Sync>::from(
            "boom".to_owned(),
        ));
        assert!(err.source().is_some());
    }

    #[test]
    fn analyzer_error_serialize_source_is_present() {
        let serde_err = serde_json::from_str::<serde_json::Value>("{invalid").unwrap_err();
        let err = AnalyzerError::Serialize(serde_err);
        assert!(err.source().is_some());
    }

    #[test]
    fn analyzer_error_unsupported_extension_has_no_source() {
        let err = AnalyzerError::UnsupportedExtension {
            path: PathBuf::from("/tmp/x.txt"),
        };
        assert!(err.source().is_none());
    }

    #[test]
    fn parses_unified_zero_hunk_ranges() {
        let diff = "\
@@ -1,0 +3,2 @@
+a
+b
@@ -10 +20 @@
-x
+y
@@ -5,1 +7,0 @@
-gone
";
        let got = parse_unified_zero_hunks(diff);
        assert_eq!(
            got,
            vec![
                LineRange { start: 3, end: 4 },
                LineRange { start: 20, end: 20 },
            ]
        );
    }

    #[test]
    fn line_range_overlap_is_inclusive() {
        let r = LineRange { start: 10, end: 12 };
        assert!(r.overlaps(12, 20));
        assert!(r.overlaps(1, 10));
        assert!(!r.overlaps(13, 20));
    }

    #[test]
    fn diff_invocation_anchors_absolute_paths_at_parent() {
        let path = Path::new("/tmp/repo/src/lib.rs");
        let (cwd, arg) = diff_invocation(path);
        assert_eq!(cwd, Path::new("/tmp/repo/src"));
        assert_eq!(arg, Path::new("lib.rs"));
    }

    #[test]
    fn changed_line_ranges_resolves_absolute_paths_inside_repo() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);

        let file = dir.path().join("lib.rs");
        let mut f = std::fs::File::create(&file).unwrap();
        f.write_all(b"fn alpha() {}\nfn beta() {}\n").unwrap();
        run_git(dir.path(), &["add", "lib.rs"]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let mut f = std::fs::File::create(&file).unwrap();
        f.write_all(b"fn alpha() { let _x = 1; }\nfn beta() {}\n")
            .unwrap();

        let ranges = changed_line_ranges(&file);
        assert!(
            ranges.iter().any(|r| r.overlaps(1, 1)),
            "expected changed range to include line 1, got {ranges:?}",
        );
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
