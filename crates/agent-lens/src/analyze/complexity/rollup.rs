//! File- and directory-level rollups of per-function complexity.
//!
//! The per-function figures answer "which function is hardest to read",
//! but they are easy to game: splitting one complex function into three
//! lowers `cognitive_max` without removing any branch. Summing over a
//! file or a directory keeps that work visible — the branches moved, the
//! total did not — and ranks the places where complexity concentrates
//! even when no single function stands out.
//!
//! Sums count each branch once. Adapters that report a nested function
//! as its own `<parent>::<name>` unit (TypeScript closures, for one)
//! have already folded its score into the parent, so a unit nested in
//! another unit of the same file is left out of the sums.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use lens_domain::FunctionComplexity;
use serde::Serialize;

use super::FileReport;

/// Aggregate figures for one file or one directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Rollup {
    pub path: String,
    pub function_count: usize,
    pub cognitive_sum: u32,
    pub cognitive_max: u32,
    pub cyclomatic_sum: u32,
}

impl Rollup {
    fn empty(path: String) -> Self {
        Self {
            path,
            function_count: 0,
            cognitive_sum: 0,
            cognitive_max: 0,
            cyclomatic_sum: 0,
        }
    }

    fn absorb(&mut self, other: &Self) {
        self.function_count += other.function_count;
        self.cognitive_sum += other.cognitive_sum;
        self.cognitive_max = self.cognitive_max.max(other.cognitive_max);
        self.cyclomatic_sum += other.cyclomatic_sum;
    }
}

/// One rollup per file, ranked by [`rank`].
pub(super) fn by_file(files: &[FileReport]) -> Vec<Rollup> {
    let mut rows: Vec<Rollup> = files
        .iter()
        .map(|f| file_rollup(&f.file, &f.functions))
        .collect();
    rank(&mut rows);
    rows
}

/// One rollup per directory, over the files directly inside it (not its
/// subdirectories, so the rows partition the corpus), ranked by [`rank`].
pub(super) fn by_directory(file_rollups: &[Rollup]) -> Vec<Rollup> {
    let mut dirs: BTreeMap<String, Rollup> = BTreeMap::new();
    for row in file_rollups {
        let dir = directory_of(&row.path);
        dirs.entry(dir.clone())
            .or_insert_with(|| Rollup::empty(dir))
            .absorb(row);
    }
    let mut rows: Vec<Rollup> = dirs.into_values().collect();
    rank(&mut rows);
    rows
}

/// Heaviest first: cognitive sum, then cyclomatic sum, then path.
fn rank(rows: &mut [Rollup]) {
    rows.sort_by(|a, b| {
        b.cognitive_sum
            .cmp(&a.cognitive_sum)
            .then_with(|| b.cyclomatic_sum.cmp(&a.cyclomatic_sum))
            .then_with(|| a.path.cmp(&b.path))
    });
}

fn directory_of(file: &str) -> String {
    match Path::new(file).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_string_lossy().into_owned(),
        _ => ".".to_owned(),
    }
}

fn file_rollup(path: &str, functions: &[FunctionComplexity]) -> Rollup {
    let mut row = Rollup::empty(path.to_owned());
    row.function_count = functions.len();
    row.cognitive_max = functions.iter().map(|f| f.cognitive).max().unwrap_or(0);
    for f in outermost(functions) {
        row.cognitive_sum += f.cognitive;
        row.cyclomatic_sum += f.cyclomatic;
    }
    row
}

/// Units not nested in another unit of the same file. A unit is nested
/// when some `::`-prefix of its name names another unit whose line span
/// covers it — the shape adapters give a function reported both on its
/// own and as part of its parent.
fn outermost(functions: &[FunctionComplexity]) -> impl Iterator<Item = &FunctionComplexity> {
    let mut spans: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
    for f in functions {
        spans
            .entry(f.name.as_str())
            .or_default()
            .push((f.start_line, f.end_line));
    }
    functions.iter().filter(move |f| {
        !f.name.match_indices("::").any(|(at, _)| {
            spans.get(&f.name[..at]).is_some_and(|ranges| {
                ranges
                    .iter()
                    .any(|&(start, end)| start <= f.start_line && f.end_line <= end)
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn unit(
        name: &str,
        lines: (usize, usize),
        cognitive: u32,
        cyclomatic: u32,
    ) -> FunctionComplexity {
        FunctionComplexity {
            name: name.to_owned(),
            start_line: lines.0,
            end_line: lines.1,
            cyclomatic,
            cognitive,
            max_nesting: 0,
            halstead: Default::default(),
            is_test: false,
        }
    }

    fn file(path: &str, functions: Vec<FunctionComplexity>) -> FileReport {
        FileReport {
            file: path.to_owned(),
            functions,
        }
    }

    #[rstest]
    #[case::closure_inside_parent(
        vec![unit("outer", (1, 10), 4, 3), unit("outer::closure#1", (2, 4), 1, 2)],
        &["outer"],
    )]
    #[case::method_of_a_non_unit_type(
        vec![unit("S::a", (1, 3), 1, 1), unit("S::b", (4, 6), 1, 1)],
        &["S::a", "S::b"],
    )]
    #[case::prefix_name_outside_the_span(
        vec![unit("f", (1, 3), 1, 1), unit("f::g", (5, 9), 2, 2)],
        &["f", "f::g"],
    )]
    #[case::nested_two_deep(
        vec![
            unit("a", (1, 20), 5, 4),
            unit("a::closure#1", (2, 10), 2, 2),
            unit("a::closure#1::closure#1", (3, 5), 1, 2),
        ],
        &["a"],
    )]
    fn outermost_drops_units_folded_into_a_parent(
        #[case] functions: Vec<FunctionComplexity>,
        #[case] expected: &[&str],
    ) {
        let names: Vec<&str> = outermost(&functions).map(|f| f.name.as_str()).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn file_rollup_sums_outermost_units_and_maxes_all() {
        let row = file_rollup(
            "src/a.ts",
            &[
                unit("outer", (1, 10), 4, 3),
                unit("outer::closure#1", (2, 4), 1, 2),
                unit("other", (12, 20), 6, 5),
            ],
        );
        assert_eq!(
            row,
            Rollup {
                path: "src/a.ts".to_owned(),
                function_count: 3,
                cognitive_sum: 10,
                cognitive_max: 6,
                cyclomatic_sum: 8,
            }
        );
    }

    #[test]
    fn splitting_a_function_keeps_the_file_sum() {
        // One function with every branch, versus the same branches spread
        // over three helpers: the max drops, the sum does not.
        let whole = file_rollup("a.rs", &[unit("f", (1, 30), 9, 7)]);
        let split = file_rollup(
            "a.rs",
            &[
                unit("f", (1, 5), 3, 3),
                unit("g", (6, 15), 3, 3),
                unit("h", (16, 30), 3, 3),
            ],
        );
        assert!(split.cognitive_max < whole.cognitive_max);
        assert_eq!(split.cognitive_sum, whole.cognitive_sum);
    }

    #[test]
    fn by_directory_partitions_files_by_parent_and_ranks_heaviest_first() {
        let files = [
            file("top.rs", vec![unit("t", (1, 2), 1, 1)]),
            file("src/a.rs", vec![unit("a", (1, 2), 2, 2)]),
            file("src/b.rs", vec![unit("b", (1, 2), 3, 1)]),
            file("src/deep/c.rs", vec![unit("c", (1, 2), 4, 4)]),
        ];
        let by_file = by_file(&files);
        assert_eq!(
            by_file.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            ["src/deep/c.rs", "src/b.rs", "src/a.rs", "top.rs"]
        );
        let dirs = by_directory(&by_file);
        let summary: Vec<(&str, usize, u32, u32)> = dirs
            .iter()
            .map(|r| {
                (
                    r.path.as_str(),
                    r.function_count,
                    r.cognitive_sum,
                    r.cognitive_max,
                )
            })
            .collect();
        assert_eq!(
            summary,
            [("src", 2, 5, 3), ("src/deep", 1, 4, 4), (".", 1, 1, 1)]
        );
    }

    #[rstest]
    #[case::tie_on_cognitive_breaks_on_cyclomatic(("b", 3, 5), ("a", 3, 2), ["b", "a"])]
    #[case::full_tie_breaks_on_path(("b", 3, 2), ("a", 3, 2), ["a", "b"])]
    fn rank_orders_ties_deterministically(
        #[case] x: (&str, u32, u32),
        #[case] y: (&str, u32, u32),
        #[case] expected: [&str; 2],
    ) {
        let row = |(path, cog, cc): (&str, u32, u32)| Rollup {
            cognitive_sum: cog,
            cyclomatic_sum: cc,
            ..Rollup::empty(path.to_owned())
        };
        let mut rows = vec![row(x), row(y)];
        rank(&mut rows);
        assert_eq!(
            rows.iter().map(|r| r.path.as_str()).collect::<Vec<_>>(),
            expected
        );
    }
}
