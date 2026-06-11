//! Bundled Claude Code skills and the `skills install` plan/apply logic.
//!
//! `agent-lens` ships the same skill files it dogfoods in this repo
//! (`.claude/skills/<name>/SKILL.md`), embedded at compile time with
//! `include_str!` so the single binary can drop them into a user's
//! project or home directory. The skills teach an agent which analyzer
//! fits a given question, so installing them is how a fresh checkout
//! gets `agent-lens`-aware routing.
//!
//! The install mirrors `hook setup`: conservative by default (a skill
//! that already exists on disk with different content is left alone and
//! reported as a conflict), idempotent on re-runs, and `--force` is the
//! only way to overwrite local edits.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Directory, relative to a scope root, where Claude Code looks for
/// project- or user-level skills.
const SKILLS_RELATIVE: &str = ".claude/skills";

/// A skill file compiled into the binary.
#[derive(Debug, Clone, Copy)]
pub struct BundledSkill {
    /// Skill directory name (also the skill's `name:` frontmatter field).
    pub name: &'static str,
    /// Full `SKILL.md` contents, embedded at build time.
    pub content: &'static str,
}

/// Embed `<repo>/.claude/skills/<name>/SKILL.md` into the binary. The
/// path is resolved from `CARGO_MANIFEST_DIR` (`crates/agent-lens`) so it
/// holds no matter where `cargo` is invoked from, and `include_str!`
/// makes cargo rebuild when the source skill changes.
macro_rules! bundled_skill {
    ($name:literal) => {
        BundledSkill {
            name: $name,
            content: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../.claude/skills/",
                $name,
                "/SKILL.md"
            )),
        }
    };
}

/// Every skill shipped with `agent-lens`, in install order.
pub const SKILLS: &[BundledSkill] = &[
    bundled_skill!("agent-lens"),
    bundled_skill!("audit-architecture"),
    bundled_skill!("find-duplicates"),
    bundled_skill!("find-refactor-targets"),
    bundled_skill!("review-pending-changes"),
];

impl BundledSkill {
    /// The `description:` line from the skill's YAML frontmatter, used to
    /// summarise the skill in `skills list`.
    pub fn description(&self) -> Option<&str> {
        frontmatter_field(self.content, "description")
    }

    /// Where this skill installs under a scope `root`:
    /// `<root>/.claude/skills/<name>/SKILL.md`.
    pub fn target_path(&self, root: &Path) -> PathBuf {
        root.join(SKILLS_RELATIVE).join(self.name).join("SKILL.md")
    }
}

/// Pull a `key: value` line out of a leading `---`-fenced YAML
/// frontmatter block. Returns the trimmed value, or `None` when there is
/// no frontmatter or no such key.
fn frontmatter_field<'a>(content: &'a str, key: &str) -> Option<&'a str> {
    let after_open = content.strip_prefix("---")?;
    let end = after_open.find("\n---")?;
    after_open[..end].lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix(key)?.strip_prefix(':')?;
        Some(rest.trim())
    })
}

/// Where to install the bundled skills.
#[derive(Debug, Clone, Copy)]
pub enum SkillsScope {
    /// `<project_root>/.claude/skills` (created if missing).
    Project,
    /// `$HOME/.claude/skills` (created if missing).
    User,
}

/// What installing a single skill would do to its target file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillAction {
    /// No file at the target path yet; the skill will be written.
    Create,
    /// A differing file exists and `--force` is set; it will be overwritten.
    Update,
    /// The target already matches the bundled skill; nothing to do.
    Unchanged,
    /// A differing file exists and `--force` is not set; left untouched.
    Conflict,
}

/// One skill's planned outcome.
#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub name: &'static str,
    pub path: PathBuf,
    pub action: SkillAction,
}

/// Result of diffing the bundled skills against a scope root.
#[derive(Debug)]
pub struct SkillsPlan {
    pub root: PathBuf,
    pub entries: Vec<SkillEntry>,
}

impl SkillsPlan {
    /// Whether applying this plan would write anything to disk.
    pub fn changed(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.action, SkillAction::Create | SkillAction::Update))
    }

    /// Skills that exist with different content and were not overwritten
    /// because `--force` was absent.
    pub fn conflicts(&self) -> impl Iterator<Item = &SkillEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.action == SkillAction::Conflict)
    }

    /// Compact JSON-friendly summary of the run.
    pub fn summary(&self, wrote: bool) -> SkillsSummary<'_> {
        let collect = |want: SkillAction| {
            self.entries
                .iter()
                .filter(move |entry| entry.action == want)
                .map(|entry| entry.name)
                .collect()
        };
        SkillsSummary {
            root: &self.root,
            wrote,
            created: collect(SkillAction::Create),
            updated: collect(SkillAction::Update),
            unchanged: collect(SkillAction::Unchanged),
            conflicts: collect(SkillAction::Conflict),
        }
    }
}

/// Compact summary of a `skills install` run, suitable for JSON on stdout.
#[derive(Debug, Serialize)]
pub struct SkillsSummary<'a> {
    pub root: &'a Path,
    pub wrote: bool,
    pub created: Vec<&'a str>,
    pub updated: Vec<&'a str>,
    pub unchanged: Vec<&'a str>,
    pub conflicts: Vec<&'a str>,
}

#[derive(Debug, thiserror::Error)]
pub enum SkillsError {
    /// `$HOME` is unset, so the user-scope path can't be resolved.
    #[error("$HOME is not set; cannot resolve user-scope skills directory")]
    HomeNotFound,
    #[error("failed to access {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Resolve the scope root the skills install under.
///
/// `project_root` is only consulted for [`SkillsScope::Project`]; the
/// caller passes the current directory so the project skills land beside
/// the repo the agent is working in.
pub fn resolve_root(scope: SkillsScope, project_root: &Path) -> Result<PathBuf, SkillsError> {
    match scope {
        SkillsScope::Project => Ok(project_root.to_path_buf()),
        SkillsScope::User => std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or(SkillsError::HomeNotFound),
    }
}

/// Diff the bundled skills against what is on disk under `root` without
/// touching the filesystem. With `force`, a skill whose on-disk content
/// differs is marked [`SkillAction::Update`] rather than
/// [`SkillAction::Conflict`].
pub fn plan(root: PathBuf, force: bool) -> Result<SkillsPlan, SkillsError> {
    let mut entries = Vec::with_capacity(SKILLS.len());
    for skill in SKILLS {
        let path = skill.target_path(&root);
        let action = match read_existing(&path)? {
            None => SkillAction::Create,
            Some(existing) if existing == skill.content => SkillAction::Unchanged,
            Some(_) if force => SkillAction::Update,
            Some(_) => SkillAction::Conflict,
        };
        entries.push(SkillEntry {
            name: skill.name,
            path,
            action,
        });
    }
    Ok(SkillsPlan { root, entries })
}

/// Write the skills the plan marked [`SkillAction::Create`] or
/// [`SkillAction::Update`], creating parent directories as needed.
/// [`SkillAction::Unchanged`] and [`SkillAction::Conflict`] entries are
/// skipped.
pub fn apply(plan: &SkillsPlan) -> Result<(), SkillsError> {
    for (skill, entry) in SKILLS.iter().zip(&plan.entries) {
        if !matches!(entry.action, SkillAction::Create | SkillAction::Update) {
            continue;
        }
        if let Some(parent) = entry.path.parent() {
            fs::create_dir_all(parent).map_err(|source| SkillsError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::write(&entry.path, skill.content).map_err(|source| SkillsError::Io {
            path: entry.path.clone(),
            source,
        })?;
    }
    Ok(())
}

/// Render the bundled skills as a Markdown list: one bullet per skill
/// with its frontmatter description, followed by the install hint.
pub fn render_list() -> String {
    use std::fmt::Write as _;

    let mut out = String::from("# Bundled skills\n\n");
    for skill in SKILLS {
        let _ = writeln!(out, "## {}", skill.name);
        if let Some(description) = skill.description() {
            let _ = writeln!(out, "\n{description}");
        }
        out.push('\n');
        let _ = writeln!(
            out,
            "Installs to `{SKILLS_RELATIVE}/{}/SKILL.md`.",
            skill.name
        );
        out.push('\n');
    }
    out.push_str("Install all of the above with `agent-lens skills install` ");
    out.push_str("(`--scope user` for `$HOME`, `--force` to overwrite local edits).\n");
    out
}

fn read_existing(path: &Path) -> Result<Option<String>, SkillsError> {
    match fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(SkillsError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn read(path: &Path) -> String {
        fs::read_to_string(path).unwrap()
    }

    #[test]
    fn every_bundled_skill_has_name_and_description_frontmatter() {
        for skill in SKILLS {
            assert!(
                skill.content.starts_with("---"),
                "{} has no frontmatter",
                skill.name,
            );
            let description = skill
                .description()
                .unwrap_or_else(|| panic!("{} has no description", skill.name));
            assert!(!description.is_empty(), "{} description empty", skill.name);
            // The directory name and the frontmatter name must agree, or
            // an installed skill would advertise itself under the wrong id.
            assert_eq!(
                frontmatter_field(skill.content, "name"),
                Some(skill.name),
                "{} name frontmatter mismatch",
                skill.name,
            );
        }
    }

    #[test]
    fn frontmatter_field_reads_value_and_ignores_other_keys() {
        let content = "---\nname: foo\ndescription: a long line\n---\n# body\n";
        assert_eq!(frontmatter_field(content, "name"), Some("foo"));
        assert_eq!(
            frontmatter_field(content, "description"),
            Some("a long line"),
        );
        assert_eq!(frontmatter_field(content, "missing"), None);
    }

    #[test]
    fn frontmatter_field_returns_none_without_a_block() {
        assert_eq!(frontmatter_field("# just a heading\n", "name"), None);
    }

    #[test]
    fn plan_marks_every_skill_create_on_an_empty_root() {
        let dir = TempDir::new().unwrap();
        let plan = plan(dir.path().to_path_buf(), false).unwrap();
        assert_eq!(plan.entries.len(), SKILLS.len());
        assert!(
            plan.entries
                .iter()
                .all(|entry| entry.action == SkillAction::Create),
        );
        assert!(plan.changed());
    }

    #[test]
    fn apply_writes_each_skill_to_its_target_path() {
        let dir = TempDir::new().unwrap();
        let plan = plan(dir.path().to_path_buf(), false).unwrap();
        apply(&plan).unwrap();
        for skill in SKILLS {
            let path = skill.target_path(dir.path());
            assert!(path.exists(), "{} not written", skill.name);
            assert_eq!(read(&path), skill.content);
        }
    }

    #[test]
    fn rerunning_install_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let first = plan(dir.path().to_path_buf(), false).unwrap();
        apply(&first).unwrap();

        let second = plan(dir.path().to_path_buf(), false).unwrap();
        assert!(!second.changed(), "second plan should be a no-op");
        assert!(
            second
                .entries
                .iter()
                .all(|entry| entry.action == SkillAction::Unchanged),
        );
    }

    #[test]
    fn differing_skill_is_a_conflict_unless_forced() {
        let dir = TempDir::new().unwrap();
        let skill = &SKILLS[0];
        let path = skill.target_path(dir.path());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "local edits\n").unwrap();

        let plan_no_force = plan(dir.path().to_path_buf(), false).unwrap();
        let entry = plan_no_force
            .entries
            .iter()
            .find(|entry| entry.name == skill.name)
            .unwrap();
        assert_eq!(entry.action, SkillAction::Conflict);
        assert_eq!(plan_no_force.conflicts().count(), 1);
        // A conflict must not be overwritten.
        apply(&plan_no_force).unwrap();
        assert_eq!(read(&path), "local edits\n");

        let plan_force = plan(dir.path().to_path_buf(), true).unwrap();
        let entry = plan_force
            .entries
            .iter()
            .find(|entry| entry.name == skill.name)
            .unwrap();
        assert_eq!(entry.action, SkillAction::Update);
        apply(&plan_force).unwrap();
        assert_eq!(read(&path), skill.content);
    }

    #[test]
    fn summary_buckets_entries_by_action() {
        let dir = TempDir::new().unwrap();
        let plan = plan(dir.path().to_path_buf(), false).unwrap();
        let summary = plan.summary(true);
        assert!(summary.wrote);
        assert_eq!(summary.created.len(), SKILLS.len());
        assert!(summary.unchanged.is_empty());
        assert!(summary.conflicts.is_empty());
    }

    #[test]
    fn resolve_root_project_returns_the_given_root() {
        let root = Path::new("/tmp/proj");
        assert_eq!(
            resolve_root(SkillsScope::Project, root).unwrap(),
            root.to_path_buf(),
        );
    }

    #[test]
    fn render_list_names_every_skill_and_the_install_command() {
        let listing = render_list();
        for skill in SKILLS {
            assert!(listing.contains(skill.name), "missing {}", skill.name);
        }
        assert!(
            listing.contains("agent-lens skills install"),
            "got: {listing}"
        );
    }

    #[test]
    fn skills_error_messages_are_descriptive() {
        assert!(SkillsError::HomeNotFound.to_string().contains("$HOME"));
        let io = SkillsError::Io {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        };
        let msg = io.to_string();
        assert!(msg.contains("/tmp/x"), "got {msg}");
        assert!(msg.contains("denied"), "got {msg}");
    }
}
