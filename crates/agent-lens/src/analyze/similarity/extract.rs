use lens_domain::{FunctionDef, StatementSeq, TypeShape};

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

pub(super) fn extract_statement_seqs(
    lang: SourceLang,
    source: &str,
) -> Result<Vec<StatementSeq>, AnalyzerError> {
    dispatch_lens!(lang, source, extract_statement_seqs).map_err(AnalyzerError::Parse)
}
