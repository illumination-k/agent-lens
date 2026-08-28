//! Per-language module-path derivation for graph nodes.
//!
//! The call graph qualifies function names with a module path derived
//! from the file's location (and, for Rust, the surrounding
//! `Cargo.toml`). It cannot call a lens crate's `build_module_tree`:
//! that walks and parses a whole tree to answer "which modules exist",
//! whereas the graph needs a *total* function from one already-read file
//! to a name, including files no module tree claims (a `#[cfg(test)]`
//! module, a second binary target, a synthetic single-file root).
//!
//! The *segment rules* are nonetheless the lens crates' own — this
//! module calls [`lens_ts::module_segments`], [`lens_py::module_segments`]
//! and [`lens_golang::package_segments`], the same functions their
//! `build_module_tree` uses — so `routes/index.tsx` cannot be
//! `crate::routes` to the coupling analyzer and `routes::index` here.
//! What stays local is the part that is genuinely a call-graph policy
//! rather than a language rule:
//!
//! * **Root prefix.** A coupling module tree roots every language at the
//!   literal `crate`. Graph nodes are read as names, so only Rust takes a
//!   root segment, and it takes the real crate name (see #374).
//! * **Fallbacks.** A module tree can decline a file; a graph node still
//!   needs a name, hence `module` for a path that yields no segments and
//!   the `package`-clause peek for a Go file at the root.
//!
//! Rust is the one language whose derivation is not shared, because
//! `lens_rust::build_module_tree` does not derive one: it reads `mod`
//! items, so a file's path is only reachable by walking from a crate
//! root that may not include it, and every crate it names is the literal
//! `crate` — which is exactly the collision this module's `Cargo.toml`
//! lookup exists to avoid across a workspace.

use std::path::{Path, PathBuf};

use crate::analyze::cargo_meta::{CrateInfo, FALLBACK_CRATE_NAME};
use crate::analyze::{SourceFile, SourceLang};

/// Name for a module whose path yields no segments at all — a file
/// sitting at the analysis root of a single-file run, where a module
/// tree would have said `crate`. Graph nodes drop that prefix, so they
/// need their own placeholder to keep qualified names non-empty.
const ROOTLESS_MODULE: &str = "module";

pub(crate) fn module_path_for(
    root: &Path,
    file: &SourceFile,
    lang: SourceLang,
    crate_info: Option<&CrateInfo>,
    source: &str,
) -> String {
    // A single-file root leaves `display_path` holding just the file
    // name, with no directory layout to read a module path out of. Only
    // two languages care: Rust and TS/JS take a whole layout to place a
    // file, so they name the root outright instead of inventing a module
    // from a bare file name. A Python module *is* its file and a Go file
    // declares its own package, so those two need no special case.
    let single_file_root = !root.is_dir();
    match lang {
        SourceLang::Rust if single_file_root => crate_info
            .map(|info| info.crate_name.clone())
            .unwrap_or_else(|| FALLBACK_CRATE_NAME.to_owned()),
        SourceLang::Rust => rust_module_path_for_file(file, crate_info),
        SourceLang::TypeScript(_) if single_file_root => ROOTLESS_MODULE.to_owned(),
        SourceLang::TypeScript(_) => ts_module_path_from_relative_file(&file.display_path),
        SourceLang::Python => python_module_path_from_relative_file(&file.display_path),
        SourceLang::Go => go_module_path_from_file(&file.display_path, source),
    }
}

/// Join `segments` into a `::`-separated path, falling back to
/// [`ROOTLESS_MODULE`] when the file sits at the analysis root and the
/// language's rules leave nothing behind.
fn join_or_placeholder(segments: Vec<String>) -> String {
    if segments.is_empty() {
        ROOTLESS_MODULE.to_owned()
    } else {
        segments.join("::")
    }
}

/// Compute the absolute Rust module path for `file`. When the
/// surrounding `Cargo.toml` is known, qualify with the real crate
/// name and resolve the file relative to `<crate_root>/src/` so
/// workspace analyses no longer collapse every crate under literal
/// `crate::` (which made same-named items collide).
///
/// Falls back to the legacy display-path heuristic with a literal
/// `crate` prefix when no manifest was found, preserving behaviour
/// for single-file analyses and tests that build a synthetic tree
/// without a `Cargo.toml`.
fn rust_module_path_for_file(file: &SourceFile, crate_info: Option<&CrateInfo>) -> String {
    let Some(info) = crate_info else {
        return rust_module_path_from_relative_file(&file.display_path);
    };
    let Some(crate_root) = info.crate_root.as_deref() else {
        return rust_module_path_from_relative_file(&file.display_path);
    };
    let src_root = crate_root.join("src");
    let relative = file
        .path
        .strip_prefix(&src_root)
        .ok()
        .map(|p| p.display().to_string().replace('\\', "/"))
        .unwrap_or_else(|| file.display_path.replace('\\', "/"));
    qualify_rust_module_segments(&relative, &info.crate_name)
}

fn rust_module_path_from_relative_file(file: &str) -> String {
    let mut rel = file.replace('\\', "/");
    if let Some(stripped) = rel.strip_prefix("src/") {
        rel = stripped.to_owned();
    }
    qualify_rust_module_segments(&rel, FALLBACK_CRATE_NAME)
}

fn qualify_rust_module_segments(rel: &str, crate_name: &str) -> String {
    if rel == "lib.rs" || rel == "main.rs" {
        return crate_name.to_owned();
    }
    let trimmed = if let Some(stripped) = rel.strip_suffix("/mod.rs") {
        stripped
    } else if let Some(stripped) = rel.strip_suffix(".rs") {
        stripped
    } else {
        rel
    };
    let module = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("::");
    if module.is_empty() {
        crate_name.to_owned()
    } else {
        format!("{crate_name}::{module}")
    }
}

fn ts_module_path_from_relative_file(file: &str) -> String {
    join_or_placeholder(lens_ts::module_segments(&normalize_separators(file)))
}

fn python_module_path_from_relative_file(file: &str) -> String {
    join_or_placeholder(lens_py::module_segments(&normalize_separators(file)))
}

/// Go's compilation unit is a *package* (directory), not a single file,
/// so two `.go` files sharing a directory must collapse to the same
/// module path — hence the file's parent, not the file itself, feeds
/// [`lens_golang::package_segments`].
///
/// A file sitting directly at the analyzer's root has no parent
/// segments, and unlike the other languages the package name is not
/// recoverable from the path. Peek at the `package` clause instead, so a
/// one-file project's qualified names stay stable (`main::caller` rather
/// than the `module::caller` placeholder).
fn go_module_path_from_file(file: &str, source: &str) -> String {
    let rel = normalize_separators(file);
    let segments = rel
        .parent()
        .map(lens_golang::package_segments)
        .unwrap_or_default();
    if !segments.is_empty() {
        return segments.join("::");
    }
    extract_go_package_name(source).unwrap_or_else(|| ROOTLESS_MODULE.to_owned())
}

/// Read a `display_path` as a [`PathBuf`].
///
/// `Path` splits on `\` only when compiled for Windows, so a
/// backslash-separated path would otherwise collapse into a single
/// segment on Unix. `/` is a separator on both platforms, so rewriting
/// to it is enough to make the segment rules host-independent.
fn normalize_separators(file: &str) -> PathBuf {
    PathBuf::from(file.replace('\\', "/"))
}

/// Pluck the package name out of the first `package <name>` line.
/// Skips line comments and blank lines; ignores `package` keywords
/// that appear inside block comments because the simple scan can't
/// see the comment boundaries — that's acceptable since `gofmt`-clean
/// Go always declares its package on the first non-comment line.
fn extract_go_package_name(source: &str) -> Option<String> {
    for raw in source.lines() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("package")
            && let Some(first) = rest.split_whitespace().next()
        {
            return Some(first.to_owned());
        }
        // First non-comment, non-blank line that wasn't a package
        // clause: bail out — anything else means we wandered past the
        // header and the file is malformed for our purposes.
        return None;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn source_file(display_path: &str) -> SourceFile {
        SourceFile {
            path: PathBuf::from(display_path),
            display_path: display_path.to_owned(),
            explicit: false,
        }
    }

    /// Whether the root is a directory decides two of the four
    /// languages, so pin both sides of that guard. With a single file as
    /// the root there is no layout to place it in: Rust names the crate
    /// and TS/JS the placeholder, rather than reading the bare file name
    /// as a module. Point the same file at a directory root and the
    /// name comes back.
    #[rstest]
    #[case(SourceLang::Rust, "helper.rs", "crate", "crate::helper")]
    #[case(
        SourceLang::TypeScript(lens_ts::Dialect::Ts),
        "helper.ts",
        "module",
        "helper"
    )]
    fn single_file_roots_name_the_root_not_the_file(
        #[case] lang: SourceLang,
        #[case] file: &str,
        #[case] as_single_file: &str,
        #[case] under_a_directory: &str,
    ) {
        let source_file = source_file(file);
        // A path that does not exist is not a directory, which is what
        // a single-file root looks like here.
        assert_eq!(
            module_path_for(Path::new(file), &source_file, lang, None, ""),
            as_single_file,
        );
        assert_eq!(
            module_path_for(Path::new("."), &source_file, lang, None, ""),
            under_a_directory,
        );
    }

    #[rstest]
    #[case("lib.rs", "crate")]
    #[case("main.rs", "crate")]
    #[case("src/analyze/function_graph.rs", "crate::analyze::function_graph")]
    #[case("src/analyze/mod.rs", "crate::analyze")]
    #[case("", "crate")]
    fn rust_module_paths_handle_crate_roots_and_nested_files(
        #[case] file: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(rust_module_path_from_relative_file(file), expected);
    }

    #[rstest]
    #[case("main.ts", "main")]
    #[case("routes/detail.tsx", "routes::detail")]
    // Matches `lens_ts::module_segments`, and so the coupling analyzer:
    // `routes/index.tsx` is the module `routes`, not `routes::index`.
    #[case("routes/index.tsx", "routes")]
    // Nothing survives the root's own index file, so the graph's
    // placeholder stands in for the `crate` a module tree would use.
    #[case("index.ts", "module")]
    #[case("", "module")]
    fn ts_module_paths_follow_the_lens_ts_segment_rules(
        #[case] file: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(ts_module_path_from_relative_file(file), expected);
    }

    #[rstest]
    #[case("main.py", "main")]
    #[case("pkg/sub/main.py", "pkg::sub::main")]
    #[case("pkg/__init__.py", "pkg")]
    #[case("__init__.py", "module")]
    #[case("", "module")]
    fn python_module_paths_follow_the_lens_py_segment_rules(
        #[case] file: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(python_module_path_from_relative_file(file), expected);
    }

    #[rstest]
    // A package is its directory, so every file in it shares a path.
    #[case("pkg/util/util.go", "package util\n", "pkg::util")]
    #[case("pkg/util/helper.go", "package util\n", "pkg::util")]
    // At the root there is no directory to name the package after, so
    // the `package` clause supplies the name.
    #[case("main.go", "package main\n\nfunc main() {}\n", "main")]
    // The package-name scanner must skip *both* blank lines and line
    // comments (the `||` between the two checks). With `||` flipped to
    // `&&`, the comment line would not be skipped, the function would
    // bail on the first comment, and the package clause below would
    // never be reached.
    #[case("solo.go", "// header\n\npackage solo\nfunc f() {}\n", "solo")]
    // No parent directory and no package clause: fall back to the
    // placeholder so qualified names stay non-empty.
    #[case("loose.go", "// pkg-less\n", "module")]
    fn go_module_paths_name_the_package_directory(
        #[case] file: &str,
        #[case] source: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(go_module_path_from_file(file, source), expected);
    }

    /// `display_path` is `/`-separated, but a Windows-shaped path must
    /// not collapse into one segment when the analyzer runs on Unix.
    #[rstest]
    #[case("pkg\\sub\\main.py", "pkg::sub::main")]
    #[case("pkg/sub/main.py", "pkg::sub::main")]
    fn backslash_separated_paths_split_the_same_way(#[case] file: &str, #[case] expected: &str) {
        assert_eq!(python_module_path_from_relative_file(file), expected);
    }
}
