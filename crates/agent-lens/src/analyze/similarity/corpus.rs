use std::path::{Path, PathBuf};
use std::time::Instant;

use lens_domain::{BlockWindowOptions, FunctionShape, SignatureShape, TreeNode, block_windows};
use rayon::prelude::*;
use tracing::debug;

use super::FunctionSelection;
use super::PROFILE_TARGET;
use super::SimilarityTarget;
use super::extract::{extract_block_sites, extract_functions, extract_types};
use crate::analyze::{
    AnalyzePathFilter, AnalyzerError, SourceFile, collect_source_files, read_source,
};

/// A single comparison unit plus the file it originated from. The corpus
/// that drives pairwise similarity is a flat `Vec<OwnedUnit>` so
/// cross-file pairs are just regular pairs with different `file`s.
///
/// Both targets share this one type: a type definition is lowered to a
/// [`FunctionShape`] at collection (its member tree as the body, its
/// synthesized member signature as the signature), so pairing, scoring,
/// diff filtering, and clustering never branch on the unit kind.
#[derive(Debug)]
pub(super) struct OwnedUnit {
    /// Filesystem path used for `git diff` lookups.
    pub(super) file: PathBuf,
    /// Display path (relative to the walk root for directory mode).
    pub(super) rel_path: String,
    pub(super) is_test: bool,
    /// Language-facing kind label (`"struct"`, `"interface"`, …) for a
    /// type unit; `None` for a function.
    pub(super) kind: Option<&'static str>,
    /// Extra facts a statement-run unit carries and the other targets do
    /// not; `None` for functions and types.
    pub(super) block: Option<BlockInfo>,
    pub(super) shape: FunctionShape,
}

/// Per-window facts for a [`SimilarityTarget::Blocks`] unit.
#[derive(Debug)]
pub(super) struct BlockInfo {
    pub(super) statement_count: usize,
    /// The window's own source text, capped at [`SNIPPET_MAX_LINES`] and
    /// dedented. Blocks have no name, so a report that only listed
    /// `file:function (L12-16)` would make an agent open every occurrence
    /// to find out what the cluster even is.
    pub(super) snippet: String,
}

/// Lines of a block kept for the representative snippet. Longer windows
/// are truncated with an elision marker: the report is agent context, and
/// the point of the snippet is recognition, not a full reproduction.
const SNIPPET_MAX_LINES: usize = 12;

impl OwnedUnit {
    pub(super) fn name(&self) -> &str {
        &self.shape.display_name
    }

    pub(super) fn start_line(&self) -> usize {
        self.shape.span.start_line
    }

    pub(super) fn end_line(&self) -> usize {
        self.shape.span.end_line
    }

    pub(super) fn line_count(&self) -> usize {
        self.shape.line_count()
    }

    pub(super) fn body_tree(&self) -> &TreeNode {
        self.shape.body_tree()
    }

    pub(super) fn signature(&self) -> Option<&SignatureShape> {
        self.shape.signature_shape()
    }

    pub(super) fn doc(&self) -> Option<&str> {
        self.shape.doc.as_deref()
    }

    pub(super) fn is_type(&self) -> bool {
        self.kind.is_some()
    }

    pub(super) fn block(&self) -> Option<&BlockInfo> {
        self.block.as_ref()
    }
}

/// The source text of lines `start_line..=end_line`, dedented by the
/// common leading whitespace and capped at [`SNIPPET_MAX_LINES`].
fn snippet_for(source: &str, start_line: usize, end_line: usize) -> String {
    let lines: Vec<&str> = source
        .lines()
        .skip(start_line.saturating_sub(1))
        .take(end_line.saturating_sub(start_line) + 1)
        .collect();
    let indent = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out: Vec<String> = lines
        .iter()
        .take(SNIPPET_MAX_LINES)
        .map(|line| line.get(indent..).unwrap_or(line.trim_start()).to_owned())
        .collect();
    if lines.len() > SNIPPET_MAX_LINES {
        out.push(format!("… (+{} lines)", lines.len() - SNIPPET_MAX_LINES));
    }
    out.join("\n")
}

/// Collect every unit of `target`'s kind under `path` into a flat
/// corpus, tagging each with the file it came from. Single-file inputs
/// return a 1-element per-file slice; directory inputs walk recursively,
/// honouring `.gitignore`.
pub(super) fn collect_corpus(
    path: &Path,
    path_filter: &AnalyzePathFilter,
    selection: FunctionSelection,
    target: SimilarityTarget,
    block_opts: BlockWindowOptions,
) -> Result<Vec<OwnedUnit>, AnalyzerError> {
    let collection_filter = if selection == FunctionSelection::OnlyTests {
        path_filter.clone().with_only_tests(false)
    } else {
        path_filter.clone()
    };
    let filter = collection_filter.compile(path)?;
    let started = Instant::now();
    let files = collect_source_files(path, &filter)?;

    let parsed: Vec<Vec<OwnedUnit>> = files
        .par_iter()
        .map(|source_file| {
            let path_is_test = filter.is_test_path(&source_file.path);
            collect_file(source_file, selection, path_is_test, target, block_opts)
        })
        .collect::<Result<_, _>>()?;

    let out: Vec<_> = parsed.into_iter().flatten().collect();
    let file_count = files.len();
    debug!(
        target: PROFILE_TARGET,
        root = %path.display(),
        file_count,
        unit_count = out.len(),
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "similarity corpus directory collected"
    );
    Ok(out)
}

fn collect_file(
    file: &SourceFile,
    selection: FunctionSelection,
    path_is_test: bool,
    target: SimilarityTarget,
    block_opts: BlockWindowOptions,
) -> Result<Vec<OwnedUnit>, AnalyzerError> {
    let started = Instant::now();
    let (lang, source) = read_source(&file.path)?;
    let out: Vec<_> = match target {
        SimilarityTarget::Functions => extract_functions(lang, &source)?
            .into_iter()
            .filter_map(|def| {
                let is_test = def.is_test || path_is_test;
                selection.includes(is_test).then(|| OwnedUnit {
                    file: file.path.clone(),
                    rel_path: file.display_path.clone(),
                    is_test,
                    kind: None,
                    block: None,
                    shape: FunctionShape::from(def),
                })
            })
            .collect(),
        SimilarityTarget::Types => extract_types(lang, &source)?
            .into_iter()
            .filter_map(|type_shape| {
                let is_test = type_shape.is_test || path_is_test;
                selection.includes(is_test).then(|| OwnedUnit {
                    file: file.path.clone(),
                    rel_path: file.display_path.clone(),
                    is_test,
                    kind: Some(type_shape.kind_label),
                    block: None,
                    shape: type_shape.into_function_shape(),
                })
            })
            .collect(),
        SimilarityTarget::Blocks => {
            let sites = extract_block_sites(lang, &source)?;
            block_windows(&sites, &block_opts)
                .into_iter()
                .filter_map(|block| {
                    let is_test = block.is_test || path_is_test;
                    if !selection.includes(is_test) {
                        return None;
                    }
                    let info = BlockInfo {
                        statement_count: block.statement_count,
                        snippet: snippet_for(&source, block.span.start_line, block.span.end_line),
                    };
                    Some(OwnedUnit {
                        file: file.path.clone(),
                        rel_path: file.display_path.clone(),
                        is_test,
                        kind: None,
                        block: Some(info),
                        shape: block.into_function_shape(),
                    })
                })
                .collect()
        }
    };
    debug!(
        target: PROFILE_TARGET,
        path = %file.path.display(),
        language = ?lang,
        bytes = source.len(),
        unit_count = out.len(),
        elapsed_ms = started.elapsed().as_secs_f64() * 1000.0,
        "similarity source parsed"
    );
    Ok(out)
}
