//! Property-based tests for the TypeScript complexity extractor.
//!
//! These mirror `lens-rust`'s properties one-for-one. The metric
//! definitions are language-independent by design — McCabe counts
//! branches, cognitive complexity weights them by nesting, max nesting
//! is the depth reached — so a divergence between the adapters is a
//! bug in one of them, not a language difference. Pinning the same
//! laws in every adapter is what makes cross-language reports
//! comparable.

use lens_domain::FunctionComplexity;
use proptest::prelude::*;

use crate::complexity::extract_complexity_units;
use crate::parser::Dialect;

/// Deepest generated nesting.
const MAX_DEPTH: usize = 6;
/// Widest generated flat branch count.
const MAX_WIDTH: usize = 8;

fn indent(level: usize) -> String {
    "    ".repeat(level + 1)
}

/// `function f(c: boolean)` whose body is `depth` nested `if`s.
fn nested_ifs(depth: usize, pad: usize) -> String {
    let mut body = String::new();
    for level in 0..depth {
        body.push_str(&format!("{}if (c) {{\n", indent(level)));
    }
    body.push_str(&format!("{}const x = 1;\n", indent(depth)));
    for level in (0..depth).rev() {
        body.push_str(&format!("{}}}\n", indent(level)));
    }
    format!("{}function f(c: boolean) {{\n{body}}}\n", "\n".repeat(pad))
}

/// `function f(c: boolean)` whose body is `count` sibling `if`s.
fn flat_ifs(count: usize, pad: usize) -> String {
    let mut body = String::new();
    for _ in 0..count {
        body.push_str("    if (c) {\n        const x = 1;\n    }\n");
    }
    format!("{}function f(c: boolean) {{\n{body}}}\n", "\n".repeat(pad))
}

fn only_unit(source: &str) -> FunctionComplexity {
    let mut units =
        extract_complexity_units(source, Dialect::Ts).expect("generated source must parse");
    assert_eq!(units.len(), 1, "generator must emit exactly one function");
    units.remove(0)
}

proptest! {
    #[test]
    fn nested_ifs_score_by_depth(depth in 0usize..=MAX_DEPTH) {
        let unit = only_unit(&nested_ifs(depth, 0));
        let depth = u32::try_from(depth).expect("depth fits in u32");
        prop_assert_eq!(unit.cyclomatic, 1 + depth);
        prop_assert_eq!(unit.max_nesting, depth);
        prop_assert_eq!(unit.cognitive, depth * (depth + 1) / 2);
    }

    #[test]
    fn flat_ifs_score_by_count(count in 0usize..=MAX_WIDTH) {
        let unit = only_unit(&flat_ifs(count, 0));
        let count = u32::try_from(count).expect("count fits in u32");
        prop_assert_eq!(unit.cyclomatic, 1 + count);
        prop_assert_eq!(unit.cognitive, count);
        prop_assert_eq!(unit.max_nesting, count.min(1));
    }

    #[test]
    fn nesting_never_scores_below_flat(n in 2usize..=MAX_DEPTH) {
        let nested = only_unit(&nested_ifs(n, 0));
        let flat = only_unit(&flat_ifs(n, 0));
        prop_assert_eq!(nested.cyclomatic, flat.cyclomatic);
        prop_assert!(nested.cognitive > flat.cognitive);
        prop_assert!(nested.max_nesting > flat.max_nesting);
    }

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
