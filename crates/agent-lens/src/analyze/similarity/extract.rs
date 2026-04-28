use lens_domain::{FunctionDef, LanguageParser};
use lens_rust::{RustParser, extract_functions_excluding_tests};

use crate::analyze::{AnalyzerError, SourceLang};

pub(super) fn extract_functions(
    lang: SourceLang,
    source: &str,
    exclude_tests: bool,
) -> Result<Vec<FunctionDef>, AnalyzerError> {
    match lang {
        SourceLang::Rust => extract_rust(source, exclude_tests),
        SourceLang::TypeScript(dialect) => extract_typescript(source, dialect, exclude_tests),
        SourceLang::Python => extract_python(source, exclude_tests),
        SourceLang::Go => extract_go(source, exclude_tests),
    }
    .map_err(AnalyzerError::Parse)
}

type ExtractError = Box<dyn std::error::Error + Send + Sync>;

fn extract_rust(source: &str, exclude_tests: bool) -> Result<Vec<FunctionDef>, ExtractError> {
    if exclude_tests {
        extract_functions_excluding_tests(source).map_err(Into::into)
    } else {
        RustParser::new()
            .extract_functions(source)
            .map_err(Into::into)
    }
}

fn extract_typescript(
    source: &str,
    dialect: lens_ts::Dialect,
    exclude_tests: bool,
) -> Result<Vec<FunctionDef>, ExtractError> {
    if exclude_tests {
        lens_ts::extract_functions_excluding_tests(source, dialect).map_err(Into::into)
    } else {
        <lens_ts::TypeScriptParser as LanguageParser>::extract_functions(
            &mut lens_ts::TypeScriptParser::with_dialect(dialect),
            source,
        )
        .map_err(Into::into)
    }
}

fn extract_python(source: &str, exclude_tests: bool) -> Result<Vec<FunctionDef>, ExtractError> {
    if exclude_tests {
        lens_py::extract_functions_excluding_tests(source).map_err(Into::into)
    } else {
        lens_py::PythonParser::new()
            .extract_functions(source)
            .map_err(Into::into)
    }
}

fn extract_go(source: &str, exclude_tests: bool) -> Result<Vec<FunctionDef>, ExtractError> {
    if exclude_tests {
        lens_golang::extract_functions_excluding_tests(source).map_err(Into::into)
    } else {
        lens_golang::GoParser::new()
            .extract_functions(source)
            .map_err(Into::into)
    }
}
