use std::path::{Path, PathBuf};

use super::{CrateAnalyzerError, SourceLang};

pub fn resolve_crate_root(path: &Path) -> Result<PathBuf, CrateAnalyzerError> {
    let meta = std::fs::metadata(path).map_err(|source| CrateAnalyzerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if meta.is_file() && SourceLang::from_path(path) == Some(SourceLang::Rust) {
        return Ok(path.to_path_buf());
    }
    if meta.is_dir()
        && let Some(probe) = first_existing_file(path, &["src/lib.rs", "src/main.rs"])
    {
        return Ok(probe);
    }
    Err(CrateAnalyzerError::UnsupportedRoot {
        path: path.to_path_buf(),
    })
}

fn first_existing_file(root: &Path, candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|c| root.join(c))
        .find(|p| p.is_file())
}
