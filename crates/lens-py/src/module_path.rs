//! Canonical Python module-path derivation.
//!
//! A Python module is a file and a package is a directory, so both come
//! from where the file sits relative to the analysis root. Two consumers
//! need that mapping — [`crate::coupling::build_module_tree`], which
//! reports coupling between modules, and `agent-lens`' call graph, which
//! qualifies function names with the module they were declared in — so
//! the rule lives here once and both call it.
//!
//! Only the *segments* are shared. Whether they are prefixed with a root
//! name and what an empty result is called are the caller's conventions,
//! not the language's.

use std::path::Path;

use lens_domain::path_segments;

/// Module-path segments for `rel`, a `.py` file path relative to the
/// analysis root.
///
/// The `.py` extension is dropped, and a trailing `__init__` collapses
/// into its directory because `pkg/__init__.py` *is* the package `pkg`,
/// not a submodule of it.
///
/// Returns no segments for the root file of a single-file analysis
/// (`rel` empty) and for a bare `__init__.py` at the root.
pub fn module_segments(rel: &Path) -> Vec<String> {
    let mut segments = path_segments(rel);
    if let Some(last) = segments.last_mut()
        && let Some(stem) = last.strip_suffix(".py")
    {
        *last = stem.to_owned();
    }
    if segments.last().is_some_and(|last| last == "__init__") {
        segments.pop();
    }
    segments.retain(|segment| !segment.is_empty());
    segments
}

/// Resolve the lexical base module of a `from X import ...` statement.
///
/// Returns `None` when a relative import outruns the available depth
/// (e.g. `from ... import x` in a top-level file).
pub(crate) fn resolve_from_base(current: &str, level: u32, module: Option<&str>) -> Option<String> {
    let mut segments: Vec<String> = if level == 0 || current.is_empty() {
        Vec::new()
    } else {
        current.split("::").map(ToOwned::to_owned).collect()
    };
    if level != 0 {
        let pops = level as usize;
        if pops > segments.len() {
            return None;
        }
        segments.truncate(segments.len() - pops);
    }
    if let Some(module) = module
        && !module.is_empty()
    {
        segments.extend(module.split('.').map(ToOwned::to_owned));
    }
    Some(segments.join("::"))
}

pub(crate) fn dotted_to_module_path(dotted: &str) -> String {
    dotted
        .split('.')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("::")
}

pub(crate) fn top_segment(dotted: &str) -> &str {
    dotted.split('.').next().unwrap_or(dotted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("main.py", vec!["main"])]
    #[case("pkg/sub/main.py", vec!["pkg", "sub", "main"])]
    // `pkg/__init__.py` is the package itself, not a child of it.
    #[case("pkg/__init__.py", vec!["pkg"])]
    #[case("__init__.py", Vec::<&str>::new())]
    // Only the trailing `.py` goes; other dots belong to the name.
    #[case("pkg/test.helpers.py", vec!["pkg", "test.helpers"])]
    // Stub and non-Python files keep their suffix — they are not `.py`.
    #[case("pkg/types.pyi", vec!["pkg", "types.pyi"])]
    #[case(".py", Vec::<&str>::new())]
    #[case("", Vec::<&str>::new())]
    fn module_segments_strip_py_suffix_and_collapse_init(
        #[case] rel: &str,
        #[case] expected: Vec<&str>,
    ) {
        assert_eq!(module_segments(Path::new(rel)), expected);
    }
}
