//! Per-function before/after comparison of one file.
//!
//! Two surfaces ask the same question of different "befores":
//! `analyze footprint` compares the working tree against the index (or
//! one side of a revision range against the other), and the session
//! checkpoint hook compares it against the snapshot it took when the
//! session started. Both reduce a file to [`FunctionFact`]s, pair them
//! up by name, and read the pairs through [`compare`].
//!
//! A function counts as changed when its body text changed, not when a
//! diff hunk overlaps its span: an edit above a function shifts its
//! lines without touching it, and a hunk that only reindents a sibling
//! would otherwise drag the neighbour in. The body hash ignores leading
//! and trailing whitespace on each line and blank lines, so a pure
//! reformat of indentation does not read as an edit either.

use std::collections::HashMap;

use lens_domain::{FunctionComplexity, WrapperFinding};
use serde::{Deserialize, Serialize};

use super::{AnalyzerError, SourceLang, dispatch_lens};

/// One function's comparable facts: where it sits, how complex it is,
/// and a hash of its body text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FunctionFact {
    pub(crate) name: String,
    pub(crate) start_line: usize,
    pub(crate) end_line: usize,
    pub(crate) cognitive: u32,
    pub(crate) cyclomatic: u32,
    /// FNV-1a over the trimmed, non-blank body lines. Stable across
    /// runs and builds, which a snapshot written by one process and read
    /// by another needs.
    pub(crate) body_hash: u64,
}

impl FunctionFact {
    fn from_unit(unit: &FunctionComplexity, lines: &[&str]) -> Self {
        Self {
            name: unit.name.clone(),
            start_line: unit.start_line,
            end_line: unit.end_line,
            cognitive: unit.cognitive,
            cyclomatic: unit.cyclomatic,
            body_hash: body_hash(lines, unit.start_line, unit.end_line),
        }
    }
}

/// Every function in `source`, as [`FunctionFact`]s in source order.
pub(crate) fn function_facts(
    lang: SourceLang,
    source: &str,
) -> Result<Vec<FunctionFact>, AnalyzerError> {
    let units = super::index::shared_complexity_units(lang, source)?;
    let lines: Vec<&str> = source.lines().collect();
    Ok(units
        .iter()
        .map(|unit| FunctionFact::from_unit(unit, &lines))
        .collect())
}

/// Every forwarding-only function in `source`.
pub(crate) fn wrapper_findings(
    lang: SourceLang,
    source: &str,
) -> Result<Vec<WrapperFinding>, AnalyzerError> {
    dispatch_lens!(lang, source, find_wrappers).map_err(AnalyzerError::Parse)
}

/// FNV-1a 64 over a byte stream. Written out rather than taken from
/// `std::hash`, whose `DefaultHasher` is documented as unstable across
/// releases — a snapshot hashed by one build must compare against a
/// file hashed by the next.
pub(crate) fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.into_iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

fn body_hash(lines: &[&str], start_line: usize, end_line: usize) -> u64 {
    let first = start_line.saturating_sub(1);
    let body = lines
        .iter()
        .skip(first)
        .take(end_line.saturating_sub(first))
        .map(|line| line.trim())
        .filter(|line| !line.is_empty());
    // A separator byte between lines keeps `ab` + `c` from hashing like
    // `a` + `bc`.
    fnv1a(body.flat_map(|line| line.bytes().chain(std::iter::once(b'\n'))))
}

/// How one function differs between the two sides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

impl ChangeKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }
}

/// One changed function, with whichever sides exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FunctionChange<'a> {
    pub(crate) kind: ChangeKind,
    pub(crate) before: Option<&'a FunctionFact>,
    pub(crate) after: Option<&'a FunctionFact>,
}

impl<'a> FunctionChange<'a> {
    /// The name the change is reported under: the surviving side's.
    pub(crate) fn name(&self) -> &'a str {
        self.after
            .or(self.before)
            .map_or("", |fact| fact.name.as_str())
    }

    /// Cognitive complexity after minus before; an added function counts
    /// from 0 and a deleted one down to 0.
    pub(crate) fn cognitive_delta(&self) -> i64 {
        let side = |fact: Option<&FunctionFact>| fact.map_or(0, |f| i64::from(f.cognitive));
        side(self.after) - side(self.before)
    }
}

/// Pair `before` and `after` by name and return every function that was
/// added, deleted, or whose body changed, in `after` order followed by
/// the deletions in `before` order.
///
/// A name that occurs more than once in a file (overloads, a `new` in
/// two `impl` blocks the adapter did not qualify) is paired by
/// occurrence: the n-th `before` with the n-th `after`. That can pair
/// the wrong two when one of several same-named functions is inserted
/// above the others, which then reads as a modification of each rather
/// than one addition — an over-report, never a missed change.
pub(crate) fn compare<'a>(
    before: &'a [FunctionFact],
    after: &'a [FunctionFact],
) -> Vec<FunctionChange<'a>> {
    let mut remaining: HashMap<&str, std::collections::VecDeque<&FunctionFact>> = HashMap::new();
    for fact in before {
        remaining
            .entry(fact.name.as_str())
            .or_default()
            .push_back(fact);
    }
    let mut out = Vec::new();
    for fact in after {
        match remaining
            .get_mut(fact.name.as_str())
            .and_then(|queue| queue.pop_front())
        {
            Some(old) if old.body_hash == fact.body_hash => {}
            Some(old) => out.push(FunctionChange {
                kind: ChangeKind::Modified,
                before: Some(old),
                after: Some(fact),
            }),
            None => out.push(FunctionChange {
                kind: ChangeKind::Added,
                before: None,
                after: Some(fact),
            }),
        }
    }
    for fact in before {
        let Some(queue) = remaining.get_mut(fact.name.as_str()) else {
            continue;
        };
        if queue.front().is_some_and(|left| std::ptr::eq(*left, fact)) {
            queue.pop_front();
            out.push(FunctionChange {
                kind: ChangeKind::Deleted,
                before: Some(fact),
                after: None,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    fn facts(source: &str) -> Vec<FunctionFact> {
        function_facts(SourceLang::Rust, source).unwrap()
    }

    fn kinds(before: &str, after: &str) -> Vec<(String, ChangeKind)> {
        let (b, a) = (facts(before), facts(after));
        compare(&b, &a)
            .into_iter()
            .map(|c| (c.name().to_owned(), c.kind))
            .collect()
    }

    #[rstest]
    #[case::unchanged("fn a() { 1; }\n", "fn a() { 1; }\n", &[])]
    #[case::shifted_down_is_not_an_edit(
        "fn a() { 1; }\n",
        "\n\n// moved\nfn a() { 1; }\n",
        &[]
    )]
    #[case::reindented_is_not_an_edit(
        "fn a() {\n    1;\n}\n",
        "fn a() {\n        1;\n\n}\n",
        &[]
    )]
    #[case::body_edit("fn a() { 1; }\n", "fn a() { 2; }\n", &[("a", ChangeKind::Modified)])]
    #[case::addition("fn a() {}\n", "fn a() {}\nfn b() {}\n", &[("b", ChangeKind::Added)])]
    #[case::deletion("fn a() {}\nfn b() {}\n", "fn a() {}\n", &[("b", ChangeKind::Deleted)])]
    fn classifies_changes(
        #[case] before: &str,
        #[case] after: &str,
        #[case] want: &[(&str, ChangeKind)],
    ) {
        let want: Vec<(String, ChangeKind)> =
            want.iter().map(|(n, k)| ((*n).to_owned(), *k)).collect();
        assert_eq!(kinds(before, after), want);
    }

    #[test]
    fn same_named_functions_pair_by_occurrence() {
        let before = "struct A;\nstruct B;\nimpl A { fn new() -> Self { A } }\nimpl B { fn new() -> Self { B } }\n";
        let after = "struct A;\nstruct B;\nimpl A { fn new() -> Self { A } }\nimpl B { fn new() -> Self { let b = B; b } }\n";
        let got = kinds(before, after);
        assert_eq!(got.len(), 1, "got {got:?}");
        assert_eq!(got[0].1, ChangeKind::Modified);
    }

    #[test]
    fn cognitive_delta_counts_missing_sides_as_zero() {
        let before = facts("fn a(x: i32) -> i32 { if x > 0 { 1 } else { 0 } }\n");
        let after = facts(
            "fn a(x: i32) -> i32 { if x > 0 { if x > 1 { 2 } else { 1 } } else { 0 } }\nfn b(x: bool) { if x {} }\n",
        );
        let changes = compare(&before, &after);
        let delta = |name: &str| {
            changes
                .iter()
                .find(|c| c.name() == name)
                .map(FunctionChange::cognitive_delta)
                .unwrap()
        };
        assert!(delta("a") > 0);
        assert_eq!(delta("b"), 1);
        let removed = compare(&after, &before);
        let b = removed.iter().find(|c| c.name() == "b").unwrap();
        assert_eq!(b.kind, ChangeKind::Deleted);
        assert_eq!(b.cognitive_delta(), -1);
    }

    #[test]
    fn fnv1a_matches_reference_vectors() {
        // Published FNV-1a 64 test vectors.
        assert_eq!(fnv1a(*b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(*b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(*b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn wrapper_findings_name_forwarders() {
        let found = wrapper_findings(
            SourceLang::Rust,
            "fn inner(x: i32) -> i32 { x * 2 + 1 }\nfn outer(x: i32) -> i32 { inner(x) }\n",
        )
        .unwrap();
        let names: Vec<&str> = found.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["outer"]);
    }

    fn arb_fact() -> impl Strategy<Value = FunctionFact> {
        ("[a-d]", 0u64..4).prop_map(|(name, body_hash)| FunctionFact {
            name,
            start_line: 1,
            end_line: 1,
            cognitive: 0,
            cyclomatic: 1,
            body_hash,
        })
    }

    proptest! {
        /// Comparing a side with itself finds nothing, and every fact on
        /// either side is accounted for exactly once: unchanged pairs,
        /// modifications, additions and deletions partition the input.
        #[test]
        fn compare_partitions_both_sides(
            before in proptest::collection::vec(arb_fact(), 0..8),
            after in proptest::collection::vec(arb_fact(), 0..8),
        ) {
            prop_assert!(compare(&after, &after).is_empty());
            let changes = compare(&before, &after);
            let count = |kind| changes.iter().filter(|c| c.kind == kind).count();
            let (added, modified, deleted) =
                (count(ChangeKind::Added), count(ChangeKind::Modified), count(ChangeKind::Deleted));
            let unchanged_after = after.len() - added - modified;
            let unchanged_before = before.len() - deleted - modified;
            prop_assert_eq!(unchanged_after, unchanged_before);
        }
    }
}
