//! Entity-joined digest of a profile run — the transposed report.
//!
//! A profile's markdown report is tool-major: thirteen independent
//! sections, each repeating the same files from its own angle, each
//! inlining the evidence a reader might want. The reader of a `run`
//! report is a tool-using agent, and for that reader the transposition
//! is the finding: a file that is simultaneously a hotspot, complex,
//! low-cohesion, and load-bearing is exactly the dangerous corner the
//! tool exists to surface, and the agent can fetch any detail on demand
//! if the digest names the command that produces it.
//!
//! So a digest keeps three things and nothing else:
//!
//! * **One row per file**, aggregating every analyzer's headline about
//!   it, ranked by cross-tool weight — the sum over analyzers of how
//!   high the file ranks within each one. Ranks are per-run positions,
//!   which sidesteps score normalization across metrics that share no
//!   unit (a rank product is how `risk` already combines churn with
//!   centrality).
//! * **One line per corpus-shaped result** — module cycles, modularity
//!   gaps, co-change pairs — that no single file owns.
//! * **A drill-down command per row and per line**, in place of the
//!   evidence the full sections inline.
//!
//! Findings are extracted from each analyzer's JSON report rather than
//! from its internal types, the way [`crate::baseline`] summarizers
//! are: the JSON shape is the surface the analyzers already commit to,
//! and a shape this extractor does not recognise degrades to "nothing
//! extracted" — never to a panic and never to an invented finding.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::config::ToolName;

/// How many files one analyzer may contribute. Every extractor reads
/// rows in the analyzer's own severity order, so the cap keeps the
/// worst offenders and drops the tail that a full section would also
/// cap behind `--top`.
const PER_TOOL_FILE_CAP: usize = 15;

/// How many entity rows the digest prints. Everything past the cap is
/// counted rather than listed — the row cut is the digest's budget, and
/// the full sections remain one `--format md` run away.
const ENTITY_ROW_CAP: usize = 40;

/// How many headline fragments one entity row carries before the rest
/// collapse into a count. Three tools agreeing is the signal; the
/// fourth and fifth opinions add weight, not information.
const ROW_FRAGMENT_CAP: usize = 4;

/// Functions below this cognitive complexity are not worth a digest
/// row: the md renderer's own `min-score` default draws the same line.
const COGNITIVE_FLOOR: u64 = 10;

/// LCOM4 1 is cohesive and 0 is empty; 2+ is the "does more than one
/// thing" population, same floor `baseline` counts split units at.
const LCOM4_FLOOR: u64 = 2;

/// One analyzer's contribution to the digest.
#[derive(Debug, Default, PartialEq)]
struct Extraction {
    /// Per-file findings, worst first: one row per file, already
    /// aggregated (an extractor never emits the same file twice).
    files: Vec<FileFinding>,
    /// Corpus-shaped headline fragments no single file owns.
    corpus: Vec<String>,
}

#[derive(Debug, PartialEq)]
struct FileFinding {
    /// Absolute path — the join key. Reports disagree about what row
    /// paths are relative to (the walk base for the source analyzers,
    /// the git repository root for the history-backed ones), so the
    /// extractors resolve both spellings to one keyspace.
    path: PathBuf,
    headline: String,
}

/// Render the digest for one profile run.
///
/// `sections` are the per-tool JSON reports in the profile's tool
/// order; `targets` are the profile's resolved target paths (the base
/// row paths are displayed against); `cwd` is where the reader will run
/// the drill-down commands from, so every printed path is spelled
/// relative to it when possible.
pub fn render(
    profile: &str,
    sections: &[(ToolName, Value)],
    targets: &[PathBuf],
    cwd: &Path,
) -> String {
    let base = crate::analyze::AnalyzeRoots::new(targets.to_vec())
        .base()
        .to_path_buf();
    let target_args = targets
        .iter()
        .map(|t| display_path(t, cwd))
        .collect::<Vec<_>>()
        .join(" ");

    let folded = Folded::collect(sections, &base, &target_args);

    let mut out = format!("# Digest: {profile}\n\n");
    let _ = writeln!(
        out,
        "Ranked rollup of {} analyzer reports. Full sections: `agent-lens run {profile} --format md`",
        sections.len(),
    );
    render_entity_rows(&mut out, &folded.rows, cwd, &target_args);
    if !folded.corpus_lines.is_empty() || !folded.unsummarized.is_empty() {
        out.push_str("\n## Corpus-level findings\n\n");
        for line in &folded.corpus_lines {
            out.push_str(line);
            out.push('\n');
        }
        for tool in &folded.unsummarized {
            // A tool the digest cannot fold is pointed at, not dropped:
            // a digest that silently loses a whole report is worse than
            // one with a longer tail.
            let _ = writeln!(
                out,
                "- {tool}: not folded into the digest — see `agent-lens run {profile} --format md`",
            );
        }
    }
    if !folded.quiet.is_empty() {
        // "Ran and found nothing" and "was not run" must stay
        // distinguishable once the sections are gone.
        let _ = write!(
            out,
            "\nNothing to report from: {}.\n",
            folded.quiet.join(", "),
        );
    }
    out
}

/// Every tool's extraction, folded across tools: entity rows keyed and
/// weighted, corpus lines rendered, the tools with nothing to say and
/// the tools the digest has no fold for kept apart.
struct Folded {
    rows: Vec<EntityRow>,
    corpus_lines: Vec<String>,
    unsummarized: Vec<&'static str>,
    quiet: Vec<&'static str>,
}

impl Folded {
    fn collect(sections: &[(ToolName, Value)], base: &Path, target_args: &str) -> Self {
        let mut entities: HashMap<PathBuf, EntityRow> = HashMap::new();
        let mut folded = Self {
            rows: Vec::new(),
            corpus_lines: Vec::new(),
            unsummarized: Vec::new(),
            quiet: Vec::new(),
        };
        for (tool, report) in sections {
            let Some(extraction) = extract(*tool, report, base) else {
                folded.unsummarized.push(tool.as_str());
                continue;
            };
            if extraction.files.is_empty() && extraction.corpus.is_empty() {
                folded.quiet.push(tool.as_str());
                continue;
            }
            // Rank weight: the top file of an analyzer scores 1, the
            // last of n scores 1/n. Summed across analyzers this is the
            // cross-tool weight — a file three analyzers rank high
            // beats a file one analyzer ranks first.
            let count = extraction.files.len().min(PER_TOOL_FILE_CAP);
            for (index, finding) in extraction.files.into_iter().take(count).enumerate() {
                let weight = (count - index) as f64 / count as f64;
                let row = entities.entry(finding.path.clone()).or_insert(EntityRow {
                    path: finding.path,
                    weight: 0.0,
                    fragments: Vec::new(),
                });
                row.weight += weight;
                row.fragments.push((weight, *tool, finding.headline));
            }
            if !extraction.corpus.is_empty() {
                folded.corpus_lines.push(format!(
                    "- {}: {} — {}",
                    tool.as_str(),
                    extraction.corpus.join("; "),
                    drill_down(*tool, target_args),
                ));
            }
        }
        folded.rows = entities.into_values().collect();
        // Weight descending; the path tiebreak keeps two runs over the
        // same tree byte-identical.
        folded.rows.sort_by(|a, b| {
            b.weight
                .total_cmp(&a.weight)
                .then_with(|| a.path.cmp(&b.path))
        });
        folded
    }
}

fn render_entity_rows(out: &mut String, rows: &[EntityRow], cwd: &Path, target_args: &str) {
    let _ = write!(
        out,
        "\n## Findings by entity ({}, ranked by cross-tool weight)\n\n",
        counted(rows.len() as u64, "file", "files"),
    );
    if rows.is_empty() {
        out.push_str("No file-level findings.\n");
    }
    let listed = rows.len().min(ENTITY_ROW_CAP);
    for row in &rows[..listed] {
        let mut fragments = row.fragments.clone();
        // Strongest claim leads the row and picks its drill-down; the
        // tool-name tiebreak keeps equal weights deterministic.
        fragments.sort_by(|a, b| {
            b.0.total_cmp(&a.0)
                .then_with(|| a.1.as_str().cmp(b.1.as_str()))
        });
        let shown = fragments.len().min(ROW_FRAGMENT_CAP);
        let mut line = fragments[..shown]
            .iter()
            .map(|(_, _, headline)| headline.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if fragments.len() > shown {
            let _ = write!(line, ", +{} more", fragments.len() - shown);
        }
        let entity = display_path(&row.path, cwd);
        let lead_tool = fragments[0].1;
        let detail_arg = if file_scoped(lead_tool) {
            &entity
        } else {
            target_args
        };
        let _ = writeln!(out, "- {entity} — {line}");
        let _ = writeln!(out, "  detail: {}", drill_down(lead_tool, detail_arg));
    }
    if rows.len() > listed {
        let _ = writeln!(out, "- … and {} below the cut", rows.len() - listed);
    }
}

struct EntityRow {
    path: PathBuf,
    weight: f64,
    fragments: Vec<(f64, ToolName, String)>,
}

/// The `agent-lens analyze …` invocation that reproduces the full
/// detail behind a digest row or corpus line.
fn drill_down(tool: ToolName, path_args: &str) -> String {
    format!(
        "`agent-lens analyze {} {path_args} --format md`",
        tool.as_str()
    )
}

/// Whether the analyzer accepts a single file as its target, so a
/// row's drill-down can point at the file itself instead of re-running
/// the tool over the whole profile target.
fn file_scoped(tool: ToolName) -> bool {
    matches!(
        tool,
        ToolName::Complexity | ToolName::Cohesion | ToolName::Similarity | ToolName::Wrapper
    )
}

/// `path` as the reader should type it: relative to `cwd` when it sits
/// under it, verbatim otherwise. `cwd` itself spells as `.` — a profile
/// targeting the repository root still needs a typeable argument.
fn display_path(path: &Path, cwd: &Path) -> String {
    let relative = path.strip_prefix(cwd).unwrap_or(path);
    if relative.as_os_str().is_empty() {
        return ".".to_owned();
    }
    relative.display().to_string()
}

/// Extract one analyzer's digest contribution from its JSON report.
///
/// `None` means the digest has no fold for this tool at all (the
/// per-question analyzers: a search or an impact report is already an
/// answer, not a ranking). `Some` with empty vecs means the tool ran
/// and found nothing digest-worthy.
fn extract(tool: ToolName, report: &Value, base: &Path) -> Option<Extraction> {
    Some(match tool {
        ToolName::Complexity => complexity(report, base),
        ToolName::Cohesion => cohesion(report, base),
        ToolName::Similarity => similarity(report, base),
        ToolName::Wrapper => wrapper(report, base),
        ToolName::Delegation => delegation(report, base),
        ToolName::Hotspot => hotspot(report, base),
        ToolName::Risk => risk(report, base),
        ToolName::Hubs => hubs(report, base),
        ToolName::SingleImpl => single_impl(report, base),
        ToolName::SingleUse => single_use(report, base),
        ToolName::TestOnly => test_only(report, base),
        ToolName::Untested => untested(report, base),
        ToolName::Unreachable => unreachable(report, base),
        ToolName::Visibility => visibility(report, base),
        ToolName::ChangeEntropy => change_entropy(report, base),
        ToolName::CoChange => co_change(report),
        ToolName::HiddenCoupling => hidden_coupling(report),
        ToolName::Coupling => coupling(report),
        ToolName::Communities => communities(report),
        ToolName::ContextSpan => context_span(report),
        ToolName::Cycles => cycles(report),
        ToolName::Layers => layers(report),
        ToolName::FunctionGraph | ToolName::GraphQuery | ToolName::Impact | ToolName::Search => {
            return None;
        }
    })
}

/// Resolve a row path written relative to the analysis base. A join
/// with an absolute row path yields that path unchanged, which is
/// exactly the single-file-root case where reports keep the caller's
/// own spelling.
fn from_base(base: &Path, row_path: &str) -> PathBuf {
    base.join(row_path)
}

/// Resolve a row path from a history-backed report, whose rows are
/// written relative to the repository root the report itself names.
fn from_repo_root(report: &Value, base: &Path, row_path: &str) -> PathBuf {
    match report.get("repo_root").and_then(Value::as_str) {
        Some(root) => Path::new(root).join(row_path),
        None => from_base(base, row_path),
    }
}

// ---- JSON access helpers -------------------------------------------------
//
// All reads are optional: a report shape that has moved under the
// extractor produces no finding rather than a panic or a zero that
// looks measured.

fn arr<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn u64_of(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn f64_of(value: &Value, key: &str) -> Option<f64> {
    value.get(key).and_then(Value::as_f64)
}

/// `"1 file"` / `"3 files"` — the digest counts a lot of things, and a
/// count that disagrees with its noun reads like a bug.
fn counted(n: u64, singular: &str, plural: &str) -> String {
    if n == 1 {
        format!("1 {singular}")
    } else {
        format!("{n} {plural}")
    }
}

/// The last `::` segment — digest rows name functions the way a person
/// says them, not with the full crate path the entity row already
/// implies.
fn short_name(qualified: &str) -> &str {
    qualified.rsplit("::").next().unwrap_or(qualified)
}

// ---- Per-file extractors -------------------------------------------------

/// Worst function per file, files with anything at or above the floor.
/// The JSON carries every function in source order, so ordering is the
/// extractor's job here.
fn complexity(report: &Value, base: &Path) -> Extraction {
    let mut rows: Vec<(u64, u64, PathBuf, String)> = Vec::new();
    for file in arr(report, "files") {
        let Some(path) = str_of(file, "file") else {
            continue;
        };
        let mut worst: Option<(u64, &str)> = None;
        let mut over_floor = 0u64;
        for function in arr(file, "functions") {
            let Some(cognitive) = u64_of(function, "cognitive") else {
                continue;
            };
            if cognitive >= COGNITIVE_FLOOR {
                over_floor += 1;
            }
            let name = str_of(function, "name").unwrap_or("?");
            if worst.is_none_or(|(max, _)| cognitive > max) {
                worst = Some((cognitive, name));
            }
        }
        if let Some((cognitive, name)) = worst
            && cognitive >= COGNITIVE_FLOOR
        {
            let mut headline = format!("cognitive {cognitive} (`{name}`)");
            if over_floor > 1 {
                let _ = write!(headline, " +{} more ≥{COGNITIVE_FLOOR}", over_floor - 1);
            }
            rows.push((cognitive, over_floor, from_base(base, path), headline));
        }
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    Extraction {
        files: rows
            .into_iter()
            .map(|(_, _, path, headline)| FileFinding { path, headline })
            .collect(),
        corpus: Vec::new(),
    }
}

/// Worst split unit per file. LCOM4 counts disconnected method groups,
/// so 1 is cohesive and the floor starts at 2.
fn cohesion(report: &Value, base: &Path) -> Extraction {
    let mut rows: Vec<(u64, u64, PathBuf, String)> = Vec::new();
    for file in arr(report, "files") {
        let Some(path) = str_of(file, "file") else {
            continue;
        };
        let mut worst: Option<(u64, &str)> = None;
        let mut split = 0u64;
        for unit in arr(file, "units") {
            let Some(lcom4) = u64_of(unit, "lcom4") else {
                continue;
            };
            if lcom4 < LCOM4_FLOOR {
                continue;
            }
            split += 1;
            let label = str_of(unit, "label").unwrap_or("?");
            if worst.is_none_or(|(max, _)| lcom4 > max) {
                worst = Some((lcom4, label));
            }
        }
        if let Some((lcom4, label)) = worst {
            let mut headline = format!("LCOM4 {lcom4} (`{label}`)");
            if split > 1 {
                let _ = write!(headline, " +{} more split units", split - 1);
            }
            rows.push((lcom4, split, from_base(base, path), headline));
        }
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    Extraction {
        files: rows
            .into_iter()
            .map(|(_, _, path, headline)| FileFinding { path, headline })
            .collect(),
        corpus: Vec::new(),
    }
}

/// Clusters are already ranked (max similarity, then size); a file's
/// rank is its best cluster's, so files are emitted in first-seen
/// order over the cluster list.
fn similarity(report: &Value, base: &Path) -> Extraction {
    let mut order: Vec<PathBuf> = Vec::new();
    let mut clusters_touching: HashMap<PathBuf, u64> = HashMap::new();
    for cluster in arr(report, "clusters") {
        let mut seen_in_cluster: Vec<PathBuf> = Vec::new();
        for unit in arr(cluster, "units") {
            let Some(file) = str_of(unit, "file") else {
                continue;
            };
            let path = from_base(base, file);
            if seen_in_cluster.contains(&path) {
                continue;
            }
            seen_in_cluster.push(path.clone());
            if !clusters_touching.contains_key(&path) {
                order.push(path.clone());
            }
            *clusters_touching.entry(path).or_insert(0) += 1;
        }
    }
    Extraction {
        files: order
            .into_iter()
            .map(|path| {
                let count = clusters_touching[&path];
                FileFinding {
                    headline: format!(
                        "in {}",
                        counted(count, "duplicate cluster", "duplicate clusters"),
                    ),
                    path,
                }
            })
            .collect(),
        corpus: Vec::new(),
    }
}

/// Wrapper counts per file. Every wrapper is a finding — the analyzer
/// has already excused adapters and facades — so volume is the rank.
fn wrapper(report: &Value, base: &Path) -> Extraction {
    let mut rows: Vec<(u64, PathBuf, String)> = Vec::new();
    for file in arr(report, "files") {
        let Some(path) = str_of(file, "file") else {
            continue;
        };
        let wrappers = arr(file, "wrappers");
        let count = wrappers.len() as u64;
        if count == 0 {
            continue;
        }
        let first = str_of(&wrappers[0], "name").unwrap_or("?");
        let mut headline = format!(
            "{} (`{first}`",
            counted(count, "forwarding wrapper", "forwarding wrappers"),
        );
        headline.push_str(if count > 1 { ", …)" } else { ")" });
        rows.push((count, from_base(base, path), headline));
    }
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    Extraction {
        files: rows
            .into_iter()
            .map(|(_, path, headline)| FileFinding { path, headline })
            .collect(),
        corpus: Vec::new(),
    }
}

/// Shared per-file rollup for the entry-list analyzers: count rows per
/// file and headline the first name — the reports sort strongest-first,
/// so that first name is the row worth opening.
fn per_file_rollup(
    report: &Value,
    base: &Path,
    entries_key: &str,
    first_name: impl Fn(&Value) -> Option<String>,
    singular: &str,
    plural: &str,
) -> Extraction {
    let mut order: Vec<PathBuf> = Vec::new();
    let mut per_file: HashMap<PathBuf, (u64, String)> = HashMap::new();
    for entry in arr(report, entries_key) {
        let Some(file) = str_of(entry, "file") else {
            continue;
        };
        let Some(name) = first_name(entry) else {
            continue;
        };
        let path = from_base(base, file);
        let slot = per_file.entry(path.clone()).or_insert_with(|| {
            order.push(path);
            (0, name)
        });
        slot.0 += 1;
    }
    let mut rows: Vec<(u64, PathBuf, String)> = order
        .into_iter()
        .filter_map(|path| {
            let (count, first) = per_file.remove(&path)?;
            let mut headline = format!("{} (`{first}`", counted(count, singular, plural));
            headline.push_str(if count > 1 { ", …)" } else { ")" });
            Some((count, path, headline))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    Extraction {
        files: rows
            .into_iter()
            .map(|(_, path, headline)| FileFinding { path, headline })
            .collect(),
        corpus: Vec::new(),
    }
}

/// Inline candidates per file, headlined by the cleanest one.
fn single_use(report: &Value, base: &Path) -> Extraction {
    per_file_rollup(
        report,
        base,
        "candidates",
        |entry| str_of(entry, "qualified_name").map(|name| short_name(name).to_owned()),
        "inline candidate",
        "inline candidates",
    )
}

/// Single-impl abstractions per file, headlined by the strongest row.
fn single_impl(report: &Value, base: &Path) -> Extraction {
    per_file_rollup(
        report,
        base,
        "findings",
        |entry| str_of(entry, "display_name").map(str::to_owned),
        "single-impl abstraction",
        "single-impl abstractions",
    )
}

/// Test-only findings per file, headlined by the strongest row.
fn test_only(report: &Value, base: &Path) -> Extraction {
    per_file_rollup(
        report,
        base,
        "findings",
        |entry| str_of(entry, "qualified_name").map(|name| short_name(name).to_owned()),
        "test-only function",
        "test-only functions",
    )
}

/// Chains grouped by terminus file — the terminus is where the work
/// actually happens, which is what an agent wants to open first.
fn delegation(report: &Value, base: &Path) -> Extraction {
    let mut order: Vec<PathBuf> = Vec::new();
    let mut per_file: HashMap<PathBuf, (u64, u64)> = HashMap::new();
    for chain in arr(report, "chains") {
        let Some(terminus) = chain.get("terminus") else {
            continue;
        };
        let Some(file) = str_of(terminus, "file") else {
            continue;
        };
        let depth = u64_of(chain, "depth").unwrap_or(0);
        let path = from_base(base, file);
        if !per_file.contains_key(&path) {
            order.push(path.clone());
        }
        let entry = per_file.entry(path).or_insert((0, 0));
        entry.0 = entry.0.max(depth);
        entry.1 += 1;
    }
    let corpus = u64_of(
        report.get("summary").unwrap_or(&Value::Null),
        "lasagna_module_count",
    )
    .filter(|&n| n > 0)
    .map(|n| {
        format!(
            "{} (mostly-forwarding)",
            counted(n, "lasagna module", "lasagna modules"),
        )
    })
    .into_iter()
    .collect();
    Extraction {
        // Chains arrive depth-descending, so first-seen order is
        // already worst-first per terminus.
        files: order
            .into_iter()
            .map(|path| {
                let (depth, count) = per_file[&path];
                FileFinding {
                    headline: format!(
                        "terminus of {} (max depth {depth})",
                        counted(count, "delegation chain", "delegation chains"),
                    ),
                    path,
                }
            })
            .collect(),
        corpus,
    }
}

/// Rows are pre-ranked by `commits × cognitive_max`; the digest keeps
/// the rank number since "hotspot #2" carries more than the raw score.
fn hotspot(report: &Value, base: &Path) -> Extraction {
    let files = arr(report, "files")
        .iter()
        .enumerate()
        .filter(|(_, row)| u64_of(row, "score").is_some_and(|s| s > 0))
        .filter_map(|(index, row)| {
            let path = str_of(row, "path")?;
            let commits = u64_of(row, "commits")?;
            let cognitive = u64_of(row, "cognitive_max").unwrap_or(0);
            Some(FileFinding {
                path: from_repo_root(report, base, path),
                headline: format!(
                    "hotspot #{} ({} × cognitive {cognitive})",
                    index + 1,
                    counted(commits, "commit", "commits"),
                ),
            })
        })
        .collect();
    Extraction {
        files,
        corpus: Vec::new(),
    }
}

/// Rows are pre-ranked by rank product (lower is riskier); like
/// hotspot, the position is the headline.
fn risk(report: &Value, base: &Path) -> Extraction {
    let files = arr(report, "files")
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let path = str_of(row, "path")?;
            let commits = u64_of(row, "commits")?;
            let mut headline = format!(
                "change risk #{} ({}",
                index + 1,
                counted(commits, "commit", "commits"),
            );
            if let Some(hottest) = row.get("hottest_function")
                && let Some(name) = str_of(hottest, "qualified_name")
            {
                let _ = write!(headline, ", load-bearing `{}`", short_name(name));
            }
            headline.push(')');
            Some(FileFinding {
                path: from_repo_root(report, base, path),
                headline,
            })
        })
        .collect();
    Extraction {
        files,
        corpus: Vec::new(),
    }
}

/// One fragment per flagged function, folded per file. Role order is
/// severity order: a bottleneck (both directions outlying) beats a god
/// function beats a misplaced function; load-bearing fan-in is a
/// blast-radius note rather than a defect, so it ranks last.
fn hubs(report: &Value, base: &Path) -> Extraction {
    const ROLES: [(&str, &str); 4] = [
        ("bottlenecks", "bottleneck"),
        ("god_functions", "god function"),
        ("misplaced", "misplaced"),
        ("load_bearing", "load-bearing"),
    ];
    /// Per-role cap: `load_bearing` alone can run to hundreds of rows
    /// on a large graph, and the digest only wants each role's worst.
    const ROLE_CAP: usize = 8;
    let mut order: Vec<PathBuf> = Vec::new();
    let mut fragments: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for (key, label) in ROLES {
        for entry in arr(report, key).iter().take(ROLE_CAP) {
            let Some(file) = str_of(entry, "file") else {
                continue;
            };
            let Some(name) = str_of(entry, "qualified_name") else {
                continue;
            };
            let mut fragment = format!("{label} `{}`", short_name(name));
            match (key, u64_of(entry, "fan_in"), u64_of(entry, "fan_out")) {
                ("god_functions", _, Some(fan_out)) => {
                    let _ = write!(fragment, " (fan-out {fan_out})");
                }
                ("load_bearing", Some(fan_in), _) => {
                    let _ = write!(fragment, " (fan-in {fan_in})");
                }
                ("bottlenecks", Some(fan_in), Some(fan_out)) => {
                    let _ = write!(fragment, " (fan-in {fan_in}, fan-out {fan_out})");
                }
                _ => {}
            }
            if key == "misplaced"
                && let Some(dominant) = entry.get("dominant_foreign_module")
                && let Some(module) = str_of(dominant, "module")
            {
                let _ = write!(fragment, " (pulled toward `{module}`)");
            }
            let path = from_base(base, file);
            if !fragments.contains_key(&path) {
                order.push(path.clone());
            }
            fragments.entry(path).or_default().push(fragment);
        }
    }
    Extraction {
        files: order
            .into_iter()
            .map(|path| {
                let mut parts = fragments.remove(&path).unwrap_or_default();
                let extra = parts.len().saturating_sub(2);
                parts.truncate(2);
                let mut headline = parts.join("; ");
                if extra > 0 {
                    let _ = write!(headline, "; +{extra} more flagged");
                }
                FileFinding { path, headline }
            })
            .collect(),
        corpus: Vec::new(),
    }
}

/// Untested functions folded per file, plus the whole-corpus share.
/// Module groups arrive sorted by untested LOC, so walking them in
/// order and re-sorting per-file totals keeps the worst files first.
fn untested(report: &Value, base: &Path) -> Extraction {
    let mut per_file: HashMap<PathBuf, (u64, u64)> = HashMap::new();
    for module in arr(report, "modules") {
        for function in arr(module, "functions") {
            let Some(file) = str_of(function, "file") else {
                continue;
            };
            let loc = u64_of(function, "loc").unwrap_or(0);
            let entry = per_file.entry(from_base(base, file)).or_insert((0, 0));
            entry.0 += 1;
            entry.1 += loc;
        }
    }
    let mut rows: Vec<(u64, u64, PathBuf)> = per_file
        .into_iter()
        .map(|(path, (count, loc))| (loc, count, path))
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
    let corpus = report
        .get("summary")
        .and_then(|summary| {
            let count = u64_of(summary, "untested_function_count")?;
            if count == 0 {
                return None;
            }
            let loc = u64_of(summary, "untested_loc").unwrap_or(0);
            let share = f64_of(summary, "untested_share").unwrap_or(0.0);
            Some(format!(
                "{} with no static test path ({loc} LOC, {:.0}% of production)",
                counted(count, "function", "functions"),
                share * 100.0,
            ))
        })
        .into_iter()
        .collect();
    Extraction {
        files: rows
            .into_iter()
            .map(|(loc, count, path)| FileFinding {
                path,
                headline: format!(
                    "{} ({loc} LOC)",
                    counted(count, "untested function", "untested functions"),
                ),
            })
            .collect(),
        corpus,
    }
}

/// Only the `confirmed` and `likely` tiers make a digest row: an
/// `unknown` is a lead the full section explains, not a finding to
/// rank files by.
fn unreachable(report: &Value, base: &Path) -> Extraction {
    let mut per_file: HashMap<PathBuf, (u64, u64, u64)> = HashMap::new();
    for module in arr(report, "modules") {
        for finding in arr(module, "findings") {
            let Some(file) = str_of(finding, "file") else {
                continue;
            };
            let loc = u64_of(finding, "loc").unwrap_or(0);
            let entry = per_file.entry(from_base(base, file)).or_insert((0, 0, 0));
            match str_of(finding, "tier") {
                Some("confirmed") => {
                    entry.0 += 1;
                    entry.1 += loc;
                }
                Some("likely") => entry.2 += 1,
                _ => {}
            }
        }
    }
    let mut rows: Vec<(u64, u64, u64, PathBuf)> = per_file
        .into_iter()
        .filter(|(_, (confirmed, _, likely))| confirmed + likely > 0)
        .map(|(path, (confirmed, loc, likely))| (confirmed, loc, likely, path))
        .collect();
    rows.sort_by(|a, b| (b.0, b.1, b.2).cmp(&(a.0, a.1, a.2)).then(a.3.cmp(&b.3)));
    let corpus = report
        .get("summary")
        .and_then(|summary| {
            let confirmed = u64_of(summary, "confirmed_count").unwrap_or(0);
            let likely = u64_of(summary, "likely_count").unwrap_or(0);
            if confirmed == 0 && likely == 0 {
                return None;
            }
            let loc = u64_of(summary, "confirmed_loc").unwrap_or(0);
            Some(format!(
                "{confirmed} confirmed-dead ({loc} LOC), {likely} likely-dead",
            ))
        })
        .into_iter()
        .collect();
    Extraction {
        files: rows
            .into_iter()
            .map(|(confirmed, loc, likely, path)| {
                let mut parts = Vec::new();
                if confirmed > 0 {
                    parts.push(format!("{confirmed} confirmed-dead ({loc} LOC)"));
                }
                if likely > 0 {
                    parts.push(format!("{likely} likely-dead"));
                }
                FileFinding {
                    path,
                    headline: parts.join(", "),
                }
            })
            .collect(),
        corpus,
    }
}

/// Over-exposed declarations folded per file, plus the corpus share.
fn visibility(report: &Value, base: &Path) -> Extraction {
    let mut per_file: HashMap<PathBuf, u64> = HashMap::new();
    for module in arr(report, "modules") {
        for finding in arr(module, "findings") {
            let Some(file) = str_of(finding, "file") else {
                continue;
            };
            *per_file.entry(from_base(base, file)).or_insert(0) += 1;
        }
    }
    let mut rows: Vec<(u64, PathBuf)> = per_file
        .into_iter()
        .map(|(path, count)| (count, path))
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let corpus = report
        .get("summary")
        .and_then(|summary| {
            let count = u64_of(summary, "over_exposed_count")?;
            if count == 0 {
                return None;
            }
            let share = f64_of(summary, "over_exposed_share").unwrap_or(0.0);
            Some(format!(
                "{} wider than their callers need ({:.0}% of public surface)",
                counted(count, "function", "functions"),
                share * 100.0,
            ))
        })
        .into_iter()
        .collect();
    Extraction {
        files: rows
            .into_iter()
            .map(|(count, path)| FileFinding {
                path,
                headline: format!(
                    "{} than needed",
                    counted(count, "fn visible wider", "fns visible wider"),
                ),
            })
            .collect(),
        corpus,
    }
}

/// History mode ranks files by accumulated history complexity;
/// diff mode (`--diff-only`) describes the one pending change set, so
/// it contributes a corpus verdict instead of file rows.
fn change_entropy(report: &Value, base: &Path) -> Extraction {
    if let Some(pending) = report.get("pending") {
        let corpus = (|| {
            let files = u64_of(pending, "files_touched")?;
            let modules = u64_of(pending, "modules_spanned").unwrap_or(0);
            let entropy = f64_of(pending, "entropy").unwrap_or(0.0);
            let mut line = format!(
                "pending change spans {} across {} (entropy {entropy:.2}",
                counted(files, "file", "files"),
                counted(modules, "module", "modules"),
            );
            if let Some(percentile) = report
                .get("reference")
                .and_then(|reference| u64_of(reference, "percentile"))
            {
                let _ = write!(line, ", p{percentile} of this repo's commits");
            }
            line.push(')');
            Some(line)
        })()
        .into_iter()
        .collect();
        return Extraction {
            files: Vec::new(),
            corpus,
        };
    }
    let files = arr(report, "files")
        .iter()
        .filter_map(|row| {
            let path = str_of(row, "path")?;
            let complexity = f64_of(row, "history_complexity")?;
            if complexity <= 0.0 {
                return None;
            }
            let commits = u64_of(row, "commits").unwrap_or(0);
            Some(FileFinding {
                path: from_repo_root(report, base, path),
                headline: format!(
                    "history complexity {complexity:.2} ({} in scattered change sets)",
                    counted(commits, "commit", "commits"),
                ),
            })
        })
        .collect();
    Extraction {
        files,
        corpus: Vec::new(),
    }
}

// ---- Corpus-shaped extractors --------------------------------------------

/// A co-change pair belongs to two files at once, so pairs stay
/// corpus-shaped: the digest names the count and the strongest pair.
fn co_change(report: &Value) -> Extraction {
    let pairs = arr(report, "pairs");
    let corpus = pairs
        .first()
        .and_then(|top| {
            let a = str_of(top, "a")?;
            let b = str_of(top, "b")?;
            let cochanges = u64_of(top, "cochanges").unwrap_or(0);
            Some(format!(
                "{}; strongest {a} <-> {b} ({cochanges} co-changes)",
                counted(pairs.len() as u64, "co-changing pair", "co-changing pairs"),
            ))
        })
        .into_iter()
        .collect();
    Extraction {
        files: Vec::new(),
        corpus,
    }
}

fn hidden_coupling(report: &Value) -> Extraction {
    let mut corpus = Vec::new();
    let hidden = arr(report, "hidden_coupling");
    if let Some(top) = hidden.first()
        && let (Some(a), Some(b)) = (str_of(top, "a"), str_of(top, "b"))
    {
        corpus.push(format!(
            "{} with no static edge; strongest {a} <-> {b} ({} co-changes)",
            counted(hidden.len() as u64, "co-changing pair", "co-changing pairs"),
            u64_of(top, "cochanges").unwrap_or(0),
        ));
    }
    let suspects = arr(report, "suspect_dependencies").len() as u64;
    if suspects > 0 {
        corpus.push(format!(
            "{} never exercised in the window",
            counted(suspects, "declared dependency", "declared dependencies"),
        ));
    }
    Extraction {
        files: Vec::new(),
        corpus,
    }
}

/// Module-granularity metrics stay corpus-shaped: a module maps to
/// many files, and pretending otherwise would double-count weight.
fn coupling(report: &Value) -> Extraction {
    let mut corpus = Vec::new();
    if let Some(cycles) = u64_of(report, "cycle_count")
        && cycles > 0
    {
        corpus.push(counted(cycles, "module cycle", "module cycles"));
    }
    let worst = |key: &str| -> Option<(u64, String)> {
        arr(report, "modules")
            .iter()
            .filter_map(|module| Some((u64_of(module, key)?, str_of(module, "path")?)))
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)))
            .map(|(value, path)| (value, path.to_owned()))
    };
    if let Some((fan_in, module)) = worst("fan_in")
        && fan_in > 0
    {
        corpus.push(format!("fan-in max {fan_in} (`{module}`)"));
    }
    if let Some((ifc, module)) = worst("ifc")
        && ifc > 0
    {
        corpus.push(format!("IFC max {ifc} (`{module}`)"));
    }
    Extraction {
        files: Vec::new(),
        corpus,
    }
}

fn communities(report: &Value) -> Extraction {
    let mut corpus = Vec::new();
    if let Some(modularity) = report.get("modularity")
        && let (Some(detected), Some(declared)) = (
            f64_of(modularity, "detected"),
            f64_of(modularity, "declared"),
        )
    {
        corpus.push(format!(
            "modularity {detected:.2} detected vs {declared:.2} declared",
        ));
    }
    let misfiled = arr(report, "misfiled").len() as u64;
    if misfiled > 0 {
        corpus.push(counted(misfiled, "misfiled member", "misfiled members"));
    }
    let spanning = arr(report, "spanning").len() as u64;
    if spanning > 0 {
        corpus.push(counted(
            spanning,
            "spanning community",
            "spanning communities",
        ));
    }
    Extraction {
        files: Vec::new(),
        corpus,
    }
}

fn context_span(report: &Value) -> Extraction {
    let corpus = arr(report, "modules")
        .iter()
        .filter_map(|module| {
            Some((
                u64_of(module, "transitive")?,
                u64_of(module, "files").unwrap_or(0),
                str_of(module, "path")?,
            ))
        })
        .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.2.cmp(a.2)))
        .filter(|&(transitive, _, _)| transitive > 0)
        .map(|(transitive, files, module)| {
            format!(
                "widest span `{module}` reaches {} ({})",
                counted(transitive, "module", "modules"),
                counted(files, "file", "files"),
            )
        })
        .into_iter()
        .collect();
    Extraction {
        files: Vec::new(),
        corpus,
    }
}

fn cycles(report: &Value) -> Extraction {
    let corpus = report
        .get("summary")
        .and_then(|summary| {
            let count = u64_of(summary, "scc_count")?;
            if count == 0 {
                return None;
            }
            let largest = u64_of(summary, "largest").unwrap_or(0);
            Some(format!(
                "{} (largest {largest} functions)",
                counted(count, "call cycle", "call cycles"),
            ))
        })
        .into_iter()
        .collect();
    Extraction {
        files: Vec::new(),
        corpus,
    }
}

fn layers(report: &Value) -> Extraction {
    let Some(summary) = report.get("summary") else {
        return Extraction::default();
    };
    let mut corpus = Vec::new();
    if let Some(cycles) = u64_of(summary, "module_cycle_count")
        && cycles > 0
    {
        corpus.push(format!(
            "{} ({} involved)",
            counted(cycles, "module cycle", "module cycles"),
            counted(
                u64_of(summary, "cyclic_module_count").unwrap_or(0),
                "module",
                "modules",
            ),
        ));
    }
    if let Some(pairs) = u64_of(summary, "skip_pair_count")
        && pairs > 0
    {
        corpus.push(counted(pairs, "skip-level pair", "skip-level pairs"));
    }
    if let Some(wide) = u64_of(summary, "wide_span_module_count")
        && wide > 0
    {
        corpus.push(counted(wide, "wide-span module", "wide-span modules"));
    }
    Extraction {
        files: Vec::new(),
        corpus,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use super::*;

    /// Every tool the digest folds, for shape-robustness sweeps.
    const FOLDED: [ToolName; 19] = [
        ToolName::Complexity,
        ToolName::Cohesion,
        ToolName::Similarity,
        ToolName::Wrapper,
        ToolName::Delegation,
        ToolName::Hotspot,
        ToolName::Risk,
        ToolName::Hubs,
        ToolName::Untested,
        ToolName::Unreachable,
        ToolName::Visibility,
        ToolName::ChangeEntropy,
        ToolName::CoChange,
        ToolName::HiddenCoupling,
        ToolName::Coupling,
        ToolName::Communities,
        ToolName::ContextSpan,
        ToolName::Cycles,
        ToolName::Layers,
    ];

    fn base() -> PathBuf {
        PathBuf::from("/repo/src")
    }

    fn files_of(extraction: &Extraction) -> Vec<(&str, &str)> {
        extraction
            .files
            .iter()
            .map(|finding| (finding.path.to_str().unwrap(), finding.headline.as_str()))
            .collect()
    }

    #[test]
    fn complexity_keeps_the_worst_function_per_file_above_the_floor() {
        let report = json!({
            "files": [
                { "file": "calm.rs", "functions": [ { "name": "tiny", "cognitive": 3 } ] },
                { "file": "busy.rs", "functions": [
                    { "name": "walk", "cognitive": 12 },
                    { "name": "audit_scope", "cognitive": 45 },
                    { "name": "helper", "cognitive": 1 },
                ] },
                { "file": "warm.rs", "functions": [ { "name": "run", "cognitive": 11 } ] },
            ],
        });
        let extraction = complexity(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [
                (
                    "/repo/src/busy.rs",
                    "cognitive 45 (`audit_scope`) +1 more ≥10"
                ),
                ("/repo/src/warm.rs", "cognitive 11 (`run`)"),
            ],
        );
    }

    #[test]
    fn cohesion_reports_split_units_only() {
        let report = json!({
            "files": [
                { "file": "one.rs", "units": [
                    { "label": "impl Fine", "lcom4": 1 },
                    { "label": "module", "lcom4": 5 },
                    { "label": "impl Torn", "lcom4": 2 },
                ] },
                { "file": "two.rs", "units": [ { "label": "impl Ok", "lcom4": 1 } ] },
            ],
        });
        let extraction = cohesion(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [("/repo/src/one.rs", "LCOM4 5 (`module`) +1 more split units")],
        );
    }

    /// On a tie the first function in source order keeps the headline:
    /// a later equal score must not silently rename the row.
    #[test]
    fn complexity_keeps_the_first_of_tied_worst_functions() {
        let report = json!({
            "files": [
                { "file": "tied.rs", "functions": [
                    { "name": "first", "cognitive": 20 },
                    { "name": "second", "cognitive": 20 },
                ] },
            ],
        });
        let extraction = complexity(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [("/repo/src/tied.rs", "cognitive 20 (`first`) +1 more ≥10")],
        );
    }

    #[test]
    fn cohesion_keeps_the_first_of_tied_worst_units() {
        let report = json!({
            "files": [
                { "file": "tied.rs", "units": [
                    { "label": "impl First", "lcom4": 4 },
                    { "label": "impl Second", "lcom4": 4 },
                ] },
            ],
        });
        let extraction = cohesion(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [(
                "/repo/src/tied.rs",
                "LCOM4 4 (`impl First`) +1 more split units"
            )],
        );
    }

    /// Cluster order is the analyzer's ranking, so a file's rank is its
    /// best cluster's position — and a file repeated inside one cluster
    /// still counts that cluster once.
    #[test]
    fn similarity_ranks_files_by_first_cluster_and_counts_distinct_clusters() {
        let report = json!({
            "clusters": [
                { "units": [ { "file": "a.rs" }, { "file": "b.rs" }, { "file": "a.rs" } ] },
                { "units": [ { "file": "b.rs" }, { "file": "c.rs" } ] },
            ],
        });
        let extraction = similarity(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [
                ("/repo/src/a.rs", "in 1 duplicate cluster"),
                ("/repo/src/b.rs", "in 2 duplicate clusters"),
                ("/repo/src/c.rs", "in 1 duplicate cluster"),
            ],
        );
    }

    #[test]
    fn single_use_rolls_candidates_up_per_file() {
        let report = json!({
            "candidates": [
                { "file": "many.rs", "qualified_name": "a::first" },
                { "file": "many.rs", "qualified_name": "a::second" },
                { "file": "one.rs", "qualified_name": "b::only" },
            ],
        });
        let extraction = single_use(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [
                ("/repo/src/many.rs", "2 inline candidates (`first`, …)"),
                ("/repo/src/one.rs", "1 inline candidate (`only`)"),
            ],
        );
    }

    #[test]
    fn single_impl_rolls_findings_up_per_file() {
        let report = json!({
            "findings": [
                { "file": "many.rs", "display_name": "Store" },
                { "file": "many.rs", "display_name": "Codec" },
                { "file": "one.rs", "display_name": "Sink" },
            ],
        });
        let extraction = single_impl(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [
                (
                    "/repo/src/many.rs",
                    "2 single-impl abstractions (`Store`, …)"
                ),
                ("/repo/src/one.rs", "1 single-impl abstraction (`Sink`)"),
            ],
        );
    }

    #[test]
    fn test_only_rolls_findings_up_per_file() {
        let report = json!({
            "findings": [
                { "file": "many.rs", "qualified_name": "a::first" },
                { "file": "many.rs", "qualified_name": "a::second" },
                { "file": "one.rs", "qualified_name": "b::only" },
            ],
        });
        let extraction = test_only(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [
                ("/repo/src/many.rs", "2 test-only functions (`first`, …)"),
                ("/repo/src/one.rs", "1 test-only function (`only`)"),
            ],
        );
    }

    #[test]
    fn wrapper_counts_per_file_and_names_the_first() {
        let report = json!({
            "files": [
                { "file": "thin.rs", "wrappers": [ { "name": "one" }, { "name": "two" } ] },
                { "file": "none.rs", "wrappers": [] },
            ],
        });
        let extraction = wrapper(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [("/repo/src/thin.rs", "2 forwarding wrappers (`one`, …)")],
        );
    }

    #[test]
    fn delegation_groups_chains_by_terminus_and_keeps_depth_order() {
        let report = json!({
            "chains": [
                { "depth": 4, "terminus": { "file": "deep.rs" } },
                { "depth": 2, "terminus": { "file": "shallow.rs" } },
                { "depth": 2, "terminus": { "file": "deep.rs" } },
            ],
            "summary": { "lasagna_module_count": 2 },
        });
        let extraction = delegation(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [
                (
                    "/repo/src/deep.rs",
                    "terminus of 2 delegation chains (max depth 4)"
                ),
                (
                    "/repo/src/shallow.rs",
                    "terminus of 1 delegation chain (max depth 2)"
                ),
            ],
        );
        assert_eq!(extraction.corpus, ["2 lasagna modules (mostly-forwarding)"]);
    }

    /// History-backed rows are written relative to the repository root
    /// the report names, not the analysis base — the join key must
    /// land both spellings on the same absolute path.
    #[test]
    fn hotspot_joins_rows_through_the_reported_repo_root() {
        let report = json!({
            "repo_root": "/repo",
            "files": [
                { "path": "src/hot.rs", "score": 72, "commits": 24, "cognitive_max": 3 },
                { "path": "src/cold.rs", "score": 0, "commits": 0, "cognitive_max": 0 },
            ],
        });
        let extraction = hotspot(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [("/repo/src/hot.rs", "hotspot #1 (24 commits × cognitive 3)")],
        );
    }

    #[test]
    fn risk_keeps_rank_order_and_shortens_the_hottest_function() {
        let report = json!({
            "repo_root": "/repo",
            "files": [
                {
                    "path": "src/mod.rs",
                    "commits": 10,
                    "hottest_function": { "qualified_name": "crate::a::from_path" },
                },
                { "path": "src/lib.rs", "commits": 2 },
            ],
        });
        let extraction = risk(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [
                (
                    "/repo/src/mod.rs",
                    "change risk #1 (10 commits, load-bearing `from_path`)"
                ),
                ("/repo/src/lib.rs", "change risk #2 (2 commits)"),
            ],
        );
    }

    #[test]
    fn hubs_folds_roles_per_file_severity_first() {
        let report = json!({
            "bottlenecks": [
                { "file": "core.rs", "qualified_name": "a::squeeze", "fan_in": 9, "fan_out": 8 },
            ],
            "god_functions": [
                { "file": "core.rs", "qualified_name": "a::sprawl", "fan_out": 21 },
            ],
            "misplaced": [
                {
                    "file": "lost.rs",
                    "qualified_name": "c::stray",
                    "dominant_foreign_module": { "module": "crate::home" },
                },
            ],
            "load_bearing": [
                { "file": "util.rs", "qualified_name": "b::pivot", "fan_in": 18 },
                { "file": "core.rs", "qualified_name": "a::anchor", "fan_in": 12 },
            ],
        });
        let extraction = hubs(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [
                (
                    "/repo/src/core.rs",
                    "bottleneck `squeeze` (fan-in 9, fan-out 8); god function `sprawl` (fan-out 21); +1 more flagged",
                ),
                (
                    "/repo/src/lost.rs",
                    "misplaced `stray` (pulled toward `crate::home`)"
                ),
                ("/repo/src/util.rs", "load-bearing `pivot` (fan-in 18)"),
            ],
        );
    }

    #[test]
    fn untested_rolls_module_functions_up_per_file_by_loc() {
        let report = json!({
            "modules": [
                { "module": "a", "functions": [
                    { "file": "big.rs", "loc": 40 },
                    { "file": "small.rs", "loc": 5 },
                ] },
                { "module": "b", "functions": [ { "file": "big.rs", "loc": 30 } ] },
            ],
            "summary": {
                "untested_function_count": 3,
                "untested_loc": 75,
                "untested_share": 0.67,
            },
        });
        let extraction = untested(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [
                ("/repo/src/big.rs", "2 untested functions (70 LOC)"),
                ("/repo/src/small.rs", "1 untested function (5 LOC)"),
            ],
        );
        assert_eq!(
            extraction.corpus,
            ["3 functions with no static test path (75 LOC, 67% of production)"],
        );
    }

    /// `unknown` findings are leads, not findings to rank files by:
    /// only the tiers the analyzer stands behind reach the digest.
    #[test]
    fn unreachable_counts_confirmed_and_likely_but_never_unknown() {
        let report = json!({
            "modules": [
                { "findings": [
                    { "file": "dead.rs", "tier": "confirmed", "loc": 50 },
                    { "file": "dead.rs", "tier": "confirmed", "loc": 30 },
                    { "file": "dead.rs", "tier": "unknown", "loc": 900 },
                    { "file": "maybe.rs", "tier": "likely", "loc": 10 },
                    { "file": "noise.rs", "tier": "unknown", "loc": 10 },
                ] },
            ],
            "summary": { "confirmed_count": 2, "likely_count": 1, "confirmed_loc": 80 },
        });
        let extraction = unreachable(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [
                ("/repo/src/dead.rs", "2 confirmed-dead (80 LOC)"),
                ("/repo/src/maybe.rs", "1 likely-dead"),
            ],
        );
        assert_eq!(
            extraction.corpus,
            ["2 confirmed-dead (80 LOC), 1 likely-dead"]
        );
    }

    /// One populated tier is enough for the corpus line — only a report
    /// with *neither* tier stays silent.
    #[test]
    fn unreachable_reports_the_corpus_line_with_one_tier_empty() {
        let report = json!({
            "modules": [],
            "summary": { "confirmed_count": 3, "likely_count": 0, "confirmed_loc": 120 },
        });
        let extraction = unreachable(&report, &base());
        assert_eq!(
            extraction.corpus,
            ["3 confirmed-dead (120 LOC), 0 likely-dead"]
        );
    }

    #[test]
    fn visibility_counts_findings_per_file_with_the_corpus_share() {
        let report = json!({
            "modules": [
                { "findings": [ { "file": "api.rs" }, { "file": "api.rs" }, { "file": "one.rs" } ] },
            ],
            "summary": { "over_exposed_count": 3, "over_exposed_share": 0.5 },
        });
        let extraction = visibility(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [
                ("/repo/src/api.rs", "2 fns visible wider than needed"),
                ("/repo/src/one.rs", "1 fn visible wider than needed"),
            ],
        );
        assert_eq!(
            extraction.corpus,
            ["3 functions wider than their callers need (50% of public surface)"],
        );
    }

    #[test]
    fn change_entropy_history_mode_ranks_files() {
        let report = json!({
            "repo_root": "/repo",
            "files": [
                { "path": "a.rs", "history_complexity": 0.42, "commits": 12 },
                { "path": "b.rs", "history_complexity": 0.0, "commits": 3 },
            ],
        });
        let extraction = change_entropy(&report, &base());
        assert_eq!(
            files_of(&extraction),
            [(
                "/repo/a.rs",
                "history complexity 0.42 (12 commits in scattered change sets)"
            )],
        );
    }

    /// `--diff-only` describes the one pending change set, which no
    /// single file owns — it digests to a corpus verdict.
    #[test]
    fn change_entropy_diff_mode_digests_to_a_corpus_verdict() {
        let report = json!({
            "pending": { "files_touched": 4, "modules_spanned": 2, "entropy": 0.66 },
            "reference": { "percentile": 80 },
        });
        let extraction = change_entropy(&report, &base());
        assert!(extraction.files.is_empty());
        assert_eq!(
            extraction.corpus,
            [
                "pending change spans 4 files across 2 modules (entropy 0.66, p80 of this repo's commits)"
            ],
        );
    }

    #[test]
    fn co_change_names_the_count_and_the_strongest_pair() {
        let report = json!({
            "pairs": [
                { "a": "README.md", "b": "src/args.rs", "cochanges": 10 },
                { "a": "x", "b": "y", "cochanges": 8 },
            ],
        });
        let extraction = co_change(&report);
        assert!(extraction.files.is_empty());
        assert_eq!(
            extraction.corpus,
            ["2 co-changing pairs; strongest README.md <-> src/args.rs (10 co-changes)"],
        );
    }

    #[test]
    fn hidden_coupling_reports_both_buckets() {
        let report = json!({
            "hidden_coupling": [ { "a": "a.rs", "b": "b.rs", "cochanges": 8 } ],
            "suspect_dependencies": [ { "a": "c.rs", "b": "d.rs" }, { "a": "e.rs", "b": "f.rs" } ],
        });
        let extraction = hidden_coupling(&report);
        assert_eq!(
            extraction.corpus,
            [
                "1 co-changing pair with no static edge; strongest a.rs <-> b.rs (8 co-changes)",
                "2 declared dependencies never exercised in the window",
            ],
        );
    }

    /// An empty suspect bucket contributes no line at all — a "0
    /// declared dependencies" claim would read as a measured finding.
    #[test]
    fn hidden_coupling_stays_silent_about_an_empty_suspect_bucket() {
        let report = json!({
            "hidden_coupling": [ { "a": "a.rs", "b": "b.rs", "cochanges": 8 } ],
            "suspect_dependencies": [],
        });
        let extraction = hidden_coupling(&report);
        assert_eq!(
            extraction.corpus,
            ["1 co-changing pair with no static edge; strongest a.rs <-> b.rs (8 co-changes)"],
        );
    }

    #[test]
    fn cycles_reports_the_count_and_the_largest_tangle() {
        let report = json!({ "summary": { "scc_count": 2, "largest": 5 } });
        let extraction = cycles(&report);
        assert!(extraction.files.is_empty());
        assert_eq!(extraction.corpus, ["2 call cycles (largest 5 functions)"]);
    }

    #[test]
    fn coupling_reports_cycles_and_the_worst_module_per_axis() {
        let report = json!({
            "cycle_count": 1,
            "modules": [
                { "path": "crate::a", "fan_in": 53, "ifc": 100 },
                { "path": "crate::b", "fan_in": 2, "ifc": 15876 },
            ],
        });
        let extraction = coupling(&report);
        assert_eq!(
            extraction.corpus,
            [
                "1 module cycle",
                "fan-in max 53 (`crate::a`)",
                "IFC max 15876 (`crate::b`)",
            ],
        );
    }

    #[test]
    fn communities_reports_the_modularity_pair_and_misfiled_count() {
        let report = json!({
            "modularity": { "detected": 0.501, "declared": 0.28 },
            "misfiled": [ {}, {} ],
            "spanning": [ {} ],
        });
        let extraction = communities(&report);
        assert_eq!(
            extraction.corpus,
            [
                "modularity 0.50 detected vs 0.28 declared",
                "2 misfiled members",
                "1 spanning community",
            ],
        );
    }

    #[test]
    fn context_span_reports_the_widest_module() {
        let report = json!({
            "modules": [
                { "path": "crate::thin", "transitive": 2, "files": 3 },
                { "path": "crate::wide", "transitive": 40, "files": 39 },
            ],
        });
        let extraction = context_span(&report);
        assert_eq!(
            extraction.corpus,
            ["widest span `crate::wide` reaches 40 modules (39 files)"],
        );
    }

    #[test]
    fn layers_reports_only_the_nonzero_structural_counts() {
        let report = json!({
            "summary": {
                "module_cycle_count": 2,
                "cyclic_module_count": 15,
                "skip_pair_count": 91,
                "wide_span_module_count": 0,
            },
        });
        let extraction = layers(&report);
        assert_eq!(
            extraction.corpus,
            [
                "2 module cycles (15 modules involved)",
                "91 skip-level pairs",
            ],
        );
    }

    #[test]
    fn layers_reports_wide_span_modules_when_present() {
        let report = json!({
            "summary": {
                "module_cycle_count": 0,
                "skip_pair_count": 0,
                "wide_span_module_count": 3,
            },
        });
        let extraction = layers(&report);
        assert_eq!(extraction.corpus, ["3 wide-span modules"],);
    }

    #[rstest]
    #[case(ToolName::Cycles, json!({ "summary": { "scc_count": 0, "largest": 0 } }))]
    #[case(ToolName::Coupling, json!({ "cycle_count": 0, "modules": [ { "path": "crate::a", "fan_in": 0, "ifc": 0 } ] }))]
    #[case(ToolName::CoChange, json!({ "pairs": [] }))]
    #[case(ToolName::Unreachable, json!({ "modules": [], "summary": { "confirmed_count": 0, "likely_count": 0 } }))]
    #[case(ToolName::Delegation, json!({ "chains": [], "summary": { "lasagna_module_count": 0 } }))]
    #[case(ToolName::HiddenCoupling, json!({ "hidden_coupling": [], "suspect_dependencies": [] }))]
    #[case(ToolName::Communities, json!({ "misfiled": [], "spanning": [] }))]
    #[case(ToolName::ContextSpan, json!({ "modules": [ { "path": "crate::a", "transitive": 0, "files": 0 } ] }))]
    #[case(ToolName::Layers, json!({ "summary": { "module_cycle_count": 0, "skip_pair_count": 0, "wide_span_module_count": 0 } }))]
    fn a_clean_report_digests_to_nothing_rather_than_a_zero_claim(
        #[case] tool: ToolName,
        #[case] report: Value,
    ) {
        assert_eq!(extract(tool, &report, &base()), Some(Extraction::default()));
    }

    #[rstest]
    #[case(ToolName::Search)]
    #[case(ToolName::GraphQuery)]
    #[case(ToolName::Impact)]
    #[case(ToolName::FunctionGraph)]
    fn per_question_tools_have_no_fold(#[case] tool: ToolName) {
        assert_eq!(extract(tool, &json!({}), &base()), None);
    }

    /// The extractors read reports produced by nineteen analyzers whose
    /// shapes are still moving; a shape that shifts under one must
    /// degrade to "nothing extracted", never to a panic or an invented
    /// finding.
    #[rstest]
    #[case(json!({}))]
    #[case(json!({ "files": "not an array", "modules": 3, "clusters": null, "summary": [] }))]
    #[case(json!({ "files": [ { "file": 7, "functions": [ { "cognitive": "many" } ] } ] }))]
    #[case(json!({ "modules": [ { "findings": "x", "functions": { "file": "y" } } ] }))]
    fn extractors_survive_shapes_they_do_not_recognise(#[case] report: Value) {
        for tool in FOLDED {
            let extraction = extract(tool, &report, &base()).unwrap();
            assert!(extraction.files.is_empty(), "{tool:?}: {extraction:?}");
        }
    }

    fn complexity_report(files: &[(&str, u64)]) -> Value {
        json!({
            "files": files
                .iter()
                .map(|(file, cognitive)| {
                    json!({ "file": file, "functions": [ { "name": "f", "cognitive": cognitive } ] })
                })
                .collect::<Vec<_>>(),
        })
    }

    #[test]
    fn render_joins_tools_on_one_file_and_ranks_by_cross_tool_weight() {
        let sections = vec![
            (
                ToolName::Complexity,
                complexity_report(&[("aa_solo.rs", 40), ("zz_both.rs", 20)]),
            ),
            (
                ToolName::Cohesion,
                json!({
                    "files": [
                        { "file": "zz_both.rs", "units": [ { "label": "module", "lcom4": 5 } ] },
                    ],
                }),
            ),
        ];
        let out = render(
            "audit",
            &sections,
            &[PathBuf::from("/repo/src")],
            Path::new("/repo"),
        );
        // Two tools at weight 0.5 + 1.0 beat one tool at weight 1.0, so
        // the shared file leads even though the solo file's single score
        // is higher and its path sorts first.
        assert!(
            out.find("src/zz_both.rs").unwrap() < out.find("src/aa_solo.rs").unwrap(),
            "got: {out}"
        );
        assert!(
            out.contains("## Findings by entity (2 files, ranked by cross-tool weight)"),
            "got: {out}"
        );
        // The row joins both tools' headlines, with no phantom overflow
        // marker when every fragment fit...
        assert!(
            out.contains("LCOM4 5 (`module`), cognitive 20 (`f`)"),
            "got: {out}"
        );
        assert!(!out.contains("+0 more"), "got: {out}");
        // ...and the strongest claim (cohesion, weight 1.0) picks the
        // drill-down, pointed at the file since cohesion is file-scoped.
        assert!(
            out.contains("detail: `agent-lens analyze cohesion src/zz_both.rs --format md`"),
            "got: {out}"
        );
        assert!(
            out.contains("Full sections: `agent-lens run audit --format md`"),
            "got: {out}"
        );
    }

    /// A lead tool that needs the whole corpus (hotspot reads git
    /// churn) must not be handed a single file as its drill-down
    /// argument — and a run with nothing corpus-shaped must not print
    /// an empty corpus section.
    #[test]
    fn render_points_a_corpus_scoped_lead_tool_at_the_target() {
        let sections = vec![(
            ToolName::Hotspot,
            json!({
                "repo_root": "/repo",
                "files": [ { "path": "src/hot.rs", "score": 9, "commits": 3, "cognitive_max": 3 } ],
            }),
        )];
        let out = render(
            "audit",
            &sections,
            &[PathBuf::from("/repo/src")],
            Path::new("/repo"),
        );
        assert!(
            out.contains("detail: `agent-lens analyze hotspot src --format md`"),
            "got: {out}"
        );
        assert!(!out.contains("## Corpus-level findings"), "got: {out}");
    }

    /// More entities than the row cap: the overflow is counted, never
    /// silently dropped.
    #[test]
    fn render_counts_the_entities_past_the_row_cap() {
        let names: Vec<String> = (0..45).map(|i| format!("f{i:03}.rs")).collect();
        let file_list = |range: std::ops::Range<usize>| {
            names[range]
                .iter()
                .map(|name| (name.as_str(), 30u64))
                .collect::<Vec<_>>()
        };
        let sections = vec![
            (ToolName::Complexity, complexity_report(&file_list(0..15))),
            (
                ToolName::Cohesion,
                json!({
                    "files": names[15..30]
                        .iter()
                        .map(|name| json!({ "file": name, "units": [ { "label": "m", "lcom4": 3 } ] }))
                        .collect::<Vec<_>>(),
                }),
            ),
            (
                ToolName::Wrapper,
                json!({
                    "files": names[30..45]
                        .iter()
                        .map(|name| json!({ "file": name, "wrappers": [ { "name": "w" } ] }))
                        .collect::<Vec<_>>(),
                }),
            ),
        ];
        let out = render(
            "audit",
            &sections,
            &[PathBuf::from("/repo/src")],
            Path::new("/repo"),
        );
        assert!(
            out.contains("## Findings by entity (45 files, ranked by cross-tool weight)"),
            "got: {out}"
        );
        assert!(out.contains("- … and 5 below the cut"), "got: {out}");
    }

    #[test]
    fn render_separates_corpus_quiet_and_unfolded_tools() {
        let sections = vec![
            (ToolName::Wrapper, json!({ "files": [] })),
            (
                ToolName::Layers,
                json!({ "summary": { "module_cycle_count": 2, "cyclic_module_count": 15, "skip_pair_count": 91 } }),
            ),
            (ToolName::Impact, json!({})),
        ];
        let out = render(
            "audit",
            &sections,
            &[PathBuf::from("/repo/crates/lens")],
            Path::new("/repo"),
        );
        assert!(out.contains("No file-level findings."), "got: {out}");
        assert!(
            out.contains(
                "- layers: 2 module cycles (15 modules involved); 91 skip-level pairs — `agent-lens analyze layers crates/lens --format md`",
            ),
            "got: {out}",
        );
        assert!(
            out.contains(
                "- impact: not folded into the digest — see `agent-lens run audit --format md`"
            ),
            "got: {out}",
        );
        assert!(
            out.contains("\nNothing to report from: wrapper.\n"),
            "got: {out}"
        );
    }

    /// A profile targeting the repository root must still print a
    /// typeable drill-down argument.
    #[test]
    fn render_spells_the_cwd_target_as_a_dot() {
        let sections = vec![(
            ToolName::CoChange,
            json!({ "pairs": [ { "a": "a.rs", "b": "b.rs", "cochanges": 3 } ] }),
        )];
        let out = render(
            "history",
            &sections,
            &[PathBuf::from("/repo")],
            Path::new("/repo"),
        );
        assert!(
            out.contains("`agent-lens analyze co-change . --format md`"),
            "got: {out}"
        );
    }

    #[test]
    fn render_caps_per_tool_contributions_and_entity_rows() {
        let many: Vec<(String, u64)> = (0..(ENTITY_ROW_CAP + PER_TOOL_FILE_CAP))
            .map(|i| (format!("f{i:03}.rs"), 100 - i as u64))
            .collect();
        let refs: Vec<(&str, u64)> = many.iter().map(|(f, c)| (f.as_str(), *c)).collect();
        let sections = vec![(ToolName::Complexity, complexity_report(&refs))];
        let out = render(
            "audit",
            &sections,
            &[PathBuf::from("/repo/src")],
            Path::new("/repo"),
        );
        // One tool contributes at most PER_TOOL_FILE_CAP rows, so the
        // entity cap is not the binding one here.
        assert_eq!(
            out.matches("\n- src/f").count(),
            PER_TOOL_FILE_CAP,
            "got: {out}"
        );
        assert!(!out.contains("below the cut"), "got: {out}");
    }

    #[test]
    fn render_folds_a_long_row_into_a_fragment_count() {
        let file_report = |file: &str| complexity_report(&[(file, 30)]);
        let sections = vec![
            (ToolName::Complexity, file_report("dense.rs")),
            (
                ToolName::Cohesion,
                json!({ "files": [ { "file": "dense.rs", "units": [ { "label": "m", "lcom4": 4 } ] } ] }),
            ),
            (
                ToolName::Wrapper,
                json!({ "files": [ { "file": "dense.rs", "wrappers": [ { "name": "w" } ] } ] }),
            ),
            (
                ToolName::Similarity,
                json!({ "clusters": [ { "units": [ { "file": "dense.rs" } ] } ] }),
            ),
            (
                ToolName::Hotspot,
                json!({
                    "repo_root": "/repo",
                    "files": [ { "path": "src/dense.rs", "score": 9, "commits": 3, "cognitive_max": 3 } ],
                }),
            ),
        ];
        let out = render(
            "audit",
            &sections,
            &[PathBuf::from("/repo/src")],
            Path::new("/repo"),
        );
        assert!(out.contains(", +1 more\n"), "got: {out}");
    }

    #[rstest]
    #[case("/repo/src/lib.rs", "/repo", "src/lib.rs")]
    #[case("/repo", "/repo", ".")]
    #[case("/elsewhere/x.rs", "/repo", "/elsewhere/x.rs")]
    fn display_path_is_relative_when_possible(
        #[case] path: &str,
        #[case] cwd: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(display_path(Path::new(path), Path::new(cwd)), expected);
    }

    #[rstest]
    #[case(0, "0 files")]
    #[case(1, "1 file")]
    #[case(2, "2 files")]
    fn counted_agrees_with_its_noun(#[case] n: u64, #[case] expected: &str) {
        assert_eq!(counted(n, "file", "files"), expected);
    }
}
