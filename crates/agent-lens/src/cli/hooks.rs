//! The hook handlers: stdin-driven `hook` / `codex-hook` subcommands and
//! the setup commands that wire them into agent configs.

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use agent_hooks::Hook;
use agent_hooks::claude_code::ClaudeCodeHookInput;
use agent_hooks::codex::CodexHookInput;
use agent_lens::hooks::codex::post_tool_use::{
    SimilarityHook as CodexSimilarityHook, WrapperHook as CodexWrapperHook,
};
use agent_lens::hooks::codex::pre_tool_use::{
    CohesionHook as CodexPreCohesionHook, ComplexityHook as CodexPreComplexityHook,
};
use agent_lens::hooks::codex::session_start::SummaryHook as CodexSessionStartSummaryHook;
use agent_lens::hooks::codex::setup::{self as codex_setup, SetupSummary as CodexSetupSummary};
use agent_lens::hooks::post_tool_use::{SimilarityHook, WrapperHook};
use agent_lens::hooks::pre_tool_use::{CohesionHook, ComplexityHook};
use agent_lens::hooks::session_start::SummaryHook as SessionStartSummaryHook;
use agent_lens::hooks::setup::{self, SetupSummary};
use tracing::info;

use super::args::{
    CodexPostToolUseCommand, CodexPreToolUseCommand, CodexSessionStartCommand, CodexSetupArgs,
    PostToolUseCommand, PreToolUseCommand, SessionStartCommand, SetupArgs,
};
use super::write_stdout_json;

pub(super) fn run_session_start(
    cmd: SessionStartCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let ClaudeCodeHookInput::SessionStart(input) = read_stdin_json::<ClaudeCodeHookInput>()? else {
        return Err("expected a SessionStart hook payload on stdin".into());
    };
    let output = match cmd {
        SessionStartCommand::Summary => SessionStartSummaryHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

pub(super) fn run_pre_tool_use(cmd: PreToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    let ClaudeCodeHookInput::PreToolUse(input) = read_stdin_json::<ClaudeCodeHookInput>()? else {
        return Err("expected a PreToolUse hook payload on stdin".into());
    };
    let output = match cmd {
        PreToolUseCommand::Complexity => ComplexityHook::new().handle(input)?,
        PreToolUseCommand::Cohesion => CohesionHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

pub(super) fn run_post_tool_use(cmd: PostToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    let ClaudeCodeHookInput::PostToolUse(input) = read_stdin_json::<ClaudeCodeHookInput>()? else {
        return Err("expected a PostToolUse hook payload on stdin".into());
    };
    let output = match cmd {
        PostToolUseCommand::Similarity => SimilarityHook::new().handle(input)?,
        PostToolUseCommand::Wrapper => WrapperHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

pub(super) fn run_hook_setup(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let path = setup::resolve_path(args.scope.into(), &cwd)?;
    let plan = setup::plan(path)?;
    let wrote = apply_setup_plan(
        args.dry_run,
        plan.changed(),
        SetupApplyContext {
            path: &plan.path,
            added_commands: plan.added_commands.len(),
            dry_run_message: "dry-run: leaving settings.json untouched",
            wrote_message: "wrote settings.json",
            unchanged_message: "settings.json already configured; nothing to do",
        },
        || setup::apply(&plan).map_err(Into::into),
    )?;
    write_stdout_json(&SetupSummary {
        path: &plan.path,
        wrote,
        added_commands: &plan.added_commands,
        settings: &plan.after,
    })
}

pub(super) fn run_codex_hook_setup(args: CodexSetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let project_root = git_top_level(&cwd).unwrap_or(cwd);
    let path = codex_setup::resolve_path(args.scope.into(), &project_root)?;
    let plan = codex_setup::plan(path)?;
    let wrote = apply_setup_plan(
        args.dry_run,
        plan.changed(),
        SetupApplyContext {
            path: &plan.path,
            added_commands: plan.added_commands.len(),
            dry_run_message: "dry-run: leaving config.toml untouched",
            wrote_message: "wrote config.toml",
            unchanged_message: "config.toml already configured; nothing to do",
        },
        || codex_setup::apply(&plan).map_err(Into::into),
    )?;
    write_stdout_json(&CodexSetupSummary {
        path: &plan.path,
        wrote,
        added_commands: &plan.added_commands,
        config: &plan.after,
    })
}

fn apply_setup_plan(
    dry_run: bool,
    changed: bool,
    context: SetupApplyContext<'_>,
    apply: impl FnOnce() -> Result<(), Box<dyn std::error::Error>>,
) -> Result<bool, Box<dyn std::error::Error>> {
    if dry_run {
        info!(path = %context.path.display(), "{}", context.dry_run_message);
        return Ok(false);
    }

    if !changed {
        info!(path = %context.path.display(), "{}", context.unchanged_message);
        return Ok(false);
    }

    apply()?;
    info!(
        path = %context.path.display(),
        added = context.added_commands,
        "{}",
        context.wrote_message,
    );
    Ok(true)
}

struct SetupApplyContext<'a> {
    path: &'a Path,
    added_commands: usize,
    dry_run_message: &'static str,
    wrote_message: &'static str,
    unchanged_message: &'static str,
}

pub(super) fn run_codex_pre_tool_use(
    cmd: CodexPreToolUseCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let CodexHookInput::PreToolUse(input) = read_stdin_json::<CodexHookInput>()? else {
        return Err("expected a Codex PreToolUse hook payload on stdin".into());
    };
    let output = match cmd {
        CodexPreToolUseCommand::Complexity => CodexPreComplexityHook::new().handle(input)?,
        CodexPreToolUseCommand::Cohesion => CodexPreCohesionHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

pub(super) fn run_codex_post_tool_use(
    cmd: CodexPostToolUseCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let CodexHookInput::PostToolUse(input) = read_stdin_json::<CodexHookInput>()? else {
        return Err("expected a Codex PostToolUse hook payload on stdin".into());
    };
    let output = match cmd {
        CodexPostToolUseCommand::Similarity => CodexSimilarityHook::new().handle(input)?,
        CodexPostToolUseCommand::Wrapper => CodexWrapperHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

pub(super) fn run_codex_session_start(
    cmd: CodexSessionStartCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let CodexHookInput::SessionStart(input) = read_stdin_json::<CodexHookInput>()? else {
        return Err("expected a Codex SessionStart hook payload on stdin".into());
    };
    let output = match cmd {
        CodexSessionStartCommand::Summary => CodexSessionStartSummaryHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

/// Resolve the enclosing git repository's top-level directory, or
/// `None` when `cwd` is not inside a git tree (or `git` isn't on
/// `PATH`). Used to anchor `--scope project` so the hook lands at the
/// repo root no matter which subdirectory the user invoked from.
fn git_top_level(cwd: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn read_stdin_json<T: serde::de::DeserializeOwned>() -> Result<T, Box<dyn std::error::Error>> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    Ok(serde_json::from_str(&buf)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_setup_plan_reports_and_runs_only_when_changed() {
        let path = Path::new("settings.json");
        let context = || SetupApplyContext {
            path,
            added_commands: 1,
            dry_run_message: "dry run",
            wrote_message: "wrote",
            unchanged_message: "unchanged",
        };

        let dry_run_applied = std::cell::Cell::new(false);
        let wrote = apply_setup_plan(true, true, context(), || {
            dry_run_applied.set(true);
            Ok(())
        })
        .unwrap();
        assert!(!wrote);
        assert!(!dry_run_applied.get());

        let unchanged_applied = std::cell::Cell::new(false);
        let wrote = apply_setup_plan(false, false, context(), || {
            unchanged_applied.set(true);
            Ok(())
        })
        .unwrap();
        assert!(!wrote);
        assert!(!unchanged_applied.get());

        let changed_applied = std::cell::Cell::new(false);
        let wrote = apply_setup_plan(false, true, context(), || {
            changed_applied.set(true);
            Ok(())
        })
        .unwrap();
        assert!(wrote);
        assert!(changed_applied.get());
    }

    #[test]
    fn git_top_level_returns_none_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        // tempdir() returns a fresh path; nothing inside it is git-tracked.
        assert!(git_top_level(dir.path()).is_none());
    }

    #[test]
    fn git_top_level_finds_repo_root_from_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let nested = dir.path().join("nested/inner");
        std::fs::create_dir_all(&nested).unwrap();
        let resolved = git_top_level(&nested).expect("inside the new repo");
        // Resolve symlinks on both sides — macOS tempdirs live under
        // /private/var/... while git emits /var/..., so a literal
        // comparison is fragile.
        let canonical_dir = std::fs::canonicalize(dir.path()).unwrap();
        let canonical_resolved = std::fs::canonicalize(&resolved).unwrap();
        assert_eq!(canonical_resolved, canonical_dir);
    }
}
