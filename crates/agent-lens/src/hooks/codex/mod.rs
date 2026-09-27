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
pub mod stop;

use std::path::Path;

use crate::hooks::core::{
    EditedSource, MissingFilePolicy, ReadEditedSourceError, read_edited_source,
};

/// Tool name Codex uses for the patch-style edit tool.
pub(crate) const APPLY_PATCH_TOOL: &str = "apply_patch";

/// Shared `apply_patch` source-preparation flow for the pre/post hooks:
/// gate on the tool name, pull the patch text out of `tool_input.command`,
/// collect the paths behind the event-specific `markers` (see
/// [`parse_patched_paths`]), and read each supported file under the event's missing-file policy.
/// Returns `Ok(vec![])` for "no opinion" cases — non-`apply_patch` tools,
/// missing patch text, or a patch that touches no readable source.
pub(crate) fn prepare_patched_sources(
    tool_name: &str,
    tool_input: &serde_json::Value,
    cwd: &Path,
    markers: &[&str],
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
    let rel_paths = parse_patched_paths(command, markers);
    let mut out = Vec::with_capacity(rel_paths.len());
    for rel_path in rel_paths {
        if let Some(source) = read_edited_source(cwd, rel_path, missing_file_policy)? {
            out.push(source);
        }
    }
    Ok(out)
}

/// Pull the paths behind any of `markers` (`*** Update File: `,
/// `*** Add File: `, …) out of an `apply_patch` envelope. A marker must
/// open its line, so patch content that merely mentions one is ignored.
fn parse_patched_paths(command: &str, markers: &[&str]) -> Vec<String> {
    command
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            markers
                .iter()
                .find_map(|marker| trimmed.strip_prefix(marker))
        })
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::parse_patched_paths;

    const PATCH: &str = "\
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

    const LOOKALIKES: &str = "\
*** Update File:
*** Update File: src/real.rs
+context line that mentions *** Update File: fake.rs
";

    #[rstest]
    #[case::update_and_add(PATCH, &["*** Update File: ", "*** Add File: "], &["src/lib.rs", "src/new.rs"])]
    #[case::update_only(PATCH, &["*** Update File: "], &["src/lib.rs"])]
    #[case::lookalikes_ignored(LOOKALIKES, &["*** Update File: ", "*** Add File: "], &["src/real.rs"])]
    fn parses_marked_paths(
        #[case] patch: &str,
        #[case] markers: &[&str],
        #[case] expected: &[&str],
    ) {
        assert_eq!(parse_patched_paths(patch, markers), expected);
    }
}
