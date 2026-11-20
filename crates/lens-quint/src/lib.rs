//! Quint-language adapter for `agent-lens` analysis.
//!
//! Quint's parser ships only inside the `quint` npm package, so this
//! adapter runs `quint parse` and reads the JSON IR and source map it
//! writes (see [`cli`]). The `quint` executable must be on `PATH`, or
//! named by [`QUINT_BIN_ENV`]; without it every extractor fails with
//! [`QuintError::NotInstalled`].
//!
//! The unit of analysis is a top-level operator definition (`def`,
//! `pure def`, `action`, `val`, `temporal`, `run`); `run` definitions
//! count as tests. Coupling is measured between Quint `module`s.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod cli;
mod complexity;
mod coupling;
mod defs;
mod dependence;
mod ir;
mod parser;
#[cfg(test)]
mod test_support;
mod unsupported;
mod wrapper;

pub use cli::{QUINT_BIN_ENV, QuintError};
pub use complexity::{ComplexityError, extract_complexity_units};
pub use coupling::{QuintModule, build_module_tree, extract_edges};
pub use defs::BUILTIN_OPCODES;
pub use dependence::QuintVocabulary;
pub use lens_domain::CouplingError;
pub use parser::QuintParser;
pub use unsupported::{
    CohesionError, extract_cohesion_units, extract_function_shapes_with_module,
    extract_statement_seqs, extract_type_defs,
};
pub use wrapper::{CallRef, extract_call_refs, find_wrappers};
