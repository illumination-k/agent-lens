//! `wrapper` PostToolUse handler.
//!
//! After an agent edits a Rust source file, parse it and report any
//! function whose body, after stripping a short chain of trivial
//! adapters, is just a forwarding call to another function. The
//! findings come back as a `systemMessage` so they land in the agent's
//! context without blocking the tool call.
//!
//! Language is picked from the file extension. Today only `.rs` is
//! supported; other extensions are treated as "no opinion" so the hook
//! stays silent instead of erroring.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent_hooks::Hook;
use agent_hooks::claude_code::{CommonHookOutput, PostToolUseInput, PostToolUseOutput};
use lens_rust::{WrapperFinding, find_wrappers};

/// Tool names whose `tool_input.file_path` points at the file that was
/// just modified. Anything outside this set is ignored.
const EDITING_TOOL_NAMES: &[&str] = &["Write", "Edit", "MultiEdit"];

/// Handler implementation for the `wrapper` PostToolUse hook.
#[derive(Debug, Clone, Default)]
pub struct WrapperHook;

impl WrapperHook {
    pub fn new() -> Self {
        Self
    }
}

/// Languages that the wrapper hook knows how to parse.
///
/// Mirrors the similarity hook's enum so that adding a new language is
/// a localised, one-spot change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Language {
    Rust,
}

impl Language {
    fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "rs" => Some(Self::Rust),
            _ => None,
        }
    }

    fn find_wrappers(self, source: &str) -> Result<Vec<WrapperFinding>, WrapperError> {
        match self {
            Self::Rust => find_wrappers(source).map_err(|e| WrapperError::Parse(Box::new(e))),
        }
    }
}

/// Errors raised while running [`WrapperHook`].
#[derive(Debug)]
pub enum WrapperError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Boxed to keep the error type language-agnostic as more parsers
    /// are added.
    Parse(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for WrapperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to read {}: {source}", path.display())
            }
            Self::Parse(e) => write!(f, "failed to parse source: {e}"),
        }
    }
}

impl std::error::Error for WrapperError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse(e) => Some(e.as_ref()),
        }
    }
}

impl Hook for WrapperHook {
    type Input = PostToolUseInput;
    type Output = PostToolUseOutput;
    type Error = WrapperError;

    fn handle(&self, input: PostToolUseInput) -> Result<PostToolUseOutput, Self::Error> {
        if !EDITING_TOOL_NAMES.contains(&input.tool_name.as_str()) {
            return Ok(PostToolUseOutput::default());
        }
        let Some(file_path) = extract_file_path(&input.tool_input) else {
            return Ok(PostToolUseOutput::default());
        };
        let rel_path = Path::new(&file_path);
        let Some(language) = rel_path
            .extension()
            .and_then(|ext| ext.to_str())
            .and_then(Language::from_extension)
        else {
            return Ok(PostToolUseOutput::default());
        };

        let abs_path = if rel_path.is_absolute() {
            rel_path.to_path_buf()
        } else {
            input.context.cwd.join(rel_path)
        };

        let source = std::fs::read_to_string(&abs_path).map_err(|source| WrapperError::Io {
            path: abs_path.clone(),
            source,
        })?;

        let findings = language.find_wrappers(&source)?;
        if findings.is_empty() {
            return Ok(PostToolUseOutput::default());
        }

        Ok(PostToolUseOutput {
            common: CommonHookOutput {
                system_message: Some(format_report(&file_path, &findings)),
                ..CommonHookOutput::default()
            },
            ..PostToolUseOutput::default()
        })
    }
}

fn extract_file_path(tool_input: &serde_json::Value) -> Option<String> {
    tool_input
        .get("file_path")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

fn format_report(file_path: &str, findings: &[WrapperFinding]) -> String {
    let mut out = format!(
        "agent-lens wrapper: {} thin wrapper(s) in {}\n",
        findings.len(),
        file_path,
    );
    for finding in findings {
        // writeln! into a String cannot fail; the result is swallowed
        // deliberately rather than unwrapped to satisfy the workspace's
        // `unwrap_used` lint.
        if finding.adapters.is_empty() {
            let _ = writeln!(
                out,
                "- {} (L{}-{}) -> {}",
                finding.name, finding.start_line, finding.end_line, finding.callee,
            );
        } else {
            let _ = writeln!(
                out,
                "- {} (L{}-{}) -> {} [via {}]",
                finding.name,
                finding.start_line,
                finding.end_line,
                finding.callee,
                finding.adapters.join(""),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_hooks::claude_code::HookContext;
    use serde_json::json;
    use std::io::Write;

    fn ctx(cwd: PathBuf) -> HookContext {
        HookContext {
            session_id: "sess".into(),
            transcript_path: PathBuf::from("/tmp/t.jsonl"),
            cwd,
            permission_mode: None,
        }
    }

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn assert_no_op(tool_name: &str, tool_input: serde_json::Value) {
        let hook = WrapperHook::new();
        let input = PostToolUseInput {
            context: ctx(PathBuf::from("/tmp")),
            tool_name: tool_name.into(),
            tool_input: tool_input.clone(),
            tool_response: json!({}),
        };
        let out = hook.handle(input).unwrap();
        assert_eq!(
            out,
            PostToolUseOutput::default(),
            "expected no-op for tool={tool_name} input={tool_input}",
        );
    }

    #[test]
    fn ignores_non_editing_tools() {
        assert_no_op("Bash", json!({"command": "ls"}));
    }

    #[test]
    fn ignores_unknown_extensions() {
        for ext in ["README.md", "notes.txt", "script.py", "app.ts"] {
            assert_no_op("Write", json!({ "file_path": ext }));
        }
    }

    #[test]
    fn ignores_extensionless_paths() {
        assert_no_op("Write", json!({"file_path": "Makefile"}));
    }

    #[test]
    fn ignores_missing_file_path() {
        assert_no_op("Edit", json!({}));
    }

    #[test]
    fn rust_extension_triggers_wrapper_detection() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
fn render(x: &str) -> String { internal_render(x) }
fn meaningful(x: i32) -> i32 { let y = x + 1; y * 2 }
"#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = WrapperHook::new();
        let input = PostToolUseInput {
            context: ctx(dir.path().to_path_buf()),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
            tool_response: json!({"success": true}),
        };
        let out = hook.handle(input).unwrap();
        let msg = out.common.system_message.expect("expected a report");
        assert!(msg.contains("render"), "should mention render: {msg}");
        assert!(
            msg.contains("internal_render"),
            "should mention forwarding target: {msg}",
        );
        assert!(
            !msg.contains("meaningful"),
            "should not flag the function with real logic: {msg}",
        );
        assert!(out.decision.is_none());
    }

    #[test]
    fn report_includes_adapter_chain() {
        let dir = tempfile::tempdir().unwrap();
        let source = "fn shim(x: i32) -> u64 { compute(x).unwrap().into() }\n";
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = WrapperHook::new();
        let input = PostToolUseInput {
            context: ctx(dir.path().to_path_buf()),
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
            tool_response: json!({}),
        };
        let out = hook.handle(input).unwrap();
        let msg = out.common.system_message.expect("expected a report");
        assert!(msg.contains("shim"), "should list the wrapper: {msg}");
        assert!(
            msg.contains("via"),
            "should annotate the adapter chain: {msg}"
        );
        assert!(msg.contains(".unwrap()"));
        assert!(msg.contains(".into()"));
    }

    #[test]
    fn no_report_when_no_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        let source = r#"
fn alpha(xs: &[i32]) -> i32 {
    let mut total = 0;
    for x in xs {
        total += *x;
    }
    total
}
"#;
        let file = write_file(dir.path(), "lib.rs", source);

        let hook = WrapperHook::new();
        let input = PostToolUseInput {
            context: ctx(dir.path().to_path_buf()),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": file.file_name().unwrap().to_str().unwrap()}),
            tool_response: json!({}),
        };
        let out = hook.handle(input).unwrap();
        assert_eq!(out, PostToolUseOutput::default());
    }

    #[test]
    fn resolves_relative_path_against_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("src");
        std::fs::create_dir_all(&nested).unwrap();
        let source = "fn shim(x: i32) -> i32 { core(x) }\n";
        write_file(&nested, "lib.rs", source);

        let hook = WrapperHook::new();
        let input = PostToolUseInput {
            context: ctx(dir.path().to_path_buf()),
            tool_name: "Edit".into(),
            tool_input: json!({"file_path": "src/lib.rs"}),
            tool_response: json!({}),
        };
        let out = hook.handle(input).unwrap();
        assert!(out.common.system_message.is_some());
    }

    #[test]
    fn missing_file_surfaces_io_error() {
        let hook = WrapperHook::new();
        let input = PostToolUseInput {
            context: ctx(PathBuf::from("/definitely/does/not/exist")),
            tool_name: "Write".into(),
            tool_input: json!({"file_path": "missing.rs"}),
            tool_response: json!({}),
        };
        let err = hook.handle(input).unwrap_err();
        assert!(matches!(err, WrapperError::Io { .. }));
    }
}
