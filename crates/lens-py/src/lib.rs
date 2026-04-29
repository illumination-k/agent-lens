//! Python-language adapter for `agent-lens` similarity analysis.
//!
//! Implements [`lens_domain::LanguageParser`] on top of the
//! [`ruff_python_parser`], extracting every top-level `def` / `async def`
//! and method inside a `class` into a [`lens_domain::FunctionDef`]. The body
//! is lowered to a generic [`lens_domain::TreeNode`] by walking the AST so
//! that control-flow statements (`if`, `while`, `for`, `match`, …) land in
//! the tree as distinct nodes that the APTED algorithm can tell apart.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod attrs;
mod cohesion;
mod complexity;
mod context_span;
mod coupling;
mod line_index;
mod parser;
mod wrapper;

pub use cohesion::{CohesionError, extract_cohesion_units};
pub use complexity::{ComplexityError, extract_complexity_units};
pub use context_span::{build_context_span_report, extract_context_spans};
pub use coupling::{CouplingError, PythonModule, build_module_tree, extract_edges};
pub use parser::{PythonParseError, PythonParser};
pub use wrapper::{WrapperError, find_wrappers};
