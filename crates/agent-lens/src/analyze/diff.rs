use std::path::Path;
use std::process::Command;

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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
pub fn changed_line_ranges(path: &Path, scope: &DiffScope) -> Vec<LineRange> {
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
    let mut out = Vec::new();
    for line in diff.lines() {
        let Some(rest) = line.strip_prefix("@@") else {
            continue;
        };
        let Some(header) = rest.split("@@").next() else {
            continue;
        };
        let Some(plus) = header.split_whitespace().find(|part| part.starts_with('+')) else {
            continue;
        };
        let coords = plus.trim_start_matches('+');
        let mut parts = coords.split(',');
        let Some(start) = parts.next().and_then(|x| x.parse::<usize>().ok()) else {
            continue;
        };
        let count = parts
            .next()
            .and_then(|x| x.parse::<usize>().ok())
            .unwrap_or(1);
        if count == 0 {
            continue;
        }
        out.push(LineRange {
            start,
            end: start.saturating_add(count.saturating_sub(1)),
        });
    }
    out
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
