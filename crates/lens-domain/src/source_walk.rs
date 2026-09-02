//! The directory walk shared by the adapters that discover modules by
//! scanning a tree, and the predicate it consults.
//!
//! Two adapters find modules by walking rather than by following
//! declarations: Go (every directory holding `.go` files is a package) and
//! Python (every `.py` file is a module). Rust walks `mod` declarations from
//! the crate root and TypeScript follows imports from an entry file, so both
//! are bounded by the source itself and never come here.
//!
//! An unbounded walk has to be told what not to read, and it has to be told
//! *before* reading: the caller's exclude globs used to be applied to the
//! finished module list, which shaped the report correctly while still paying
//! to open and parse every excluded file. [`SourceFilter`] moves that decision
//! into the walk. The caller-facing filter lives in `agent-lens`, the layer
//! that knows about CLI flags, so this is the narrow view of it the adapters
//! depend on instead of that crate.
//!
//! The walk itself lives here rather than once per adapter because it encodes
//! policy, and policy kept in two places drifts: a hand-rolled `read_dir`
//! recursion using `Path::is_dir` follows symlinks, which turned one Go module
//! of 559 files into a walk of 188,069 as soon as a build system parked
//! convenience links to its output base in the repo root, and it had no cycle
//! guard, so a symlink loop was unbounded recursion rather than a slow walk.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

/// Decides whether a walked file joins the module tree.
pub trait SourceFilter {
    /// Whether `path` — an existing file the walk has just reached — should be
    /// read and parsed.
    fn includes(&self, path: &Path) -> bool;
}

/// The filter for a caller that has nothing to exclude.
///
/// Named rather than an `Option<&dyn SourceFilter>` so an adapter never has to
/// branch on presence: "no filter" and "a filter that keeps everything" are the
/// same walk, and only one of the two spellings can be forgotten at a call site.
#[derive(Debug, Clone, Copy, Default)]
pub struct IncludeAll;

impl SourceFilter for IncludeAll {
    fn includes(&self, _path: &Path) -> bool {
        true
    }
}

/// Every file under `dir` with `extension` that `filter` keeps.
///
/// Walks with `ignore`, the same walker every file-scoped analyzer is built
/// on, so a module is discovered under the rules the rest of the tool already
/// applies: version-control ignores and hidden directories are skipped, and
/// symlinked directories are not followed.
///
/// Order is the walker's; callers that need determinism sort the result.
pub fn collect_files_with_extension(
    dir: &Path,
    extension: &str,
    filter: &dyn SourceFilter,
) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut out = Vec::new();
    for entry in WalkBuilder::new(dir).build() {
        let entry = entry.map_err(std::io::Error::other)?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.into_path();
        if path.extension().and_then(std::ffi::OsStr::to_str) == Some(extension)
            && filter.includes(&path)
        {
            out.push(path);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(&path, body).expect("write");
    }

    fn names(dir: &Path, ext: &str, filter: &dyn SourceFilter) -> Vec<String> {
        let mut found: Vec<String> = collect_files_with_extension(dir, ext, filter)
            .expect("walk")
            .iter()
            .filter_map(|p| p.strip_prefix(dir).ok())
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }

    #[test]
    fn include_all_keeps_every_path() {
        assert!(IncludeAll.includes(Path::new("a/b/c.go")));
        assert!(IncludeAll.includes(Path::new("")));
    }

    #[test]
    fn only_the_requested_extension_is_collected() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a.go", "");
        write(dir.path(), "b.py", "");
        write(dir.path(), "nested/c.go", "");
        assert_eq!(
            names(dir.path(), "go", &IncludeAll),
            ["a.go", "nested/c.go"]
        );
    }

    /// The reason this walk is shared rather than copied per adapter.
    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_not_followed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "own.go", "");
        write(outside.path(), "foreign.go", "");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked")).expect("symlink");
        assert_eq!(names(dir.path(), "go", &IncludeAll), ["own.go"]);
    }

    #[test]
    fn the_filter_decides_before_the_file_is_read() {
        struct SkipDrop;
        impl SourceFilter for SkipDrop {
            fn includes(&self, path: &Path) -> bool {
                path.file_name().is_none_or(|n| n != "drop.go")
            }
        }
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "keep.go", "");
        write(dir.path(), "drop.go", "");
        assert_eq!(names(dir.path(), "go", &SkipDrop), ["keep.go"]);
    }

    #[test]
    fn hidden_directories_are_skipped_and_gitignore_applies_in_a_repository() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("git marker");
        write(dir.path(), ".gitignore", "generated/\n");
        write(dir.path(), "kept.go", "");
        write(dir.path(), "generated/gen.go", "");
        write(dir.path(), ".hidden/h.go", "");
        assert_eq!(names(dir.path(), "go", &IncludeAll), ["kept.go"]);
    }
}
