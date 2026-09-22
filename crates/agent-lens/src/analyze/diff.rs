use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use super::index::AnalysisIndex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineRange {
    pub start: usize,
    pub end: usize,
}

impl LineRange {
    pub fn overlaps(self, start: usize, end: usize) -> bool {
        self.start <= end && start <= self.end
    }
}

pub(crate) fn overlaps_any(start: usize, end: usize, ranges: &[LineRange]) -> bool {
    ranges.iter().any(|r| r.overlaps(start, end))
}

/// Which diff the `--diff-only` / `--diff-range` gate reads.
///
/// The two flags answer the same question — "which lines count as
/// changed?" — and differ only in the diff they ask. Modelling them as
/// one value rather than a `bool` plus an `Option<String>` keeps an
/// analyzer from holding a pair that contradicts itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub enum DiffScope {
    /// No gate: every unit is reported.
    #[default]
    Disabled,
    /// Unstaged working-tree changes, as `git diff -U0`.
    WorkingTree,
    /// A git revision range, as `git diff -U0 <range>`. Held verbatim
    /// and handed to git unparsed, so every spelling git accepts
    /// (`HEAD~1..HEAD`, `main...topic`, a bare commit) works here.
    Range(String),
    /// Lines a caller already knows changed, with no git involved. The
    /// session checkpoint compares against a snapshot it took itself,
    /// not against any commit, and hands the analyzers the result
    /// through the same gate the git scopes use.
    Lines(ChangedLines),
}

/// An explicit changed-lines map for [`DiffScope::Lines`], keyed by
/// canonical absolute path so a lookup agrees with however the walk
/// spelled the file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ChangedLines(Arc<BTreeMap<PathBuf, Vec<LineRange>>>);

impl ChangedLines {
    /// Build the map. A path that cannot be canonicalized (it no longer
    /// exists) is kept as given: it can then only match a lookup that
    /// spells it the same way, which is the most a missing file allows.
    pub fn new(entries: impl IntoIterator<Item = (PathBuf, Vec<LineRange>)>) -> Self {
        let mut map: BTreeMap<PathBuf, Vec<LineRange>> = BTreeMap::new();
        for (path, ranges) in entries {
            let key = path.canonicalize().unwrap_or(path);
            map.entry(key).or_default().extend(ranges);
        }
        Self(Arc::new(map))
    }

    fn ranges_for(&self, path: &Path) -> Vec<LineRange> {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.0.get(&key).cloned().unwrap_or_default()
    }
}

impl DiffScope {
    /// Fold the two parsed flags into one scope. A range wins over
    /// `diff_only`, which cannot happen through the CLI or a config
    /// file — both reject the combination before this runs — but keeps
    /// the fold total for direct API callers.
    pub fn new(diff_only: bool, diff_range: Option<String>) -> Self {
        match (diff_only, diff_range) {
            (_, Some(range)) => Self::Range(range),
            (true, None) => Self::WorkingTree,
            (false, None) => Self::Disabled,
        }
    }

    /// Whether any gate applies. `false` is the common case and lets
    /// callers skip the git invocation entirely.
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }
}

/// Reject a `--diff-range` value that git would read as an option
/// rather than a revision range.
///
/// The value reaches `git diff` as its own argv entry, so there is no
/// shell to quote against, but git still parses a leading `-` as a
/// flag — `--output=…` passed as a "range" would write a file. Anything
/// else is git's to judge: this refuses the shapes that change what the
/// command *is*, not the ones that merely fail to resolve.
pub fn validate_diff_range(range: &str) -> Result<(), String> {
    if range.trim().is_empty() {
        return Err("must name a git revision range, e.g. `HEAD~1..HEAD`".to_owned());
    }
    if range.starts_with('-') {
        return Err(format!(
            "`{range}` starts with `-`, which git reads as an option rather than a revision range",
        ));
    }
    Ok(())
}

/// clap `value_parser` for `--diff-range`, so an option-shaped range is
/// rejected at parse time with the flag named in the error.
pub fn parse_diff_range(range: &str) -> Result<String, String> {
    validate_diff_range(range)?;
    Ok(range.to_owned())
}

/// Changed line ranges for `path` under `scope`.
///
/// Returns empty for [`DiffScope::Disabled`], and for a git invocation
/// that fails. A failure is worth a `warn`: an unresolvable range would
/// otherwise read as "this commit changed nothing", which is exactly
/// what a caller batching over history must not silently believe.
///
/// Under an active [`AnalysisIndex`] the answer comes from one
/// `git diff` over the file's whole repository, memoized per
/// `(repository, scope)` — the per-file `git diff` this function
/// otherwise spawns is a process per file *per analyzer*, which is
/// where a diff-gated profile run used to spend most of its time.
pub fn changed_line_ranges(path: &Path, scope: &DiffScope) -> Vec<LineRange> {
    if !scope.is_enabled() {
        return Vec::new();
    }
    if let DiffScope::Lines(lines) = scope {
        return lines.ranges_for(path);
    }
    if let Some(index) = AnalysisIndex::active()
        && let Some(ranges) = indexed_changed_line_ranges(&index, path, scope)
    {
        return ranges;
    }
    per_file_changed_line_ranges(path, scope)
}

/// The batch path: resolve the file's repository root (memoized per
/// directory), diff the whole repository once (memoized per root and
/// scope), and look the file up. `None` falls back to the per-file
/// invocation — the file has no resolvable canonical path or no
/// enclosing repository, and the per-file path owns the warn for that.
fn indexed_changed_line_ranges(
    index: &AnalysisIndex,
    path: &Path,
    scope: &DiffScope,
) -> Option<Vec<LineRange>> {
    let abs = path.canonicalize().ok()?;
    let dir = if abs.is_dir() {
        abs.as_path()
    } else {
        abs.parent()?
    };
    let root = index.repo_root(dir.to_path_buf(), || repo_root_for(dir));
    let root = root.as_ref().clone()?;
    let map = index.repo_changed_ranges((root.clone(), scope.clone()), || {
        diff_repository(&root, scope)
    });
    Some(map.get(&abs).cloned().unwrap_or_default())
}

/// The enclosing working-tree root of `dir`, or `None` outside any
/// repository. Canonicalized so lookups against canonicalized file
/// paths agree on one spelling.
fn repo_root_for(dir: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    PathBuf::from(root.trim_end()).canonicalize().ok()
}

/// One `git diff -U0` over the whole repository, split per file and
/// keyed by canonical absolute path. Failures warn and come back empty
/// for the same reason the per-file invocation's do.
fn diff_repository(root: &Path, scope: &DiffScope) -> HashMap<PathBuf, Vec<LineRange>> {
    let range = match scope {
        DiffScope::Disabled | DiffScope::Lines(_) => return HashMap::new(),
        DiffScope::WorkingTree => None,
        DiffScope::Range(range) => Some(range.as_str()),
    };
    let mut cmd = Command::new("git");
    // The prefixes are forced because the parser keys files off
    // `+++ b/…`: a user's `diff.noprefix` / `diff.mnemonicPrefix`
    // config would otherwise change the header shape and make every
    // file read as unchanged. `--no-color` guards the hunk headers
    // against `color.ui=always` the same way.
    cmd.args([
        "diff",
        "--no-ext-diff",
        "--no-color",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--unified=0",
    ]);
    if let Some(range) = range {
        cmd.arg(range);
    }
    cmd.current_dir(root);
    let output = match cmd.output() {
        Ok(output) => output,
        Err(source) => {
            tracing::warn!(root = %root.display(), %source, "could not run `git diff`");
            return HashMap::new();
        }
    };
    if !output.status.success() {
        tracing::warn!(
            root = %root.display(),
            range = range.unwrap_or("<working tree>"),
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "`git diff` failed; treating the repository as unchanged",
        );
        return HashMap::new();
    }
    let Ok(stdout) = String::from_utf8(output.stdout) else {
        return HashMap::new();
    };
    parse_unified_zero_by_file(&stdout)
        .into_iter()
        .map(|(rel, ranges)| {
            let joined = root.join(rel);
            (joined.canonicalize().unwrap_or(joined), ranges)
        })
        .collect()
}

fn per_file_changed_line_ranges(path: &Path, scope: &DiffScope) -> Vec<LineRange> {
    let range = match scope {
        DiffScope::Disabled | DiffScope::Lines(_) => return Vec::new(),
        DiffScope::WorkingTree => None,
        DiffScope::Range(range) => Some(range.as_str()),
    };
    let (cwd, path_arg) = diff_invocation(path);
    let mut cmd = Command::new("git");
    cmd.args(["diff", "--no-ext-diff", "--unified=0"]);
    if let Some(range) = range {
        cmd.arg(range);
    }
    cmd.arg("--").arg(path_arg).current_dir(cwd);
    let output = match cmd.output() {
        Ok(output) => output,
        Err(source) => {
            tracing::warn!(path = %path.display(), %source, "could not run `git diff`");
            return Vec::new();
        }
    };
    if !output.status.success() {
        tracing::warn!(
            path = %path.display(),
            range = range.unwrap_or("<working tree>"),
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "`git diff` failed; treating the file as unchanged",
        );
        return Vec::new();
    }
    let Ok(stdout) = String::from_utf8(output.stdout) else {
        return Vec::new();
    };
    parse_unified_zero_hunks(&stdout)
}

fn diff_invocation(path: &Path) -> (&Path, &Path) {
    if path.is_absolute() {
        let cwd = path.parent().unwrap_or(path);
        let arg = path.file_name().map_or(path, Path::new);
        (cwd, arg)
    } else {
        (Path::new("."), path)
    }
}

fn parse_unified_zero_hunks(diff: &str) -> Vec<LineRange> {
    diff.lines().filter_map(parse_hunk_header).collect()
}

/// The post-image range of one `@@ -a,b +c,d @@` hunk header, or `None`
/// for any other line (and for pure deletions, whose post-image count
/// is zero — no surviving line changed).
fn parse_hunk_header(line: &str) -> Option<LineRange> {
    let header = line.strip_prefix("@@")?.split("@@").next()?;
    let plus = header
        .split_whitespace()
        .find(|part| part.starts_with('+'))?;
    let coords = plus.trim_start_matches('+');
    let mut parts = coords.split(',');
    let start = parts.next().and_then(|x| x.parse::<usize>().ok())?;
    let count = parts
        .next()
        .and_then(|x| x.parse::<usize>().ok())
        .unwrap_or(1);
    if count == 0 {
        return None;
    }
    Some(LineRange {
        start,
        end: start.saturating_add(count.saturating_sub(1)),
    })
}

/// Split a whole-repository unified diff into per-file post-image
/// ranges, keyed by the repository-relative path each `+++ b/…` header
/// names. Deleted files (`+++ /dev/null`) contribute nothing, and a
/// renamed file is keyed by its new name — the name the analyzers see
/// on disk.
fn parse_unified_zero_by_file(diff: &str) -> HashMap<String, Vec<LineRange>> {
    let mut out: HashMap<String, Vec<LineRange>> = HashMap::new();
    let mut current: Option<String> = None;
    for line in diff.lines() {
        if let Some(target) = line.strip_prefix("+++ ") {
            current = diff_target_path(target);
            continue;
        }
        if let (Some(range), Some(file)) = (parse_hunk_header(line), &current) {
            out.entry(file.clone()).or_default().push(range);
        }
    }
    out
}

/// The repository-relative path a `+++ ` target names, with git's
/// C-style quoting undone; `None` for `/dev/null` or an unparseable
/// spelling (that file then reads as unchanged, the same degraded
/// answer a failed per-file diff gives).
fn diff_target_path(target: &str) -> Option<String> {
    diff_side_path(target, "b/")
}

/// Undo git's `core.quotePath` C-style quoting: surrounding quotes,
/// backslash escapes, and octal byte escapes for non-ASCII names.
fn unquote_c_style(quoted: &str) -> Option<String> {
    let inner = quoted.strip_prefix('"')?.strip_suffix('"')?;
    let mut bytes = Vec::with_capacity(inner.len());
    let mut chars = inner.bytes().peekable();
    while let Some(b) = chars.next() {
        if b != b'\\' {
            bytes.push(b);
            continue;
        }
        match chars.next()? {
            b'\\' => bytes.push(b'\\'),
            b'"' => bytes.push(b'"'),
            b'n' => bytes.push(b'\n'),
            b't' => bytes.push(b'\t'),
            b'r' => bytes.push(b'\r'),
            first @ b'0'..=b'7' => {
                let mut value = u32::from(first - b'0');
                while let Some(&digit @ b'0'..=b'7') = chars.peek() {
                    value = value * 8 + u32::from(digit - b'0');
                    chars.next();
                }
                bytes.push(u8::try_from(value).ok()?);
            }
            _ => return None,
        }
    }
    String::from_utf8(bytes).ok()
}

/// One file's side of a two-sided `git diff -U0`: which lines left, which
/// arrived, and under which names. What `analyze footprint` reads, where
/// the post-image ranges the other analyzers gate on are only half the
/// question — a deleted function has no post-image line to overlap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FileDiff {
    /// Repository-relative pre-image path; `None` for an added file.
    pub(crate) old_path: Option<String>,
    /// Repository-relative post-image path; `None` for a deleted file.
    pub(crate) new_path: Option<String>,
    /// Pre-image line ranges the diff removed or rewrote.
    pub(crate) removed: Vec<LineRange>,
    /// Post-image line ranges the diff added or rewrote.
    pub(crate) added: Vec<LineRange>,
    pub(crate) added_lines: usize,
    pub(crate) deleted_lines: usize,
}

/// Run `git diff -U0 -M` for `scope` under `root`, limited to
/// `pathspecs` (empty means the whole tree), and split it per file with
/// both sides of every hunk. Unlike [`changed_line_ranges`], a git
/// failure is an error: the caller's whole report is this diff, and an
/// empty one would read as "nothing changed".
pub(crate) fn diff_files(
    root: &Path,
    scope: &DiffScope,
    pathspecs: &[String],
) -> Result<Vec<FileDiff>, String> {
    let range = match scope {
        DiffScope::Disabled | DiffScope::Lines(_) => return Ok(Vec::new()),
        DiffScope::WorkingTree => None,
        DiffScope::Range(range) => Some(range.as_str()),
    };
    let mut cmd = Command::new("git");
    // Prefixes and colour are forced for the same reason
    // `diff_repository` forces them: the parser keys off `a/` / `b/`.
    cmd.args([
        "diff",
        "--no-ext-diff",
        "--no-color",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--unified=0",
        "-M",
    ]);
    if let Some(range) = range {
        cmd.arg(range);
    }
    cmd.arg("--");
    if pathspecs.is_empty() {
        cmd.arg(".");
    } else {
        cmd.args(pathspecs);
    }
    cmd.current_dir(root);
    let output = cmd
        .output()
        .map_err(|source| format!("could not run `git diff`: {source}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(parse_file_diffs(&String::from_utf8_lossy(&output.stdout)))
}

/// Untracked, non-ignored files under `pathspecs`, as whole-file
/// additions. `git diff` never shows them, so a working-tree footprint
/// that stopped at the diff would miss every file the edit created —
/// which is where an over-edit's scaffolding usually lands. A file that
/// is not UTF-8 text is skipped.
pub(crate) fn untracked_file_diffs(
    root: &Path,
    pathspecs: &[String],
) -> Result<Vec<FileDiff>, String> {
    let mut cmd = Command::new("git");
    cmd.args(["ls-files", "--others", "--exclude-standard", "-z", "--"]);
    if pathspecs.is_empty() {
        cmd.arg(".");
    } else {
        cmd.args(pathspecs);
    }
    cmd.current_dir(root);
    let output = cmd
        .output()
        .map_err(|source| format!("could not run `git ls-files`: {source}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    Ok(listing
        .split('\0')
        .filter(|path| !path.is_empty())
        .filter_map(|path| {
            let text = std::fs::read_to_string(root.join(path)).ok()?;
            let lines = text.lines().count();
            Some(FileDiff {
                old_path: None,
                new_path: Some(path.to_owned()),
                removed: Vec::new(),
                added: side_range((1, lines)).into_iter().collect(),
                added_lines: lines,
                deleted_lines: 0,
            })
        })
        .collect())
}

/// Split a `-U0` diff into [`FileDiff`]s. A file with no hunk (a pure
/// rename, a mode change, a binary file) still appears, with empty
/// ranges and zero counts.
fn parse_file_diffs(diff: &str) -> Vec<FileDiff> {
    let mut out: Vec<FileDiff> = Vec::new();
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            out.push(FileDiff::default());
        } else if let Some(current) = out.last_mut() {
            fold_diff_line(current, line);
        }
    }
    out
}

/// The per-file header lines [`fold_diff_line`] reads a path from.
const PATH_HEADERS: [&str; 4] = ["--- ", "+++ ", "rename from ", "rename to "];

/// Fold one line of a file's section into `file`: a header naming
/// either side, or a hunk header. Body lines are skipped — under `-U0`
/// the hunk header already carries every count.
fn fold_diff_line(file: &mut FileDiff, line: &str) {
    if let Some((old, new)) = parse_hunk_sides(line) {
        file.deleted_lines += old.1;
        file.added_lines += new.1;
        file.removed.extend(side_range(old));
        file.added.extend(side_range(new));
        return;
    }
    let Some((header, rest)) = PATH_HEADERS
        .into_iter()
        .find_map(|header| Some((header, line.strip_prefix(header)?)))
    else {
        return;
    };
    match header {
        "--- " => file.old_path = diff_side_path(rest, "a/"),
        "+++ " => file.new_path = diff_side_path(rest, "b/"),
        "rename from " => file.old_path = unquote_path(rest),
        _ => file.new_path = unquote_path(rest),
    }
}

/// `(start, count)` for both sides of a `@@ -a,b +c,d @@` header.
fn parse_hunk_sides(line: &str) -> Option<((usize, usize), (usize, usize))> {
    let header = line.strip_prefix("@@")?.split("@@").next()?;
    let mut parts = header.split_whitespace();
    let old = parse_side(parts.next()?.strip_prefix('-')?)?;
    let new = parse_side(parts.next()?.strip_prefix('+')?)?;
    Some((old, new))
}

fn parse_side(coords: &str) -> Option<(usize, usize)> {
    let mut parts = coords.split(',');
    let start = parts.next()?.parse::<usize>().ok()?;
    let count = match parts.next() {
        Some(count) => count.parse::<usize>().ok()?,
        None => 1,
    };
    Some((start, count))
}

fn side_range((start, count): (usize, usize)) -> Option<LineRange> {
    (count > 0).then(|| LineRange {
        start,
        end: start + count - 1,
    })
}

/// The repository-relative path one side of a file header names:
/// `prefix` is `a/` for `--- ` and `b/` for `+++ `.
fn diff_side_path(target: &str, prefix: &str) -> Option<String> {
    let unquoted = unquote_path(target)?;
    if unquoted == "/dev/null" {
        return None;
    }
    unquoted.strip_prefix(prefix).map(str::to_owned)
}

/// A header path with git's C-style quoting undone, if it was quoted.
fn unquote_path(target: &str) -> Option<String> {
    let target = target.trim_end();
    if target.starts_with('"') {
        unquote_c_style(target)
    } else {
        Some(target.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::run_git;
    use rstest::rstest;
    use std::io::Write;

    #[test]
    fn parses_unified_zero_hunk_ranges() {
        let diff = "\
@@ -1,0 +3,2 @@
+a
+b
@@ -10 +20 @@
-x
+y
@@ -5,1 +7,0 @@
-gone
";
        let got = parse_unified_zero_hunks(diff);
        assert_eq!(
            got,
            vec![
                LineRange { start: 3, end: 4 },
                LineRange { start: 20, end: 20 },
            ]
        );
    }

    #[test]
    fn parses_a_whole_repository_diff_per_file() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -1,0 +3,2 @@
+a
+b
diff --git a/gone.rs b/gone.rs
--- a/gone.rs
+++ /dev/null
@@ -1,4 +0,0 @@
-x
diff --git \"a/sp ace.rs\" \"b/sp ace.rs\"
--- \"a/sp ace.rs\"
+++ \"b/sp ace.rs\"
@@ -10 +20 @@
-x
+y
diff --git a/old.rs b/new.rs
--- a/old.rs
+++ b/new.rs
@@ -5,1 +7,3 @@
+z
";
        let got = parse_unified_zero_by_file(diff);
        assert_eq!(
            got.get("src/a.rs"),
            Some(&vec![LineRange { start: 3, end: 4 }]),
        );
        assert_eq!(
            got.get("sp ace.rs"),
            Some(&vec![LineRange { start: 20, end: 20 }]),
            "quoted paths are unquoted",
        );
        assert_eq!(
            got.get("new.rs"),
            Some(&vec![LineRange { start: 7, end: 9 }]),
            "a rename is keyed by its new name",
        );
        assert_eq!(
            got.len(),
            3,
            "the deleted file contributes nothing: {got:?}"
        );
    }

    #[rstest]
    #[case::plain("\"a b\"", Some("a b"))]
    #[case::escaped_quote("\"a\\\"b\"", Some("a\"b"))]
    #[case::backslash("\"a\\\\b\"", Some("a\\b"))]
    #[case::tab("\"a\\tb\"", Some("a\tb"))]
    #[case::octal("\"\\303\\251.rs\"", Some("é.rs"))]
    #[case::unterminated("\"a", None)]
    #[case::bad_escape("\"a\\qb\"", None)]
    fn unquotes_c_style_paths(#[case] quoted: &str, #[case] want: Option<&str>) {
        assert_eq!(unquote_c_style(quoted).as_deref(), want);
    }

    /// The batch path answers exactly what the per-file path answers,
    /// for every scope kind, including the file the diff never touched.
    /// This is the equivalence the index-backed fast path stands on.
    #[rstest]
    #[case::working_tree(DiffScope::WorkingTree)]
    #[case::committed_range(DiffScope::Range("HEAD~1..HEAD".to_owned()))]
    fn indexed_batch_diff_matches_the_per_file_diff(#[case] scope: DiffScope) {
        let (dir, file) = repo_with_history_and_unstaged_edit();
        let untouched = dir.path().join("untouched.rs");
        std::fs::write(&untouched, b"fn quiet() {}\n").unwrap();

        let per_file = (
            changed_line_ranges(&file, &scope),
            changed_line_ranges(&untouched, &scope),
        );
        let scope_guard = crate::analyze::AnalysisIndexScope::activate();
        let indexed = (
            changed_line_ranges(&file, &scope),
            changed_line_ranges(&untouched, &scope),
        );
        assert_eq!(per_file, indexed);
        assert!(
            !indexed.0.is_empty(),
            "the fixture's edited file must report ranges under {scope:?}",
        );
        let (hits, _) = scope_guard.index().stats();
        assert!(
            hits > 0,
            "the second lookup reuses the memoized repository diff",
        );
    }

    /// A user's diff-shape config must not change what the batch parser
    /// sees: `diff.noprefix` would drop the `b/` prefix the per-file
    /// keying relies on, and `diff.mnemonicPrefix` would replace it.
    /// The forced `--src-prefix`/`--dst-prefix` flags make the batch
    /// path immune, so the edited file still reports its range.
    #[test]
    fn indexed_batch_diff_survives_noprefix_and_mnemonic_config() {
        let (dir, file) = repo_with_history_and_unstaged_edit();
        run_git(dir.path(), &["config", "diff.noprefix", "true"]);
        run_git(dir.path(), &["config", "diff.mnemonicPrefix", "true"]);

        let _scope = crate::analyze::AnalysisIndexScope::activate();
        let ranges = changed_line_ranges(&file, &DiffScope::WorkingTree);
        assert!(
            ranges.iter().any(|r| r.overlaps(2, 2)),
            "expected the unstaged edit on line 2, got {ranges:?}",
        );
    }

    #[test]
    fn line_range_overlap_is_inclusive() {
        let r = LineRange { start: 10, end: 12 };
        assert!(r.overlaps(12, 20));
        assert!(r.overlaps(1, 10));
        assert!(!r.overlaps(13, 20));
    }

    #[test]
    fn diff_invocation_anchors_absolute_paths_at_parent() {
        let path = Path::new("/tmp/repo/src/lib.rs");
        let (cwd, arg) = diff_invocation(path);
        assert_eq!(cwd, Path::new("/tmp/repo/src"));
        assert_eq!(arg, Path::new("lib.rs"));
    }

    #[test]
    fn changed_line_ranges_resolves_absolute_paths_inside_repo() {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);

        let file = dir.path().join("lib.rs");
        let mut f = std::fs::File::create(&file).unwrap();
        f.write_all(b"fn alpha() {}\nfn beta() {}\n").unwrap();
        run_git(dir.path(), &["add", "lib.rs"]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        let mut f = std::fs::File::create(&file).unwrap();
        f.write_all(b"fn alpha() { let _x = 1; }\nfn beta() {}\n")
            .unwrap();

        let ranges = changed_line_ranges(&file, &DiffScope::WorkingTree);
        assert!(
            ranges.iter().any(|r| r.overlaps(1, 1)),
            "expected changed range to include line 1, got {ranges:?}",
        );
    }

    /// A repo whose committed history edits line 1 and whose *working
    /// tree* edits line 2, so each scope has a line only it can see.
    /// One fixture, three assertions: the scopes cannot be mixed up.
    fn repo_with_history_and_unstaged_edit() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init", "-q", "-b", "main"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Test"]);

        let file = dir.path().join("lib.rs");
        let write = |body: &[u8]| std::fs::write(&file, body).unwrap();

        write(b"fn alpha() {}\nfn beta() {}\n");
        run_git(dir.path(), &["add", "lib.rs"]);
        run_git(dir.path(), &["commit", "-q", "-m", "initial"]);

        // HEAD~1..HEAD touches line 1 only.
        write(b"fn alpha() { let _x = 1; }\nfn beta() {}\n");
        run_git(dir.path(), &["add", "lib.rs"]);
        run_git(dir.path(), &["commit", "-q", "-m", "edit alpha"]);

        // The working tree touches line 2 only.
        write(b"fn alpha() { let _x = 1; }\nfn beta() { let _y = 2; }\n");

        (dir, file)
    }

    #[rstest]
    #[case::committed_range(DiffScope::Range("HEAD~1..HEAD".to_owned()), true, false)]
    #[case::working_tree(DiffScope::WorkingTree, false, true)]
    #[case::disabled(DiffScope::Disabled, false, false)]
    fn scope_selects_which_diff_is_read(
        #[case] scope: DiffScope,
        #[case] sees_line_1: bool,
        #[case] sees_line_2: bool,
    ) {
        let (_dir, file) = repo_with_history_and_unstaged_edit();
        let ranges = changed_line_ranges(&file, &scope);
        assert_eq!(
            ranges.iter().any(|r| r.overlaps(1, 1)),
            sees_line_1,
            "line 1 (committed edit) under {scope:?}, got {ranges:?}",
        );
        assert_eq!(
            ranges.iter().any(|r| r.overlaps(2, 2)),
            sees_line_2,
            "line 2 (unstaged edit) under {scope:?}, got {ranges:?}",
        );
    }

    /// A range git cannot resolve must come back empty rather than
    /// falling back to the working-tree diff — a silent fallback would
    /// attribute unrelated pending edits to the requested revision.
    #[test]
    fn unresolvable_range_yields_no_ranges() {
        let (_dir, file) = repo_with_history_and_unstaged_edit();
        let scope = DiffScope::Range("no-such-ref..HEAD".to_owned());
        assert_eq!(changed_line_ranges(&file, &scope), Vec::new());
    }

    #[rstest]
    #[case::simple_range("HEAD~1..HEAD")]
    #[case::triple_dot("main...topic")]
    #[case::bare_commit("8c6f196")]
    fn validate_diff_range_accepts_revision_ranges(#[case] range: &str) {
        assert!(validate_diff_range(range).is_ok(), "rejected {range}");
    }

    #[rstest]
    #[case::empty("")]
    #[case::blank("   ")]
    #[case::option_like("--output=/tmp/pwned")]
    #[case::short_option("-U9")]
    fn validate_diff_range_rejects_options_and_blanks(#[case] range: &str) {
        assert!(validate_diff_range(range).is_err(), "accepted {range}");
    }

    #[rstest]
    #[case::neither(false, None, DiffScope::Disabled)]
    #[case::diff_only(true, None, DiffScope::WorkingTree)]
    #[case::range(false, Some("HEAD~1..HEAD"), DiffScope::Range("HEAD~1..HEAD".to_owned()))]
    #[case::range_wins(true, Some("HEAD~1..HEAD"), DiffScope::Range("HEAD~1..HEAD".to_owned()))]
    fn new_folds_the_two_flags(
        #[case] diff_only: bool,
        #[case] diff_range: Option<&str>,
        #[case] want: DiffScope,
    ) {
        let got = DiffScope::new(diff_only, diff_range.map(str::to_owned));
        assert_eq!(got, want);
        assert_eq!(got.is_enabled(), want != DiffScope::Disabled);
    }

    #[test]
    fn parses_both_sides_of_every_file() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -3,2 +3,0 @@
-x
-y
@@ -9 +7,3 @@
-z
+a
+b
+c
diff --git a/gone.rs b/gone.rs
--- a/gone.rs
+++ /dev/null
@@ -1,4 +0,0 @@
-x
diff --git a/new.rs b/new.rs
--- /dev/null
+++ b/new.rs
@@ -0,0 +1,2 @@
+x
diff --git a/old.rs b/moved.rs
similarity index 100%
rename from old.rs
rename to moved.rs
";
        let got = parse_file_diffs(diff);
        assert_eq!(got.len(), 4, "got {got:?}");
        assert_eq!(
            got[0],
            FileDiff {
                old_path: Some("src/a.rs".to_owned()),
                new_path: Some("src/a.rs".to_owned()),
                removed: vec![
                    LineRange { start: 3, end: 4 },
                    LineRange { start: 9, end: 9 }
                ],
                added: vec![LineRange { start: 7, end: 9 }],
                added_lines: 3,
                deleted_lines: 3,
            }
        );
        assert_eq!(got[1].new_path, None, "a deletion has no post-image");
        assert_eq!(got[1].deleted_lines, 4);
        assert_eq!(got[2].old_path, None, "an addition has no pre-image");
        assert_eq!(got[2].added, vec![LineRange { start: 1, end: 2 }]);
        assert_eq!(got[3].old_path.as_deref(), Some("old.rs"));
        assert_eq!(got[3].new_path.as_deref(), Some("moved.rs"));
        assert_eq!((got[3].added_lines, got[3].deleted_lines), (0, 0));
    }

    #[test]
    fn diff_files_and_untracked_files_read_the_working_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init", "-q"]);
        crate::test_support::write_file(root, "a.rs", "fn a() {}\nfn b() {}\n");
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-q", "-m", "init"]);
        crate::test_support::write_file(root, "a.rs", "fn a() {}\nfn b() { 1; }\nfn c() {}\n");
        crate::test_support::write_file(root, "fresh.rs", "fn f() {}\n");

        let diffs = diff_files(root, &DiffScope::WorkingTree, &[]).unwrap();
        assert_eq!(diffs.len(), 1, "untracked files are not in `git diff`");
        assert_eq!(diffs[0].new_path.as_deref(), Some("a.rs"));
        assert_eq!((diffs[0].added_lines, diffs[0].deleted_lines), (2, 1));

        let untracked = untracked_file_diffs(root, &[]).unwrap();
        assert_eq!(untracked.len(), 1);
        assert_eq!(untracked[0].new_path.as_deref(), Some("fresh.rs"));
        assert_eq!(untracked[0].added_lines, 1);

        assert!(
            diff_files(root, &DiffScope::Disabled, &[])
                .unwrap()
                .is_empty()
        );
        let err = diff_files(root, &DiffScope::Range("no-such-rev..HEAD".to_owned()), &[]);
        assert!(
            err.is_err(),
            "an unresolvable range is an error, not an empty diff"
        );
    }

    #[test]
    fn explicit_lines_answer_without_git() {
        let dir = tempfile::tempdir().unwrap();
        let file = crate::test_support::write_file(dir.path(), "a.rs", "fn a() {}\n");
        let lines = ChangedLines::new([(file.clone(), vec![LineRange { start: 1, end: 1 }])]);
        let scope = DiffScope::Lines(lines);
        assert!(scope.is_enabled());
        assert_eq!(
            changed_line_ranges(&file, &scope),
            vec![LineRange { start: 1, end: 1 }]
        );
        // A different spelling of the same file still matches.
        let spelled = dir.path().join(".").join("a.rs");
        assert_eq!(changed_line_ranges(&spelled, &scope).len(), 1);
        let other = crate::test_support::write_file(dir.path(), "b.rs", "fn b() {}\n");
        assert!(changed_line_ranges(&other, &scope).is_empty());
    }
}
