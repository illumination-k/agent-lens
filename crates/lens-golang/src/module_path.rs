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

/// The name a Go source file's `package` clause declares, read
/// textually: leading `//` and `/* */` comments (build constraints,
/// license headers, doc comments) are skipped, and the first token after
/// them must be `package`. `None` when the file does not open with a
/// package clause.
///
/// This is the one fact the directory cannot supply: an external test
/// package (`package foo_test`) shares its directory with `foo` but is
/// a separate package that sees only `foo`'s exported names.
pub fn package_clause_name(source: &str) -> Option<&str> {
    let mut rest = source.trim_start_matches('\u{feff}');
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("//") {
            rest = after.split_once('\n').map_or("", |(_, tail)| tail);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/")?.1;
        } else {
            break;
        }
    }
    let after = rest.strip_prefix("package")?;
    if !after.starts_with(|c: char| c.is_whitespace() || c == '/') {
        return None;
    }
    let name = after.trim_start_matches([' ', '\t']);
    let end = name
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(name.len());
    (end > 0).then(|| &name[..end])
}

/// Whether `source` belongs to an external test package — a `package
/// <name>_test` clause, which Go only accepts in a `_test.go` file.
pub fn is_external_test_package(source: &str) -> bool {
    package_clause_name(source).is_some_and(|name| name.ends_with("_test"))
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

    #[rstest]
    #[case::plain("package a\n\nfunc F() {}\n", Some("a"))]
    #[case::external_test("package a_test\n", Some("a_test"))]
    #[case::trailing_comment("package main // entry\n", Some("main"))]
    #[case::line_comments("// Copyright\n//go:build linux\n\npackage x\n", Some("x"))]
    #[case::block_comment("/* license\n package fake */\npackage real\n", Some("real"))]
    #[case::bom("\u{feff}package b\n", Some("b"))]
    #[case::no_clause("func F() {}\n", None)]
    #[case::prefix_only("packages x\n", None)]
    #[case::missing_name("package\n", None)]
    #[case::unterminated_block("/* never closed\npackage x\n", None)]
    #[case::empty("", None)]
    fn package_clause_name_reads_the_declared_name(
        #[case] source: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(package_clause_name(source), expected);
    }

    #[rstest]
    #[case("package a_test\n", true)]
    #[case("package a\n", false)]
    #[case("package testing\n", false)]
    #[case("", false)]
    fn external_test_package_needs_the_test_suffix(#[case] source: &str, #[case] expected: bool) {
        assert_eq!(is_external_test_package(source), expected);
    }
}
