//! The one-or-more paths an analyzer walks.
//!
//! Every file-walking analyzer used to take a single `&Path`, which made
//! a monorepo awkward: the interesting scope is often several sibling
//! trees (`packages cli web/src`) whose only common ancestor is the repo
//! root, and running the analyzer once per tree is not equivalent —
//! cross-file clustering (similarity) and cross-module edges (the call
//! graph) cannot see a relationship that spans two invocations.
//!
//! [`AnalyzeRoots`] is that scope as one value. A single root behaves
//! exactly as the bare `&Path` did, down to the spelling of every display
//! path, so nothing about the one-path case changed. With several roots
//! the shared vocabulary — display paths, `--exclude` globs, the report's
//! `root` field — is anchored at the roots' deepest common ancestor, which
//! is what keeps `src/lib.rs` from meaning two different files in one
//! report.

use std::path::{Component, Path, PathBuf};

/// The analysis scope: one or more roots plus the base their file paths
/// are described against.
///
/// Construct with [`AnalyzeRoots::new`], or from anything a single-root
/// caller already holds — `&Path`, `&PathBuf`, `PathBuf` — via `From`, so
/// `analyzer.analyze(dir, format)` keeps working unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzeRoots {
    roots: Vec<PathBuf>,
    base: PathBuf,
    relative_display: bool,
}

impl AnalyzeRoots {
    /// Build a scope from `roots`, in the order the caller gave them.
    ///
    /// An empty list is not a meaningful target — the CLI requires at
    /// least one path — so it collapses to the current directory, letting
    /// every accessor below promise a non-empty root list.
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        let mut deduped: Vec<PathBuf> = Vec::new();
        for root in roots {
            if !deduped.contains(&root) {
                deduped.push(root);
            }
        }
        if deduped.is_empty() {
            deduped.push(PathBuf::from("."));
        }
        // A single file root keeps the path exactly as the caller spelled
        // it: a one-file report has always named that file, not its
        // basename relative to a directory the caller never mentioned.
        let single_file = deduped.len() == 1 && !deduped[0].is_dir();
        let base = match deduped.as_slice() {
            [only] => only.clone(),
            many => common_ancestor(many),
        };
        Self {
            roots: deduped,
            base,
            relative_display: !single_file,
        }
    }

    /// The roots themselves, in the order given. Never empty.
    pub fn paths(&self) -> &[PathBuf] {
        &self.roots
    }

    /// The single root, when there is exactly one.
    ///
    /// A few behaviours are only defined for a one-root scope — a single
    /// file keeps its spelled display path rather than a base-relative
    /// one, which the churn scope behind `hotspot` and `risk` has to
    /// mirror when it keys graph paths into git's path space — and this
    /// is how that case is recognised.
    pub fn single(&self) -> Option<&Path> {
        match self.roots.as_slice() {
            [only] => Some(only),
            _ => None,
        }
    }

    /// The directory display paths are written relative to.
    ///
    /// For one root this *is* that root (so a caller that used to pass
    /// `path` straight through keeps passing the same value); for several
    /// it is their deepest common ancestor, or `.` when they share none.
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// How this scope is named in a report's `root` field: the single
    /// root verbatim, or every root comma-separated.
    pub fn display(&self) -> String {
        self.roots
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// How `file` is spelled in the report. Relative to [`Self::base`],
    /// except under a single file root, where the caller's own spelling
    /// is the answer.
    pub(crate) fn display_path(&self, file: &Path) -> String {
        if self.relative_display {
            super::relative_display_path(file, &self.base)
        } else {
            file.display().to_string()
        }
    }
}

impl From<&Path> for AnalyzeRoots {
    fn from(path: &Path) -> Self {
        Self::new([path.to_path_buf()])
    }
}

impl From<&PathBuf> for AnalyzeRoots {
    fn from(path: &PathBuf) -> Self {
        Self::new([path.clone()])
    }
}

impl From<PathBuf> for AnalyzeRoots {
    fn from(path: PathBuf) -> Self {
        Self::new([path])
    }
}

impl From<Vec<PathBuf>> for AnalyzeRoots {
    fn from(paths: Vec<PathBuf>) -> Self {
        Self::new(paths)
    }
}

impl From<&AnalyzeRoots> for AnalyzeRoots {
    fn from(roots: &AnalyzeRoots) -> Self {
        roots.clone()
    }
}

/// Deepest path shared by every root, component by component.
///
/// Roots are deduplicated before this runs, so two or more distinct roots
/// always differ in some component and the result is a directory — never
/// one of the roots when that root is a file. Relative roots that share no
/// leading component yield `.` rather than an empty path: the result is
/// used as a filter base and a walk anchor, and both want a real
/// directory.
fn common_ancestor(roots: &[PathBuf]) -> PathBuf {
    let components: Vec<Vec<Component<'_>>> =
        roots.iter().map(|p| p.components().collect()).collect();
    let Some((first, rest)) = components.split_first() else {
        return PathBuf::from(".");
    };
    let shared = first
        .iter()
        .enumerate()
        .take_while(|(idx, component)| rest.iter().all(|other| other.get(*idx) == Some(component)))
        .map(|(_, component)| component.as_os_str())
        .fold(PathBuf::new(), |mut acc, part| {
            acc.push(part);
            acc
        });
    if shared.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        shared
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use rstest::rstest;

    #[test]
    fn a_single_directory_root_is_its_own_base() {
        let dir = tempfile::tempdir().unwrap();
        let roots = AnalyzeRoots::from(dir.path());
        assert_eq!(roots.base(), dir.path());
        assert_eq!(roots.single(), Some(dir.path()));
        assert_eq!(roots.display(), dir.path().display().to_string());
        assert_eq!(
            roots.display_path(&dir.path().join("src/lib.rs")),
            "src/lib.rs"
        );
    }

    /// A single file keeps the caller's spelling — the one case where
    /// display paths are not base-relative.
    #[test]
    fn a_single_file_root_reports_the_path_as_spelled() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "src/lib.rs", "fn a() {}\n");
        let roots = AnalyzeRoots::from(&file);
        assert_eq!(roots.base(), file.as_path());
        assert_eq!(roots.display_path(&file), file.display().to_string());
    }

    #[test]
    fn several_roots_anchor_display_paths_at_their_common_ancestor() {
        let roots = AnalyzeRoots::new([PathBuf::from("crates/a"), PathBuf::from("crates/b")]);
        assert_eq!(roots.base(), Path::new("crates"));
        assert_eq!(roots.single(), None);
        assert_eq!(roots.display(), "crates/a, crates/b");
        assert_eq!(
            roots.display_path(Path::new("crates/a/src/lib.rs")),
            "a/src/lib.rs"
        );
        assert_eq!(
            roots.display_path(Path::new("crates/b/src/lib.rs")),
            "b/src/lib.rs"
        );
    }

    /// Sibling trees with nothing in common are the motivating case: the
    /// base is the current directory, so each file keeps the tree it came
    /// from in its name.
    #[test]
    fn roots_sharing_no_prefix_fall_back_to_the_current_directory() {
        let roots = AnalyzeRoots::new([PathBuf::from("packages"), PathBuf::from("cli")]);
        assert_eq!(roots.base(), Path::new("."));
        assert_eq!(
            roots.display_path(Path::new("packages/x/a.ts")),
            "packages/x/a.ts"
        );
        assert_eq!(roots.display_path(Path::new("cli/main.ts")), "cli/main.ts");
    }

    #[test]
    fn an_empty_root_list_collapses_to_the_current_directory() {
        let roots = AnalyzeRoots::new(Vec::new());
        assert_eq!(roots.paths(), [PathBuf::from(".")]);
        assert_eq!(roots.single(), Some(Path::new(".")));
    }

    #[rstest]
    #[case::siblings(&["crates/a", "crates/b"], "crates")]
    #[case::nested(&["crates", "crates/a/src"], "crates")]
    #[case::file_and_directory(&["pkg/a.ts", "pkg/sub"], "pkg")]
    #[case::absolute(&["/repo/a", "/repo/b"], "/repo")]
    #[case::absolute_root_only(&["/a", "/b"], "/")]
    #[case::disjoint(&["packages", "cli"], ".")]
    fn common_ancestor_is_the_deepest_shared_path(#[case] roots: &[&str], #[case] expected: &str) {
        let roots: Vec<PathBuf> = roots.iter().map(PathBuf::from).collect();
        assert_eq!(common_ancestor(&roots), PathBuf::from(expected));
    }

    /// Repeating a root is a no-op rather than a second walk of the same
    /// tree — and it keeps the single-root spelling rules in force.
    #[test]
    fn duplicate_roots_collapse() {
        let roots = AnalyzeRoots::new([PathBuf::from("crates"), PathBuf::from("crates")]);
        assert_eq!(roots.paths(), [PathBuf::from("crates")]);
        assert_eq!(roots.single(), Some(Path::new("crates")));
    }
}
