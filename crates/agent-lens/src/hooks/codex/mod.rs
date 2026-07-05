//! Codex hook handlers, grouped by event.
//!
//! Each submodule is one hook event; the CLI wires individual handlers
//! to clap subcommands so that typos surface at parse time rather than at
//! runtime. `setup` is a separate one-shot command that writes the
//! handler entries into the user's `~/.codex/config.toml`.

pub mod post_tool_use;
pub mod pre_tool_use;
pub mod session_start;
pub mod setup;

use std::path::Path;

use crate::hooks::core::{
    EditedSource, MissingFilePolicy, ReadEditedSourceError, read_edited_source,
};

/// Tool name Codex uses for the patch-style edit tool.
pub(crate) const APPLY_PATCH_TOOL: &str = "apply_patch";

/// Shared `apply_patch` source-preparation flow for the pre/post hooks:
/// gate on the tool name, pull the patch text out of `tool_input.command`,
/// parse the touched paths with the event-specific `parse_paths`, and
/// read each supported file under the event's missing-file policy.
/// Returns `Ok(vec![])` for "no opinion" cases — non-`apply_patch` tools,
/// missing patch text, or a patch that touches no readable source.
pub(crate) fn prepare_patched_sources(
    tool_name: &str,
    tool_input: &serde_json::Value,
    cwd: &Path,
    parse_paths: impl FnOnce(&str) -> Vec<String>,
    missing_file_policy: MissingFilePolicy,
) -> Result<Vec<EditedSource>, ReadEditedSourceError> {
    if tool_name != APPLY_PATCH_TOOL {
        return Ok(Vec::new());
    }
    let Some(command) = tool_input
        .get("command")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(Vec::new());
    };
    let rel_paths = parse_paths(command);
    let mut out = Vec::with_capacity(rel_paths.len());
    for rel_path in rel_paths {
        if let Some(source) = read_edited_source(cwd, rel_path, missing_file_policy)? {
            out.push(source);
        }
    }
    Ok(out)
}
