//! Decorator and naming predicates shared by analysers that need to tell
//! pytest / unittest scaffolding apart from production code.
//!
//! The helpers here are intentionally conservative — mirroring `lens-rust`'s
//! `attrs` module — so that anything more elaborate than the canonical
//! pytest / unittest shapes falls through and is treated as production code.
//! Borderline conventions (e.g. project-local `@fixture_alias`) are better
//! handled with an analyser flag than guessed at here.

use ruff_python_ast::{Decorator, Expr, Identifier, StmtClassDef};

/// True iff `name` follows the pytest convention for a test function:
/// it starts with `test_`, or is exactly `test`. Matches what pytest's
/// default collection rule (`python_functions = test_*`) would pick up.
pub(crate) fn name_looks_like_test_function(name: &Identifier) -> bool {
    let s = name.as_str();
    s == "test" || s.starts_with("test_")
}

/// True iff `name` follows the pytest / unittest convention for a test
/// class: it starts with `Test`. Pytest's default collection rule
/// (`python_classes = Test*`) and the unittest convention both follow
/// this shape.
pub(crate) fn name_looks_like_test_class(name: &Identifier) -> bool {
    name.as_str().starts_with("Test")
}

/// True iff `class` inherits (directly) from `TestCase` or
/// `unittest.TestCase`. We only look at directly listed bases — chasing
/// imports/aliases would require a whole-module pass and false positives
/// here are cheap to spot.
pub(crate) fn class_inherits_test_case(class: &StmtClassDef) -> bool {
    class.bases().iter().any(|base| {
        matches!(
            dotted_path(base).as_deref(),
            Some(["TestCase"]) | Some(["unittest", "TestCase"])
        )
    })
}

/// True iff one of `decorators` marks the function as a pytest fixture
/// or a `pytest.mark.*` test. Recognised shapes:
///
/// * `@fixture` — bare import (`from pytest import fixture`),
/// * `@pytest.fixture` / `@pytest.fixture(...)`,
/// * `@pytest.mark.<anything>` / `@pytest.mark.<anything>(...)`.
pub(crate) fn has_pytest_decorator(decorators: &[Decorator]) -> bool {
    decorators
        .iter()
        .any(|d| is_pytest_decorator(&d.expression))
}

/// True iff one of `decorators` is the unittest skip family
/// (`@unittest.skip`, `@unittest.skipIf`, `@unittest.skipUnless`,
/// `@unittest.expectedFailure`, or the same names imported bare).
pub(crate) fn has_unittest_skip_decorator(decorators: &[Decorator]) -> bool {
    decorators.iter().any(|d| {
        let Some(path) = dotted_path(&d.expression) else {
            return false;
        };
        let last = match path.as_slice() {
            [name] => *name,
            [.., last] if path[0] == "unittest" => *last,
            _ => return false,
        };
        matches!(last, "skip" | "skipIf" | "skipUnless" | "expectedFailure")
    })
}

fn is_pytest_decorator(expr: &Expr) -> bool {
    let Some(path) = dotted_path(expr) else {
        return false;
    };
    matches!(
        path.as_slice(),
        ["fixture"] | ["pytest", "fixture"] | ["pytest", "mark", ..]
    )
}

/// Walk a decorator expression and recover its dotted path
/// (e.g. `pytest.mark.parametrize` → `["pytest", "mark", "parametrize"]`).
/// `Call` nodes unwrap to their callee so `@pytest.mark.skip("reason")`
/// matches the same path as the bare attribute form.
fn dotted_path(expr: &Expr) -> Option<Vec<&str>> {
    match expr {
        Expr::Name(name) => Some(vec![name.id.as_str()]),
        Expr::Attribute(attr) => {
            let mut base = dotted_path(&attr.value)?;
            base.push(attr.attr.as_str());
            Some(base)
        }
        Expr::Call(call) => dotted_path(&call.func),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use ruff_python_ast::{Stmt, StmtFunctionDef};
    use ruff_python_parser::parse_module;

    fn first_function(src: &str) -> StmtFunctionDef {
        let module = parse_module(src).unwrap().into_syntax();
        for stmt in module.body {
            if let Stmt::FunctionDef(f) = stmt {
                return f;
            }
        }
        panic!("no function in: {src}");
    }

    fn first_class(src: &str) -> StmtClassDef {
        let module = parse_module(src).unwrap().into_syntax();
        for stmt in module.body {
            if let Stmt::ClassDef(c) = stmt {
                return c;
            }
        }
        panic!("no class in: {src}");
    }

    #[rstest]
    #[case::test_underscore("def test_foo(): pass\n", true)]
    #[case::just_test("def test(): pass\n", true)]
    #[case::production("def my_helper(): pass\n", false)]
    #[case::testlike_no_underscore("def testify(): pass\n", false)]
    fn name_looks_like_test_function_matches_pytest_convention(
        #[case] src: &str,
        #[case] expected: bool,
    ) {
        let func = first_function(src);
        assert_eq!(name_looks_like_test_function(&func.name), expected);
    }

    #[rstest]
    #[case::test_class("class TestThing: pass\n", true)]
    #[case::just_test_prefix("class Tests: pass\n", true)]
    #[case::production("class Service: pass\n", false)]
    fn name_looks_like_test_class_matches_pytest_convention(
        #[case] src: &str,
        #[case] expected: bool,
    ) {
        let class = first_class(src);
        assert_eq!(name_looks_like_test_class(&class.name), expected);
    }

    #[rstest]
    #[case::bare_testcase("class Foo(TestCase): pass\n", true)]
    #[case::qualified_testcase("class Foo(unittest.TestCase): pass\n", true)]
    #[case::no_bases("class Foo: pass\n", false)]
    #[case::other_base("class Foo(Bar): pass\n", false)]
    fn class_inherits_test_case_matches_canonical_unittest_shapes(
        #[case] src: &str,
        #[case] expected: bool,
    ) {
        let class = first_class(src);
        assert_eq!(class_inherits_test_case(&class), expected);
    }

    #[rstest]
    #[case::bare_fixture("@fixture\ndef foo(): pass\n", true)]
    #[case::qualified_fixture("@pytest.fixture\ndef foo(): pass\n", true)]
    #[case::called_fixture("@pytest.fixture()\ndef foo(): pass\n", true)]
    #[case::called_fixture_with_arg("@pytest.fixture(scope=\"session\")\ndef foo(): pass\n", true)]
    #[case::pytest_mark_skip("@pytest.mark.skip\ndef foo(): pass\n", true)]
    #[case::pytest_mark_parametrize(
        "@pytest.mark.parametrize(\"x\", [1, 2])\ndef foo(): pass\n",
        true
    )]
    #[case::no_decorator("def foo(): pass\n", false)]
    #[case::unrelated_decorator("@cached_property\ndef foo(self): pass\n", false)]
    fn has_pytest_decorator_recognises_canonical_shapes(#[case] src: &str, #[case] expected: bool) {
        let func = first_function(src);
        assert_eq!(has_pytest_decorator(&func.decorator_list), expected);
    }

    #[rstest]
    #[case::bare_skip("@skip\ndef foo(): pass\n", true)]
    #[case::qualified_skip("@unittest.skip(\"reason\")\ndef foo(): pass\n", true)]
    #[case::skip_if("@unittest.skipIf(True, \"x\")\ndef foo(): pass\n", true)]
    #[case::expected_failure("@unittest.expectedFailure\ndef foo(): pass\n", true)]
    #[case::no_decorator("def foo(): pass\n", false)]
    #[case::unrelated("@my_decorator\ndef foo(): pass\n", false)]
    // Multi-segment path whose tail name happens to match the unittest
    // skip family but whose root isn't `unittest`. Covers the
    // `path[0] == "unittest"` match guard against a mutant that
    // replaces it with `true` (which would falsely classify
    // `@asyncio.skip` as an `@unittest.skip`).
    #[case::tail_match_non_unittest_root("@asyncio.skip\ndef foo(): pass\n", false)]
    fn has_unittest_skip_decorator_recognises_canonical_shapes(
        #[case] src: &str,
        #[case] expected: bool,
    ) {
        let func = first_function(src);
        assert_eq!(has_unittest_skip_decorator(&func.decorator_list), expected);
    }
}
