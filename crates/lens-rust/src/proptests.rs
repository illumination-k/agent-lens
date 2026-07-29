//! Property-based tests for the Rust complexity extractor.
//!
//! Example-based tests pin a handful of hand-written snippets; these
//! pin the *shape* of the metric across the whole input range, so a
//! regression that only shows up at depth 5 (or with 7 flat branches)
//! can't slip through. The three laws asserted here are the ones the
//! module doc claims:
//!
//! * McCabe counts one branch per `if`, whether nested or flat.
//! * Cognitive complexity is nesting-weighted (`1 + nesting` per
//!   construct), so `depth` nested `if`s cost the triangular number
//!   while `depth` flat ones cost only `depth`.
//! * Max nesting is the depth actually reached.
//!
//! A fourth property pins line spans against blank-line padding, which
//! is the observable end of span→line conversion.

use lens_domain::FunctionComplexity;
use proptest::prelude::*;

use crate::complexity::extract_complexity_units;

/// Deepest generated nesting. Kept small: the interesting boundary is
/// 0/1/2, and syn's recursion cost grows with the source size.
const MAX_DEPTH: usize = 6;
/// Widest generated flat branch count.
const MAX_WIDTH: usize = 8;

fn indent(level: usize) -> String {
    "    ".repeat(level + 1)
}

/// `fn f(c: bool)` whose body is `depth` `if`s nested inside each other.
fn nested_ifs(depth: usize, pad: usize) -> String {
    let mut body = String::new();
    for level in 0..depth {
        body.push_str(&format!("{}if c {{\n", indent(level)));
    }
    body.push_str(&format!("{}let _x = 1;\n", indent(depth)));
    for level in (0..depth).rev() {
        body.push_str(&format!("{}}}\n", indent(level)));
    }
    format!("{}fn f(c: bool) {{\n{body}}}\n", "\n".repeat(pad))
}

/// `fn f(c: bool)` whose body is `count` sibling `if`s.
fn flat_ifs(count: usize, pad: usize) -> String {
    let mut body = String::new();
    for _ in 0..count {
        body.push_str("    if c {\n        let _x = 1;\n    }\n");
    }
    format!("{}fn f(c: bool) {{\n{body}}}\n", "\n".repeat(pad))
}

/// Extract the single function the generators produce.
fn only_unit(source: &str) -> FunctionComplexity {
    let mut units = extract_complexity_units(source).expect("generated source must parse");
    assert_eq!(units.len(), 1, "generator must emit exactly one function");
    units.remove(0)
}

proptest! {
    /// `depth` nested `if`s: one McCabe branch each, nesting-weighted
    /// cognitive cost (`0 + 1 + … + depth-1`), and max nesting equal to
    /// the generated depth.
    #[test]
    fn nested_ifs_score_by_depth(depth in 0usize..=MAX_DEPTH) {
        let unit = only_unit(&nested_ifs(depth, 0));
        let depth = u32::try_from(depth).expect("depth fits in u32");
        prop_assert_eq!(unit.cyclomatic, 1 + depth);
        prop_assert_eq!(unit.max_nesting, depth);
        prop_assert_eq!(unit.cognitive, depth * (depth + 1) / 2);
    }

    /// `count` sibling `if`s: the same McCabe count as the nested form,
    /// but flat cognitive cost and a nesting depth that never exceeds 1.
    /// This is the pair that makes cognitive complexity worth reporting
    /// alongside cyclomatic.
    #[test]
    fn flat_ifs_score_by_count(count in 0usize..=MAX_WIDTH) {
        let unit = only_unit(&flat_ifs(count, 0));
        let count = u32::try_from(count).expect("count fits in u32");
        prop_assert_eq!(unit.cyclomatic, 1 + count);
        prop_assert_eq!(unit.cognitive, count);
        prop_assert_eq!(unit.max_nesting, count.min(1));
    }

    /// Nesting never scores below the flat form at equal branch count,
    /// and strictly above it once there is something to nest.
    #[test]
    fn nesting_never_scores_below_flat(n in 2usize..=MAX_DEPTH) {
        let nested = only_unit(&nested_ifs(n, 0));
        let flat = only_unit(&flat_ifs(n, 0));
        prop_assert_eq!(nested.cyclomatic, flat.cyclomatic);
        prop_assert!(nested.cognitive > flat.cognitive);
        prop_assert!(nested.max_nesting > flat.max_nesting);
    }

    /// Blank lines before the function shift both span ends by exactly
    /// that many lines, and the span always stays inside the source.
    #[test]
    fn line_spans_shift_with_padding(depth in 0usize..=MAX_DEPTH, pad in 0usize..=5) {
        let base = only_unit(&nested_ifs(depth, 0));
        let padded_source = nested_ifs(depth, pad);
        let padded = only_unit(&padded_source);
        prop_assert_eq!(padded.start_line, base.start_line + pad);
        prop_assert_eq!(padded.end_line, base.end_line + pad);
        prop_assert!(padded.start_line >= 1);
        prop_assert!(padded.start_line <= padded.end_line);
        prop_assert!(padded.end_line <= padded_source.lines().count());
    }
}
