//! Codex `PreToolUse` hook handlers.
//!
//! Mirrors the PostToolUse adapter: the `apply_patch` envelope is parsed
//! to discover the files Codex is about to touch, and each one is read
//! off disk so the engine-agnostic [`crate::hooks::core`] runners can
//! analyse the *pre-edit* state. Two notable differences from
//! PostToolUse:
//!
//! * Only `*** Update File:` paths matter here. `*** Add File:` entries
//!   describe content that does not yet exist on disk, so there is
//!   nothing to read; `*** Delete File:` entries are about to vanish
//!   and the agent doesn't need pre-edit context for them.
//! * Returning the result via `additionalContext` (PostToolUse-style)
//!   would let Codex append it to the conversation. PreToolUse only
//!   honours `system_message` today, so the hook surfaces the report
//!   that way and lets future Codex versions opt into a richer surface
//!   without a schema change here.

use std::path::Path;

use agent_hooks::codex::{CommonHookOutput, PreToolUseInput, PreToolUseOutput};

use crate::analyze::SourceLang;
use crate::hooks::core::{EditedSource, HookEnvelope, ReadEditedSourceError};

/// Tool name Codex uses for the patch-style edit tool.
pub(crate) const APPLY_PATCH_TOOL: &str = "apply_patch";

/// Codex's PreToolUse adapter for the engine-agnostic hook runner.
pub struct CodexPreToolUse;

impl HookEnvelope for CodexPreToolUse {
    type Input = PreToolUseInput;
    type Output = PreToolUseOutput;

    /// Prepare every patched source file the analysers can reason about
    /// *before* Codex applies the patch.
    ///
    /// Returns `Ok(vec![])` for "no opinion" cases — non-`apply_patch`
    /// tools, missing patch text, or a patch that only touches files in
    /// unsupported languages or only adds brand-new files.
    fn prepare_sources(input: &Self::Input) -> Result<Vec<EditedSource>, ReadEditedSourceError> {
        if input.tool_name != APPLY_PATCH_TOOL {
            return Ok(Vec::new());
        }
        let Some(command) = extract_patch_command(&input.tool_input) else {
            return Ok(Vec::new());
        };

        let rel_paths = parse_pre_edit_paths(&command);
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
            let source = match std::fs::read_to_string(&abs_path) {
                Ok(s) => s,
                // The patch claims to update an existing file but it
                // isn't on disk — for example a stale or speculative
                // patch. Stay silent rather than failing the tool call.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(source) => {
                    return Err(ReadEditedSourceError {
                        path: abs_path,
                        source,
                    });
                }
            };
            out.push(EditedSource {
                rel_path,
                lang,
                source,
            });
        }
        Ok(out)
    }

    fn wrap_report(report: String) -> Self::Output {
        PreToolUseOutput {
            common: CommonHookOutput {
                system_message: Some(report),
                ..CommonHookOutput::default()
            },
            ..PreToolUseOutput::default()
        }
    }
}

/// Codex `complexity` PreToolUse hook handler.
pub type ComplexityHook = crate::hooks::core::ComplexityHook<CodexPreToolUse>;
/// Codex `cohesion` PreToolUse hook handler.
pub type CohesionHook = crate::hooks::core::CohesionHook<CodexPreToolUse>;
/// Re-exported for symmetry with the PostToolUse handlers.
pub type ComplexityError = crate::hooks::core::HookError;
/// Re-exported for symmetry with the PostToolUse handlers.
pub type CohesionError = crate::hooks::core::HookError;

fn extract_patch_command(tool_input: &serde_json::Value) -> Option<String> {
    tool_input
        .get("command")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Pull `*** Update File: ...` paths out of an `apply_patch` envelope.
///
/// `*** Add File:` and `*** Delete File:` entries are skipped: the
/// former has no current on-disk content, the latter is going away and
/// not worth pre-edit context for.
fn parse_pre_edit_paths(command: &str) -> Vec<String> {
    const MARKER: &str = "*** Update File: ";
    let mut out = Vec::new();
    for line in command.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix(MARKER) {
            let path = rest.trim();
            if !path.is_empty() {
                out.push(path.to_owned());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks::Hook;
    use agent_hooks::codex::HookContext;
    use serde_json::json;
    use std::io::Write;
    use std::path::PathBuf;

    fn ctx(cwd: PathBuf) -> HookContext {
        HookContext {
            session_id: "sess".into(),
            transcript_path: Some(PathBuf::from("/tmp/t.jsonl")),
            cwd,
            model: "gpt-5".into(),
        }
    }

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn input(cwd: PathBuf, tool_name: &str, command: &str) -> PreToolUseInput {
        PreToolUseInput {
            context: ctx(cwd),
            turn_id: "turn-1".into(),
            tool_name: tool_name.into(),
            tool_use_id: "call-1".into(),
            tool_input: json!({"command": command}),
        }
    }

    // -- patch parsing ----------------------------------------------

    #[test]
    fn parses_only_update_markers() {
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
        let paths = parse_pre_edit_paths(patch);
        assert_eq!(paths, vec!["src/lib.rs"]);
    }

    #[test]
    fn ignores_lines_that_only_resemble_markers() {
        let patch = "\
*** Update File:
*** Update File: src/real.rs
+context line that mentions *** Update File: fake.rs
";
        let paths = parse_pre_edit_paths(patch);
        assert_eq!(paths, vec!["src/real.rs"]);
    }

    // -- complexity --------------------------------------------------

    #[test]
    fn complexity_ignores_non_apply_patch_tools() {
        let hook = ComplexityHook::new();
        let payload = input(PathBuf::from("/tmp"), "Bash", "ls");
        let out = hook.handle(payload).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }

    #[test]
    fn complexity_ignores_add_only_patches() {
        let dir = tempfile::tempdir().unwrap();
        // Note: the file is *not* created on disk because Add means
        // "this file does not yet exist."
        let patch = "*** Begin Patch\n*** Add File: lib.rs\n*** End Patch\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = ComplexityHook::new().handle(payload).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }

    #[test]
    fn complexity_reports_via_system_message_for_nontrivial_function() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
fn nested(n: i32) -> i32 {
    if n > 0 { if n > 1 { if n > 2 { if n > 3 { return n; } } } }
    0
}
"#;
        write_file(dir.path(), "lib.rs", source);

        let patch = "*** Update File: lib.rs\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);

        let out = ComplexityHook::new().handle(payload).unwrap();
        let msg = out.common.system_message.expect("expected a report");
        assert!(msg.contains("lib.rs"), "should mention file: {msg}");
        assert!(msg.contains("nested"), "should mention function: {msg}");
        assert!(msg.contains("cog="), "should include cognitive: {msg}");
        assert!(out.decision.is_none());
        assert!(out.hook_specific_output.is_none());
    }

    #[test]
    fn complexity_silent_when_only_add_entries_exist() {
        // A multi-file patch that only adds new files: nothing on disk
        // matches, so the hook stays silent.
        let dir = tempfile::tempdir().unwrap();
        let patch = "\
*** Begin Patch
*** Add File: src/new1.rs
*** Add File: src/new2.rs
*** End Patch
";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = ComplexityHook::new().handle(payload).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }

    #[test]
    fn complexity_silent_when_update_target_does_not_exist() {
        // A speculative patch that claims to update a file that isn't
        // there yet: skip rather than fail the tool call.
        let dir = tempfile::tempdir().unwrap();
        let patch = "*** Update File: missing.rs\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = ComplexityHook::new().handle(payload).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }

    #[test]
    fn complexity_aggregates_across_multiple_updated_files() {
        let dir = tempfile::tempdir().unwrap();
        let nested = r#"
fn nested(n: i32) -> i32 {
    if n > 0 { if n > 1 { if n > 2 { if n > 3 { return n; } } } }
    0
}
"#;
        write_file(dir.path(), "a.rs", nested);
        write_file(dir.path(), "b.rs", nested);

        let patch = "\
*** Begin Patch
*** Update File: a.rs
*** Update File: b.rs
*** End Patch
";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = ComplexityHook::new().handle(payload).unwrap();
        let msg = out.common.system_message.expect("expected a report");
        assert!(msg.contains("a.rs"));
        assert!(msg.contains("b.rs"));
    }

    // -- cohesion ---------------------------------------------------

    #[test]
    fn cohesion_reports_split_impl_via_system_message() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
struct Thing { a: i32, b: i32 }
impl Thing {
    fn ga(&self) -> i32 { self.a }
    fn gb(&self) -> i32 { self.b }
}
"#;
        write_file(dir.path(), "lib.rs", source);

        let patch = "*** Update File: lib.rs\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = CohesionHook::new().handle(payload).unwrap();

        let msg = out.common.system_message.expect("expected a report");
        assert!(msg.contains("lib.rs"));
        assert!(msg.contains("impl Thing"));
        assert!(msg.contains("LCOM4=2"));
    }

    #[test]
    fn cohesion_silent_when_impl_is_cohesive() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
struct Counter { n: i32 }
impl Counter {
    fn inc(&mut self) { self.n += 1; }
    fn get(&self) -> i32 { self.n }
}
"#;
        write_file(dir.path(), "lib.rs", source);

        let patch = "*** Update File: lib.rs\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = CohesionHook::new().handle(payload).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }

    #[test]
    fn cohesion_silent_when_only_add_entries_exist() {
        let dir = tempfile::tempdir().unwrap();
        let patch = "*** Add File: src/new.rs\n";
        let payload = input(dir.path().to_path_buf(), "apply_patch", patch);
        let out = CohesionHook::new().handle(payload).unwrap();
        assert_eq!(out, PreToolUseOutput::default());
    }
}
