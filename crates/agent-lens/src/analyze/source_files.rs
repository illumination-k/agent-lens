use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

use super::{AnalyzerError, CompiledPathFilter, SourceLang};

#[derive(Debug)]
pub(crate) struct SourceFile {
    pub path: PathBuf,
    pub display_path: String,
}

pub(crate) fn collect_source_files(
    path: &Path,
    filter: &CompiledPathFilter,
) -> Result<Vec<SourceFile>, AnalyzerError> {
    if path.is_dir() {
        collect_directory_source_files(path, filter)
    } else if filter.includes_path(path) {
        Ok(vec![SourceFile {
            path: path.to_path_buf(),
            display_path: path.display().to_string(),
        }])
    } else {
        Ok(Vec::new())
    }
}

fn collect_directory_source_files(
    root: &Path,
    filter: &CompiledPathFilter,
) -> Result<Vec<SourceFile>, AnalyzerError> {
    let mut out = Vec::new();
    for entry in WalkBuilder::new(root).build() {
        let entry = entry.map_err(|e| AnalyzerError::Io {
            path: root.to_path_buf(),
            source: std::io::Error::other(e),
        })?;
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let p = entry.path();
        if !filter.includes_path(p) || SourceLang::from_path(p).is_none() {
            continue;
        }
        out.push(SourceFile {
            path: p.to_path_buf(),
            display_path: super::relative_display_path(p, root),
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

pub fn read_source(path: &Path) -> Result<(SourceLang, String), AnalyzerError> {
    let lang = SourceLang::from_path(path).ok_or_else(|| AnalyzerError::UnsupportedExtension {
        path: path.to_path_buf(),
    })?;
    let source = std::fs::read_to_string(path).map_err(|source| AnalyzerError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok((lang, source))
}
