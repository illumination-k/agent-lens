//! Hook handlers, grouped by agent and then by event.
//!
//! The Claude Code handlers live directly under `post_tool_use` for
//! historical reasons (they were the first to land); Codex handlers are
//! namespaced under [`codex`]. The CLI wires each handler to a clap
//! subcommand so typos surface at parse time.
//!
//! Only the engine-specific adapters are per-agent. The analysis and the
//! runners live in [`core`], and the `settings.json` / `config.toml`
//! merge lives in [`setup_engine`]; [`setup`] and [`codex::setup`] each
//! contribute just the format half of it.

pub mod codex;
pub mod core;
pub mod post_tool_use;
pub mod pre_tool_use;
pub mod session_start;
pub mod setup;
pub mod setup_engine;

use std::path::Path;

use crate::hooks::core::{
    EditedSource, MissingFilePolicy, ReadEditedSourceError, read_edited_source,
};

/// Claude Code tool names that modify a source file. Shared by the
/// pre/post hook adapters so both gates stay in lock-step.
pub(crate) const EDITING_TOOL_NAMES: &[&str] = &["Write", "Edit", "MultiEdit"];

/// Shared single-file source-preparation flow for the Claude Code
/// pre/post hooks: gate on the editing tool names, pull
/// `tool_input.file_path`, and read the file under the event's
/// missing-file policy. Returns `Ok(vec![])` for "no opinion" cases —
/// non-editing tools, missing `file_path`, or an extension the analysers
/// can't handle. The list is at most one element long; returning a `Vec`
/// lets the engine-agnostic core treat Claude Code and Codex inputs the
/// same way.
pub(crate) fn prepare_single_edited_source(
    tool_name: &str,
    tool_input: &serde_json::Value,
    cwd: &Path,
    missing_file_policy: MissingFilePolicy,
) -> Result<Vec<EditedSource>, ReadEditedSourceError> {
    if !EDITING_TOOL_NAMES.contains(&tool_name) {
        return Ok(Vec::new());
    }
    let Some(rel_path) = tool_input
        .get("file_path")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(Vec::new());
    };
    Ok(
        read_edited_source(cwd, rel_path.to_owned(), missing_file_policy)?
            .into_iter()
            .collect(),
    )
}
