//! Naming predicates shared by analysers that need to tell test
//! scaffolding apart from production code.
//!
//! TypeScript / JavaScript has no `#[test]`-equivalent attribute. The
//! standard frameworks (Jest, Mocha, Vitest, AVA) lean on top-level
//! `describe()` / `it()` / `test()` calls with arrow-function
//! callbacks — callbacks that the walker already skips because they
//! aren't bound to a name. What's left is the smaller class of named,
//! declaration-level test artefacts (xUnit-style `class TestFoo {}`
//! suites, hand-written `function test_foo()` runners) which we
//! recognise here by name.
//!
//! The helpers are intentionally conservative: anything more elaborate
//! than the canonical `test` / `test_*` and `Test*` shapes falls
//! through and is treated as production code. Borderline conventions
//! (e.g. `testHelper` as a production utility) would be better handled
//! with an analyser flag than guessed at here.

/// True iff `name` follows the canonical test-function naming
/// convention picked up by xUnit-flavoured runners and the long tail of
/// CLI tools that crawl declaration-level functions:
///
/// * exactly `test`,
/// * `test_<rest>` with a non-empty trailing identifier.
///
/// camelCase variants (`testFoo`) are deliberately *not* matched —
/// `testHelper` on a production class is a real shape, and a stricter
/// snake-case rule keeps false positives out.
pub(crate) fn name_looks_like_test_function(name: &str) -> bool {
    if name == "test" {
        return true;
    }
    name.strip_prefix("test_")
        .is_some_and(|rest| !rest.is_empty())
}

/// True iff `name` follows the xUnit-style test-class naming
/// convention: it starts with `Test`. Mirrors the conservative rule
/// `lens-py` uses for `class Test*` so a class called `Testing` is
/// still classified as a test container.
pub(crate) fn name_looks_like_test_class(name: &str) -> bool {
    name.starts_with("Test")
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::just_test("test", true)]
    #[case::snake_test("test_foo", true)]
    #[case::snake_test_long("test_does_a_thing", true)]
    #[case::production("compute", false)]
    #[case::camel_case_test_helper_is_production("testHelper", false)]
    #[case::trailing_underscore_only("test_", false)]
    #[case::testify_is_production("testify", false)]
    fn name_looks_like_test_function_matches_xunit_convention(
        #[case] name: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(name_looks_like_test_function(name), expected);
    }

    #[rstest]
    #[case::test_class("TestThing", true)]
    #[case::just_test_prefix("Tests", true)]
    #[case::production("Service", false)]
    #[case::lowercase_test("testThing", false)]
    fn name_looks_like_test_class_matches_xunit_convention(
        #[case] name: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(name_looks_like_test_class(name), expected);
    }
}
