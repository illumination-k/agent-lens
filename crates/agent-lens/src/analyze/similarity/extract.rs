use lens_domain::FunctionDef;

use crate::analyze::{AnalyzerError, SourceLang};

pub(super) fn extract_functions(
    lang: SourceLang,
    source: &str,
) -> Result<Vec<FunctionDef>, AnalyzerError> {
    let mut parser = lang.create_language_parser();
    parser
        .extract_functions(source)
        .map_err(|err| AnalyzerError::Parse(Box::new(err)))
}
