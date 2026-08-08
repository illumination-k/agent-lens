//! Go-language adapter for `agent-lens` analysis.
//!
//! Implements [`lens_domain::LanguageParser`] on top of
//! [`tree-sitter-go`](https://crates.io/crates/tree-sitter-go), pulling
//! every top-level `func` declaration and method into a
//! [`lens_domain::FunctionDef`]. Method names are qualified as
//! `Receiver::method` (with the leading `*` stripped from pointer
//! receivers) so two methods on the same type stay distinguishable in
//! similarity reports.
//!
//! Bodies are lowered to a generic [`lens_domain::TreeNode`] by walking
//! the named-child structure of the tree-sitter parse so that
//! control-flow nodes (`if_statement`, `for_statement`,
//! `expression_switch_statement`, …) land as distinct labels that the
//! APTED algorithm can tell apart.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod attrs;
mod call_index;
mod cohesion;
mod complexity;
mod context_span;
mod coupling;
mod interfaces;
mod method_names;
mod module_path;
mod node_text;
mod parser;
#[cfg(test)]
mod proptests;
mod statements;
#[cfg(test)]
mod test_support;
mod type_defs;
mod walk;
mod wrapper;

pub use call_index::{extract_call_shapes_with_module, extract_function_shapes_with_module};
pub use cohesion::extract_cohesion_units;
pub use complexity::extract_complexity_units;
pub use context_span::{build_context_span_report, extract_context_spans};
pub use coupling::{CouplingError, GoPackage, build_module_tree, extract_edges, module_prefix};
pub use interfaces::extract_interface_shapes_with_module;
pub use method_names::{BUILTIN_FUNCTION_NAMES, INERT_ATTRIBUTE_NAMES, UBIQUITOUS_METHOD_NAMES};
pub use module_path::package_segments;
pub use parser::{GoParseError, GoParser};
pub use statements::extract_statement_seqs;
pub use type_defs::extract_type_defs;
pub use wrapper::find_wrappers;
