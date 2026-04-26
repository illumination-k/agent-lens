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

mod complexity;
mod line_index;
mod parser;
mod tree;
mod walk;

pub use complexity::{ComplexityError, extract_complexity_units};
pub use parser::{TsParseError, TypeScriptParser};
