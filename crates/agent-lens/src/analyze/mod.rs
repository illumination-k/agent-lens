//! On-demand analyzers that emit LLM-friendly context.
//!
//! Each submodule is one analyzer (cohesion, complexity, coupling,
//! similarity, wrapper, hotspot, …) and is wired to a clap subcommand so
//! typos surface at parse time. Output is always written to stdout as JSON
//! by default; analyzers can opt in to a `--format md` mode for a more
//! compact human-readable summary.

pub mod cohesion;
pub mod complexity;
pub mod coupling;
pub mod hotspot;
pub mod similarity;
pub mod wrapper;

use std::path::{Path, PathBuf};
use std::process::Command;

pub use cohesion::CohesionAnalyzer;
pub use complexity::ComplexityAnalyzer;
pub use coupling::{CouplingAnalyzer, CouplingAnalyzerError};
pub use hotspot::{HotspotAnalyzer, HotspotError};
pub use similarity::{DEFAULT_THRESHOLD as DEFAULT_SIMILARITY_THRESHOLD, SimilarityAnalyzer};
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
/// `match` arm for the new variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLang {
    Rust,
    TypeScript,
}

impl SourceLang {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            // `.tsx` / `.mts` / `.cts` parse with a different
            // `SourceType`; until the lens-ts entry points accept one,
            // we restrict the TypeScript variant to plain `.ts` so a
            // user pointing at a `.tsx` file gets a clear
            // UnsupportedExtension instead of a confusing parse error.
            "ts" => Some(Self::TypeScript),
            _ => None,
        }
    }

    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }
}

/// Errors common to single-file analyzers (cohesion, complexity).
///
/// Coupling carries extra variants (`UnsupportedRoot`, `MissingMod`) so it
/// keeps its own error type. The shared variants here keep the simple
/// analyzers from each repeating the same Io / extension / parse /
/// serialize boilerplate.
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
    use std::error::Error as _;
    use std::io;
    use std::io::Write;

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
