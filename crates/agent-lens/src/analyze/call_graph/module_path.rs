//! Per-language module-path derivation for graph nodes.
//!
//! The call graph qualifies function names with a module path derived
//! from the file's location (and, for Rust, the surrounding
//! `Cargo.toml`). This is intentionally file-path based rather than
//! reusing the lens crates' module trees: the graph needs a total
//! function over arbitrary single files and synthetic roots, including
//! files no module tree claims.

use std::path::Path;

use crate::analyze::cargo_meta::{CrateInfo, FALLBACK_CRATE_NAME};
use crate::analyze::{SourceFile, SourceLang};

pub(crate) fn module_path_for(
    root: &Path,
    file: &SourceFile,
    lang: SourceLang,
    crate_info: Option<&CrateInfo>,
    source: &str,
) -> String {
    if !root.is_dir() {
        return match lang {
            SourceLang::Rust => crate_info
                .map(|info| info.crate_name.clone())
                .unwrap_or_else(|| FALLBACK_CRATE_NAME.to_owned()),
            SourceLang::TypeScript(_) => "module".to_owned(),
            SourceLang::Python => python_module_path_from_relative_file(&file.display_path),
            SourceLang::Go => go_module_path_from_file(&file.display_path, source),
        };
    }
    match lang {
        SourceLang::Rust => rust_module_path_for_file(file, crate_info),
        SourceLang::TypeScript(_) => ts_module_path_from_relative_file(&file.display_path),
        SourceLang::Python => python_module_path_from_relative_file(&file.display_path),
        SourceLang::Go => go_module_path_from_file(&file.display_path, source),
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
    let mut rel = file.replace('\\', "/");
    for ext in [".tsx", ".ts", ".jsx", ".js", ".mts", ".cts", ".mjs", ".cjs"] {
        if let Some(stripped) = rel.strip_suffix(ext) {
            rel = stripped.to_owned();
            break;
        }
    }
    let module = rel
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("::");
    if module.is_empty() {
        "module".to_owned()
    } else {
        module
    }
}

fn python_module_path_from_relative_file(file: &str) -> String {
    let mut rel = file.replace('\\', "/");
    if let Some(stripped) = rel.strip_suffix(".py") {
        rel = stripped.to_owned();
    }
    let mut segments: Vec<&str> = rel
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.last() == Some(&"__init__") {
        segments.pop();
    }
    if segments.is_empty() {
        "module".to_owned()
    } else {
        segments.join("::")
    }
}

/// Go's compilation unit is a *package* (directory), not a single file,
/// so two `.go` files sharing a directory must collapse to the same
/// module path. Use the parent-directory segments when the file lives
/// in a subdirectory; for files that sit directly at the analyzer's
/// root, peek at the `package` clause and use the declared package
/// name. That keeps a one-file project's qualified names stable
/// (`main::caller` rather than the `module::caller` placeholder a bare
/// path-based heuristic would produce).
fn go_module_path_from_file(file: &str, source: &str) -> String {
    let rel = file.replace('\\', "/");
    let segments: Vec<&str> = rel
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() > 1 {
        return segments[..segments.len() - 1].join("::");
    }
    extract_go_package_name(source).unwrap_or_else(|| "module".to_owned())
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

    #[test]
    fn module_paths_handle_crate_roots_nested_files_and_empty_relative_paths() {
        assert_eq!(rust_module_path_from_relative_file("lib.rs"), "crate");
        assert_eq!(rust_module_path_from_relative_file("main.rs"), "crate");
        assert_eq!(
            rust_module_path_from_relative_file("src/analyze/function_graph.rs"),
            "crate::analyze::function_graph"
        );
        assert_eq!(
            rust_module_path_from_relative_file("src/analyze/mod.rs"),
            "crate::analyze"
        );
        assert_eq!(rust_module_path_from_relative_file(""), "crate");
        assert_eq!(
            ts_module_path_from_relative_file("routes/index.tsx"),
            "routes::index"
        );
        assert_eq!(ts_module_path_from_relative_file("main.ts"), "main");
        assert_eq!(python_module_path_from_relative_file("main.py"), "main");
        assert_eq!(
            python_module_path_from_relative_file("pkg/sub/main.py"),
            "pkg::sub::main"
        );
        assert_eq!(
            python_module_path_from_relative_file("pkg/__init__.py"),
            "pkg"
        );
        assert_eq!(python_module_path_from_relative_file(""), "module");

        assert_eq!(
            go_module_path_from_file("main.go", "package main\n\nfunc main() {}\n"),
            "main",
        );
        assert_eq!(
            go_module_path_from_file("pkg/util/util.go", "package util\n"),
            "pkg::util",
        );
        // No trailing parent and a missing package clause fall back to
        // the placeholder so qualified names stay non-empty.
        assert_eq!(
            go_module_path_from_file("loose.go", "// pkg-less\n"),
            "module"
        );
        // The package-name scanner must skip *both* blank lines and
        // line comments (the `||` between the two checks). With `||`
        // flipped to `&&`, the comment line would not be skipped, the
        // function would bail on the first comment, and the package
        // clause below would never be reached.
        assert_eq!(
            go_module_path_from_file("solo.go", "// header\n\npackage solo\nfunc f() {}\n"),
            "solo",
        );
    }
}
