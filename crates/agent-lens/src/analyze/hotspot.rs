//! `analyze hotspot` — rank files by `commits × cognitive_max`.
//!
//! Walks a path on disk, parses every supported source file (Rust,
//! TypeScript, Python, Go) for per-function complexity, asks `git` how often
//! each file has been touched, and emits the joined ranking. The point
//! is to point an agent at "frequently changed *and* complex" code:
//! high churn alone is just noise, high complexity alone is static
//! information, the product is where bugs live.
//!
//! Limitations:
//!
//! * Only files whose extension is recognised by [`SourceLang`] (`.rs`,
//!   `.ts`, `.py`, `.go` today) are analyzed.
//! * Directory walks use the shared source-file collector, so `.gitignore`
//!   and hidden-file filtering match the other analyzers.
//! * For directory roots we use a single path-scoped `git log` invocation,
//!   which counts a renamed file under each of its names. This is good
//!   enough for ranking.
//! * Files that fail to parse are reported on stderr and retained with
//!   zero complexity so the report still reflects current source files.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use lens_domain::{FileChurn, FileComplexity, FunctionComplexity, HotspotEntry, compute_hotspots};
use serde::Serialize;
use tracing::warn;

use super::{
    AnalyzePathFilter, AnalyzerError, CompiledPathFilter, OutputFormat, PathFilterError,
    SourceLang, collect_source_files,
};

/// Errors raised while running the hotspot analyzer.
#[derive(Debug, thiserror::Error)]
pub enum HotspotError {
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// `git` is missing or returned a non-zero exit status. The captured
    /// stderr is forwarded so the agent has a useful diagnostic.
    #[error("git failed: {}", stderr.trim_end())]
    Git { stderr: String },
    /// The provided path is not inside any git working tree.
    #[error("{path:?} is not inside a git working tree")]
    NotInGitRepo { path: PathBuf },
    #[error("failed to serialize report: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error(transparent)]
    PathFilter(#[from] PathFilterError),
}

/// Stateful hotspot runner. `since` is plumbed through to git's
/// `--since=` flag so callers can scope churn to a recent window
/// (e.g. `"90.days.ago"` or `"2024-01-01"`).
#[derive(Debug, Default, Clone)]
pub struct HotspotAnalyzer {
    since: Option<String>,
    top: Option<usize>,
    path_filter: AnalyzePathFilter,
}

impl HotspotAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Restrict the churn count to commits made in the given git
    /// `--since=` window. Anything git's `approxidate` parser accepts
    /// works (`"30.days.ago"`, `"2024-01-01"`, `"2.weeks.ago"`).
    pub fn with_since(mut self, since: impl Into<String>) -> Self {
        self.since = Some(since.into());
        self
    }

    /// Like [`Self::with_since`] but accepts an `Option`, leaving the
    /// window unchanged when `None` is passed. Lets callers thread a
    /// `--since` CLI flag through without an extra `if let`.
    pub fn with_since_opt(mut self, since: Option<String>) -> Self {
        if let Some(s) = since {
            self.since = Some(s);
        }
        self
    }

    /// Cap the markdown report's table to the top-N entries. JSON
    /// output always carries the full list. `None` keeps every row.
    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_only_tests(only_tests);
        self
    }

    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.path_filter = self.path_filter.with_exclude_tests(exclude_tests);
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.path_filter = self.path_filter.with_exclude_patterns(exclude);
        self
    }

    pub fn analyze(&self, path: &Path, format: OutputFormat) -> Result<String, HotspotError> {
        let collection = self.collect(path)?;
        let view = ReportView::new(
            &collection.target,
            &collection.repo_root,
            collection.since.as_deref(),
            &collection.entries,
        );
        match format {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&view).map_err(HotspotError::Serialize)
            }
            OutputFormat::Md => Ok(format_markdown(&view, self.top)),
        }
    }

    /// Resolve `path`, gather churn and complexity, and return the typed
    /// [`HotspotCollection`] used by the renderer and by the baseline
    /// subsystem. Returns the canonicalised target and detected repo
    /// root so adapters can produce stable item ids without re-walking
    /// the filesystem.
    pub fn collect(&self, path: &Path) -> Result<HotspotCollection, HotspotError> {
        let abs = canonicalize(path)?;
        let repo_root = git_repo_root(&abs)?;
        let scope_rel = relative_to(&abs, &repo_root);
        let filter = self.path_filter.compile(&repo_root)?;

        let mut churn = collect_churn(&repo_root, scope_rel.as_deref(), self.since.as_deref())?;
        churn.retain(|c| filter.includes_relative(&c.path));
        let complexity = collect_complexity(&abs, &repo_root, &filter)?;
        let entries = compute_hotspots(churn, complexity);

        Ok(HotspotCollection {
            target: abs,
            repo_root,
            since: self.since.clone(),
            entries,
        })
    }
}

/// Public typed result of [`HotspotAnalyzer::collect`]. Carries enough
/// context (target, repo root, since window) for downstream consumers
/// such as baseline adapters to attach stable identifiers to each
/// [`HotspotEntry`].
#[derive(Debug)]
pub struct HotspotCollection {
    pub target: PathBuf,
    pub repo_root: PathBuf,
    pub since: Option<String>,
    pub entries: Vec<HotspotEntry>,
}

fn canonicalize(path: &Path) -> Result<PathBuf, HotspotError> {
    path.canonicalize().map_err(|source| HotspotError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Walk parents of `path` looking for a `.git` entry. Returns the
/// directory containing it, which is what git would call the working
/// tree root.
fn git_repo_root(path: &Path) -> Result<PathBuf, HotspotError> {
    let start = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    for ancestor in start.ancestors() {
        if ancestor.join(".git").exists() {
            return Ok(ancestor.to_path_buf());
        }
    }
    Err(HotspotError::NotInGitRepo {
        path: path.to_path_buf(),
    })
}

/// Express `target` as a path relative to `base`, returning `None` when
/// `target == base` (i.e. the user pointed at the repo root).
fn relative_to(target: &Path, base: &Path) -> Option<String> {
    let rel = target.strip_prefix(base).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    Some(rel.to_string_lossy().replace('\\', "/"))
}

fn collect_churn(
    repo_root: &Path,
    scope: Option<&str>,
    since: Option<&str>,
) -> Result<Vec<FileChurn>, HotspotError> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo_root)
        .arg("log")
        .arg("--pretty=format:")
        .arg("--name-only");
    if let Some(s) = since {
        cmd.arg(format!("--since={s}"));
    }
    if let Some(scope) = scope {
        cmd.arg("--").arg(scope);
    }

    let output = cmd.output().map_err(|source| HotspotError::Io {
        path: repo_root.to_path_buf(),
        source,
    })?;
    if !output.status.success() {
        return Err(HotspotError::Git {
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut counts: BTreeMap<String, u32> = BTreeMap::new();
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        *counts.entry(trimmed.to_owned()).or_insert(0) += 1;
    }
    Ok(counts
        .into_iter()
        .map(|(path, commits)| FileChurn { path, commits })
        .collect())
}

fn collect_complexity(
    target: &Path,
    repo_root: &Path,
    filter: &CompiledPathFilter,
) -> Result<Vec<FileComplexity>, HotspotError> {
    let files = collect_source_files(target, filter)
        .map_err(|error| source_collection_error(target, error))?;

    let mut out = Vec::with_capacity(files.len());
    for source_file in files {
        let file = source_file.path;
        if SourceLang::from_path(&file).is_none() {
            continue;
        }
        let source = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(source) => {
                warn!(path = %file.display(), %source, "skipping file: read failed");
                continue;
            }
        };
        let units = extract_units(&file, &source).unwrap_or_default();
        let key = relative_to(&file, repo_root).unwrap_or_else(|| file.display().to_string());
        let function_count = units.len();
        let loc = units.iter().map(|f| f.loc()).sum();
        let cyclomatic_max = units.iter().map(|f| f.cyclomatic).max().unwrap_or(0);
        let cognitive_max = units.iter().map(|f| f.cognitive).max().unwrap_or(0);
        out.push(FileComplexity {
            path: key,
            function_count,
            loc,
            cyclomatic_max,
            cognitive_max,
        });
    }
    Ok(out)
}

fn source_collection_error(path: &Path, error: AnalyzerError) -> HotspotError {
    match error {
        AnalyzerError::Io { path, source } => HotspotError::Io { path, source },
        AnalyzerError::PathFilter(source) => HotspotError::PathFilter(source),
        other => HotspotError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::other(other),
        },
    }
}

/// Dispatch to the per-language complexity extractor based on `file`'s
/// extension. Returns `None` (and warns) on parse failure or unsupported
/// extension so the caller can keep walking other files.
fn extract_units(file: &Path, source: &str) -> Option<Vec<FunctionComplexity>> {
    let lang = SourceLang::from_path(file)?;
    let result: Result<Vec<FunctionComplexity>, Box<dyn std::error::Error>> = match lang {
        SourceLang::Rust => {
            lens_rust::extract_complexity_units(source).map_err(|e| Box::new(e) as _)
        }
        SourceLang::TypeScript(dialect) => {
            lens_ts::extract_complexity_units(source, dialect).map_err(|e| Box::new(e) as _)
        }
        SourceLang::Python => {
            lens_py::extract_complexity_units(source).map_err(|e| Box::new(e) as _)
        }
        SourceLang::Go => {
            lens_golang::extract_complexity_units(source).map_err(|e| Box::new(e) as _)
        }
    };
    match result {
        Ok(u) => Some(u),
        Err(err) => {
            warn!(path = %file.display(), error = %err, "skipping file: parse failed");
            None
        }
    }
}

#[derive(Debug, Serialize)]
struct ReportView<'a> {
    target: String,
    repo_root: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    since: Option<&'a str>,
    file_count: usize,
    summary: Summary,
    files: Vec<FileView<'a>>,
}

impl<'a> ReportView<'a> {
    fn new(
        target: &Path,
        repo_root: &Path,
        since: Option<&'a str>,
        entries: &'a [HotspotEntry],
    ) -> Self {
        Self {
            target: target.display().to_string(),
            repo_root: repo_root.display().to_string(),
            since,
            file_count: entries.len(),
            summary: Summary::from_entries(entries),
            files: entries.iter().map(FileView::from).collect(),
        }
    }
}

#[derive(Debug, Serialize)]
struct Summary {
    score_max: u64,
    commits_max: u32,
    cognitive_max: u32,
}

impl Summary {
    fn from_entries(entries: &[HotspotEntry]) -> Self {
        Self {
            score_max: entries.iter().map(|e| e.score).max().unwrap_or(0),
            commits_max: entries.iter().map(|e| e.commits).max().unwrap_or(0),
            cognitive_max: entries.iter().map(|e| e.cognitive_max).max().unwrap_or(0),
        }
    }
}

#[derive(Debug, Serialize)]
struct FileView<'a> {
    path: &'a str,
    score: u64,
    commits: u32,
    function_count: usize,
    loc: usize,
    cyclomatic_max: u32,
    cognitive_max: u32,
}

impl<'a> From<&'a HotspotEntry> for FileView<'a> {
    fn from(e: &'a HotspotEntry) -> Self {
        Self {
            path: e.path.as_str(),
            score: e.score,
            commits: e.commits,
            function_count: e.function_count,
            loc: e.loc,
            cyclomatic_max: e.cyclomatic_max,
            cognitive_max: e.cognitive_max,
        }
    }
}

const DEFAULT_TOP: usize = 20;

fn format_markdown(view: &ReportView<'_>, top: Option<usize>) -> String {
    let scope = view
        .since
        .map_or_else(String::new, |s| format!(", since {s}"));
    let mut out = format!(
        "# Hotspot report: {} ({} file(s){scope})\n",
        view.target, view.file_count,
    );
    if view.files.is_empty() {
        out.push_str("\n_No files matched._\n");
        return out;
    }
    let _ = writeln!(
        &mut out,
        "\n## Summary\n\
         - score_max: {}\n\
         - commits_max: {}\n\
         - cognitive_max: {}",
        view.summary.score_max, view.summary.commits_max, view.summary.cognitive_max,
    );

    let limit = top.unwrap_or(DEFAULT_TOP);
    let _ = writeln!(
        &mut out,
        "\n## Top {limit} hotspots (commits × cognitive_max)\n"
    );
    let _ = writeln!(
        &mut out,
        "| file | score | commits | cog | cc | loc | fns |"
    );
    let _ = writeln!(
        &mut out,
        "| --- | ---: | ---: | ---: | ---: | ---: | ---: |"
    );
    for f in view.files.iter().take(limit) {
        let _ = writeln!(
            &mut out,
            "| {} | {} | {} | {} | {} | {} | {} |",
            f.path, f.score, f.commits, f.cognitive_max, f.cyclomatic_max, f.loc, f.function_count,
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{run_git, write_file};

    fn init_repo_with_two_files(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        write_file(dir, "src/lib.rs", "pub mod a;\npub mod b;\n");
        write_file(
            dir,
            "src/a.rs",
            "pub fn nest(n: i32) -> i32 {\n    if n > 0 {\n        if n > 10 { return 1; }\n    }\n    0\n}\n",
        );
        write_file(dir, "src/b.rs", "pub fn flat() -> i32 { 0 }\n");
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", "initial"]);
        // Touch a.rs again so its churn dominates b.rs.
        write_file(
            dir,
            "src/a.rs",
            "pub fn nest(n: i32) -> i32 {\n    if n > 0 {\n        if n > 10 { return 1; }\n        if n > 5 { return 2; }\n    }\n    0\n}\n",
        );
        run_git(dir, &["add", "src/a.rs"]);
        run_git(dir, &["commit", "-q", "-m", "tweak a"]);
    }

    #[test]
    fn json_report_ranks_by_score() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_two_files(dir.path());
        let json = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        let a = files.iter().find(|f| f["path"] == "src/a.rs").unwrap();
        let b = files.iter().find(|f| f["path"] == "src/b.rs").unwrap();
        // a was committed twice and contains nested branches; b once and flat.
        assert!(a["commits"].as_u64().unwrap() >= 2);
        assert!(b["commits"].as_u64().unwrap() >= 1);
        assert!(a["score"].as_u64().unwrap() >= b["score"].as_u64().unwrap());
        // Ordering: highest score first.
        assert_eq!(files[0]["path"], "src/a.rs");
    }

    #[test]
    fn markdown_report_lists_top_hotspots() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_two_files(dir.path());
        let md = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("Hotspot report"));
        assert!(md.contains("Top "));
        assert!(md.contains("src/a.rs"));
    }

    #[test]
    fn since_filter_drops_old_commits() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_two_files(dir.path());
        // No commits in the future → both files have churn = 0.
        let json = HotspotAnalyzer::new()
            .with_since("2099-01-01")
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        assert!(files.iter().all(|f| f["commits"].as_u64().unwrap() == 0));
        assert!(files.iter().all(|f| f["score"].as_u64().unwrap() == 0));
    }

    #[test]
    fn since_option_filter_is_applied_when_present() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_two_files(dir.path());
        let json = HotspotAnalyzer::new()
            .with_since_opt(Some("2099-01-01".to_owned()))
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["since"], "2099-01-01");
        let files = parsed["files"].as_array().unwrap();
        assert!(files.iter().all(|f| f["commits"].as_u64().unwrap() == 0));
    }

    #[test]
    fn target_directory_outside_git_errors() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "lone.rs", "fn x() {}\n");
        let err = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, HotspotError::NotInGitRepo { .. }));
    }

    #[test]
    fn missing_path_surfaces_io_error() {
        let err = HotspotAnalyzer::new()
            .analyze(Path::new("/definitely/does/not/exist"), OutputFormat::Json)
            .unwrap_err();
        assert!(matches!(err, HotspotError::Io { .. }));
    }

    #[test]
    fn target_subdirectory_scopes_churn() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_two_files(dir.path());
        // Pointing at src/a.rs alone should still find churn for it.
        let json = HotspotAnalyzer::new()
            .analyze(&dir.path().join("src/a.rs"), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], "src/a.rs");
        assert!(files[0]["commits"].as_u64().unwrap() >= 2);
        // Single-file path must still extract complexity (cognitive_max > 0
        // for the nested-if body).
        assert!(files[0]["cognitive_max"].as_u64().unwrap() > 0);
        assert!(files[0]["function_count"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn complexity_metrics_are_included_for_walked_rust_files() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_two_files(dir.path());
        let json = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        let a = files.iter().find(|f| f["path"] == "src/a.rs").unwrap();
        // a.rs has nested ifs, so cognitive_max must be > 0. This guards
        // against `collect_complexity` or source collection short-circuiting.
        assert!(
            a["cognitive_max"].as_u64().unwrap() > 0,
            "expected non-zero cognitive complexity, got {a:?}",
        );
        assert!(a["function_count"].as_u64().unwrap() >= 1);
        assert!(a["loc"].as_u64().unwrap() >= 1);
    }

    fn init_repo_with_typescript_file(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        write_file(
            dir,
            "src/a.ts",
            "export function nest(n: number): number {\n    if (n > 0) {\n        if (n > 10) { return 1; }\n    }\n    return 0;\n}\n",
        );
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", "initial"]);
    }

    fn init_repo_with_python_file(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        write_file(
            dir,
            "src/a.py",
            "def nest(n):\n    if n > 0:\n        if n > 10:\n            return 1\n    return 0\n",
        );
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", "initial"]);
    }

    fn init_repo_with_go_file(dir: &Path) {
        run_git(dir, &["init", "-q", "-b", "main"]);
        run_git(dir, &["config", "user.email", "test@example.com"]);
        run_git(dir, &["config", "user.name", "Test"]);
        write_file(
            dir,
            "src/a.go",
            "package p\n\nfunc nest(n int) int {\n    if n > 0 {\n        if n > 10 { return 1 }\n    }\n    return 0\n}\n",
        );
        run_git(dir, &["add", "."]);
        run_git(dir, &["commit", "-q", "-m", "initial"]);
    }

    #[test]
    fn typescript_files_are_analyzed() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_typescript_file(dir.path());
        let json = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        let a = files.iter().find(|f| f["path"] == "src/a.ts").unwrap();
        assert!(
            a["cognitive_max"].as_u64().unwrap() > 0,
            "expected non-zero cognitive complexity for TS file, got {a:?}",
        );
        assert!(a["function_count"].as_u64().unwrap() >= 1);
        assert!(a["commits"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn python_files_are_analyzed() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_python_file(dir.path());
        let json = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        let a = files.iter().find(|f| f["path"] == "src/a.py").unwrap();
        assert!(
            a["cognitive_max"].as_u64().unwrap() > 0,
            "expected non-zero cognitive complexity for Python file, got {a:?}",
        );
        assert!(a["function_count"].as_u64().unwrap() >= 1);
        assert!(a["commits"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn go_files_are_analyzed() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_go_file(dir.path());
        let json = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        let a = files.iter().find(|f| f["path"] == "src/a.go").unwrap();
        assert!(
            a["cognitive_max"].as_u64().unwrap() > 0,
            "expected non-zero cognitive complexity for Go file, got {a:?}",
        );
        assert!(a["function_count"].as_u64().unwrap() >= 1);
        assert!(a["commits"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn mixed_language_files_appear_together() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(dir.path(), "src/a.rs", "pub fn r() -> i32 { 0 }\n");
        write_file(
            dir.path(),
            "src/b.ts",
            "export function t(): number { return 0; }\n",
        );
        write_file(dir.path(), "src/c.py", "def p():\n    return 0\n");
        write_file(
            dir.path(),
            "src/d.go",
            "package p\n\nfunc g() int { return 0 }\n",
        );
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let json = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
        assert!(paths.contains(&"src/a.rs"), "missing rs in {paths:?}");
        assert!(paths.contains(&"src/b.ts"), "missing ts in {paths:?}");
        assert!(paths.contains(&"src/c.py"), "missing py in {paths:?}");
        assert!(paths.contains(&"src/d.go"), "missing go in {paths:?}");
    }

    #[test]
    fn pycache_directories_are_not_descended() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_python_file(dir.path());
        write_file(dir.path(), ".gitignore", "__pycache__/\n");
        // A `.py` file under __pycache__ would be unusual, but when the
        // project ignores it the shared walker must skip it just like the
        // other analyzers do. Use a parseable body so a leak would
        // actually contribute to the report.
        write_file(
            dir.path(),
            "src/__pycache__/extra.py",
            "def cached():\n    if 1:\n        return 1\n    return 0\n",
        );
        let json = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        assert!(
            files
                .iter()
                .all(|f| f["path"] != "src/__pycache__/extra.py"),
            "__pycache__ file leaked into report: {files:?}",
        );
    }

    #[test]
    fn non_rust_files_are_excluded_from_complexity_walk() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_two_files(dir.path());
        // Add a non-Rust file alongside the .rs files. It must not appear
        // in the complexity rollup (function_count = 0) even if churn
        // mentions it later — but here it is uncommitted so the report
        // should not mention it at all.
        write_file(dir.path(), "src/notes.txt", "just text\n");
        let json = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        assert!(
            files.iter().all(|f| f["path"] != "src/notes.txt"),
            "non-Rust file leaked into report: {files:?}",
        );
    }

    #[test]
    fn skip_dirs_are_not_descended() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_two_files(dir.path());
        write_file(dir.path(), ".gitignore", "target/\n");
        // Drop a Rust file under an ignored target/ directory. It should
        // be skipped by the shared ignore-aware walker even though it has
        // the right extension.
        write_file(
            dir.path(),
            "target/generated.rs",
            "pub fn deep() -> i32 { if 1 > 0 { 1 } else { 0 } }\n",
        );
        let json = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        assert!(
            files.iter().all(|f| f["path"] != "target/generated.rs"),
            "target/ file leaked into report: {files:?}",
        );
    }

    #[test]
    fn gitignored_generated_source_is_not_analyzed() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_two_files(dir.path());
        write_file(dir.path(), ".gitignore", "dist/\n");
        write_file(
            dir.path(),
            "dist/assets/index.js",
            "export function generated(n) {\n  if (n > 0) {\n    if (n > 10) {\n      if (n > 20) { return 1; }\n    }\n  }\n  return 0;\n}\n",
        );

        let json = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        assert!(
            files.iter().all(|f| f["path"] != "dist/assets/index.js"),
            "ignored generated source leaked into report: {files:?}",
        );
    }

    #[test]
    fn churn_only_deleted_files_are_not_reported() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        write_file(dir.path(), "src/live.rs", "pub fn live() -> i32 { 1 }\n");
        write_file(
            dir.path(),
            "src/deleted.rs",
            "pub fn gone() -> i32 { if 1 > 0 { 1 } else { 0 } }\n",
        );
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);
        std::fs::remove_file(dir.path().join("src/deleted.rs")).unwrap();
        run_git(dir.path(), &["add", "-A"]);
        run_git(dir.path(), &["commit", "-q", "-m", "delete old file"]);

        let json = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        assert!(
            files.iter().all(|f| f["path"] != "src/deleted.rs"),
            "deleted churn-only file leaked into report: {files:?}",
        );
        assert!(files.iter().any(|f| f["path"] == "src/live.rs"));
    }

    #[test]
    fn dotfile_directories_are_not_descended() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_two_files(dir.path());
        // Add a Rust file under a hidden dir — it must be skipped.
        write_file(
            dir.path(),
            ".hidden/extra.rs",
            "pub fn h() -> i32 { if 1 > 0 { 1 } else { 0 } }\n",
        );
        let json = HotspotAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let files = parsed["files"].as_array().unwrap();
        assert!(
            files.iter().all(|f| f["path"] != ".hidden/extra.rs"),
            "dotfile dir leaked into report: {files:?}",
        );
    }

    #[test]
    fn pointing_at_non_rust_file_yields_no_complexity_entry() {
        // Single-file path: must skip when extension is not Rust. After
        // the walker exits, only the churn side could contribute and the
        // file is not committed, so the report must be empty.
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_two_files(dir.path());
        let txt = write_file(dir.path(), "loose.txt", "hello\n");
        let json = HotspotAnalyzer::new()
            .analyze(&txt, OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        // The non-Rust file is not committed, so it must not appear via
        // churn either; the file list is empty.
        assert_eq!(parsed["file_count"], 0);
    }

    #[test]
    fn path_filters_apply_to_churn_and_complexity_inputs() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);
        let body = "pub fn f(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }\n";
        write_file(dir.path(), "src/lib.rs", body);
        write_file(dir.path(), "tests/lib_test.rs", body);
        write_file(dir.path(), "src/generated.rs", body);
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let only_tests = HotspotAnalyzer::new()
            .with_only_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&only_tests).unwrap();
        assert_eq!(parsed["file_count"], 1);
        assert_eq!(parsed["files"][0]["path"], "tests/lib_test.rs");

        let exclude_tests = HotspotAnalyzer::new()
            .with_exclude_tests(true)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exclude_tests).unwrap();
        let paths: Vec<&str> = parsed["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"src/lib.rs"));
        assert!(!paths.contains(&"tests/lib_test.rs"));

        let exclude_generated = HotspotAnalyzer::new()
            .with_exclude_patterns(vec!["generated.rs".to_owned()])
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&exclude_generated).unwrap();
        let paths: Vec<&str> = parsed["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert!(!paths.contains(&"src/generated.rs"));
    }

    #[test]
    fn with_top_caps_the_markdown_table() {
        let dir = tempfile::tempdir().unwrap();
        init_repo_with_two_files(dir.path());
        let md = HotspotAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        // src/b.rs must NOT appear as a row when top=1 (only top-ranked
        // src/a.rs survives the cap).
        assert!(md.contains("Top 1 hotspots"), "got {md}");
        assert!(md.contains("| src/a.rs |"), "got {md}");
        assert!(!md.contains("| src/b.rs |"), "got {md}");
    }

    #[test]
    fn hotspot_error_io_display_includes_path_and_source() {
        let err = HotspotError::Io {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/x"), "got {msg}");
        assert!(msg.contains("missing"), "got {msg}");
        assert!(msg.starts_with("failed to read"), "got {msg}");
    }

    #[test]
    fn hotspot_error_git_display_trims_stderr() {
        let err = HotspotError::Git {
            stderr: "fatal: not a git repo\n".to_owned(),
        };
        let msg = err.to_string();
        assert!(msg.contains("fatal: not a git repo"), "got {msg}");
        assert!(!msg.ends_with('\n'), "trailing newline should be trimmed");
    }

    #[test]
    fn hotspot_error_not_in_git_repo_display_includes_path() {
        let err = HotspotError::NotInGitRepo {
            path: PathBuf::from("/tmp/lonely"),
        };
        let msg = err.to_string();
        assert!(msg.contains("/tmp/lonely"), "got {msg}");
        assert!(msg.contains("not inside a git working tree"), "got {msg}");
    }

    #[test]
    fn hotspot_error_serialize_display_includes_inner() {
        let serde_err = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        let err = HotspotError::Serialize(serde_err);
        let msg = err.to_string();
        assert!(msg.contains("serialize"), "got {msg}");
    }

    #[test]
    fn hotspot_error_io_source_is_present() {
        use std::error::Error as _;
        let err = HotspotError::Io {
            path: PathBuf::from("/tmp/x"),
            source: std::io::Error::other("boom"),
        };
        assert!(err.source().is_some());
    }

    #[test]
    fn hotspot_error_serialize_source_is_present() {
        use std::error::Error as _;
        let serde_err = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        let err = HotspotError::Serialize(serde_err);
        assert!(err.source().is_some());
    }

    #[test]
    fn hotspot_error_variants_without_source_return_none() {
        use std::error::Error as _;
        let err = HotspotError::Git {
            stderr: "fatal".to_owned(),
        };
        assert!(err.source().is_none());
        let err = HotspotError::NotInGitRepo {
            path: PathBuf::from("/tmp"),
        };
        assert!(err.source().is_none());
    }
}
