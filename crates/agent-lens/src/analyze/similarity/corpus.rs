use std::path::PathBuf;
use std::time::Instant;

use lens_domain::{BlockWindowOptions, FunctionShape, SignatureShape, TreeNode, block_windows};
use rayon::prelude::*;
use tracing::debug;

use super::FunctionSelection;
use super::PROFILE_TARGET;
use super::SimilarityTarget;
use super::extract::{extract_functions, extract_statement_seqs, extract_types};
use crate::analyze::{
    AnalyzePathFilter, AnalyzeRoots, AnalyzerError, SourceFile, collect_source_files, read_source,
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
    /// Trait this function implements, when the language marks it
    /// syntactically (`impl Trait for Type` methods). Two units carrying
    /// the same trait name share their signature by construction, so
    /// scoring drops the signature component for such pairs.
    pub(super) implements: Option<String>,
    pub(super) shape: FunctionShape,
}

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

    pub(super) fn implements(&self) -> Option<&str> {
        self.implements.as_deref()
    }
}

/// Collect every unit of `target`'s kind under `roots` into a flat
/// corpus, tagging each with the file it came from. Single-file inputs
/// return a 1-element per-file slice; directory inputs walk recursively,
/// honouring `.gitignore`. Several roots are walked into one corpus, so a
/// cluster spanning two of them is still found.
///
/// `min_lines` is only consulted for [`SimilarityTarget::Blocks`], where
/// it bounds the window population at collection time rather than
/// filtering afterwards: a corpus of every statement run regardless of
/// length would be an order of magnitude larger than the one anybody
/// asked for.
pub(super) fn collect_corpus(
    roots: &AnalyzeRoots,
    path_filter: &AnalyzePathFilter,
    selection: FunctionSelection,
    target: SimilarityTarget,
    min_lines: usize,
) -> Result<Vec<OwnedUnit>, AnalyzerError> {
    let collection_filter = if selection == FunctionSelection::OnlyTests {
        path_filter.clone().with_only_tests(false)
    } else {
        path_filter.clone()
    };
    let filter = collection_filter.compile(roots.base())?;
    let started = Instant::now();
    let files = collect_source_files(roots, &filter)?;

    let parsed: Vec<Vec<OwnedUnit>> = files
        .par_iter()
        .map(|source_file| {
            let path_is_test = filter.is_test_path(&source_file.path);
            collect_file(source_file, selection, path_is_test, target, min_lines)
        })
        .collect::<Result<_, _>>()?;

    let out: Vec<_> = parsed.into_iter().flatten().collect();
    let file_count = files.len();
    debug!(
        target: PROFILE_TARGET,
        root = %roots.display(),
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
    min_lines: usize,
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
                    implements: def.implements.clone(),
                    shape: FunctionShape::from(def),
                })
            })
            .collect(),
        SimilarityTarget::Types => extract_types(lang, &source)?
            .into_iter()
            // A definition nothing was extracted from is not a small
            // shape, it is an absent one: it matches every other empty
            // shape at 1.0 regardless of what the two declarations say.
            // `--min-lines` cannot catch these — a marker interface and
            // an empty enum can both be spelled across several lines.
            .filter(|type_shape| !type_shape.is_shapeless())
            .filter_map(|type_shape| {
                let is_test = type_shape.is_test || path_is_test;
                selection.includes(is_test).then(|| OwnedUnit {
                    file: file.path.clone(),
                    rel_path: file.display_path.clone(),
                    is_test,
                    kind: Some(type_shape.kind_label),
                    implements: None,
                    shape: type_shape.into_function_shape(),
                })
            })
            .collect(),
        SimilarityTarget::Blocks => {
            let seqs: Vec<_> = extract_statement_seqs(lang, &source)?
                .into_iter()
                .filter(|seq| selection.includes(seq.is_test || path_is_test))
                .map(|mut seq| {
                    seq.is_test = seq.is_test || path_is_test;
                    seq
                })
                .collect();
            block_windows(
                &seqs,
                BlockWindowOptions {
                    min_lines,
                    ..BlockWindowOptions::default()
                },
            )
            .into_iter()
            .map(|window| OwnedUnit {
                file: file.path.clone(),
                rel_path: file.display_path.clone(),
                is_test: window.is_test,
                kind: None,
                implements: None,
                shape: window.into_function_shape(),
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
