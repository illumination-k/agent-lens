//! Rust-language adapter for `agent-lens` similarity analysis.
//!
//! Implements [`lens_domain::LanguageParser`] on top of [`syn`], extracting
//! every free, `impl`-bound, and `trait`-default function into a
//! [`lens_domain::FunctionDef`]. Each function is lowered to a generic
//! [`lens_domain::TreeNode`] by projecting the `syn` signature and body into
//! syntax categories such as parameters, type paths, calls, method calls, and
//! control-flow nodes that the APTED algorithm can tell apart.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod attrs;
mod call_index;
mod cohesion;
mod common;
mod complexity;
mod coupling;
mod parser;
mod wrapper;

pub use call_index::{
    CallIndexOptions, CallKind, CallSite, UseAlias,
    extract_call_shapes_with_options_and_base_module, extract_call_sites,
    extract_call_sites_with_options, extract_call_sites_with_options_and_base_module,
};
pub use cohesion::{CohesionError, extract_cohesion_units};
pub use complexity::{ComplexityError, extract_complexity_units};
pub use coupling::{CouplingError, CrateModule, build_module_tree, extract_edges};
pub use lens_domain::WrapperFinding;
pub use parser::{
    RustFunctionDef, RustParseError, RustParser, extract_function_shapes_with_modules,
    extract_functions_with_modules,
};
pub use wrapper::find_wrappers;
