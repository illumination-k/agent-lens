//! Render-time spelling of module paths.
//!
//! The module graph keeps one canonical [`ModulePath`] shape for every
//! language — a `crate` root with `::`-joined segments — so the graph
//! algorithms, the edge resolvers, and the cross-language tests all
//! compare like with like. That shape is Rust's, and it is wrong on
//! every other language's report: `crate::internal::port::store` is not
//! how a Go package is spelled, and `crate` means something specific
//! and different in the language the word was borrowed from.
//!
//! [`ModuleLabeler`] is the translation layer. It lives at the
//! rendering boundary (report views and the SessionStart summary), so
//! the internal representation stays uniform and only the output learns
//! about per-language path syntax:
//!
//! | language | root module | descendant |
//! | --- | --- | --- |
//! | Rust | `crate` | `crate::analyze::coupling` |
//! | Go (with `go.mod`) | `github.com/x/proj` | `github.com/x/proj/pkg/util` |
//! | Go (no `go.mod`) | `.` | `pkg/util` |
//! | TypeScript / JavaScript | `.` | `components/Chat` |
//! | Python | `.` | `util.text` |

use lens_domain::ModulePath;

/// The canonical root segment every adapter emits.
const CANONICAL_ROOT: &str = "crate";
/// The canonical separator every adapter emits.
const CANONICAL_SEPARATOR: &str = "::";
/// Stand-in for the root module when there is no name to give it. The
/// modules below it are rendered project-relative, so the root itself
/// is "the directory those paths are relative to".
const RELATIVE_ROOT: &str = ".";

/// How a canonical [`ModulePath`] is spelled back to the reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModuleLabeler {
    /// Replacement for the canonical `crate` root, prepended to every
    /// descendant. `None` renders descendants project-relative and
    /// spells the root module itself [`RELATIVE_ROOT`].
    root: Option<String>,
    /// Separator between path segments in the target language.
    separator: &'static str,
}

impl ModuleLabeler {
    /// Rust keeps the canonical spelling: `crate::a::b` is already the
    /// language's own syntax.
    pub(crate) fn rust() -> Self {
        Self {
            root: Some(CANONICAL_ROOT.to_owned()),
            separator: CANONICAL_SEPARATOR,
        }
    }

    /// Go packages are named by their import path. With the `module`
    /// directive from `go.mod` the rendered label is exactly the string
    /// that appears in an `import` block; without one (single-file
    /// roots, fixtures) the label falls back to the package's path
    /// relative to the analysis root.
    pub(crate) fn go(module_prefix: Option<String>) -> Self {
        Self {
            root: module_prefix,
            separator: "/",
        }
    }

    /// One TS/JS module is one file, so the useful label is the file's
    /// path relative to the module tree's source root.
    pub(crate) fn typescript() -> Self {
        Self {
            root: None,
            separator: "/",
        }
    }

    /// Python modules are spelled with dots, and the root package is
    /// the analysis root itself, so descendants stay relative to it.
    pub(crate) fn python() -> Self {
        Self {
            root: None,
            separator: ".",
        }
    }

    /// Spell `path` the way the analyzed language spells it.
    pub(crate) fn label(&self, path: &ModulePath) -> String {
        let tail = descendant_segments(path.as_str());
        if tail.is_empty() {
            return self
                .root
                .clone()
                .unwrap_or_else(|| RELATIVE_ROOT.to_owned());
        }
        let tail = tail.replace(CANONICAL_SEPARATOR, self.separator);
        match &self.root {
            Some(root) => format!("{root}{}{tail}", self.separator),
            None => tail,
        }
    }
}

/// Strip the canonical root from `path`, leaving the `::`-joined
/// segments below it (empty for the root module itself).
///
/// A path that doesn't start with the canonical root is passed through
/// untouched: it can only come from a caller that built a `ModulePath`
/// by hand, and mangling it would hide that rather than surface it.
fn descendant_segments(path: &str) -> &str {
    if path == CANONICAL_ROOT {
        return "";
    }
    path.strip_prefix(CANONICAL_ROOT)
        .and_then(|rest| rest.strip_prefix(CANONICAL_SEPARATOR))
        .unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn go_with_mod() -> ModuleLabeler {
        ModuleLabeler::go(Some("github.com/x/proj".to_owned()))
    }

    #[rstest]
    #[case::rust_root(ModuleLabeler::rust(), "crate", "crate")]
    #[case::rust_nested(
        ModuleLabeler::rust(),
        "crate::analyze::coupling",
        "crate::analyze::coupling"
    )]
    #[case::go_root(go_with_mod(), "crate", "github.com/x/proj")]
    #[case::go_nested(
        go_with_mod(),
        "crate::internal::port::store",
        "github.com/x/proj/internal/port/store"
    )]
    #[case::go_without_manifest_root(ModuleLabeler::go(None), "crate", ".")]
    #[case::go_without_manifest(ModuleLabeler::go(None), "crate::pkg::util", "pkg/util")]
    #[case::ts_root(ModuleLabeler::typescript(), "crate", ".")]
    #[case::ts_nested(
        ModuleLabeler::typescript(),
        "crate::components::Chat",
        "components/Chat"
    )]
    #[case::py_root(ModuleLabeler::python(), "crate", ".")]
    #[case::py_nested(ModuleLabeler::python(), "crate::util::text", "util.text")]
    fn labels_render_per_language(
        #[case] labeler: ModuleLabeler,
        #[case] canonical: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(labeler.label(&ModulePath::new(canonical)), expected);
    }

    /// A segment that merely *starts* with the root word is not the
    /// root: `crate_utils` is an ordinary module name and must survive
    /// intact.
    #[test]
    fn a_segment_prefixed_with_the_root_word_is_not_stripped() {
        let labeler = ModuleLabeler::python();
        assert_eq!(
            labeler.label(&ModulePath::new("crate_utils")),
            "crate_utils"
        );
        assert_eq!(
            labeler.label(&ModulePath::new("crate::crate_utils")),
            "crate_utils"
        );
    }

    /// A path that never carried the canonical root keeps all of its
    /// segments — nothing is stripped as if it were a root — while
    /// still being spelled with the target language's separator.
    #[test]
    fn paths_without_the_canonical_root_keep_every_segment() {
        assert_eq!(
            ModuleLabeler::typescript().label(&ModulePath::new("standalone::mod")),
            "standalone/mod"
        );
    }

    /// The root is dropped only when it is the whole path, so a Go
    /// module whose prefix ends in a segment shared with a package name
    /// still renders both.
    #[test]
    fn go_prefix_and_package_segments_both_appear() {
        assert_eq!(
            ModuleLabeler::go(Some("example.com/util".to_owned()))
                .label(&ModulePath::new("crate::util")),
            "example.com/util/util"
        );
    }
}
