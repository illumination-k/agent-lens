//! `PostToolUse` hook handlers.
//!
//! Each submodule is one handler; the CLI wires them to clap
//! subcommands so that typos surface at parse time rather than at
//! runtime.

pub mod similarity;
pub mod wrapper;

pub use similarity::{SimilarityError, SimilarityHook};
pub use wrapper::{WrapperError, WrapperHook};

use std::path::{Path, PathBuf};

use agent_hooks::claude_code::PostToolUseInput;

use crate::analyze::SourceLang;

/// Tool names whose `tool_input.file_path` points at the file that was
/// just modified. Anything outside this set is ignored.
pub(crate) const EDITING_TOOL_NAMES: &[&str] = &["Write", "Edit", "MultiEdit"];

/// One file that an agent just edited, prepared for a hook to analyze.
pub(crate) struct EditedSource {
    /// `file_path` as it appeared in `tool_input` — the original string is
    /// what hooks include in user-facing reports.
    pub rel_path: String,
    pub lang: SourceLang,
    pub source: String,
}

/// IO failure raised while preparing an [`EditedSource`].
///
/// Each hook converts this into its own `Io` variant via `From`, keeping
/// the public hook errors stable while the shared pipeline owns one
/// canonical IO shape.
#[derive(Debug)]
pub(crate) struct ReadEditedSourceError {
    pub path: PathBuf,
    pub source: std::io::Error,
}

/// Prepare the edited file for a PostToolUse hook to analyze.
///
/// Returns `Ok(None)` for "no opinion" cases — non-editing tools, missing
/// `file_path`, or an extension the analyzers can't handle. Returns
/// `Ok(Some(...))` once the file has been read into memory and the
/// language has been resolved, leaving the analyzer-specific work to the
/// caller.
pub(crate) fn prepare_edited_source(
    input: &PostToolUseInput,
) -> Result<Option<EditedSource>, ReadEditedSourceError> {
    if !EDITING_TOOL_NAMES.contains(&input.tool_name.as_str()) {
        return Ok(None);
    }
    let Some(rel_path) = extract_file_path(&input.tool_input) else {
        return Ok(None);
    };
    let rel = Path::new(&rel_path);
    let Some(lang) = SourceLang::from_path(rel) else {
        return Ok(None);
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
    Ok(Some(EditedSource {
        rel_path,
        lang,
        source,
    }))
}

fn extract_file_path(tool_input: &serde_json::Value) -> Option<String> {
    tool_input
        .get("file_path")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}
