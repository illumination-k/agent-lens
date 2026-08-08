//! Names for the function units TypeScript never names itself: nested
//! closures, and the callbacks a test harness registers.
//!
//! A JS/TS test suite is not made of declarations. `describe("…", () =>
//! …)` and `it("…", () => …)` are plain calls whose bodies happen to hold
//! every assertion in the file, so the walker has to mint a name for each
//! one. Two shapes exist:
//!
//! * `closure#<N>` — a nested function with nothing to name it after
//!   (`el.onclick = () => …`, an IIFE, a `map` callback).
//! * `<callee>#<N>("<title>")` — a function argument to a recognised
//!   harness call, named after the callee and, when the call opens with a
//!   string literal, that title: `it#2("accepts numbers that agree")`.
//!
//! Both carry `#<N>`, a 1-based counter scoped to the enclosing unit, so
//! two `it("same name")` cases in one suite still get distinct names. The
//! `#` is what makes a segment recognisable as synthetic: an identifier
//! can only carry one as the leading character of a `#private` field.
//!
//! Titles are sanitised rather than copied verbatim — `::` separates name
//! segments and `("…")` delimits the title, so those characters cannot
//! survive inside one.

use oxc_ast::ast::*;

/// Callee segment for a nested function with no registering call —
/// the `<parent>::closure#N` shape the walker has always minted.
pub(crate) const CLOSURE_CALLEE: &str = "closure";

/// Callees whose function arguments are test units.
///
/// The list is deliberately short: every entry is a name that means
/// "register a test" across Jest / Vitest / Mocha / Playwright and is
/// vanishingly rare as a production function taking a callback. Mocha's
/// `context` / `before` / `after` are left out for the opposite reason —
/// they read as ordinary application vocabulary. Their callbacks are
/// still extracted, just as `closure#N`.
///
/// Modifier chains (`it.skip`, `test.each(table)(…)`,
/// `describe.concurrent`) resolve through [`callee_root_identifier`] to
/// the root name, so they need no entries of their own.
const HARNESS_CALLEES: &[&str] = &[
    "afterAll",
    "afterEach",
    "beforeAll",
    "beforeEach",
    "bench",
    "describe",
    "it",
    "specify",
    "suite",
    "test",
];

/// Longest title kept in a unit name; longer ones are cut and suffixed
/// with `...`. Test titles run to sentences, and the name ends up in
/// every report row and graph node that mentions the unit.
const MAX_TITLE_CHARS: usize = 48;

/// The harness name a call registers a callback under, or `None` when the
/// callee is not a recognised harness entry point.
pub(crate) fn harness_callee(callee: &Expression) -> Option<&'static str> {
    let root = callee_root_identifier(callee)?;
    HARNESS_CALLEES.iter().copied().find(|name| *name == root)
}

/// The leftmost identifier of a callee expression: `it` for `it`,
/// `it.skip`, `it.each(table)`, and `` it.each`table` ``. A receiver that
/// is not an identifier chain (`suites[0].it`, `this.test`) resolves to
/// `None` rather than guessing — a member call on a runtime value is not
/// evidence of a harness.
fn callee_root_identifier<'a>(expr: &'a Expression<'a>) -> Option<&'a str> {
    match expr {
        Expression::Identifier(id) => Some(id.name.as_str()),
        Expression::StaticMemberExpression(member) => callee_root_identifier(&member.object),
        Expression::CallExpression(call) => callee_root_identifier(&call.callee),
        Expression::TaggedTemplateExpression(tagged) => callee_root_identifier(&tagged.tag),
        _ => None,
    }
}

/// The title a harness call opens with — `it("adds", fn)` → `adds`.
/// Only a literal first argument counts; a computed title
/// (`it(name, fn)`) has no value at parse time, so the unit falls back to
/// its `#N` index alone.
pub(crate) fn call_title(call: &CallExpression) -> Option<String> {
    let raw = match call.arguments.first()?.as_expression()? {
        Expression::StringLiteral(literal) => literal.value.as_str().to_owned(),
        Expression::TemplateLiteral(template) if template.expressions.is_empty() => template
            .quasis
            .iter()
            .map(|quasi| quasi.value.raw.as_str())
            .collect(),
        _ => return None,
    };
    sanitize_title(&raw)
}

/// Spell one synthetic name segment. `index` is 1-based and scoped to the
/// enclosing unit.
pub(crate) fn synthetic_segment(callee: &str, index: usize, title: Option<&str>) -> String {
    match title {
        Some(title) => format!("{callee}#{index}(\"{title}\")"),
        None => format!("{callee}#{index}"),
    }
}

/// The callee a synthetic segment was minted from (`closure`, `it`, …),
/// or `None` when `segment` is an ordinary identifier.
///
/// The grammar is exactly what [`synthetic_segment`] writes:
/// `<ident>#<digits>` optionally followed by `("<title>")`.
pub(crate) fn synthetic_segment_callee(segment: &str) -> Option<&str> {
    let (callee, rest) = segment.split_once('#')?;
    if callee.is_empty() || !callee.bytes().all(is_identifier_byte) {
        return None;
    }
    let after_index = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    if after_index.len() == rest.len() {
        return None;
    }
    let titled = after_index.starts_with("(\"") && after_index.ends_with("\")");
    (after_index.is_empty() || titled).then_some(callee)
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'
}

/// True iff `segment` was minted by the walker rather than written in the
/// source.
pub(crate) fn is_synthetic_segment(segment: &str) -> bool {
    synthetic_segment_callee(segment).is_some()
}

/// True iff `segment` names a callback registered with a test harness —
/// the syntactic evidence that a unit is test code regardless of what the
/// file is called.
pub(crate) fn is_harness_segment(segment: &str) -> bool {
    synthetic_segment_callee(segment).is_some_and(|callee| HARNESS_CALLEES.contains(&callee))
}

/// Fold a raw title into something that can live inside a `::`-separated
/// qualified name: no segment separators, no title delimiters, no
/// newlines, and bounded length.
fn sanitize_title(raw: &str) -> Option<String> {
    // Reserved characters become spaces rather than vanishing, so
    // `Foo::bar() works` reads as `Foo bar works` and not `Foobar works`.
    let stripped: String = raw
        .chars()
        .map(|c| match c {
            ':' | '(' | ')' | '"' | '\\' => ' ',
            c if c.is_control() => ' ',
            c => c,
        })
        .collect();
    let collapsed = stripped.split_whitespace().collect::<Vec<_>>().join(" ");
    (!collapsed.is_empty()).then(|| truncate_chars(&collapsed, MAX_TITLE_CHARS))
}

fn truncate_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::closure("closure#1", Some("closure"))]
    #[case::titled("it#2(\"adds two numbers\")", Some("it"))]
    #[case::untitled_harness("beforeEach#1", Some("beforeEach"))]
    #[case::underscored_callee("my_helper#1", Some("my_helper"))]
    #[case::dollar_callee("$fn#1", Some("$fn"))]
    #[case::plain_identifier("checkExtraction", None)]
    #[case::private_field("#count", None)]
    #[case::indexed_without_callee("#1", None)]
    #[case::non_identifier_callee("a b#1", None)]
    #[case::no_index("closure#", None)]
    #[case::unterminated_title("it#1(\"adds", None)]
    #[case::trailing_junk("it#1(\"adds\")x", None)]
    fn synthetic_segments_round_trip_their_callee(
        #[case] segment: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(synthetic_segment_callee(segment), expected);
    }

    #[rstest]
    #[case::harness("it#1(\"adds\")", true)]
    #[case::closure_is_not_harness("closure#1", false)]
    #[case::identifier("describe", false)]
    fn harness_segments_are_recognised(#[case] segment: &str, #[case] expected: bool) {
        assert_eq!(is_harness_segment(segment), expected);
    }

    #[rstest]
    #[case::plain("adds two numbers", Some("adds two numbers"))]
    #[case::separator_stripped("Foo::bar() works", Some("Foo bar works"))]
    #[case::whitespace_collapsed("  wraps\n  lines ", Some("wraps lines"))]
    #[case::control_character_becomes_a_space("bell\u{7}rings", Some("bell rings"))]
    #[case::empty_after_strip("()", None)]
    #[case::blank("   ", None)]
    fn titles_are_sanitised(#[case] raw: &str, #[case] expected: Option<&str>) {
        assert_eq!(sanitize_title(raw).as_deref(), expected);
    }

    #[test]
    fn long_titles_are_truncated_to_a_bounded_name() {
        let title = sanitize_title(&"a".repeat(MAX_TITLE_CHARS * 2)).expect("title kept");
        assert_eq!(title, format!("{}...", "a".repeat(MAX_TITLE_CHARS)));
    }

    #[test]
    fn a_sanitised_title_survives_segment_parsing() {
        // The round trip is what keeps a hostile test name from splitting
        // a qualified name into extra segments.
        let title = sanitize_title("handles \"quoted::values\" (edge)").expect("title kept");
        let segment = synthetic_segment("it", 3, Some(&title));
        assert_eq!(synthetic_segment_callee(&segment), Some("it"));
        assert!(!title.contains("::"));
    }
}
