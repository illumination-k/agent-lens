use std::path::{Path, PathBuf};
use std::time::Instant;

use lens_domain::{FunctionDef, TestFilter};
use rayon::prelude::*;
use tracing::debug;

use super::PROFILE_TARGET;
use super::extract::extract_functions;
use crate::analyze::{
    AnalyzePathFilter, AnalyzerError, SourceFile, collect_source_files, read_source,
};

/// A single function plus the file it originated from. The corpus that
/// drives pairwise similarity is a flat `Vec<OwnedFunction>` so cross-file
/// pairs are just regular pairs with different `file`s.
#[derive(Debug)]
pub(super) struct OwnedFunction {
    /// Filesystem path used for `git diff` lookups.
    pub(super) file: PathBuf,
    /// Display path (relative to the walk root for directory mode).
    pub(super) rel_path: String,
    pub(super) def: FunctionDef,
}

/// Collect every function under `path` into a flat corpus, tagging each
/// with the file it came from. Single-file inputs return a 1-element
/// per-file slice; directory inputs walk recursively, honouring `.gitignore`.
pub(super) fn collect_corpus(
    path: &Path,
    path_filter: &AnalyzePathFilter,
    test_filter: TestFilter,
) -> Result<Vec<OwnedFunction>, AnalyzerError> {
    let collection_filter = if test_filter == TestFilter::Only {
        path_filter.clone().with_only_tests(false)
    } else {
        path_filter.clone()
    };
    let filter = collection_filter.compile(path)?;
    let started = Instant::now();
    let files = collect_source_files(path, &filter)?;

    let parsed: Vec<Vec<OwnedFunction>> = files
        .par_iter()
        .map(|source_file| {
            let file_filter =
                if test_filter == TestFilter::Only && filter.is_test_path(&source_file.path) {
                    TestFilter::All
                } else {
                    test_filter
                };
            collect_file(source_file, file_filter)
        })
        .collect::<Result<_, _>>()?;

    let out: Vec<_> = parsed.into_iter().flatten().collect();
    let file_count = files.len();
    debug!(
        target: PROFILE_TARGET,
        root = %path.display(),
        file_count,
        function_count = out.len(),
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "similarity corpus directory collected"
    );
    Ok(out)
}

fn collect_file(
    file: &SourceFile,
    test_filter: TestFilter,
) -> Result<Vec<OwnedFunction>, AnalyzerError> {
    let started = Instant::now();
    let (lang, source) = read_source(&file.path)?;
    let funcs = extract_functions(lang, &source, test_filter)?;
    let out: Vec<_> = funcs
        .into_iter()
        .map(|def| OwnedFunction {
            file: file.path.clone(),
            rel_path: file.display_path.clone(),
            def,
        })
        .collect();
    debug!(
        target: PROFILE_TARGET,
        path = %file.path.display(),
        language = ?lang,
        bytes = source.len(),
        function_count = out.len(),
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "similarity source parsed"
    );
    Ok(out)
}
