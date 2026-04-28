//! Language-agnostic hotspot scoring.
//!
//! A "hotspot" is a file that is both **frequently changed** and
//! **complex** — the place where bugs are most likely to be introduced
//! and where a refactor is most likely to pay off. Following Adam
//! Tornhill's formulation (Code as a Crime Scene), we score each file as
//! `commits × cognitive_max`. Cognitive complexity is preferred over
//! cyclomatic because flat exhaustive matches inflate cyclomatic without
//! actually being hard to read; cognitive penalises nesting and
//! short-circuit chains the way a human reader does.
//!
//! This module owns the **scoring** and the data shapes only. Producing
//! the inputs is left to language-specific adapters (which extract
//! complexity per file) and the CLI (which talks to git).

use std::collections::BTreeMap;

/// Per-file commit count, keyed by a path relative to the repo root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChurn {
    pub path: String,
    pub commits: u32,
}

/// Per-file complexity rollup, keyed by a path relative to the repo root.
#[derive(Debug, Clone, PartialEq)]
pub struct FileComplexity {
    pub path: String,
    pub function_count: usize,
    /// Total LOC across all functions (signature through closing brace).
    pub loc: usize,
    /// Maximum McCabe Cyclomatic Complexity across functions.
    pub cyclomatic_max: u32,
    /// Maximum Sonar Cognitive Complexity across functions.
    pub cognitive_max: u32,
}

impl Eq for FileComplexity {}

/// One row of the hotspot report.
///
/// `commits` and `cognitive_max` are kept as separate fields so the
/// agent can see *why* a file scored high. `score` is
/// `commits × cognitive_max` with saturating arithmetic.
#[derive(Debug, Clone, PartialEq)]
pub struct HotspotEntry {
    pub path: String,
    pub commits: u32,
    pub function_count: usize,
    pub loc: usize,
    pub cyclomatic_max: u32,
    pub cognitive_max: u32,
    pub score: u64,
}

impl Eq for HotspotEntry {}

/// Merge churn and complexity into a single scored list.
///
/// By default the result is scoped to currently analyzed source files
/// plus any positive-score entries. Churn-only paths from git history
/// are dropped because they have no current complexity signal and would
/// otherwise flood JSON reports with zero-score deleted, generated, or
/// non-source files.
///
/// The resulting list is sorted by descending `score` with ties broken
/// by descending `commits`, then lexicographic path (for determinism).
pub fn compute_hotspots(
    churn: Vec<FileChurn>,
    complexity: Vec<FileComplexity>,
) -> Vec<HotspotEntry> {
    let mut churn_by_path: BTreeMap<String, u32> = BTreeMap::new();
    for c in churn {
        // Defensive: the same path appearing twice in churn input means
        // the upstream collector double-counted. Take the larger value
        // rather than overwriting silently — the agent should see the
        // worst case.
        let entry = churn_by_path.entry(c.path).or_insert(0);
        *entry = (*entry).max(c.commits);
    }
    let mut complexity_by_path: BTreeMap<String, FileComplexity> = BTreeMap::new();
    for fc in complexity {
        complexity_by_path.insert(fc.path.clone(), fc);
    }

    let mut paths: BTreeMap<String, ()> = BTreeMap::new();
    for k in churn_by_path.keys() {
        paths.insert(k.clone(), ());
    }
    for k in complexity_by_path.keys() {
        paths.insert(k.clone(), ());
    }

    let mut out = Vec::new();
    for p in paths.into_keys() {
        let commits = churn_by_path.get(&p).copied().unwrap_or(0);
        let fc = complexity_by_path.get(&p);
        let is_current_source = fc.is_some();
        let function_count = fc.map_or(0, |f| f.function_count);
        let loc = fc.map_or(0, |f| f.loc);
        let cyclomatic_max = fc.map_or(0, |f| f.cyclomatic_max);
        let cognitive_max = fc.map_or(0, |f| f.cognitive_max);
        let score = u64::from(commits).saturating_mul(u64::from(cognitive_max));
        if score == 0 && !is_current_source {
            continue;
        }
        out.push(HotspotEntry {
            path: p,
            commits,
            function_count,
            loc,
            cyclomatic_max,
            cognitive_max,
            score,
        });
    }

    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| b.commits.cmp(&a.commits))
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn churn(path: &str, commits: u32) -> FileChurn {
        FileChurn {
            path: path.to_owned(),
            commits,
        }
    }

    fn complexity(path: &str, cog: u32, cc: u32) -> FileComplexity {
        FileComplexity {
            path: path.to_owned(),
            function_count: 1,
            loc: 10,
            cyclomatic_max: cc,
            cognitive_max: cog,
        }
    }

    #[test]
    fn score_is_commits_times_cognitive_max() {
        let entries = compute_hotspots(
            vec![churn("a.rs", 5), churn("b.rs", 2)],
            vec![complexity("a.rs", 3, 7), complexity("b.rs", 10, 4)],
        );
        let a = entries.iter().find(|e| e.path == "a.rs").unwrap();
        let b = entries.iter().find(|e| e.path == "b.rs").unwrap();
        assert_eq!(a.score, 15); // 5 * 3
        assert_eq!(b.score, 20); // 2 * 10
    }

    #[test]
    fn entries_are_sorted_by_score_descending() {
        let entries = compute_hotspots(
            vec![churn("low.rs", 1), churn("high.rs", 10)],
            vec![complexity("low.rs", 1, 1), complexity("high.rs", 5, 5)],
        );
        assert_eq!(entries[0].path, "high.rs");
        assert_eq!(entries[1].path, "low.rs");
    }

    #[test]
    fn files_with_only_churn_are_dropped_by_default() {
        let entries = compute_hotspots(vec![churn("config.toml", 50)], vec![]);
        assert!(entries.is_empty());
    }

    #[test]
    fn files_with_only_complexity_are_kept_with_zero_score() {
        let entries = compute_hotspots(vec![], vec![complexity("new.rs", 12, 8)]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].commits, 0);
        assert_eq!(entries[0].cognitive_max, 12);
        assert_eq!(entries[0].score, 0);
    }

    #[test]
    fn duplicate_churn_entries_keep_the_larger_count() {
        let entries = compute_hotspots(
            vec![churn("a.rs", 3), churn("a.rs", 7)],
            vec![complexity("a.rs", 1, 1)],
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].commits, 7);
    }

    #[test]
    fn ties_break_by_commits_then_path() {
        // Both score 6 (3*2 and 2*3) but commits differ.
        let entries = compute_hotspots(
            vec![churn("a.rs", 3), churn("b.rs", 2)],
            vec![complexity("a.rs", 2, 1), complexity("b.rs", 3, 1)],
        );
        assert_eq!(entries[0].path, "a.rs");
        assert_eq!(entries[1].path, "b.rs");
    }

    #[test]
    fn empty_input_produces_empty_output() {
        assert!(compute_hotspots(vec![], vec![]).is_empty());
    }

    #[test]
    fn saturating_score_does_not_overflow() {
        let entries = compute_hotspots(
            vec![churn("a.rs", u32::MAX)],
            vec![complexity("a.rs", u32::MAX, 1)],
        );
        // u64::from(u32::MAX) * u64::from(u32::MAX) fits in u64 already
        // (u32::MAX^2 ≈ 1.8e19 < 2^64), but verify the call path.
        assert!(entries[0].score > 0);
    }
}
