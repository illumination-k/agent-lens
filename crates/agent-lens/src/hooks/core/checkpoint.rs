//! Session quality checkpoint: a snapshot at `SessionStart`, and at
//! every `Stop` / `SubagentStop` only what got worse since.
//!
//! The per-edit hooks see one file at a time, right after it changed.
//! What they cannot see is the session: the duplicate that only exists
//! once the second copy lands three files away, the complexity that
//! crept up over six small edits each under any threshold, the caller
//! whose removal left a function unreachable, the load-bearing function
//! edited in passing. A long session drifts — away from the codebase's
//! conventions, and away from the goal — and the place to notice is the
//! point where the agent is about to hand back control.
//!
//! So the checkpoint takes a snapshot when the session starts and, at
//! each stop, recomputes only what the session could have changed:
//!
//! * per-file content hashes, so an unchanged file costs one read;
//! * per-function facts (cognitive complexity, body hash) and the
//!   forwarding-only wrapper set, recomputed for changed files only;
//! * near-duplicate pairs, scored at a stop only for the functions the
//!   session added or modified, against the whole tree;
//! * confirmed/likely unreachable functions and the call-graph hubs,
//!   from whole-tree runs, since a deletion anywhere can strand a
//!   function elsewhere.
//!
//! The report lists regressions only — new duplicate pairs, functions
//! at or above cognitive [`ADDED_COGNITIVE_FLOOR`] that got more complex,
//! new wrappers, newly unreachable functions, and edited hubs — and
//! nothing when there are none. Each finding carries a key; keys already
//! reported at an earlier stop are remembered in the snapshot, so a
//! caller can tell "new since the last checkpoint" from "still there".
//!
//! The snapshot lives at `<repo root>/target/agent-lens/session-<id>.json`
//! (the working directory stands in for the repo root outside git), with
//! a `.gitignore` of `*` beside it so it never shows up as an untracked
//! file in a project that does not ignore `target/` already.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::warn;

use crate::analyze::footprint::ADDED_COGNITIVE_FLOOR;
use crate::analyze::function_delta::{
    ChangeKind, FunctionChange, FunctionFact, compare, fnv1a, function_facts, wrapper_findings,
};
use crate::analyze::{
    AnalysisIndexScope, AnalyzePathFilter, AnalyzeRoots, ChangedLines, DiffScope,
    FunctionSelection, HubsAnalyzer, LineRange, OutputFormat, SimilarityAnalyzer, SourceLang,
    UnreachableAnalyzer, collect_source_files, read_source,
};

/// Bumped whenever a field changes meaning. A snapshot from another
/// schema is ignored rather than misread.
const SCHEMA_VERSION: u32 = 1;

/// Fan-in at or above which an edited function counts as a hub edit.
const HUB_MIN_FAN_IN: u64 = 5;

/// Unreachable tiers a new row has to be in to count as a regression.
/// `unknown` rows are leads, not verdicts, and a session that adds a
/// trait impl would otherwise report every method on it.
const UNREACHABLE_TIERS: &[&str] = &["confirmed", "likely"];

/// Errors the checkpoint cannot recover from. Soft failures — an
/// analyzer that does not support the tree, a file that no longer
/// parses — are logged and leave that section out instead.
#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("failed to read {path:?}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write {path:?}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("snapshot {path:?} is not valid JSON: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error(transparent)]
    Analyzer(#[from] crate::analyze::AnalyzerError),
}

/// What a session looked like when it started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Snapshot {
    schema_version: u32,
    tool_version: String,
    session_id: String,
    /// Analysis root: the session's working directory.
    root: PathBuf,
    /// Keyed by path relative to `root`.
    files: BTreeMap<String, FileState>,
    /// Near-duplicate pairs as sorted `file::name` keys; `None` when the
    /// run failed (the tree was too broad, say), in which case every
    /// pair a stop finds counts as new.
    similar_pairs: Option<BTreeSet<(String, String)>>,
    /// `file::qualified_name` of every confirmed/likely unreachable row.
    unreachable: Option<BTreeSet<String>>,
    hubs: Option<Vec<Hub>>,
    /// Finding keys an earlier stop already reported.
    #[serde(default)]
    reported: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct FileState {
    hash: u64,
    functions: Vec<FunctionFact>,
    wrappers: BTreeSet<String>,
    /// The file did not parse when the snapshot was taken, so it has no
    /// baseline to compare against: every function in it would
    /// otherwise read as added.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    unparsed: bool,
}

impl FileState {
    fn unparsed(hash: u64) -> Self {
        Self {
            hash,
            functions: Vec::new(),
            wrappers: BTreeSet::new(),
            unparsed: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Hub {
    file: String,
    qualified_name: String,
    start_line: u64,
    fan_in: u64,
    kind: String,
}

/// Where the snapshot for `session_id` lives when the session runs in
/// `cwd`. The id is reduced to `[A-Za-z0-9_-]` so it cannot name a path
/// outside the directory.
pub fn snapshot_path(cwd: &Path, session_id: &str) -> PathBuf {
    let root = crate::paths::git_repo_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let id: String = session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    root.join("target")
        .join("agent-lens")
        .join(format!("session-{id}.json"))
}

/// Record the session-start snapshot for `session_id`, unless one
/// already exists — a resumed or compacted session keeps the baseline it
/// started with. Returns the path written, or `None` when nothing was
/// written (already present, or no supported source file under `cwd`).
pub fn take_snapshot(cwd: &Path, session_id: &str) -> Result<Option<PathBuf>, CheckpointError> {
    let path = snapshot_path(cwd, session_id);
    if path.exists() {
        return Ok(None);
    }
    let _index = AnalysisIndexScope::activate();
    let current = scan(cwd)?;
    if current.is_empty() {
        return Ok(None);
    }
    let files = current
        .into_iter()
        .map(|(rel, file)| {
            let state = file
                .state()
                .unwrap_or_else(|| FileState::unparsed(file.hash));
            (rel, state)
        })
        .collect();
    let snapshot = Snapshot {
        schema_version: SCHEMA_VERSION,
        tool_version: env!("CARGO_PKG_VERSION").to_owned(),
        session_id: session_id.to_owned(),
        root: cwd.to_path_buf(),
        files,
        similar_pairs: similar_pairs(cwd, None),
        unreachable: unreachable_keys(cwd),
        hubs: hubs(cwd),
        reported: BTreeSet::new(),
    };
    write_snapshot(&path, &snapshot)?;
    Ok(Some(path))
}

/// What a stop found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delta {
    /// Rendered report, regressions only.
    pub report: String,
    /// Findings no earlier stop reported.
    pub new_findings: usize,
}

/// Compare the tree against the session's snapshot and return the
/// regressions, or `None` when there are none — or no snapshot to
/// compare against. Remembers what it reported in the snapshot.
pub fn session_delta(cwd: &Path, session_id: &str) -> Result<Option<Delta>, CheckpointError> {
    let path = snapshot_path(cwd, session_id);
    let Some(mut snapshot) = read_snapshot(&path)? else {
        return Ok(None);
    };
    let _index = AnalysisIndexScope::activate();
    let current = scan(cwd)?;
    let changed: Vec<(&String, &ScannedFile)> = current
        .iter()
        .filter(|(rel, file)| snapshot.files.get(*rel).is_none_or(|s| s.hash != file.hash))
        .collect();
    let deleted: Vec<&String> = snapshot
        .files
        .keys()
        .filter(|rel| !current.contains_key(*rel))
        .collect();
    if changed.is_empty() && deleted.is_empty() {
        return Ok(None);
    }

    let empty = FileState {
        unparsed: false,
        ..FileState::unparsed(0)
    };
    let mut findings = Findings::default();
    let mut edits = Edits::default();
    for (rel, file) in &changed {
        let before = snapshot.files.get(*rel).unwrap_or(&empty);
        let Some(after) = file.state().filter(|_| !before.unparsed) else {
            continue;
        };
        let changes = compare(&before.functions, &after.functions);
        findings.complexity(rel, &changes);
        findings.wrappers(rel, &changes, before, &after);
        edits.record(rel, &file.path, &changes);
    }
    for rel in &deleted {
        edits.record_deleted(rel, &snapshot.files[*rel]);
    }

    if !edits.focus.is_empty() {
        let lines = ChangedLines::new(edits.focus);
        if let Some(pairs) = scored_pairs(cwd, Some(lines)) {
            findings.duplicates(&pairs, snapshot.similar_pairs.as_ref());
        }
    }
    if let (Some(before), Some(after)) = (&snapshot.unreachable, unreachable_rows(cwd)) {
        findings.unreachable(before, &after);
    }
    if let Some(hubs) = &snapshot.hubs {
        findings.hubs(hubs, &edits.by_file);
    }
    if findings.is_empty() {
        return Ok(None);
    }

    let keys = findings.keys();
    let new_findings = keys.difference(&snapshot.reported).count();
    let report = findings.render(changed.len() + deleted.len(), edits.touched, new_findings);
    snapshot.reported.extend(keys);
    write_snapshot(&path, &snapshot)?;
    Ok(Some(Delta {
        report,
        new_findings,
    }))
}

/// What the session did to the files it changed.
#[derive(Debug, Default)]
struct Edits {
    /// Spans of added and modified functions, for the focused
    /// similarity run.
    focus: Vec<(PathBuf, Vec<LineRange>)>,
    /// Functions added, modified, or deleted.
    touched: usize,
    /// Per file, each changed function's kind and name.
    by_file: BTreeMap<String, Vec<(ChangeKind, String)>>,
}

impl Edits {
    fn record(&mut self, rel: &str, path: &Path, changes: &[FunctionChange<'_>]) {
        self.touched += changes.len();
        let ranges: Vec<LineRange> = changes
            .iter()
            .filter_map(|c| c.after)
            .map(|f| LineRange {
                start: f.start_line,
                end: f.end_line,
            })
            .collect();
        if !ranges.is_empty() {
            self.focus.push((path.to_path_buf(), ranges));
        }
        self.by_file.insert(
            rel.to_owned(),
            changes
                .iter()
                .map(|c| (c.kind, c.name().to_owned()))
                .collect(),
        );
    }

    /// A file that is gone: every function it had is deleted.
    fn record_deleted(&mut self, rel: &str, state: &FileState) {
        self.touched += state.functions.len();
        self.by_file.insert(
            rel.to_owned(),
            state
                .functions
                .iter()
                .map(|f| (ChangeKind::Deleted, f.name.clone()))
                .collect(),
        );
    }
}

/// One walked source file: where it is and what it hashed to. Function
/// facts are read on demand, since at a stop only changed files need
/// them.
#[derive(Debug)]
struct ScannedFile {
    path: PathBuf,
    lang: SourceLang,
    source: String,
    hash: u64,
}

impl ScannedFile {
    /// Facts and wrappers, or `None` (with a warning) when the file does
    /// not parse — mid-edit syntax errors are ordinary at a stop.
    fn state(&self) -> Option<FileState> {
        let parsed = function_facts(self.lang, &self.source).and_then(|functions| {
            let wrappers = wrapper_findings(self.lang, &self.source)?
                .into_iter()
                .map(|w| w.name)
                .collect();
            Ok((functions, wrappers))
        });
        match parsed {
            Ok((functions, wrappers)) => Some(FileState {
                hash: self.hash,
                functions,
                wrappers,
                unparsed: false,
            }),
            Err(e) => {
                warn!(path = %self.path.display(), error = %e, "checkpoint: skipping unparsable file");
                None
            }
        }
    }
}

/// Every production source file under `cwd`, keyed by relative path.
/// Tests are left out on both sides: a session's test code is expected
/// to repeat itself and to be reached only by the harness.
fn scan(cwd: &Path) -> Result<BTreeMap<String, ScannedFile>, CheckpointError> {
    let roots = AnalyzeRoots::from(cwd);
    let filter = AnalyzePathFilter::new()
        .with_exclude_tests(true)
        .compile(cwd)
        .map_err(crate::analyze::AnalyzerError::from)?;
    let mut out = BTreeMap::new();
    for file in collect_source_files(&roots, &filter)? {
        let (lang, source) = match read_source(&file.path) {
            Ok(read) => read,
            Err(e) => {
                warn!(path = %file.path.display(), error = %e, "checkpoint: skipping unreadable file");
                continue;
            }
        };
        let hash = fnv1a(source.bytes());
        out.insert(
            file.display_path,
            ScannedFile {
                path: file.path,
                lang,
                source,
                hash,
            },
        );
    }
    Ok(out)
}

fn read_snapshot(path: &Path) -> Result<Option<Snapshot>, CheckpointError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CheckpointError::Read {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let snapshot: Snapshot =
        serde_json::from_str(&text).map_err(|source| CheckpointError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    if snapshot.schema_version != SCHEMA_VERSION {
        warn!(path = %path.display(), "checkpoint: ignoring a snapshot from another schema");
        return Ok(None);
    }
    Ok(Some(snapshot))
}

fn write_snapshot(path: &Path, snapshot: &Snapshot) -> Result<(), CheckpointError> {
    let write_err = |source| CheckpointError::Write {
        path: path.to_path_buf(),
        source,
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(write_err)?;
        let ignore = dir.join(".gitignore");
        if !ignore.exists() {
            std::fs::write(&ignore, "*\n").map_err(write_err)?;
        }
    }
    let text = serde_json::to_string(snapshot).map_err(|source| CheckpointError::Json {
        path: path.to_path_buf(),
        source,
    })?;
    std::fs::write(path, text).map_err(write_err)
}

/// Run a whole-tree analyzer and parse its JSON report, or `None` with a
/// warning when it fails — an unsupported root, a tree too broad for
/// similarity. The section it feeds is then left out.
fn json_report(
    what: &str,
    run: impl FnOnce() -> Result<String, crate::analyze::AnalyzerError>,
) -> Option<Value> {
    let text = run()
        .map_err(|e| warn!(analyzer = what, error = %e, "checkpoint: section skipped"))
        .ok()?;
    serde_json::from_str(&text)
        .map_err(|e| warn!(analyzer = what, error = %e, "checkpoint: report is not JSON"))
        .ok()
}

/// Similar production-function pairs under `cwd`, keyed by sorted
/// `file::name` pairs with their score. With `focus`, only pairs with at
/// least one side in the focused lines are scored.
fn similar_pairs(cwd: &Path, focus: Option<ChangedLines>) -> Option<BTreeSet<(String, String)>> {
    scored_pairs(cwd, focus).map(|pairs| pairs.into_keys().collect())
}

fn scored_pairs(
    cwd: &Path,
    focus: Option<ChangedLines>,
) -> Option<BTreeMap<(String, String), PairRow>> {
    let mut analyzer = SimilarityAnalyzer::new()
        .with_exclude_tests(true)
        .with_function_selection(FunctionSelection::ExcludeTests);
    if let Some(lines) = focus {
        analyzer = analyzer.with_diff_scope(DiffScope::Lines(lines));
    }
    let report = json_report("similarity", || analyzer.analyze(cwd, OutputFormat::Json))?;
    let mut out = BTreeMap::new();
    for pair in report["clusters"]
        .as_array()?
        .iter()
        .flat_map(|c| c["pairs"].as_array().map(Vec::as_slice).unwrap_or_default())
    {
        let (Some(a), Some(b)) = (unit_ref(&pair["a"]), unit_ref(&pair["b"])) else {
            continue;
        };
        let similarity = pair["similarity"].as_f64().unwrap_or(0.0);
        let (a, b) = if a.key <= b.key { (a, b) } else { (b, a) };
        out.insert((a.key.clone(), b.key.clone()), PairRow { a, b, similarity });
    }
    Some(out)
}

#[derive(Debug, Clone)]
struct UnitRef {
    key: String,
    file: String,
    name: String,
    line: u64,
}

fn unit_ref(v: &Value) -> Option<UnitRef> {
    let file = v["file"].as_str()?.to_owned();
    let name = v["name"].as_str()?.to_owned();
    Some(UnitRef {
        key: format!("{file}::{name}"),
        line: v["start_line"].as_u64().unwrap_or(0),
        file,
        name,
    })
}

#[derive(Debug, Clone)]
struct PairRow {
    a: UnitRef,
    b: UnitRef,
    similarity: f64,
}

#[derive(Debug, Clone)]
struct UnreachableRow {
    file: String,
    line: u64,
    name: String,
    tier: String,
}

/// Confirmed and likely unreachable rows, keyed `file::qualified_name`.
fn unreachable_rows(cwd: &Path) -> Option<BTreeMap<String, UnreachableRow>> {
    let report = json_report("unreachable", || {
        UnreachableAnalyzer::new().analyze(cwd, OutputFormat::Json)
    })?;
    let mut out = BTreeMap::new();
    for module in report["modules"].as_array()? {
        for finding in module["findings"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let tier = finding["tier"].as_str().unwrap_or_default();
            if !UNREACHABLE_TIERS.contains(&tier) {
                continue;
            }
            let (Some(file), Some(name)) =
                (finding["file"].as_str(), finding["qualified_name"].as_str())
            else {
                continue;
            };
            out.insert(
                format!("{file}::{name}"),
                UnreachableRow {
                    file: file.to_owned(),
                    line: finding["start_line"].as_u64().unwrap_or(0),
                    name: name.to_owned(),
                    tier: tier.to_owned(),
                },
            );
        }
    }
    Some(out)
}

fn unreachable_keys(cwd: &Path) -> Option<BTreeSet<String>> {
    unreachable_rows(cwd).map(|rows| rows.into_keys().collect())
}

/// Functions other code leans on: fan-in at or above
/// [`HUB_MIN_FAN_IN`], plus the analyzer's bottlenecks (high
/// betweenness whatever their fan-in). God functions are left out — they
/// are hubs by fan-out, and editing the function that does too much is
/// not what puts its callers at risk.
///
/// Read from the full `functions` table rather than the `load_bearing`
/// section alone, whose cutoff is relative to the tree and can exclude
/// every function in a small one.
fn hubs(cwd: &Path) -> Option<Vec<Hub>> {
    let report = json_report("hubs", || {
        HubsAnalyzer::new().analyze(cwd, OutputFormat::Json)
    })?;
    let ids_in = |section: &str| -> BTreeSet<String> {
        report[section]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|row| row["id"].as_str().map(str::to_owned))
            .collect()
    };
    let (load_bearing, bottlenecks) = (ids_in("load_bearing"), ids_in("bottlenecks"));
    let mut out = Vec::new();
    for row in report["functions"].as_array()? {
        let id = row["id"].as_str().unwrap_or_default();
        let fan_in = row["fan_in"].as_u64().unwrap_or(0);
        let kind = if bottlenecks.contains(id) {
            "bottleneck"
        } else if load_bearing.contains(id) {
            "load-bearing"
        } else {
            "fan-in"
        };
        if fan_in < HUB_MIN_FAN_IN && kind != "bottleneck" {
            continue;
        }
        let (Some(file), Some(name)) = (row["file"].as_str(), row["qualified_name"].as_str())
        else {
            continue;
        };
        out.push(Hub {
            file: file.to_owned(),
            qualified_name: name.to_owned(),
            start_line: row["start_line"].as_u64().unwrap_or(0),
            fan_in,
            kind: kind.to_owned(),
        });
    }
    Some(out)
}

/// The regressions one stop found, by kind. Each list holds
/// `(key, rendered row)`; the key is what "already reported" compares.
#[derive(Debug, Default)]
struct Findings {
    duplicates: Vec<(String, String)>,
    complexity: Vec<(String, String)>,
    wrappers: Vec<(String, String)>,
    unreachable: Vec<(String, String)>,
    hubs: Vec<(String, String)>,
}

impl Findings {
    /// A function at or above the floor that got more complex. The key
    /// carries the new score, so a function that keeps climbing is
    /// reported again at each step.
    fn complexity(&mut self, rel: &str, changes: &[FunctionChange<'_>]) {
        for change in changes {
            let Some(after) = change.after else {
                continue;
            };
            let delta = change.cognitive_delta();
            if delta <= 0 || after.cognitive < ADDED_COGNITIVE_FLOOR {
                continue;
            }
            let before = change.before.map_or(0, |f| f.cognitive);
            self.complexity.push((
                format!("complexity:{rel}::{}:{}", after.name, after.cognitive),
                format!(
                    "{rel}:{} `{}` cog {before}→{} (+{delta}, {})",
                    after.start_line,
                    after.name,
                    after.cognitive,
                    change.kind.as_str(),
                ),
            ));
        }
    }

    fn wrappers(
        &mut self,
        rel: &str,
        changes: &[FunctionChange<'_>],
        before: &FileState,
        after: &FileState,
    ) {
        for change in changes {
            let Some(fact) = change.after else {
                continue;
            };
            if !after.wrappers.contains(&fact.name) || before.wrappers.contains(&fact.name) {
                continue;
            }
            self.wrappers.push((
                format!("wrapper:{rel}::{}", fact.name),
                format!(
                    "{rel}:{} `{}` ({})",
                    fact.start_line,
                    fact.name,
                    change.kind.as_str()
                ),
            ));
        }
    }

    /// Pairs the focused similarity run found that the snapshot did
    /// not have. Without a snapshot set, every pair counts as new.
    fn duplicates(
        &mut self,
        found: &BTreeMap<(String, String), PairRow>,
        before: Option<&BTreeSet<(String, String)>>,
    ) {
        for (key, row) in found {
            if before.is_some_and(|known| known.contains(key)) {
                continue;
            }
            let (a, b) = (&row.a, &row.b);
            self.duplicates.push((
                format!("duplicate:{}|{}", key.0, key.1),
                format!(
                    "{}:{} `{}` ≈ {}:{} `{}` ({:.2})",
                    a.file, a.line, a.name, b.file, b.line, b.name, row.similarity,
                ),
            ));
        }
    }

    fn unreachable(&mut self, before: &BTreeSet<String>, after: &BTreeMap<String, UnreachableRow>) {
        for (key, row) in after {
            if before.contains(key) {
                continue;
            }
            self.unreachable.push((
                format!("unreachable:{key}"),
                format!("{}:{} `{}` ({})", row.file, row.line, row.name, row.tier),
            ));
        }
    }

    /// Hubs whose function this session modified or deleted.
    fn hubs(&mut self, hubs: &[Hub], edited: &BTreeMap<String, Vec<(ChangeKind, String)>>) {
        for hub in hubs {
            let Some(changes) = edited.get(&hub.file) else {
                continue;
            };
            let hit = changes.iter().find(|(kind, name)| {
                *kind != ChangeKind::Added && qualified_matches(&hub.qualified_name, name)
            });
            let Some((kind, _)) = hit else {
                continue;
            };
            let key = format!("hub:{}::{}", hub.file, hub.qualified_name);
            if self.hubs.iter().any(|(k, _)| *k == key) {
                continue;
            }
            self.hubs.push((
                key,
                format!(
                    "{}:{} `{}` ({}, fan_in {}) {}",
                    hub.file,
                    hub.start_line,
                    hub.qualified_name,
                    hub.kind,
                    hub.fan_in,
                    kind.as_str(),
                ),
            ));
        }
    }

    fn is_empty(&self) -> bool {
        self.sections().iter().all(|(_, rows)| rows.is_empty())
    }

    fn keys(&self) -> BTreeSet<String> {
        self.sections()
            .iter()
            .flat_map(|(_, rows)| rows.iter().map(|(key, _)| key.clone()))
            .collect()
    }

    fn sections(&self) -> [(&'static str, &[(String, String)]); 5] {
        [
            ("New near-duplicates", &self.duplicates),
            ("Complexity increases", &self.complexity),
            ("New wrappers", &self.wrappers),
            ("Newly unreachable", &self.unreachable),
            ("Hubs edited", &self.hubs),
        ]
    }

    fn render(&self, file_count: usize, function_count: usize, new_findings: usize) -> String {
        let mut out = String::from("# agent-lens session checkpoint\n");
        let _ = writeln!(
            out,
            "Since session start: {file_count} file(s) changed, {function_count} function(s) touched. Regressions only; {new_findings} not reported at an earlier stop.",
        );
        for (title, rows) in self.sections() {
            if rows.is_empty() {
                continue;
            }
            let _ = writeln!(out, "\n## {title} ({})", rows.len());
            for (_, row) in rows {
                let _ = writeln!(out, "- {row}");
            }
        }
        out
    }
}

/// Whether a hub's qualified name (`crate::module::Owner::method`) names
/// the complexity unit `name` (`Owner::method`).
fn qualified_matches(qualified: &str, name: &str) -> bool {
    qualified == name
        || qualified
            .strip_suffix(name)
            .is_some_and(|prefix| prefix.ends_with("::") || prefix.ends_with('.'))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::test_support::{run_git, write_file};

    const BEFORE: &str = "\
pub fn entry(xs: &[i32]) -> i32 {
    total(xs) + helper(3) + dropped()
}

fn total(xs: &[i32]) -> i32 {
    let mut acc = 0;
    for x in xs {
        if *x > 0 {
            acc += x;
        }
    }
    acc
}

fn helper(n: i32) -> i32 {
    n * 2 + 1
}

fn dropped() -> i32 {
    7
}

pub fn shared(n: i32) -> i32 { n + 1 }
pub fn a1() -> i32 { shared(1) + 1 }
pub fn a2() -> i32 { shared(2) + 2 }
pub fn a3() -> i32 { shared(3) + 3 }
pub fn a4() -> i32 { shared(4) + 4 }
pub fn a5() -> i32 { shared(5) + 5 }
";

    const COMPLEX_BODY: &str = "\
    let mut acc = 0;
    for x in xs {
        if *x > 0 {
            if *x > 10 {
                for _ in 0..2 {
                    if acc > 3 && *x < 100 || acc < -5 {
                        acc += x;
                    }
                }
            } else {
                acc -= 1;
            }
        }
    }
    acc
";

    /// Every regression the checkpoint reports, in one edit: `total`
    /// gets complex, `sum_positive` copies it, `helper` becomes a
    /// forwarder, `dropped` loses its only caller, and the hub `shared`
    /// is edited.
    fn after() -> String {
        format!(
            "\
pub fn entry(xs: &[i32]) -> i32 {{
    total(xs) + helper(3)
}}

fn total(xs: &[i32]) -> i32 {{
{COMPLEX_BODY}}}

fn sum_positive(xs: &[i32]) -> i32 {{
{COMPLEX_BODY}}}

fn helper(n: i32) -> i32 {{
    other(n)
}}

fn other(n: i32) -> i32 {{ n * 3 + sum_positive(&[n]) }}

fn dropped() -> i32 {{
    7
}}

pub fn shared(n: i32) -> i32 {{ n + 2 }}
pub fn a1() -> i32 {{ shared(1) + 1 }}
pub fn a2() -> i32 {{ shared(2) + 2 }}
pub fn a3() -> i32 {{ shared(3) + 3 }}
pub fn a4() -> i32 {{ shared(4) + 4 }}
pub fn a5() -> i32 {{ shared(5) + 5 }}
"
        )
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q"]);
        write_file(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        write_file(dir.path(), "src/lib.rs", BEFORE);
        dir
    }

    #[test]
    fn snapshot_path_is_under_the_repo_root_and_sanitized() {
        let dir = repo();
        let nested = dir.path().join("src");
        let path = snapshot_path(&nested, "../../etc/pass wd");
        assert_eq!(
            path,
            dir.path()
                .join("target/agent-lens/session-______etc_pass_wd.json")
        );
    }

    #[test]
    fn no_snapshot_means_no_delta() {
        let dir = repo();
        assert_eq!(session_delta(dir.path(), "s").unwrap(), None);
    }

    #[test]
    fn a_tree_without_sources_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "README.md", "hi\n");
        assert_eq!(take_snapshot(dir.path(), "s").unwrap(), None);
        assert!(!dir.path().join("target").exists());
    }

    #[test]
    fn a_resumed_session_keeps_its_baseline() {
        let dir = repo();
        let path = take_snapshot(dir.path(), "s").unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(path.parent().unwrap().join(".gitignore")).unwrap(),
            "*\n"
        );
        write_file(dir.path(), "src/lib.rs", &after());
        assert_eq!(
            take_snapshot(dir.path(), "s").unwrap(),
            None,
            "kept, not retaken"
        );
        assert!(session_delta(dir.path(), "s").unwrap().is_some());
    }

    #[test]
    fn an_unchanged_tree_has_no_delta() {
        let dir = repo();
        take_snapshot(dir.path(), "s").unwrap();
        assert_eq!(session_delta(dir.path(), "s").unwrap(), None);
    }

    #[test]
    fn a_session_that_made_things_worse_reports_each_regression_once() {
        let dir = repo();
        take_snapshot(dir.path(), "s").unwrap();
        write_file(dir.path(), "src/lib.rs", &after());

        let delta = session_delta(dir.path(), "s").unwrap().unwrap();
        let report = &delta.report;
        assert!(
            report.starts_with("# agent-lens session checkpoint\n"),
            "{report}"
        );
        assert!(report.contains("## New near-duplicates (1)"), "{report}");
        assert!(
            report.contains("`sum_positive` ≈ src/lib.rs:5 `total`"),
            "{report}"
        );
        assert!(
            report.contains("`total` cog 3→18 (+15, modified)"),
            "{report}"
        );
        assert!(
            report.contains("`sum_positive` cog 0→18 (+18, added)"),
            "{report}"
        );
        assert!(
            report.contains("## New wrappers (1)\n- src/lib.rs:"),
            "{report}"
        );
        assert!(report.contains("`helper` (modified)"), "{report}");
        assert!(report.contains("## Newly unreachable (1)"), "{report}");
        assert!(report.contains("`demo::dropped` (confirmed)"), "{report}");
        assert!(report.contains("## Hubs edited (1)"), "{report}");
        // Whether the hubs analyzer also calls it load-bearing depends
        // on the tree's own fan-in distribution; the fan-in floor alone
        // is what puts it here.
        assert!(report.contains("`demo::shared` ("), "{report}");
        assert!(report.contains("fan_in 5) modified"), "{report}");
        assert_eq!(delta.new_findings, 6, "{report}");

        let again = session_delta(dir.path(), "s").unwrap().unwrap();
        assert_eq!(again.new_findings, 0, "already reported");
        assert!(again.report.contains("0 not reported at an earlier stop"));

        // Undoing the edit clears every finding.
        write_file(dir.path(), "src/lib.rs", BEFORE);
        assert_eq!(session_delta(dir.path(), "s").unwrap(), None);
    }

    #[test]
    fn a_file_that_did_not_parse_at_start_has_no_baseline() {
        let dir = repo();
        write_file(dir.path(), "src/broken.rs", "fn broken( {\n");
        take_snapshot(dir.path(), "s").unwrap();
        write_file(
            dir.path(),
            "src/broken.rs",
            &format!("fn fixed(xs: &[i32]) -> i32 {{\n{COMPLEX_BODY}}}\n"),
        );
        let delta = session_delta(dir.path(), "s").unwrap();
        assert!(
            delta.as_ref().is_none_or(|d| !d.report.contains("`fixed`")),
            "no baseline, no comparison: {delta:?}",
        );
    }

    #[test]
    fn a_snapshot_from_another_schema_is_ignored() {
        let dir = repo();
        let path = take_snapshot(dir.path(), "s").unwrap().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(
            &path,
            text.replace("\"schema_version\":1", "\"schema_version\":99"),
        )
        .unwrap();
        write_file(dir.path(), "src/lib.rs", &after());
        assert_eq!(session_delta(dir.path(), "s").unwrap(), None);
    }

    #[test]
    fn qualified_names_match_on_a_segment_boundary() {
        assert!(qualified_matches("demo::Owner::run", "Owner::run"));
        assert!(qualified_matches("run", "run"));
        assert!(qualified_matches("pkg.mod.run", "run"));
        assert!(!qualified_matches("demo::rerun", "run"));
    }
}
