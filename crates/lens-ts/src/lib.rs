//! TypeScript / JavaScript adapter for `agent-lens` similarity analysis.
//!
//! Implements [`lens_domain::LanguageParser`] on top of the
//! [oxlint parser (`oxc_parser`)](https://docs.rs/oxc_parser), pulling
//! every function-shaped item — `function` declarations, methods on
//! classes, and arrow / function expressions bound to a `const`/`let`/
//! `var` — into a [`lens_domain::FunctionDef`]. Bodies are lowered to
//! generic [`lens_domain::TreeNode`]s by walking the AST so that
//! structural keywords (`If`, `While`, `For`, `Switch`, …) land in the
//! tree as distinct nodes that the APTED algorithm can tell apart.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod attrs;
mod cohesion;
mod complexity;
mod coupling;
mod line_index;
mod parser;
mod tree;
mod walk;
mod wrapper;

pub use cohesion::{CohesionError, extract_cohesion_units};
pub use complexity::{ComplexityError, extract_complexity_units};
pub use coupling::{CouplingError, TsModule, build_module_tree, extract_edges};
pub use parser::{Dialect, TsParseError, TypeScriptParser, extract_functions_excluding_tests};
pub use wrapper::find_wrappers;
