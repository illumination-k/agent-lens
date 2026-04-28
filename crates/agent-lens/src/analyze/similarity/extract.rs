use lens_domain::{FunctionDef, TestFilter};

use crate::analyze::{AnalyzerError, SourceLang};

pub(super) fn extract_functions(
    lang: SourceLang,
    source: &str,
    test_filter: TestFilter,
) -> Result<Vec<FunctionDef>, AnalyzerError> {
    let mut parser = lang.create_language_parser(test_filter);
    parser
        .extract_functions(source)
        .map_err(|err| AnalyzerError::Parse(Box::new(err)))
}
