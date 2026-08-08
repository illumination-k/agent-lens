//! Canonical TS/JS module-path derivation.
//!
//! A TS/JS module is a file, so its name comes from where the file sits
//! relative to the analysis root. Two consumers need that mapping —
//! [`crate::coupling::build_module_tree`], which reports coupling
//! between file modules, and `agent-lens`' call graph, which qualifies
//! function names with the module they were declared in. They used to
//! derive it separately and disagreed (`routes/index.tsx` was
//! `crate::routes` to one and `routes::index` to the other), so the rule
//! lives here once and both call it.
//!
//! Only the *segments* are shared. Whether they are prefixed with a root
//! name and what an empty result is called are the caller's conventions,
//! not the language's.

use std::path::Path;

use lens_domain::path_segments;

/// Extensions that mark a file as a TS/JS module. The extension names
/// the dialect, not the module, so it is not part of the module path.
const MODULE_EXTENSIONS: &[&str] = &["ts", "tsx", "mts", "cts", "js", "jsx", "mjs", "cjs"];

/// Module-path segments for `rel`, a source file path relative to the
/// analysis root.
///
/// The file's own extension is dropped, and a trailing `index` segment
/// collapses into its directory because `./routes` and
/// `./routes/index.ts` name the same module to the TS resolver.
/// Directory names are kept verbatim, dots included, so a `v1.2/`
/// directory stays one segment.
///
/// Returns no segments for the root file of a single-file analysis
/// (`rel` empty) and for a bare `index.ts` at the root.
pub fn module_segments(rel: &Path) -> Vec<String> {
    let mut segments = path_segments(rel);
    if let Some(last) = segments.last_mut()
        && let Some((stem, ext)) = last.rsplit_once('.')
        && MODULE_EXTENSIONS.contains(&ext)
    {
        *last = stem.to_owned();
    }
    if segments.last().is_some_and(|last| last == "index") {
        segments.pop();
    }
    segments.retain(|segment| !segment.is_empty());
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("main.ts", vec!["main"])]
    #[case("routes/detail.tsx", vec!["routes", "detail"])]
    // `./routes` and `./routes/index.ts` resolve to the same module.
    #[case("routes/index.tsx", vec!["routes"])]
    #[case("index.ts", Vec::<&str>::new())]
    // Only the module extension goes; other dots are part of the name.
    #[case("setup.test.ts", vec!["setup.test"])]
    #[case("v1.2/api.mjs", vec!["v1.2", "api"])]
    // A non-module extension is not a dialect marker, so it stays.
    #[case("data.json", vec!["data.json"])]
    // An `index` directory is not the resolver's `index` file.
    #[case("index/handler.ts", vec!["index", "handler"])]
    // A dotfile whose whole name is an extension leaves nothing behind.
    #[case(".ts", Vec::<&str>::new())]
    #[case("", Vec::<&str>::new())]
    fn module_segments_strip_dialect_extension_and_collapse_index(
        #[case] rel: &str,
        #[case] expected: Vec<&str>,
    ) {
        assert_eq!(module_segments(Path::new(rel)), expected);
    }
}
