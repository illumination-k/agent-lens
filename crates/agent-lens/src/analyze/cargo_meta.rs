//! Lightweight `Cargo.toml` lookup used by the function-graph analyzer.
//!
//! Walks up from a given source file to find the nearest `Cargo.toml`
//! that declares `[package].name` and returns the Rust crate name
//! (hyphens normalised to underscores). Used to qualify module paths
//! with a real crate prefix instead of the literal `crate`, so
//! same-named items in different workspace crates do not collide in
//! the function-graph resolver.
//!
//! Intentionally minimal: no `cargo metadata` invocation, no
//! `[lib].path` / `[lib].name` overrides, no workspace inheritance
//! beyond walking up to the next manifest. When no manifest is found
//! the caller falls back to the historical `"crate"` literal.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use toml_edit::DocumentMut;

/// Crate-name fallback when no enclosing `Cargo.toml` declares one.
pub(crate) const FALLBACK_CRATE_NAME: &str = "crate";

/// Memoised crate-name lookups keyed by the directory containing the
/// source file. Many files share a directory, so the cache turns N
/// disk reads into one per directory.
#[derive(Debug, Default)]
pub(crate) struct CrateNameCache {
    by_dir: HashMap<PathBuf, CrateInfo>,
}

#[derive(Debug, Clone)]
pub(crate) struct CrateInfo {
    /// Rust crate name (hyphens replaced with underscores). Falls
    /// back to [`FALLBACK_CRATE_NAME`] when no manifest was found.
    pub crate_name: String,
    /// Directory holding the manifest that produced `crate_name`.
    /// `None` when the fallback was used.
    pub crate_root: Option<PathBuf>,
}

impl CrateNameCache {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Resolve the crate that owns `file`. Walks up parent
    /// directories looking for a `Cargo.toml` with `[package].name`.
    pub(crate) fn lookup(&mut self, file: &Path) -> CrateInfo {
        let dir = file.parent().unwrap_or(file).to_path_buf();
        if let Some(cached) = self.by_dir.get(&dir) {
            return cached.clone();
        }
        let info = lookup_uncached(&dir);
        self.by_dir.insert(dir, info.clone());
        info
    }
}

fn lookup_uncached(start: &Path) -> CrateInfo {
    let mut current = Some(start);
    while let Some(dir) = current {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file()
            && let Some(name) = read_package_name(&manifest)
        {
            return CrateInfo {
                crate_name: rust_crate_name(&name),
                crate_root: Some(dir.to_path_buf()),
            };
        }
        current = dir.parent();
    }
    CrateInfo {
        crate_name: FALLBACK_CRATE_NAME.to_owned(),
        crate_root: None,
    }
}

fn read_package_name(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let doc = text.parse::<DocumentMut>().ok()?;
    doc.get("package")
        .and_then(|t| t.as_table_like())
        .and_then(|t| t.get("name"))
        .and_then(|n| n.as_value())
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
}

fn rust_crate_name(package_name: &str) -> String {
    package_name.replace('-', "_")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;

    #[test]
    fn returns_fallback_when_no_manifest_found() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src/lib.rs");
        let mut cache = CrateNameCache::new();
        let info = cache.lookup(&file);
        assert_eq!(info.crate_name, FALLBACK_CRATE_NAME);
        assert!(info.crate_root.is_none());
    }

    #[test]
    fn finds_nearest_package_manifest() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "Cargo.toml", "[package]\nname = \"my-pkg\"\n");
        write_file(dir.path(), "src/lib.rs", "");
        let mut cache = CrateNameCache::new();
        let info = cache.lookup(&dir.path().join("src/lib.rs"));
        assert_eq!(info.crate_name, "my_pkg");
        assert_eq!(info.crate_root.as_deref(), Some(dir.path()));
    }

    #[test]
    fn skips_workspace_only_manifest_and_finds_member() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        write_file(
            dir.path(),
            "crates/agent-lens/Cargo.toml",
            "[package]\nname = \"agent-lens\"\n",
        );
        write_file(dir.path(), "crates/agent-lens/src/lib.rs", "");
        let mut cache = CrateNameCache::new();
        let info = cache.lookup(&dir.path().join("crates/agent-lens/src/lib.rs"));
        assert_eq!(info.crate_name, "agent_lens");
        assert_eq!(
            info.crate_root.as_deref(),
            Some(dir.path().join("crates/agent-lens").as_path()),
        );
    }

    #[test]
    fn caches_lookups_per_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "Cargo.toml", "[package]\nname = \"pkg\"\n");
        write_file(dir.path(), "src/a.rs", "");
        write_file(dir.path(), "src/b.rs", "");
        let mut cache = CrateNameCache::new();
        let info_a = cache.lookup(&dir.path().join("src/a.rs"));
        let info_b = cache.lookup(&dir.path().join("src/b.rs"));
        assert_eq!(info_a.crate_name, "pkg");
        assert_eq!(info_b.crate_name, "pkg");
        // Both calls touched the same directory; the cache should now
        // hold exactly one entry.
        assert_eq!(cache.by_dir.len(), 1);
    }

    #[test]
    fn rust_crate_name_normalises_hyphens() {
        assert_eq!(rust_crate_name("agent-lens"), "agent_lens");
        assert_eq!(rust_crate_name("plain"), "plain");
        assert_eq!(rust_crate_name("a-b-c"), "a_b_c");
    }
}
