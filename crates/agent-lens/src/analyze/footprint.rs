//! `analyze footprint` — how far a pending diff reaches, and what it
//! left behind.
//!
//! The per-unit `--diff-only` gates answer "is anything *in* the diff
//! bad". Over-editing is a property of the diff as a whole: a request
//! for a one-line fix that comes back touching nine functions in four
//! modules, adding helpers nothing calls, or turning a real body into a
//! forwarder has no single unit a per-unit report could flag. This
//! reads the diff once, both sides, and reports its shape:
//!
//! * **Size.** Files, added and deleted lines and their ratio, and the
//!   functions touched — added, modified (body text changed), deleted.
//! * **Scatter.** Touched functions whose blast radius (the callers
//!   `analyze impact` would walk, capped at `--depth` hops) shares
//!   nothing with the largest group of touched functions. An edit there
//!   is outside the change's impact closure: it did not need the rest
//!   of the diff, and the rest of the diff did not need it.
//! * **Complexity delta.** Cognitive complexity before and after, per
//!   touched function.
//! * **New wrappers.** Functions the diff added or rewrote that are now
//!   forwarding-only and were not before.
//! * **Uncalled additions.** Functions the diff added that have no
//!   resolved or ambiguous caller outside tests — scaffolding nobody
//!   uses, or code written only for the test that exercises it.
//!
//! The "before" side is the index for the working-tree diff (the same
//! `git diff` every `--diff-only` reads), and the range's left side for
//! `--diff-range`. The working-tree reading also counts untracked,
//! non-ignored files as whole-file additions: `git diff` never shows a
//! file the edit created, and that is where scaffolding lands. The call graph is built from the files on disk, so a
//! range whose right side is not the working tree reads callers as they
//! are now; the report says which diff it read.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;
use tracing::warn;

use super::call_graph::algo::{bfs, reverse_adjacency};
use super::call_graph::model::CallGraphNode;
use super::call_graph::{CallGraph, CallGraphBuilder, delegate_call_graph_builders};
use super::diff::{FileDiff, diff_files, untracked_file_diffs};
use super::function_delta::{
    ChangeKind, FunctionChange, FunctionFact, compare, function_facts, wrapper_findings,
};
use super::options::analyzer_options;
use super::runner::render_report;
use super::{
    AnalyzeRoots, AnalyzerError, DiffScope, OutputFormat, SourceLang, collect_source_files,
    format_optional_f64, relative_display_path,
};

const SCHEMA_VERSION: u32 = 1;

/// Default blast-radius depth for the scatter check, in caller hops.
/// Shallower than `analyze impact`'s default on purpose: at five hops
/// nearly every function in a CLI shares `main` as an ancestor, and the
/// check would call every diff focused.
pub const DEFAULT_FOOTPRINT_DEPTH: usize = 2;

/// Markdown list cap when `--top` is not given.
const DEFAULT_TOP: usize = 15;

const NOTE: &str = "Shape of the pending diff, not a verdict on it. A function is touched when \
     its body text changed (reindents and line shifts do not count). `complexity_increases` keeps \
     modified functions whose cognitive complexity rose and added ones at 8 or above. Touched \
     functions are grouped when they share a file or their callers within `closure_depth` hops \
     intersect (trait methods, whose callers the graph cannot see, are left ungrouped); `outside_closure` lists the ones outside the largest group — edits the rest of \
     the diff neither reaches nor is reached by, which is what an unrequested edit looks like, \
     and also what a change spread across dispatch tables looks like. `uncalled_additions` are \
     added functions with no resolved or ambiguous call site outside tests whose name appears \
     nowhere else in the analyzed sources; trait methods, tests, and entry points are skipped. \
     The working-tree reading counts untracked files as added.";

/// Errors raised while measuring a footprint.
#[derive(Debug, thiserror::Error)]
pub enum FootprintError {
    #[error(transparent)]
    Analyzer(#[from] AnalyzerError),
    /// The analyzed path is not inside any git working tree, so there is
    /// no diff to measure.
    #[error("{path:?} is not inside a git working tree")]
    NotInGitRepo { path: PathBuf },
    /// `git diff` failed; its stderr is forwarded.
    #[error("git failed: {stderr}")]
    Git { stderr: String },
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

analyzer_options! {
    /// `analyze footprint` flags, and the `[profile.<name>.footprint]` table.
    pub struct FootprintOptions {
        @shared(ranking, diff);
        /// Caller hops the scatter check walks from each touched function.
        #[arg(long)]
        pub depth: Option<usize>,
    }
}

/// Analyzer entry point for `analyze footprint`.
#[derive(Debug, Clone)]
pub struct FootprintAnalyzer {
    builder: CallGraphBuilder,
    diff: DiffScope,
    depth: Option<usize>,
    top: Option<usize>,
}

/// A footprint *is* a diff, so an unset scope reads the working tree —
/// the same reasoning `analyze impact` defaults on.
impl Default for FootprintAnalyzer {
    fn default() -> Self {
        Self {
            builder: CallGraphBuilder::default(),
            diff: DiffScope::WorkingTree,
            depth: None,
            top: None,
        }
    }
}

impl FootprintAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a whole [`FootprintOptions`] group.
    pub fn with_options(self, opts: FootprintOptions) -> Self {
        let diff = opts.diff_scope();
        self.with_diff_scope(diff)
            .with_depth(opts.depth)
            .with_top(opts.top)
    }

    /// Which diff to measure. [`DiffScope::Disabled`] reads the working
    /// tree: there is no footprint without a diff.
    pub fn with_diff_scope(mut self, diff: DiffScope) -> Self {
        self.diff = match diff {
            DiffScope::Disabled => DiffScope::WorkingTree,
            other => other,
        };
        self
    }

    pub fn with_depth(mut self, depth: Option<usize>) -> Self {
        self.depth = depth;
        self
    }

    pub fn with_top(mut self, top: Option<usize>) -> Self {
        self.top = top;
        self
    }

    delegate_call_graph_builders! {
        builder,
        only_tests,
        exclude_tests,
    }

    fn resolved_depth(&self) -> usize {
        self.depth.unwrap_or(DEFAULT_FOOTPRINT_DEPTH)
    }

    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, FootprintError> {
        let roots = roots.into();
        let report = self.measure(&roots)?;
        Ok(render_report(&report, format, || {
            format_markdown(&report, self.top.unwrap_or(DEFAULT_TOP))
        })?)
    }

    /// Build the report without rendering it — the seam the
    /// post-tool-use hook reads.
    pub(crate) fn measure(&self, roots: &AnalyzeRoots) -> Result<Report, FootprintError> {
        let base = canonicalize(roots.base())?;
        let repo_root =
            crate::paths::git_repo_root(&base).ok_or_else(|| FootprintError::NotInGitRepo {
                path: roots.base().to_path_buf(),
            })?;
        let pathspecs = pathspecs(roots, &repo_root)?;
        let mut diffs = diff_files(&repo_root, &self.diff, &pathspecs)
            .map_err(|stderr| FootprintError::Git { stderr })?;
        if self.diff == DiffScope::WorkingTree {
            diffs.extend(
                untracked_file_diffs(&repo_root, &pathspecs)
                    .map_err(|stderr| FootprintError::Git { stderr })?,
            );
        }
        let revs = Revisions::for_scope(&repo_root, &self.diff);

        let filter = self
            .builder
            .collection_filter()
            .compile(roots.base())
            .map_err(AnalyzerError::from)?;
        let walked: HashMap<PathBuf, String> = collect_source_files(roots, &filter)?
            .into_iter()
            .filter_map(|sf| Some((sf.path.canonicalize().ok()?, sf.display_path)))
            .collect();

        let mut files = Vec::new();
        let mut skipped_file_count = 0;
        for diff in &diffs {
            match self.measure_file(&repo_root, &base, diff, &revs, &walked, &filter) {
                Some(file) => files.push(file),
                None => skipped_file_count += 1,
            }
        }

        let graph = if files.iter().any(FileFootprint::has_live_change) {
            Some(self.builder.build(roots)?)
        } else {
            None
        };
        Ok(Report::build(
            roots,
            &self.diff,
            files,
            skipped_file_count,
            graph.as_deref().map(|graph| (graph, &walked)),
            self.resolved_depth(),
        ))
    }

    /// Both sides of one changed file, or `None` when the file is not a
    /// source file this run analyzes (another language, filtered out,
    /// outside the analyzed path) or one side failed to parse.
    fn measure_file(
        &self,
        repo_root: &Path,
        base: &Path,
        diff: &FileDiff,
        revs: &Revisions,
        walked: &HashMap<PathBuf, String>,
        filter: &super::CompiledPathFilter,
    ) -> Option<FileFootprint> {
        let display = match (&diff.new_path, &diff.old_path) {
            (Some(new), _) => {
                let abs = repo_root.join(new).canonicalize().ok()?;
                walked.get(&abs)?.clone()
            }
            // A deletion has no file to walk; judge it by its old name.
            (None, Some(old)) => {
                let abs = repo_root.join(old);
                if !abs.starts_with(base) || !filter.includes_path(&abs) {
                    return None;
                }
                relative_display_path(&abs, base)
            }
            (None, None) => return None,
        };
        let lang = SourceLang::from_path(Path::new(&display))?;
        let path_label = display.clone();
        let before = match &diff.old_path {
            Some(old) => Some(revs.before(repo_root, old)?),
            None => None,
        };
        let after = match &diff.new_path {
            Some(new) => Some(revs.after(repo_root, new)?),
            None => None,
        };
        let parse = |text: &Option<String>| -> Option<Side> {
            let Some(text) = text else {
                return Some(Side::default());
            };
            match Side::parse(lang, text) {
                Ok(side) => Some(side),
                Err(e) => {
                    warn!(file = %path_label, error = %e, "footprint: skipping unparsable file");
                    None
                }
            }
        };
        let (before, after) = (parse(&before)?, parse(&after)?);
        Some(FileFootprint::new(display, diff, &before, &after))
    }
}

/// Every root's path relative to the repository root, as git pathspecs.
/// A root that *is* the repository root widens the diff to the whole
/// tree, which an empty list means.
fn pathspecs(roots: &AnalyzeRoots, repo_root: &Path) -> Result<Vec<String>, FootprintError> {
    let mut out = Vec::new();
    for root in roots.paths() {
        let abs = canonicalize(root)?;
        match abs.strip_prefix(repo_root) {
            Ok(rel) if rel.as_os_str().is_empty() => return Ok(Vec::new()),
            Ok(rel) => out.push(rel.to_string_lossy().replace('\\', "/")),
            Err(_) => {
                return Err(FootprintError::NotInGitRepo {
                    path: root.to_path_buf(),
                });
            }
        }
    }
    Ok(out)
}

fn canonicalize(path: &Path) -> Result<PathBuf, FootprintError> {
    path.canonicalize().map_err(|source| FootprintError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// Where each side of the diff is read from.
#[derive(Debug)]
struct Revisions {
    /// Revision the pre-image is read at; `None` is the index.
    before: Option<String>,
    /// Revision the post-image is read at; `None` is the working tree.
    after: Option<String>,
}

impl Revisions {
    /// Resolve the two sides the way `git diff` itself reads `scope`:
    /// the working-tree diff is index → disk, `A..B` is A → B, `A...B`
    /// is merge-base(A, B) → B, and a bare revision is it → disk. An
    /// empty side of a range is `HEAD`, as in git.
    fn for_scope(repo_root: &Path, scope: &DiffScope) -> Self {
        let DiffScope::Range(range) = scope else {
            return Self {
                before: None,
                after: None,
            };
        };
        let or_head = |side: &str| {
            if side.is_empty() {
                "HEAD".to_owned()
            } else {
                side.to_owned()
            }
        };
        if let Some((left, right)) = range.split_once("...") {
            let (left, right) = (or_head(left), or_head(right));
            let base = git_stdout(repo_root, &["merge-base", &left, &right])
                .map(|out| out.trim().to_owned())
                .unwrap_or(left);
            return Self {
                before: Some(base),
                after: Some(right),
            };
        }
        if let Some((left, right)) = range.split_once("..") {
            return Self {
                before: Some(or_head(left)),
                after: Some(or_head(right)),
            };
        }
        Self {
            before: Some(range.clone()),
            after: None,
        }
    }

    fn before(&self, repo_root: &Path, path: &str) -> Option<String> {
        let spec = match &self.before {
            Some(rev) => format!("{rev}:{path}"),
            None => format!(":{path}"),
        };
        git_stdout(repo_root, &["show", &spec])
    }

    fn after(&self, repo_root: &Path, path: &str) -> Option<String> {
        match &self.after {
            Some(rev) => git_stdout(repo_root, &["show", &format!("{rev}:{path}")]),
            None => std::fs::read_to_string(repo_root.join(path)).ok(),
        }
    }
}

/// Stdout of one `git` invocation under `repo_root`, or `None` when it
/// failed — the file then reads as unanalyzable and is skipped, never
/// as empty.
fn git_stdout(repo_root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        warn!(
            args = ?args,
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "footprint: git read failed",
        );
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// One side of a file, reduced to what the comparison reads.
#[derive(Debug, Default)]
struct Side {
    facts: Vec<FunctionFact>,
    /// Forwarding-only functions, name → callee.
    wrappers: BTreeMap<String, String>,
}

impl Side {
    fn parse(lang: SourceLang, text: &str) -> Result<Self, AnalyzerError> {
        let facts = function_facts(lang, text)?;
        let wrappers = wrapper_findings(lang, text)?
            .into_iter()
            .map(|w| (w.name, w.callee))
            .collect();
        Ok(Self { facts, wrappers })
    }
}

/// One file's contribution to the footprint.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct FileFootprint {
    pub(crate) file: String,
    status: &'static str,
    lines_added: usize,
    lines_deleted: usize,
    pub(crate) functions: Vec<FunctionRow>,
    #[serde(skip)]
    new_wrappers: Vec<WrapperRow>,
}

impl FileFootprint {
    fn new(file: String, diff: &FileDiff, before: &Side, after: &Side) -> Self {
        let status = match (&diff.old_path, &diff.new_path) {
            (None, _) => "added",
            (_, None) => "deleted",
            (Some(old), Some(new)) if old != new => "renamed",
            _ => "modified",
        };
        let changes = compare(&before.facts, &after.facts);
        let functions: Vec<FunctionRow> = changes
            .iter()
            .map(|change| FunctionRow::new(&file, change))
            .collect();
        let new_wrappers = changes
            .iter()
            .filter(|change| change.kind != ChangeKind::Deleted)
            .filter_map(|change| {
                let fact = change.after?;
                let callee = after.wrappers.get(&fact.name)?;
                if before.wrappers.contains_key(&fact.name) && change.kind == ChangeKind::Modified {
                    return None;
                }
                Some(WrapperRow {
                    file: file.clone(),
                    name: fact.name.clone(),
                    line: fact.start_line,
                    callee: callee.clone(),
                    change: change.kind,
                })
            })
            .collect();
        Self {
            file,
            status,
            lines_added: diff.added_lines,
            lines_deleted: diff.deleted_lines,
            functions,
            new_wrappers,
        }
    }

    fn has_live_change(&self) -> bool {
        self.functions
            .iter()
            .any(|row| row.change != ChangeKind::Deleted)
    }
}

/// One touched function.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct FunctionRow {
    pub(crate) file: String,
    pub(crate) name: String,
    pub(crate) change: ChangeKind,
    /// Span on the surviving side (the pre-image for a deletion).
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    cognitive_before: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cognitive_after: Option<u32>,
    pub(crate) cognitive_delta: i64,
    /// Scatter group the function landed in; `None` for deletions and
    /// functions the call graph did not model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cluster: Option<usize>,
}

impl FunctionRow {
    fn new(file: &str, change: &FunctionChange<'_>) -> Self {
        let span = change.after.or(change.before);
        Self {
            file: file.to_owned(),
            name: change.name().to_owned(),
            change: change.kind,
            start_line: span.map_or(0, |f| f.start_line),
            end_line: span.map_or(0, |f| f.end_line),
            cognitive_before: change.before.map(|f| f.cognitive),
            cognitive_after: change.after.map(|f| f.cognitive),
            cognitive_delta: change.cognitive_delta(),
            cluster: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct WrapperRow {
    pub(crate) file: String,
    pub(crate) name: String,
    pub(crate) line: usize,
    pub(crate) callee: String,
    change: ChangeKind,
}

/// A touched function whose blast radius is disjoint from the largest
/// group's.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct OutsideRow {
    pub(crate) file: String,
    pub(crate) name: String,
    pub(crate) line: usize,
    cluster: usize,
    cluster_size: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct UncalledRow {
    pub(crate) file: String,
    pub(crate) name: String,
    pub(crate) line: usize,
    #[serde(skip)]
    end_line: usize,
    /// Test functions that call it. Non-zero is code written for the
    /// test that exercises it and for nothing else.
    test_callers: usize,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ComplexityRow {
    pub(crate) file: String,
    pub(crate) name: String,
    pub(crate) line: usize,
    change: ChangeKind,
    before: u32,
    pub(crate) after: u32,
    pub(crate) delta: i64,
}

#[derive(Debug, Clone, Serialize)]
struct Summary {
    file_count: usize,
    skipped_file_count: usize,
    lines_added: usize,
    lines_deleted: usize,
    /// `lines_added / lines_deleted`; absent when nothing was deleted.
    #[serde(skip_serializing_if = "Option::is_none")]
    add_delete_ratio: Option<f64>,
    functions_touched: usize,
    functions_added: usize,
    functions_modified: usize,
    functions_deleted: usize,
    cluster_count: usize,
    closure_depth: usize,
    outside_closure_count: usize,
    complexity_increase_count: usize,
    new_wrapper_count: usize,
    uncalled_addition_count: usize,
}

/// The whole footprint report.
#[derive(Debug, Serialize)]
pub(crate) struct Report {
    schema_version: u32,
    root: String,
    diff: String,
    note: &'static str,
    summary: Summary,
    pub(crate) files: Vec<FileFootprint>,
    pub(crate) outside_closure: Vec<OutsideRow>,
    pub(crate) complexity_increases: Vec<ComplexityRow>,
    pub(crate) new_wrappers: Vec<WrapperRow>,
    pub(crate) uncalled_additions: Vec<UncalledRow>,
}

impl Report {
    fn build(
        roots: &AnalyzeRoots,
        diff: &DiffScope,
        mut files: Vec<FileFootprint>,
        skipped_file_count: usize,
        graph: Option<(&CallGraph, &HashMap<PathBuf, String>)>,
        depth: usize,
    ) -> Self {
        let mut uncalled_additions = Vec::new();
        let mut outside_closure = Vec::new();
        let mut cluster_count = 0;
        if let Some((graph, walked)) = graph {
            let touched = touched_nodes(graph, &files);
            uncalled_additions = uncalled(graph, &files, &touched);
            drop_referenced(&mut uncalled_additions, walked, &TestSpans::of(graph));
            // A trait method's callers are invisible, so its closure is
            // itself alone and it would read as scatter every time.
            let judged: BTreeMap<(usize, usize), usize> = touched
                .iter()
                .filter(|&(_, &node)| !is_dispatched(&graph.nodes[node]))
                .map(|(&key, &node)| (key, node))
                .collect();
            let clusters = Clusters::of(graph, &judged, depth);
            cluster_count = clusters.count;
            outside_closure = clusters.apply(&mut files);
        }
        let complexity_increases = complexity_increases(&files);
        let new_wrappers: Vec<WrapperRow> = files
            .iter()
            .flat_map(|f| f.new_wrappers.iter().cloned())
            .collect();
        let count = |kind| {
            files
                .iter()
                .flat_map(|f| &f.functions)
                .filter(|row| row.change == kind)
                .count()
        };
        let (added, modified, deleted) = (
            count(ChangeKind::Added),
            count(ChangeKind::Modified),
            count(ChangeKind::Deleted),
        );
        let lines_added: usize = files.iter().map(|f| f.lines_added).sum();
        let lines_deleted: usize = files.iter().map(|f| f.lines_deleted).sum();
        #[allow(clippy::cast_precision_loss)]
        let add_delete_ratio =
            (lines_deleted > 0).then(|| lines_added as f64 / lines_deleted as f64);
        let summary = Summary {
            file_count: files.len(),
            skipped_file_count,
            lines_added,
            lines_deleted,
            add_delete_ratio,
            functions_touched: added + modified + deleted,
            functions_added: added,
            functions_modified: modified,
            functions_deleted: deleted,
            cluster_count,
            closure_depth: depth,
            outside_closure_count: outside_closure.len(),
            complexity_increase_count: complexity_increases.len(),
            new_wrapper_count: new_wrappers.len(),
            uncalled_addition_count: uncalled_additions.len(),
        };
        Self {
            schema_version: SCHEMA_VERSION,
            root: roots.display(),
            diff: describe_scope(diff),
            note: NOTE,
            summary,
            files,
            outside_closure,
            complexity_increases,
            new_wrappers,
            uncalled_additions,
        }
    }

    /// Keep only the flagged rows whose file passes `keep`, and bring
    /// the summary counts along. The size figures stay whole-diff.
    pub(crate) fn retain_flags_in(&mut self, keep: impl Fn(&str) -> bool) {
        self.outside_closure.retain(|row| keep(&row.file));
        self.complexity_increases.retain(|row| keep(&row.file));
        self.new_wrappers.retain(|row| keep(&row.file));
        self.uncalled_additions.retain(|row| keep(&row.file));
        self.summary.outside_closure_count = self.outside_closure.len();
        self.summary.complexity_increase_count = self.complexity_increases.len();
        self.summary.new_wrapper_count = self.new_wrappers.len();
        self.summary.uncalled_addition_count = self.uncalled_additions.len();
    }

    /// The `--format md` rendering, with each list capped at `top`.
    pub(crate) fn render_markdown(&self, top: usize) -> String {
        format_markdown(self, top)
    }

    /// Whether anything beyond the size figures is worth an agent's
    /// attention.
    pub(crate) fn has_flags(&self) -> bool {
        !(self.outside_closure.is_empty()
            && self.complexity_increases.is_empty()
            && self.new_wrappers.is_empty()
            && self.uncalled_additions.is_empty())
    }
}

fn describe_scope(scope: &DiffScope) -> String {
    match scope {
        DiffScope::Range(range) => range.clone(),
        _ => "working tree".to_owned(),
    }
}

/// Graph node index of every added or modified function, keyed by
/// `(file, row index)` in `files`.
fn touched_nodes(graph: &CallGraph, files: &[FileFootprint]) -> BTreeMap<(usize, usize), usize> {
    let mut by_file: HashMap<&str, Vec<usize>> = HashMap::new();
    for (idx, node) in graph.nodes.iter().enumerate() {
        by_file.entry(node.file.as_str()).or_default().push(idx);
    }
    let mut out = BTreeMap::new();
    for (fi, file) in files.iter().enumerate() {
        let Some(candidates) = by_file.get(file.file.as_str()) else {
            continue;
        };
        for (ri, row) in file.functions.iter().enumerate() {
            if row.change == ChangeKind::Deleted {
                continue;
            }
            if let Some(node) = node_for(graph, candidates, row) {
                out.insert((fi, ri), node);
            }
        }
    }
    out
}

/// The graph node for `row`. The graph is built from the same adapter
/// facts as the complexity units, so a function starts on the same line
/// in both.
fn node_for(graph: &CallGraph, candidates: &[usize], row: &FunctionRow) -> Option<usize> {
    candidates
        .iter()
        .copied()
        .find(|&idx| graph.nodes[idx].start_line == row.start_line)
}

/// Added, non-test, non-trait functions with no resolved or ambiguous
/// call site from production code.
fn uncalled(
    graph: &CallGraph,
    files: &[FileFootprint],
    touched: &BTreeMap<(usize, usize), usize>,
) -> Vec<UncalledRow> {
    let index_by_id = graph.node_index_by_id();
    let mut prod_called: HashSet<usize> = HashSet::new();
    let mut test_callers: HashMap<usize, BTreeSet<usize>> = HashMap::new();
    for edge in &graph.edges {
        let from = edge.from.as_deref().and_then(|id| index_by_id.get(id));
        let targets = edge
            .to
            .iter()
            .chain(edge.candidates.iter())
            .filter_map(|id| index_by_id.get(id.as_str()));
        for &to in targets {
            match from {
                Some(&from) if from == to => {}
                Some(&from) if graph.nodes[from].is_test => {
                    test_callers.entry(to).or_default().insert(from);
                }
                // An unattributed call site (module-level code, a
                // closure the adapter could not place) still calls it.
                _ => {
                    prod_called.insert(to);
                }
            }
        }
    }
    let mut out = Vec::new();
    for (&(fi, ri), &node_idx) in touched {
        let row = &files[fi].functions[ri];
        let node = &graph.nodes[node_idx];
        if row.change != ChangeKind::Added || !may_need_a_caller(node) {
            continue;
        }
        if prod_called.contains(&node_idx) {
            continue;
        }
        out.push(UncalledRow {
            file: row.file.clone(),
            name: row.name.clone(),
            line: row.start_line,
            end_line: row.end_line,
            test_callers: test_callers.get(&node_idx).map_or(0, BTreeSet::len),
        });
    }
    out
}

/// Drop every row whose bare name appears anywhere in the analyzed
/// sources outside its own definition — a path reference
/// (`iter().any(Owner::check)`), a call the graph left unresolved, a
/// macro argument. The static graph misses all of those, and the only
/// cheap check that sees them is the name itself. Common names are then
/// never reported, which trades recall for a list worth reading. One
/// pass over the sources, and none when there is nothing to check.
fn drop_referenced(
    rows: &mut Vec<UncalledRow>,
    walked: &HashMap<PathBuf, String>,
    tests: &TestSpans<'_>,
) {
    if rows.is_empty() {
        return;
    }
    let bare = |name: &str| -> String { name.rsplit([':', '.']).next().unwrap_or(name).to_owned() };
    let wanted: BTreeSet<String> = rows.iter().map(|row| bare(&row.name)).collect();
    let mut sites: HashMap<String, Vec<(&str, usize)>> = HashMap::new();
    for (path, display) in walked {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            for ident in super::unreachable::identifiers(line) {
                if wanted.contains(ident) {
                    sites
                        .entry(ident.to_owned())
                        .or_default()
                        .push((display.as_str(), i + 1));
                }
            }
        }
    }
    rows.retain(|row| {
        // Its own definition names it, and so does every test that
        // calls it — which `test_callers` already reports.
        let own = |&(file, line): &(&str, usize)| {
            (file == row.file && (row.line..=row.end_line).contains(&line))
                || tests.contains(file, line)
        };
        sites
            .get(&bare(&row.name))
            .is_none_or(|found| found.iter().all(own))
    });
}

/// Line spans of the test functions the graph found, per file.
struct TestSpans<'g>(HashMap<&'g str, Vec<(usize, usize)>>);

impl<'g> TestSpans<'g> {
    fn of(graph: &'g CallGraph) -> Self {
        let mut spans: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
        for node in graph.nodes.iter().filter(|node| node.is_test) {
            spans
                .entry(node.file.as_str())
                .or_default()
                .push((node.start_line, node.end_line));
        }
        Self(spans)
    }

    fn contains(&self, file: &str, line: usize) -> bool {
        self.0
            .get(file)
            .is_some_and(|spans| spans.iter().any(|&(s, e)| (s..=e).contains(&line)))
    }
}

/// Whether "nothing calls it" means anything for this node. Tests,
/// entry points, and trait methods (reached through dispatch the graph
/// does not resolve) are supposed to have no static caller.
fn may_need_a_caller(node: &CallGraphNode) -> bool {
    if node.is_test {
        return false;
    }
    if node.impl_owner.is_none() && matches!(node.name.as_str(), "main" | "init") {
        return false;
    }
    !is_dispatched(node)
}

/// Trait declarations and trait-impl methods: reached through dispatch
/// the graph does not resolve, so their callers — and therefore their
/// blast radius — are invisible to it.
fn is_dispatched(node: &CallGraphNode) -> bool {
    matches!(
        node.owner_kind,
        Some(lens_domain::OwnerKind::TraitImpl | lens_domain::OwnerKind::Trait)
    )
}

/// Touched functions grouped by overlapping blast radius.
struct Clusters {
    /// Cluster id per touched `(file, row)`, numbered from 1 by size
    /// (largest first), then by first appearance.
    ids: BTreeMap<(usize, usize), usize>,
    sizes: BTreeMap<usize, usize>,
    count: usize,
}

impl Clusters {
    /// Union two touched functions when their capped caller closures
    /// intersect — which includes one calling the other within `depth`
    /// hops, since each closure contains its own function — or when they
    /// sit in the same file.
    fn of(graph: &CallGraph, touched: &BTreeMap<(usize, usize), usize>, depth: usize) -> Self {
        let keys: Vec<(usize, usize)> = touched.keys().copied().collect();
        let starts: Vec<usize> = keys.iter().map(|key| touched[key]).collect();
        let owners = closure_owners(graph, &starts, depth);
        let mut parent: Vec<usize> = (0..keys.len()).collect();
        let shared_closure = owners.values().flat_map(|members| members.windows(2));
        for pair in shared_closure {
            union(&mut parent, pair[0], pair[1]);
        }
        // Edits within one file travel together: the file is the unit a
        // reviewer reads, and a builder method nobody resolves a call to
        // would otherwise count as scatter from its own constructor.
        let same_file = (1..keys.len()).filter(|&k| keys[k - 1].0 == keys[k].0);
        for k in same_file.collect::<Vec<_>>() {
            union(&mut parent, k - 1, k);
        }
        let mut ordered = components(&mut parent);
        ordered.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a[0].cmp(&b[0])));
        let ids = ordered
            .iter()
            .enumerate()
            .flat_map(|(i, members)| {
                let keys = &keys;
                members.iter().map(move |&k| (keys[k], i + 1))
            })
            .collect();
        let sizes = ordered
            .iter()
            .enumerate()
            .map(|(i, members)| (i + 1, members.len()))
            .collect();
        Self {
            ids,
            sizes,
            count: ordered.len(),
        }
    }

    /// Stamp each touched row with its cluster and list the ones outside
    /// cluster 1. A tie for the largest cluster leaves every function
    /// "inside": with no single main change, none is the stray.
    fn apply(&self, files: &mut [FileFootprint]) -> Vec<OutsideRow> {
        let main_is_unique = self.sizes.get(&1) != self.sizes.get(&2);
        let mut out = Vec::new();
        for (&(fi, ri), &id) in &self.ids {
            let row = &mut files[fi].functions[ri];
            row.cluster = Some(id);
            if id != 1 && main_is_unique {
                out.push(OutsideRow {
                    file: row.file.clone(),
                    name: row.name.clone(),
                    line: row.start_line,
                    cluster: id,
                    cluster_size: self.sizes.get(&id).copied().unwrap_or(0),
                });
            }
        }
        out
    }
}

/// For every node within `depth` caller hops of some start, the indices
/// (into `starts`) of the starts whose closure holds it.
fn closure_owners(graph: &CallGraph, starts: &[usize], depth: usize) -> HashMap<usize, Vec<usize>> {
    let reverse = reverse_adjacency(&graph.resolved_adjacency());
    let mut owners: HashMap<usize, Vec<usize>> = HashMap::new();
    for (k, &start) in starts.iter().enumerate() {
        let visits = bfs(&reverse, &[start]);
        for visit in visits.iter().take_while(|visit| visit.depth <= depth) {
            owners.entry(visit.node).or_default().push(k);
        }
    }
    owners
}

/// The union-find forest as member lists, each in ascending order.
fn components(parent: &mut [usize]) -> Vec<Vec<usize>> {
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for k in 0..parent.len() {
        let root = find(parent, k);
        groups.entry(root).or_default().push(k);
    }
    groups.into_values().collect()
}

/// The root of `x`'s tree, compressing the path on the way back.
fn find(parent: &mut [usize], x: usize) -> usize {
    let up = parent[x];
    if up == x {
        return x;
    }
    let root = find(parent, up);
    parent[x] = root;
    root
}

/// Join the trees of `a` and `b`. Which root survives does not matter:
/// [`components`] lists members in index order either way.
fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    parent[rb] = ra;
}

/// Cognitive complexity a newly added function has to reach before it
/// is listed. Every added function "rises" from zero, so listing each
/// one would bury the modified functions that actually got harder;
/// matches the floor the pre-edit complexity hook reports at.
pub(crate) const ADDED_COGNITIVE_FLOOR: u32 = 8;

/// Modified functions whose cognitive complexity rose, and added ones
/// at or above [`ADDED_COGNITIVE_FLOOR`], largest rise first.
fn complexity_increases(files: &[FileFootprint]) -> Vec<ComplexityRow> {
    let mut out: Vec<ComplexityRow> = files
        .iter()
        .flat_map(|f| &f.functions)
        .filter(|row| match row.change {
            ChangeKind::Modified => row.cognitive_delta > 0,
            ChangeKind::Added => row.cognitive_after.unwrap_or(0) >= ADDED_COGNITIVE_FLOOR,
            ChangeKind::Deleted => false,
        })
        .map(|row| ComplexityRow {
            file: row.file.clone(),
            name: row.name.clone(),
            line: row.start_line,
            change: row.change,
            before: row.cognitive_before.unwrap_or(0),
            after: row.cognitive_after.unwrap_or(0),
            delta: row.cognitive_delta,
        })
        .collect();
    out.sort_by(|a, b| {
        b.delta
            .cmp(&a.delta)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });
    out
}

fn format_markdown(report: &Report, top: usize) -> String {
    let s = &report.summary;
    let mut out = format!("# footprint: {} ({})\n\n", report.root, report.diff);
    let _ = writeln!(
        out,
        "{} file(s), +{}/-{} lines (added/deleted {}), {} function(s) touched ({} added, {} modified, {} deleted)",
        s.file_count,
        s.lines_added,
        s.lines_deleted,
        format_optional_f64(s.add_delete_ratio, 2),
        s.functions_touched,
        s.functions_added,
        s.functions_modified,
        s.functions_deleted,
    );
    if s.cluster_count > 0 {
        let _ = writeln!(
            out,
            "{} cluster(s) of overlapping blast radius at depth {}; {} function(s) outside the main one",
            s.cluster_count, s.closure_depth, s.outside_closure_count,
        );
    }
    section(
        &mut out,
        "Outside the main cluster",
        &report.outside_closure,
        top,
        |r| {
            format!(
                "{}:{} `{}` (cluster {}, size {})",
                r.file, r.line, r.name, r.cluster, r.cluster_size
            )
        },
    );
    section(
        &mut out,
        "Complexity increases",
        &report.complexity_increases,
        top,
        |r| {
            format!(
                "{}:{} `{}` cog {}→{} (+{}, {})",
                r.file,
                r.line,
                r.name,
                r.before,
                r.after,
                r.delta,
                r.change.as_str()
            )
        },
    );
    section(&mut out, "New wrappers", &report.new_wrappers, top, |r| {
        format!("{}:{} `{}` -> `{}`", r.file, r.line, r.name, r.callee)
    });
    section(
        &mut out,
        "Added with no caller",
        &report.uncalled_additions,
        top,
        |r| {
            if r.test_callers > 0 {
                format!(
                    "{}:{} `{}` (called only by {} test(s))",
                    r.file, r.line, r.name, r.test_callers
                )
            } else {
                format!("{}:{} `{}`", r.file, r.line, r.name)
            }
        },
    );
    out
}

fn section<T>(out: &mut String, title: &str, rows: &[T], top: usize, line: impl Fn(&T) -> String) {
    if rows.is_empty() {
        return;
    }
    let _ = writeln!(out, "\n## {title} ({})", rows.len());
    for row in rows.iter().take(top) {
        let _ = writeln!(out, "- {}", line(row));
    }
    if rows.len() > top {
        let _ = writeln!(out, "- … +{} more", rows.len() - top);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::test_support::{run_git, write_file};
    use rstest::rstest;
    use serde_json::Value;

    const LIB_BEFORE: &str = "\
mod far;

pub fn entry(n: i32) -> i32 {
    step(n) + other(n)
}

fn step(n: i32) -> i32 {
    if n > 0 { n } else { -n }
}

fn other(n: i32) -> i32 {
    n * 3 + 1
}
";

    const LIB_AFTER: &str = "\
mod far;

pub fn entry(n: i32) -> i32 {
    step(n) + other(n)
}

fn step(n: i32) -> i32 {
    if n > 0 {
        if n > 10 { for _ in 0..n { if n % 2 == 0 { return n; } } }
        n
    } else {
        -n
    }
}

fn other(n: i32) -> i32 {
    helper(n)
}

fn helper(n: i32) -> i32 {
    n * 3 + 1
}

fn unused_helper() -> i32 {
    42
}
";

    const FAR_BEFORE: &str = "pub fn far_away(n: i32) -> i32 {\n    n - 7\n}\n";
    const FAR_AFTER: &str = "pub fn far_away(n: i32) -> i32 {\n    n - 8\n}\n";

    /// A committed crate plus a working-tree edit that has one of
    /// everything: a complexity rise, a new wrapper, an added helper
    /// something calls, one nothing calls, an edit in a file the rest of
    /// the change never reaches, and an untracked new file.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init", "-q"]);
        write_file(
            root,
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write_file(root, "src/lib.rs", LIB_BEFORE);
        write_file(root, "src/far.rs", FAR_BEFORE);
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "init"]);
        write_file(root, "src/lib.rs", LIB_AFTER);
        write_file(root, "src/far.rs", FAR_AFTER);
        write_file(
            root,
            "src/extra.rs",
            "pub fn extra_thing() -> i32 {\n    5\n}\n",
        );
        dir
    }

    fn json(analyzer: &FootprintAnalyzer, root: &Path) -> Value {
        serde_json::from_str(&analyzer.analyze(root, OutputFormat::Json).unwrap()).unwrap()
    }

    fn names(report: &Value, list: &str) -> Vec<String> {
        let mut out: Vec<String> = report[list]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap().to_owned())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn working_tree_footprint_reports_every_kind_of_flag() {
        let dir = fixture();
        let report = json(&FootprintAnalyzer::new(), dir.path());
        let s = &report["summary"];
        assert_eq!(s["file_count"], 3, "untracked extra.rs counts: {report}");
        assert_eq!(s["functions_added"], 3);
        assert_eq!(s["functions_modified"], 3);
        assert_eq!(s["functions_deleted"], 0);
        assert_eq!(s["closure_depth"], DEFAULT_FOOTPRINT_DEPTH);
        assert_eq!(names(&report, "complexity_increases"), ["step"]);
        assert_eq!(names(&report, "new_wrappers"), ["other"]);
        assert_eq!(report["new_wrappers"][0]["callee"], "helper");
        assert_eq!(
            names(&report, "uncalled_additions"),
            ["extra_thing", "unused_helper"],
            "`helper` has a caller and is not listed",
        );
        assert_eq!(
            names(&report, "outside_closure"),
            ["extra_thing", "far_away"],
            "the lib.rs edits form the main cluster",
        );
        let extra = report["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["file"] == "src/extra.rs")
            .unwrap();
        assert_eq!(extra["status"], "added");
    }

    #[test]
    fn range_footprint_reads_both_sides_from_git() {
        let dir = fixture();
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "edit"]);
        let clean = json(&FootprintAnalyzer::new(), dir.path());
        assert_eq!(
            clean["summary"]["file_count"], 0,
            "nothing pending: {clean}"
        );
        assert!(clean["outside_closure"].as_array().unwrap().is_empty());

        let analyzer =
            FootprintAnalyzer::new().with_diff_scope(DiffScope::Range("HEAD~1..HEAD".to_owned()));
        let report = json(&analyzer, dir.path());
        assert_eq!(report["diff"], "HEAD~1..HEAD");
        assert_eq!(report["summary"]["functions_added"], 3);
        assert_eq!(names(&report, "new_wrappers"), ["other"]);
        let triple =
            FootprintAnalyzer::new().with_diff_scope(DiffScope::Range("HEAD~1...HEAD".to_owned()));
        assert_eq!(json(&triple, dir.path())["summary"], report["summary"]);
    }

    #[test]
    fn a_deleted_function_is_touched_but_not_flagged() {
        let dir = fixture();
        write_file(dir.path(), "src/far.rs", "\n");
        let report = json(&FootprintAnalyzer::new(), dir.path());
        assert_eq!(report["summary"]["functions_deleted"], 1);
        assert_eq!(
            report["summary"]["functions_touched"], 6,
            "3 added, 2 modified, 1 deleted"
        );
        let far = report["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["file"] == "src/far.rs")
            .unwrap();
        assert_eq!(far["functions"][0]["change"], "deleted");
        assert!(!names(&report, "outside_closure").contains(&"far_away".to_owned()));
    }

    #[test]
    fn exclude_tests_and_depth_reach_the_report() {
        let dir = fixture();
        write_file(
            dir.path(),
            "tests/it.rs",
            "#[test]\nfn t() { assert_eq!(1, 1); }\n",
        );
        let with_tests = json(&FootprintAnalyzer::new(), dir.path());
        let without = json(
            &FootprintAnalyzer::new()
                .with_exclude_tests(true)
                .with_depth(Some(4)),
            dir.path(),
        );
        assert_eq!(
            with_tests["summary"]["file_count"].as_u64().unwrap(),
            without["summary"]["file_count"].as_u64().unwrap() + 1,
        );
        assert_eq!(without["summary"]["closure_depth"], 4);
    }

    #[test]
    fn markdown_lists_each_flag_and_caps_at_top() {
        let dir = fixture();
        let md = FootprintAnalyzer::new()
            .with_top(Some(1))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.starts_with("# footprint: "), "got {md}");
        assert!(
            md.contains("function(s) touched (3 added, 3 modified, 0 deleted)"),
            "got {md}"
        );
        assert!(md.contains("## Outside the main cluster (2)"), "got {md}");
        assert!(md.contains("- … +1 more"), "`--top 1` hides one row: {md}");
        assert!(
            md.contains("## New wrappers (1)\n- src/lib.rs:"),
            "got {md}"
        );
        assert!(md.contains("`other` -> `helper`"), "got {md}");
        assert!(md.contains("## Added with no caller (2)"), "got {md}");
        assert!(md.contains("## Complexity increases (1)"), "got {md}");
    }

    #[test]
    fn a_function_named_elsewhere_is_not_uncalled() {
        let dir = fixture();
        // A path reference the graph does not model as a call.
        write_file(
            dir.path(),
            "src/far.rs",
            "pub fn far_away(n: i32) -> i32 {\n    let _f = super::unused_helper;\n    n - 8\n}\n",
        );
        let report = json(&FootprintAnalyzer::new(), dir.path());
        assert_eq!(names(&report, "uncalled_additions"), ["extra_thing"]);
    }

    #[test]
    fn a_trait_method_is_never_scatter() {
        let dir = fixture();
        write_file(
            dir.path(),
            "src/far.rs",
            "pub struct W;\nimpl std::fmt::Display for W {\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n        write!(f, \"w\")\n    }\n}\n",
        );
        run_git(dir.path(), &["add", "src/far.rs"]);
        run_git(dir.path(), &["commit", "-q", "-m", "trait impl"]);
        write_file(
            dir.path(),
            "src/far.rs",
            "pub struct W;\nimpl std::fmt::Display for W {\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n        write!(f, \"ww\")\n    }\n}\n",
        );
        let report = json(&FootprintAnalyzer::new(), dir.path());
        let outside = names(&report, "outside_closure");
        assert!(
            !outside.iter().any(|n| n.ends_with("fmt")),
            "got {outside:?}"
        );
        assert_eq!(
            report["summary"]["functions_modified"], 3,
            "still touched: {report}"
        );
    }

    #[test]
    fn size_figures_add_up_and_non_source_files_are_skipped() {
        let dir = fixture();
        write_file(dir.path(), "NOTES.md", "not source\n");
        let report = json(&FootprintAnalyzer::new(), dir.path());
        let s = &report["summary"];
        assert_eq!(s["skipped_file_count"], 1, "NOTES.md: {report}");
        assert_eq!(s["functions_touched"], 6);
        let files = report["files"].as_array().unwrap();
        let sum = |key: &str| files.iter().map(|f| f[key].as_u64().unwrap()).sum::<u64>();
        assert_eq!(s["lines_added"].as_u64().unwrap(), sum("lines_added"));
        assert_eq!(s["lines_deleted"].as_u64().unwrap(), sum("lines_deleted"));
        let (added, deleted) = (sum("lines_added") as f64, sum("lines_deleted") as f64);
        assert!(deleted > 0.0);
        assert_eq!(s["add_delete_ratio"].as_f64().unwrap(), added / deleted);
    }

    #[test]
    fn a_pure_addition_has_no_ratio() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        write_file(dir.path(), "a.rs", "fn a() {}\n");
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "init"]);
        write_file(dir.path(), "b.rs", "fn b() {}\n");
        let report = json(&FootprintAnalyzer::new(), dir.path());
        assert_eq!(report["summary"]["lines_added"], 1);
        assert!(
            report["summary"].get("add_delete_ratio").is_none(),
            "{report}"
        );
        let md = FootprintAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("(added/deleted n/a)"), "{md}");
    }

    /// Four files, a call chain `top -> mid -> leaf`, and two loners.
    /// Editing `top`, `leaf` and both loners: at depth 2 `leaf`'s callers
    /// reach `top`, so those two are one cluster and the loners stray; at
    /// depth 1 they do not, every cluster is a singleton, and a tie for
    /// the largest leaves nothing outside.
    #[rstest]
    #[case::depth_two(2, &["lone", "lone2"])]
    #[case::depth_one(1, &[])]
    fn callers_within_depth_join_edits_across_files(#[case] depth: usize, #[case] want: &[&str]) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init", "-q"]);
        write_file(
            root,
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write_file(
            root,
            "src/lib.rs",
            "pub mod a;\npub mod b;\npub mod c;\npub mod d;\npub mod e;\n",
        );
        let files = [
            (
                "src/a.rs",
                "pub fn top() -> i32 {\n    crate::b::mid() + 1\n}\n",
            ),
            (
                "src/b.rs",
                "pub fn mid() -> i32 {\n    crate::c::leaf() + 1\n}\n",
            ),
            ("src/c.rs", "pub fn leaf() -> i32 {\n    1\n}\n"),
            ("src/d.rs", "pub fn lone() -> i32 {\n    1\n}\n"),
            ("src/e.rs", "pub fn lone2() -> i32 {\n    1\n}\n"),
        ];
        for (path, text) in files {
            write_file(root, path, text);
        }
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "init"]);
        for (path, text) in files {
            if path != "src/b.rs" {
                write_file(root, path, &text.replace("1\n}", "2\n}"));
            }
        }
        let report = json(&FootprintAnalyzer::new().with_depth(Some(depth)), root);
        assert_eq!(report["summary"]["functions_modified"], 4, "{report}");
        assert_eq!(names(&report, "outside_closure"), want, "{report}");
    }

    #[test]
    fn only_the_analyzed_subtree_is_read() {
        let dir = fixture();
        write_file(dir.path(), "tools/x.rs", "pub fn x() -> i32 {\n    1\n}\n");
        write_file(
            dir.path(),
            "tools/gone.rs",
            "pub fn gone() -> i32 {\n    1\n}\n",
        );
        run_git(dir.path(), &["add", "tools"]);
        run_git(dir.path(), &["commit", "-q", "-m", "tools"]);
        write_file(dir.path(), "tools/x.rs", "pub fn x() -> i32 {\n    2\n}\n");
        std::fs::remove_file(dir.path().join("tools/gone.rs")).unwrap();

        let whole = json(&FootprintAnalyzer::new(), dir.path());
        assert_eq!(whole["summary"]["file_count"], 5, "{whole}");
        let src = json(&FootprintAnalyzer::new(), &dir.path().join("src"));
        assert_eq!(src["summary"]["file_count"], 3, "{src}");
        assert_eq!(
            src["summary"]["skipped_file_count"], 0,
            "tools/ is not diffed: {src}"
        );
        assert_eq!(src["summary"]["functions_deleted"], 0, "{src}");
    }

    #[test]
    fn a_deleted_test_file_follows_the_test_filter() {
        let dir = fixture();
        write_file(
            dir.path(),
            "tests/it.rs",
            "#[test]\nfn t() {\n    assert_eq!(1, 1);\n}\n",
        );
        run_git(dir.path(), &["add", "tests"]);
        run_git(dir.path(), &["commit", "-q", "-m", "tests"]);
        std::fs::remove_file(dir.path().join("tests/it.rs")).unwrap();
        let with = json(&FootprintAnalyzer::new(), dir.path());
        assert_eq!(with["summary"]["functions_deleted"], 1, "{with}");
        let without = json(
            &FootprintAnalyzer::new().with_exclude_tests(true),
            dir.path(),
        );
        assert_eq!(without["summary"]["functions_deleted"], 0, "{without}");
    }

    #[test]
    fn uncalled_additions_skip_tests_self_calls_and_named_functions() {
        let dir = fixture();
        let extra = "
fn recurse(n: i32) -> i32 {
    if n > 0 { recurse(n - 1) } else { 0 }
}

fn test_only_helper() -> i32 {
    3
}

// named_in_a_comment stays for the next change.
fn named_in_a_comment() -> i32 {
    4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brand_new_test() {
        let got = test_only_helper();
        assert_eq!(got, 3);
    }
}
";
        write_file(dir.path(), "src/lib.rs", &format!("{LIB_AFTER}{extra}"));
        let report = json(&FootprintAnalyzer::new(), dir.path());
        assert_eq!(
            names(&report, "uncalled_additions"),
            [
                "extra_thing",
                "recurse",
                "test_only_helper",
                "unused_helper"
            ],
            "a self-call is not a caller; a test is not listed; a name elsewhere is a reference",
        );
        let only_tested = report["uncalled_additions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["name"] == "test_only_helper")
            .unwrap();
        assert_eq!(only_tested["test_callers"], 1, "{report}");
        let md = FootprintAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("`test_only_helper` (called only by 1 test(s))"),
            "{md}"
        );
        assert!(md.contains("`recurse`\n"), "{md}");
    }

    #[test]
    fn markdown_shows_the_cluster_line_only_with_clusters() {
        let dir = fixture();
        let md = FootprintAnalyzer::new()
            .with_top(Some(2))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("3 cluster(s) of overlapping blast radius at depth 2; 2 function(s) outside the main one"),
            "{md}"
        );
        assert!(
            !md.contains("more"),
            "exactly `--top` rows hide nothing: {md}"
        );

        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-q", "-m", "edit"]);
        let clean = FootprintAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(!clean.contains("cluster(s)"), "{clean}");
    }

    /// The graph half on its own, before the name check: a production
    /// caller rules a function out, a test caller does not. Through the
    /// whole report the name check would hide the difference, since a
    /// production call names its callee too.
    #[test]
    fn uncalled_reads_production_callers_from_the_graph() {
        let dir = fixture();
        let roots = AnalyzeRoots::from(dir.path());
        let analyzer = FootprintAnalyzer::new();
        let report = analyzer.measure(&roots).unwrap();
        let graph = analyzer.builder.build(&roots).unwrap();
        let touched = touched_nodes(&graph, &report.files);
        let rows = uncalled(&graph, &report.files, &touched);
        let mut names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["extra_thing", "unused_helper"],
            "`helper` is called by `other`"
        );
    }

    #[test]
    fn outside_a_git_tree_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "a.rs", "fn a() {}\n");
        let err = FootprintAnalyzer::new()
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(
            matches!(err, FootprintError::NotInGitRepo { .. }),
            "got {err}"
        );
    }

    #[rstest]
    #[case::disabled_reads_the_working_tree(DiffScope::Disabled, DiffScope::WorkingTree)]
    #[case::working_tree(DiffScope::WorkingTree, DiffScope::WorkingTree)]
    #[case::range(
        DiffScope::Range("a..b".to_owned()),
        DiffScope::Range("a..b".to_owned())
    )]
    fn scope_defaults_to_the_working_tree(#[case] given: DiffScope, #[case] want: DiffScope) {
        assert_eq!(FootprintAnalyzer::new().with_diff_scope(given).diff, want);
    }

    #[test]
    fn a_tie_for_the_largest_cluster_puts_nothing_outside() {
        let clusters = Clusters {
            ids: BTreeMap::from([((0, 0), 1), ((1, 0), 2)]),
            sizes: BTreeMap::from([(1, 1), (2, 1)]),
            count: 2,
        };
        let row = |file: &str| FileFootprint {
            file: file.to_owned(),
            status: "modified",
            lines_added: 1,
            lines_deleted: 1,
            functions: vec![FunctionRow {
                file: file.to_owned(),
                name: "f".to_owned(),
                change: ChangeKind::Modified,
                start_line: 1,
                end_line: 1,
                cognitive_before: Some(0),
                cognitive_after: Some(0),
                cognitive_delta: 0,
                cluster: None,
            }],
            new_wrappers: Vec::new(),
        };
        let mut files = vec![row("a.rs"), row("b.rs")];
        assert!(clusters.apply(&mut files).is_empty());
        assert_eq!(files[1].functions[0].cluster, Some(2));
    }
}
