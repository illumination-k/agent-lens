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

pub fn changed_line_ranges(path: &Path) -> Vec<LineRange> {
    let (cwd, path_arg) = diff_invocation(path);
    let output = Command::new("git")
        .args(["diff", "--no-ext-diff", "--unified=0", "--"])
        .arg(path_arg)
        .current_dir(cwd)
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
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

        let ranges = changed_line_ranges(&file);
        assert!(
            ranges.iter().any(|r| r.overlaps(1, 1)),
            "expected changed range to include line 1, got {ranges:?}",
        );
    }
}
