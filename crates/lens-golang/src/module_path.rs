//! Canonical Go package-path derivation.
//!
//! Go's compilation unit is a *package*, which is a directory rather
//! than a file: two `.go` files sharing a directory belong to one
//! package and must resolve to one path. Two consumers need that
//! mapping — [`crate::coupling::build_module_tree`], which reports
//! coupling between packages, and `agent-lens`' call graph, which
//! qualifies function names with the package they were declared in — so
//! the rule lives here once and both call it.
//!
//! Only the *segments* are shared. Whether they are prefixed with a root
//! name and what an empty result is called are the caller's conventions,
//! not the language's.

use std::path::Path;

use lens_domain::path_segments;

/// Package-path segments for `dir`, a package directory relative to the
/// analysis root.
///
/// Directory names carry through verbatim: unlike a file-per-module
/// language there is no extension to strip and no index file to
/// collapse. Callers holding a file path pass its parent, so every file
/// in a package lands on the same segments.
///
/// Returns no segments for the package sitting at the analysis root
/// itself. That package's name is not recoverable from the path — it is
/// declared in the source's `package` clause — so callers that need a
/// name for it supply their own.
pub fn package_segments(dir: &Path) -> Vec<String> {
    path_segments(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("pkg/util", vec!["pkg", "util"])]
    #[case("internal", vec!["internal"])]
    // Dots are ordinary characters in a directory name.
    #[case("api/v1.2", vec!["api", "v1.2"])]
    // The root package's name lives in its `package` clause, not here.
    #[case("", Vec::<&str>::new())]
    #[case(".", Vec::<&str>::new())]
    fn package_segments_follow_the_directory(#[case] dir: &str, #[case] expected: Vec<&str>) {
        assert_eq!(package_segments(Path::new(dir)), expected);
    }
}
