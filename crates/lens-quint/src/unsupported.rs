//! Extractor families Quint has no counterpart for, or that this adapter
//! does not cover yet. Each returns an empty list without running
//! `quint`, so an analyzer that cannot use Quint costs no Node start-up
//! per `.qnt` file and raises no missing-CLI warning.
//!
//! * cohesion — Quint has no `impl` / class to measure LCOM4 over.
//! * statement windows — Quint bodies are single expressions.
//! * type definitions — not extracted yet.
//! * function shapes — the call graph does not read Quint yet.

use lens_domain::{CohesionUnit, FunctionShape, StatementSeq, TypeShape};

use crate::cli::QuintError;

pub type CohesionError = QuintError;

pub fn extract_cohesion_units(_source: &str) -> Result<Vec<CohesionUnit>, CohesionError> {
    Ok(Vec::new())
}

pub fn extract_statement_seqs(_source: &str) -> Result<Vec<StatementSeq>, QuintError> {
    Ok(Vec::new())
}

pub fn extract_type_defs(_source: &str) -> Result<Vec<TypeShape>, QuintError> {
    Ok(Vec::new())
}

pub fn extract_function_shapes_with_module(
    _source: &str,
    _module: &str,
) -> Result<Vec<FunctionShape>, QuintError> {
    Ok(Vec::new())
}
