use lens_domain::{BlockSite, FunctionDef, TypeShape};

use crate::analyze::{AnalyzerError, SourceLang, dispatch_lens};

pub(super) fn extract_functions(
    lang: SourceLang,
    source: &str,
) -> Result<Vec<FunctionDef>, AnalyzerError> {
    let mut parser = lang.create_language_parser();
    parser
        .extract_functions(source)
        .map_err(|err| AnalyzerError::Parse(Box::new(err)))
}

pub(super) fn extract_types(
    lang: SourceLang,
    source: &str,
) -> Result<Vec<TypeShape>, AnalyzerError> {
    dispatch_lens!(lang, source, extract_type_defs).map_err(AnalyzerError::Parse)
}

/// Every statement sequence inside every function, ready to be cut into
/// sliding windows by [`lens_domain::block_windows`].
pub(super) fn extract_block_sites(
    lang: SourceLang,
    source: &str,
) -> Result<Vec<BlockSite>, AnalyzerError> {
    dispatch_lens!(lang, source, extract_blocks).map_err(AnalyzerError::Parse)
}
