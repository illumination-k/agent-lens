use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::index::AnalysisIndex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        DiffScope::Disabled => return HashMap::new(),
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
        DiffScope::Disabled => return Vec::new(),
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
    let target = target.trim_end();
    let unquoted = if target.starts_with('"') {
        unquote_c_style(target)?
    } else {
        target.to_owned()
    };
    if unquoted == "/dev/null" {
        return None;
    }
    unquoted.strip_prefix("b/").map(str::to_owned)
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
}
