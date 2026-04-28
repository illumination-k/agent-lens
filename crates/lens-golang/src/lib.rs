//! Go-language adapter for `agent-lens` similarity and complexity analysis.
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
mod complexity;
mod parser;

pub use complexity::extract_complexity_units;
pub use parser::{GoParseError, GoParser, extract_functions_excluding_tests};
