//! Codex `PostToolUse` hook handlers.
//!
//! Codex's only source-modifying tool today is `apply_patch`, which carries
//! the entire patch as a single string in `tool_input.command`. The shared
//! pipeline here parses that envelope, walks the `*** Update File:` and
//! `*** Add File:` markers, and reads each touched file off disk so each
//! handler can focus on the analysis it actually wants to run.

pub mod similarity;
pub mod wrapper;

pub use similarity::{SimilarityError, SimilarityHook};
pub use wrapper::{WrapperError, WrapperHook};

use std::path::{Path, PathBuf};

use agent_hooks::codex::PostToolUseInput;

use crate::analyze::SourceLang;

/// Tool name Codex uses for the patch-style edit tool.
pub(crate) const APPLY_PATCH_TOOL: &str = "apply_patch";

/// One file that Codex just patched, prepared for a hook to analyze.
pub(crate) struct EditedSource {
    /// Path as it appeared inside the patch envelope — kept verbatim so
    /// hooks can quote it back to the agent without surprising it with
    /// resolved absolute paths.
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

/// Prepare every patched source file that the analyzers can handle.
///
/// Returns `Ok(vec![])` for "no opinion" cases — non-`apply_patch` tools,
/// missing patch text, or a patch that only touches files in unsupported
/// languages. `*** Delete File:` entries are skipped because the file is
/// gone by the time the hook runs.
pub(crate) fn prepare_edited_sources(
    input: &PostToolUseInput,
) -> Result<Vec<EditedSource>, ReadEditedSourceError> {
    if input.tool_name != APPLY_PATCH_TOOL {
        return Ok(Vec::new());
    }
    let Some(command) = extract_patch_command(&input.tool_input) else {
        return Ok(Vec::new());
    };

    let rel_paths = parse_patched_paths(&command);
    let mut out = Vec::with_capacity(rel_paths.len());
    for rel_path in rel_paths {
        let rel = Path::new(&rel_path);
        let Some(lang) = SourceLang::from_path(rel) else {
            continue;
        };
        let abs_path = if rel.is_absolute() {
            rel.to_path_buf()
        } else {
            input.context.cwd.join(rel)
        };
        let source =
            std::fs::read_to_string(&abs_path).map_err(|source| ReadEditedSourceError {
                path: abs_path,
                source,
            })?;
        out.push(EditedSource {
            rel_path,
            lang,
            source,
        });
    }
    Ok(out)
}

fn extract_patch_command(tool_input: &serde_json::Value) -> Option<String> {
    tool_input
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Pull `*** Update File: ...` and `*** Add File: ...` paths out of an
/// `apply_patch` envelope.
fn parse_patched_paths(command: &str) -> Vec<String> {
    const MARKERS: &[&str] = &["*** Update File: ", "*** Add File: "];
    let mut out = Vec::new();
    for line in command.lines() {
        let trimmed = line.trim_start();
        for marker in MARKERS {
            if let Some(rest) = trimmed.strip_prefix(marker) {
                let path = rest.trim();
                if !path.is_empty() {
                    out.push(path.to_owned());
                }
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_update_and_add_markers() {
        let patch = "\
*** Begin Patch
*** Update File: src/lib.rs
@@
-old
+new
*** Add File: src/new.rs
+content
*** Delete File: src/gone.rs
*** End Patch
";
        let paths = parse_patched_paths(patch);
        assert_eq!(paths, vec!["src/lib.rs", "src/new.rs"]);
    }

    #[test]
    fn ignores_lines_that_only_resemble_markers() {
        let patch = "\
*** Update File:
*** Update File: src/real.rs
+context line that mentions *** Update File: fake.rs
";
        let paths = parse_patched_paths(patch);
        assert_eq!(paths, vec!["src/real.rs"]);
    }
}
