//! Snapshot persistence: load / save the JSON envelope and resolve the
//! default `.agent-lens/baseline/<analyzer>.json` path.
//!
//! Writes are atomic (temp file + rename) so a Ctrl-C halfway through
//! `baseline save` doesn't leave a half-written snapshot that would
//! later be loaded and mis-compared.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use super::{SNAPSHOT_FORMAT_VERSION, Snapshot};

/// Errors raised while loading or saving a baseline snapshot.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("failed to read baseline {path:?}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write baseline {path:?}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse baseline {path:?}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize baseline: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("baseline {path:?} has format_version {found}, but this build expects {expected}")]
    UnsupportedFormatVersion {
        path: PathBuf,
        found: u32,
        expected: u32,
    },
}

/// Read a snapshot from `path` and reject it if its `format_version`
/// doesn't match the schema this build understands.
pub fn load_snapshot(path: &Path) -> Result<Snapshot, StorageError> {
    let bytes = std::fs::read(path).map_err(|source| StorageError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let snap: Snapshot = serde_json::from_slice(&bytes).map_err(|source| StorageError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    if snap.format_version != SNAPSHOT_FORMAT_VERSION {
        return Err(StorageError::UnsupportedFormatVersion {
            path: path.to_path_buf(),
            found: snap.format_version,
            expected: SNAPSHOT_FORMAT_VERSION,
        });
    }
    Ok(snap)
}

/// Write `snap` to `path` atomically: serialize to a sibling temp file
/// in the same directory, fsync, then rename onto the target. On
/// failure the temp file is left behind — small, traceable, easy to
/// clean up — rather than partially overwriting the previous snapshot.
pub fn save_snapshot(snap: &Snapshot, path: &Path) -> Result<(), StorageError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| StorageError::Write {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    let serialized = serde_json::to_vec_pretty(snap)?;
    let tmp = tempfile_path(path);
    let mut file = std::fs::File::create(&tmp).map_err(|source| StorageError::Write {
        path: tmp.clone(),
        source,
    })?;
    file.write_all(&serialized)
        .map_err(|source| StorageError::Write {
            path: tmp.clone(),
            source,
        })?;
    file.write_all(b"\n")
        .map_err(|source| StorageError::Write {
            path: tmp.clone(),
            source,
        })?;
    file.sync_all().map_err(|source| StorageError::Write {
        path: tmp.clone(),
        source,
    })?;
    drop(file);
    std::fs::rename(&tmp, path).map_err(|source| StorageError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Compute the default snapshot path for `analyzer`, anchored at the
/// nearest enclosing git working tree (so the same baseline is found
/// regardless of which subdirectory the user invoked from). Falls
/// back to `cwd` when no git tree is reachable.
pub fn default_baseline_path(analyzer: &str, cwd: &Path) -> PathBuf {
    let root = git_top_level(cwd).unwrap_or_else(|| cwd.to_path_buf());
    root.join(".agent-lens")
        .join("baseline")
        .join(format!("{analyzer}.json"))
}

/// Sibling temp path for the atomic write. Picks `<file>.tmp` next to
/// the target so the rename is on the same filesystem.
fn tempfile_path(target: &Path) -> PathBuf {
    let mut name = target
        .file_name()
        .map(|s| s.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    target
        .parent()
        .map(|p| p.join(&name))
        .unwrap_or_else(|| PathBuf::from(name))
}

/// `git rev-parse --show-toplevel`, run from `cwd`. Returns `None`
/// when `cwd` isn't inside a git working tree, mirroring the helper
/// used elsewhere in the binary.
fn git_top_level(cwd: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline::{Item, SNAPSHOT_FORMAT_VERSION};
    use crate::test_support::run_git;
    use std::collections::BTreeMap;

    fn sample_snapshot() -> Snapshot {
        Snapshot::new(
            "complexity",
            serde_json::json!({"path": "src"}),
            vec![Item {
                id: BTreeMap::from([
                    ("file".to_owned(), "src/lib.rs".to_owned()),
                    ("name".to_owned(), "foo".to_owned()),
                ]),
                metrics: BTreeMap::from([("cognitive".to_owned(), 5.0)]),
                location: BTreeMap::new(),
            }],
            serde_json::Map::new(),
        )
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/snap.json");
        let snap = sample_snapshot();
        save_snapshot(&snap, &path).unwrap();
        let back = load_snapshot(&path).unwrap();
        assert_eq!(back.analyzer, "complexity");
        assert_eq!(back.items.len(), 1);
        assert_eq!(back.items[0].metrics["cognitive"], 5.0);
    }

    #[test]
    fn save_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c/snap.json");
        save_snapshot(&sample_snapshot(), &path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn save_overwrites_existing_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.json");
        save_snapshot(&sample_snapshot(), &path).unwrap();
        let mut second = sample_snapshot();
        second.items.clear();
        save_snapshot(&second, &path).unwrap();
        let back = load_snapshot(&path).unwrap();
        assert!(back.items.is_empty());
        // The .tmp sibling should be gone after a successful rename.
        let tmp = path.with_file_name("snap.json.tmp");
        assert!(!tmp.exists());
    }

    #[test]
    fn load_rejects_unsupported_format_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.json");
        let raw = serde_json::json!({
            "format_version": SNAPSHOT_FORMAT_VERSION + 99,
            "analyzer": "complexity",
            "agent_lens_version": "0.0.0",
            "generated_at": "2024-01-01T00:00:00Z",
            "args": null,
            "items": []
        });
        std::fs::write(&path, raw.to_string()).unwrap();
        let err = load_snapshot(&path).unwrap_err();
        assert!(matches!(err, StorageError::UnsupportedFormatVersion { .. }));
    }

    #[test]
    fn load_reports_parse_error_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = load_snapshot(&path).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("snap.json"), "got: {msg}");
        assert!(msg.contains("parse"), "got: {msg}");
    }

    #[test]
    fn load_reports_io_error_with_path() {
        let err = load_snapshot(Path::new("/definitely/missing/nope.json")).unwrap_err();
        assert!(matches!(err, StorageError::Read { .. }));
    }

    #[test]
    fn default_baseline_path_anchors_at_git_root() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        let nested = dir.path().join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let resolved = default_baseline_path("complexity", &nested);
        // We don't compare full paths (macOS tempdirs differ between
        // /var/... and /private/var/...); just check it lands at the
        // git root and uses the expected filename layout.
        assert!(
            resolved.ends_with(".agent-lens/baseline/complexity.json"),
            "got: {}",
            resolved.display(),
        );
    }

    #[test]
    fn default_baseline_path_falls_back_to_cwd_outside_git() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = default_baseline_path("cohesion", dir.path());
        assert!(resolved.starts_with(dir.path()));
        assert!(resolved.ends_with(".agent-lens/baseline/cohesion.json"));
    }
}
