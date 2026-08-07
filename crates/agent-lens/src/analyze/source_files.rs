use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use super::{AnalyzeRoots, AnalyzerError, CompiledPathFilter, SourceLang};

#[derive(Debug)]
pub(crate) struct SourceFile {
    pub path: PathBuf,
    pub display_path: String,
}

/// Every supported source file under `roots`, deterministically ordered
/// and deduplicated.
///
/// Roots are walked in turn, so overlapping trees (`packages` and
/// `packages/core`) yield each file once: display paths are anchored at
/// the roots' shared base, which makes the same file the same entry
/// whichever root reached it.
pub(crate) fn collect_source_files(
    roots: &AnalyzeRoots,
    filter: &CompiledPathFilter,
) -> Result<Vec<SourceFile>, AnalyzerError> {
    let mut out = Vec::new();
    for root in roots.paths() {
        collect_root_source_files(root, roots, filter, &mut out)?;
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out.dedup_by(|a, b| a.path == b.path);
    Ok(out)
}

fn collect_root_source_files(
    root: &Path,
    roots: &AnalyzeRoots,
    filter: &CompiledPathFilter,
    out: &mut Vec<SourceFile>,
) -> Result<(), AnalyzerError> {
    // Checked before the file/directory split: an extension-less path
    // that does not exist would otherwise take the single-file branch
    // and be reported as an unsupported extension, which sends the
    // reader looking for a language-support problem.
    if !root.exists() {
        return Err(AnalyzerError::PathNotFound {
            path: root.to_path_buf(),
        });
    }
    if !root.is_dir() {
        if filter.includes_path(root) {
            out.push(SourceFile {
                path: root.to_path_buf(),
                display_path: roots.display_path(root),
            });
        }
        return Ok(());
    }
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
            display_path: roots.display_path(p),
        });
    }
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::AnalyzePathFilter;
    use crate::test_support::write_file;

    fn collect(roots: &AnalyzeRoots) -> Vec<String> {
        let filter = AnalyzePathFilter::new().compile(roots.base()).unwrap();
        collect_source_files(roots, &filter)
            .unwrap()
            .into_iter()
            .map(|f| f.display_path)
            .collect()
    }

    /// The motivating case: sibling trees with no useful common ancestor
    /// are walked into one file list, and each file keeps the tree it
    /// came from in its name so the two `lib.rs` files stay distinct.
    #[test]
    fn sibling_roots_are_walked_into_one_list_with_unambiguous_names() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "packages/core/src/lib.rs", "fn a() {}\n");
        write_file(dir.path(), "cli/src/lib.rs", "fn b() {}\n");
        write_file(dir.path(), "web/src/app.ts", "export function c() {}\n");

        let roots = AnalyzeRoots::new([dir.path().join("packages"), dir.path().join("cli")]);
        assert_eq!(
            collect(&roots),
            ["cli/src/lib.rs", "packages/core/src/lib.rs"],
            "the untouched `web` tree must stay out",
        );
    }

    /// Overlapping roots must not double-count: the shared display base
    /// makes a file the same entry whichever walk reached it.
    #[test]
    fn overlapping_roots_yield_each_file_once() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "pkg/core/lib.rs", "fn a() {}\n");
        write_file(dir.path(), "pkg/other.rs", "fn b() {}\n");

        let roots = AnalyzeRoots::new([dir.path().join("pkg"), dir.path().join("pkg/core")]);
        assert_eq!(collect(&roots), ["core/lib.rs", "other.rs"]);
    }

    /// A file and a directory mix: both are valid roots, and the file's
    /// name is base-relative like everything else once there is more
    /// than one root.
    #[test]
    fn a_file_root_and_a_directory_root_can_be_combined() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "loose.rs", "fn a() {}\n");
        write_file(dir.path(), "pkg/lib.rs", "fn b() {}\n");

        let roots = AnalyzeRoots::new([file, dir.path().join("pkg")]);
        assert_eq!(collect(&roots), ["loose.rs", "pkg/lib.rs"]);
    }

    /// One missing root fails the whole walk rather than quietly
    /// reporting on the roots that do exist — a typo'd path must not
    /// read as "nothing to report there".
    #[test]
    fn a_missing_root_is_an_error_even_when_another_root_exists() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "pkg/lib.rs", "fn a() {}\n");

        let roots = AnalyzeRoots::new([dir.path().join("pkg"), dir.path().join("nope")]);
        let filter = AnalyzePathFilter::new().compile(roots.base()).unwrap();
        let err = collect_source_files(&roots, &filter).unwrap_err();
        assert!(matches!(err, AnalyzerError::PathNotFound { .. }), "{err:?}");
    }

    /// The single-root spellings are unchanged: a directory root gives
    /// root-relative names, a file root gives the path as spelled.
    #[test]
    fn a_single_root_keeps_its_established_display_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "pkg/lib.rs", "fn a() {}\n");

        assert_eq!(collect(&AnalyzeRoots::from(dir.path())), ["pkg/lib.rs"]);
        assert_eq!(
            collect(&AnalyzeRoots::from(&file)),
            [file.display().to_string()],
        );
    }
}
