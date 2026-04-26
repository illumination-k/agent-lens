//! `PostToolUse` hook handlers for Claude Code.
//!
//! Each submodule is one handler; the CLI wires them to clap
//! subcommands so that typos surface at parse time rather than at
//! runtime. The actual analysis lives in [`crate::hooks::core`]; this
//! module is just the Claude Code adapter (input shape → `EditedSource`
//! list, output string → `systemMessage`).

pub mod similarity;
pub mod wrapper;

pub use similarity::{SimilarityError, SimilarityHook};
pub use wrapper::{WrapperError, WrapperHook};

use std::path::Path;

use agent_hooks::claude_code::PostToolUseInput;

use crate::analyze::SourceLang;
use crate::hooks::core::{EditedSource, ReadEditedSourceError};

/// Tool names whose `tool_input.file_path` points at the file that was
/// just modified. Anything outside this set is ignored.
pub(crate) const EDITING_TOOL_NAMES: &[&str] = &["Write", "Edit", "MultiEdit"];

/// Prepare the edited file for a PostToolUse hook to analyse.
///
/// Returns an empty `Vec` for "no opinion" cases — non-editing tools,
/// missing `file_path`, or an extension the analysers can't handle. The
/// list is at most one element long; returning a `Vec` lets the engine-
/// agnostic core treat Claude Code and Codex inputs the same way.
pub(crate) fn prepare_edited_sources(
    input: &PostToolUseInput,
) -> Result<Vec<EditedSource>, ReadEditedSourceError> {
    if !EDITING_TOOL_NAMES.contains(&input.tool_name.as_str()) {
        return Ok(Vec::new());
    }
    let Some(rel_path) = extract_file_path(&input.tool_input) else {
        return Ok(Vec::new());
    };
    let rel = Path::new(&rel_path);
    let Some(lang) = SourceLang::from_path(rel) else {
        return Ok(Vec::new());
    };
    let abs_path = if rel.is_absolute() {
        rel.to_path_buf()
    } else {
        input.context.cwd.join(rel)
    };
    let source = std::fs::read_to_string(&abs_path).map_err(|source| ReadEditedSourceError {
        path: abs_path,
        source,
    })?;
    Ok(vec![EditedSource {
        rel_path,
        lang,
        source,
    }])
}

fn extract_file_path(tool_input: &serde_json::Value) -> Option<String> {
    tool_input
        .get("file_path")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}
