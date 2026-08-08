//! `hook setup` / `codex-hook setup`: wire the handlers in
//! [`super::hooks`] into an agent's own config file.
//!
//! Both commands are the same run over a different config format, so
//! they share [`run_setup`] and differ only in the format they name and
//! where "the project" is anchored. Unlike the hook handlers these are
//! ordinary CLI commands: an error propagates and the process exits
//! non-zero.

use std::path::Path;

use agent_lens::hooks::codex::setup::CodexConfig;
use agent_lens::hooks::setup::ClaudeSettings;
use agent_lens::hooks::setup_engine::{self, ConfigFormat, SetupScope, SetupSummary};
use tracing::info;

use super::args::{CodexSetupArgs, SetupArgs};
use super::write_stdout_json;

pub(super) fn run_hook_setup(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    run_setup::<ClaudeSettings>(args.scope, args.dry_run, &cwd)
}

pub(super) fn run_codex_hook_setup(args: CodexSetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    // Codex reads `<repo>/.codex/config.toml`, so project scope has to
    // anchor at the working-tree root rather than wherever the command
    // happens to run from.
    let project_root = agent_lens::paths::git_repo_root(&cwd).unwrap_or(cwd);
    run_setup::<CodexConfig>(args.scope, args.dry_run, &project_root)
}

/// Plan the merge for one config format, apply it unless this is a dry
/// run or a no-op, and report what happened as JSON on stdout.
fn run_setup<F: ConfigFormat>(
    scope: SetupScope,
    dry_run: bool,
    project_root: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = setup_engine::resolve_path::<F>(scope, project_root)?;
    let plan = setup_engine::plan::<F>(path)?;
    let wrote = apply_setup_plan(dry_run, &plan, F::FILE_LABEL, || {
        setup_engine::apply::<F>(&plan)
    })?;
    write_stdout_json(&SetupSummary::<F> {
        path: &plan.path,
        wrote,
        added_commands: &plan.added_commands,
        document: &plan.after,
    })
}

fn apply_setup_plan<T: PartialEq, E: Into<Box<dyn std::error::Error>>>(
    dry_run: bool,
    plan: &setup_engine::SetupPlan<T>,
    file_label: &str,
    apply: impl FnOnce() -> Result<(), E>,
) -> Result<bool, Box<dyn std::error::Error>> {
    let path = plan.path.display();
    if dry_run {
        info!(path = %path, "dry-run: leaving {file_label} untouched");
        return Ok(false);
    }

    if !plan.changed() {
        info!(path = %path, "{file_label} already configured; nothing to do");
        return Ok(false);
    }

    apply().map_err(Into::into)?;
    info!(
        path = %path,
        added = plan.added_commands.len(),
        "wrote {file_label}",
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn plan_of(after: u8, added: &[&str]) -> setup_engine::SetupPlan<u8> {
        setup_engine::SetupPlan {
            path: PathBuf::from("settings.json"),
            before: Some(0),
            after,
            added_commands: added.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn apply_setup_plan_reports_and_runs_only_when_changed() {
        let changed = plan_of(1, &["cmd"]);
        let unchanged = plan_of(0, &[]);

        let dry_run_applied = std::cell::Cell::new(false);
        let wrote = apply_setup_plan(true, &changed, "settings.json", || {
            dry_run_applied.set(true);
            Ok::<(), Box<dyn std::error::Error>>(())
        })
        .unwrap();
        assert!(!wrote);
        assert!(!dry_run_applied.get());

        let unchanged_applied = std::cell::Cell::new(false);
        let wrote = apply_setup_plan(false, &unchanged, "settings.json", || {
            unchanged_applied.set(true);
            Ok::<(), Box<dyn std::error::Error>>(())
        })
        .unwrap();
        assert!(!wrote);
        assert!(!unchanged_applied.get());

        let changed_applied = std::cell::Cell::new(false);
        let wrote = apply_setup_plan(false, &changed, "settings.json", || {
            changed_applied.set(true);
            Ok::<(), Box<dyn std::error::Error>>(())
        })
        .unwrap();
        assert!(wrote);
        assert!(changed_applied.get());
    }

    #[test]
    fn scope_project_finds_no_root_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        // tempdir() returns a fresh path; nothing inside it is git-tracked.
        assert!(agent_lens::paths::git_repo_root(dir.path()).is_none());
    }

    #[test]
    fn scope_project_anchors_at_the_repo_root_from_a_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let nested = dir.path().join("nested/inner");
        std::fs::create_dir_all(&nested).unwrap();
        let resolved = agent_lens::paths::git_repo_root(&nested).expect("inside the new repo");
        // Resolve symlinks on both sides — macOS tempdirs live under
        // /private/var/... while git emits /var/..., so a literal
        // comparison is fragile.
        let canonical_dir = std::fs::canonicalize(dir.path()).unwrap();
        let canonical_resolved = std::fs::canonicalize(&resolved).unwrap();
        assert_eq!(canonical_resolved, canonical_dir);
    }
}
