//! Decorator and naming predicates shared by analysers that need to tell
//! pytest / unittest scaffolding apart from production code.
//!
//! The helpers here are intentionally conservative — mirroring `lens-rust`'s
//! `attrs` module — so that anything more elaborate than the canonical
//! pytest / unittest shapes falls through and is treated as production code.
//! Borderline conventions (e.g. project-local `@fixture_alias`) are better
//! handled with an analyser flag than guessed at here.

use ruff_python_ast::{Decorator, Expr, Identifier, Stmt, StmtClassDef, StmtFunctionDef};

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
        // Generic forms: `Protocol[T]`, `unittest.TestCase[T]`. Treat the
        // subscript as transparent so the path of the subscripted base
        // matches the path of the bare one.
        Expr::Subscript(sub) => dotted_path(&sub.value),
        _ => None,
    }
}

/// True iff `class` lists `Protocol` (PEP 544 typing protocol) among its
/// direct bases. Methods inside such a class are by definition stubs that
/// describe a structural contract — including them in similarity / wrapper
/// / cohesion / complexity analysis only adds noise. Mirrors how
/// `lens-rust` drops trait methods that have no `default` body.
pub(crate) fn inherits_protocol(class: &StmtClassDef) -> bool {
    class.bases().iter().any(|base| {
        matches!(
            dotted_path(base).as_deref(),
            Some(["Protocol"])
                | Some(["typing", "Protocol"])
                | Some(["typing_extensions", "Protocol"])
        )
    })
}

/// True iff one of `decorators` marks the function as abstract. Recognised
/// shapes: `@abstractmethod` and the deprecated-but-still-extant
/// `@abstractproperty` / `@abstractclassmethod` / `@abstractstaticmethod`,
/// either bare (`from abc import abstractmethod`) or qualified through
/// `abc.*`.
pub(crate) fn has_abstractmethod_decorator(decorators: &[Decorator]) -> bool {
    decorators.iter().any(|d| {
        let Some(path) = dotted_path(&d.expression) else {
            return false;
        };
        let last = match path.as_slice() {
            [name] => *name,
            [.., last] if path[0] == "abc" => *last,
            _ => return false,
        };
        matches!(
            last,
            "abstractmethod" | "abstractproperty" | "abstractclassmethod" | "abstractstaticmethod"
        )
    })
}

/// True iff one of `decorators` is `@overload` (or `@typing.overload` /
/// `@typing_extensions.overload`). `@overload`-decorated functions only
/// declare a typing variant; their bodies are stubs by convention.
pub(crate) fn has_overload_decorator(decorators: &[Decorator]) -> bool {
    decorators.iter().any(|d| {
        matches!(
            dotted_path(&d.expression).as_deref(),
            Some(["overload"])
                | Some(["typing", "overload"])
                | Some(["typing_extensions", "overload"])
        )
    })
}

/// True iff `body` is a non-empty sequence whose every statement is a
/// stub: a `pass`, a bare `...`, a docstring (string-literal expression
/// statement), or `raise NotImplementedError(...)`. Combinations like
/// `"""docstring"""\n...` are accepted because both halves are
/// individually stubs. A function with such a body carries no analysable
/// content — keeping it pollutes similarity reports (every Protocol
/// method collapses to the same one-node tree) and inflates complexity
/// stats with trivial CC=1 entries.
pub(crate) fn is_stub_body(body: &[Stmt]) -> bool {
    !body.is_empty() && body.iter().all(is_stub_stmt)
}

fn is_stub_stmt(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Pass(_) => true,
        Stmt::Expr(stmt_expr) => matches!(
            stmt_expr.value.as_ref(),
            Expr::EllipsisLiteral(_) | Expr::StringLiteral(_) | Expr::FString(_)
        ),
        Stmt::Raise(stmt_raise) => stmt_raise
            .exc
            .as_deref()
            .is_some_and(is_not_implemented_error),
        _ => false,
    }
}

fn is_not_implemented_error(expr: &Expr) -> bool {
    let inner = match expr {
        Expr::Call(call) => call.func.as_ref(),
        other => other,
    };
    matches!(inner, Expr::Name(name) if name.id.as_str() == "NotImplementedError")
}

/// True iff `func` should be treated as a stub for analysis purposes:
/// `@overload` / `@abstractmethod`-decorated, or a body that is purely
/// `pass` / `...` / docstring / `raise NotImplementedError`. Used by
/// every analyser entry point so stubs disappear before they reach a
/// downstream metric.
pub(crate) fn is_stub_function(func: &StmtFunctionDef) -> bool {
    has_overload_decorator(&func.decorator_list)
        || has_abstractmethod_decorator(&func.decorator_list)
        || is_stub_body(&func.body)
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

    #[rstest]
    #[case::bare_protocol("class Foo(Protocol): pass\n", true)]
    #[case::qualified_protocol("class Foo(typing.Protocol): pass\n", true)]
    #[case::typing_extensions_protocol("class Foo(typing_extensions.Protocol): pass\n", true)]
    // Generic-protocol form: `Protocol[T]` parses as a `Subscript` whose
    // value is the `Protocol` name. The `Subscript` arm of `dotted_path`
    // must let the path through unchanged for this to match. Without it
    // a generic Protocol slips past the filter.
    #[case::generic_protocol("class Foo(Protocol[T]): pass\n", true)]
    #[case::no_bases("class Foo: pass\n", false)]
    #[case::other_base("class Foo(Bar): pass\n", false)]
    // `ABC` subclasses commonly mix abstract and concrete methods; the
    // class itself is not a stub. Per-method `@abstractmethod` filtering
    // is the right granularity for ABCs.
    #[case::abc_subclass("class Foo(ABC): pass\n", false)]
    fn inherits_protocol_recognises_canonical_shapes(#[case] src: &str, #[case] expected: bool) {
        let class = first_class(src);
        assert_eq!(inherits_protocol(&class), expected);
    }

    #[rstest]
    #[case::bare_abstractmethod("@abstractmethod\ndef foo(self): ...\n", true)]
    #[case::qualified_abstractmethod("@abc.abstractmethod\ndef foo(self): ...\n", true)]
    #[case::abstractproperty("@abstractproperty\ndef foo(self): ...\n", true)]
    #[case::abstractclassmethod("@abstractclassmethod\ndef foo(cls): ...\n", true)]
    #[case::abstractstaticmethod("@abstractstaticmethod\ndef foo(): ...\n", true)]
    #[case::stacked_with_property("@property\n@abstractmethod\ndef foo(self): ...\n", true)]
    #[case::no_decorator("def foo(self): pass\n", false)]
    #[case::unrelated("@cached_property\ndef foo(self): pass\n", false)]
    // Tail name collides with `abstractmethod` but root is unrelated.
    // `path[0] == "abc"` guard must hold; tightening it to `true` would
    // falsely classify `@my.abstractmethod`.
    #[case::tail_match_non_abc_root("@my.abstractmethod\ndef foo(self): ...\n", false)]
    fn has_abstractmethod_decorator_recognises_canonical_shapes(
        #[case] src: &str,
        #[case] expected: bool,
    ) {
        let func = first_function(src);
        assert_eq!(has_abstractmethod_decorator(&func.decorator_list), expected);
    }

    #[rstest]
    #[case::bare_overload("@overload\ndef foo(x): ...\n", true)]
    #[case::qualified_overload("@typing.overload\ndef foo(x): ...\n", true)]
    #[case::typing_extensions_overload("@typing_extensions.overload\ndef foo(x): ...\n", true)]
    #[case::no_decorator("def foo(x): pass\n", false)]
    #[case::unrelated("@cached_property\ndef foo(self): pass\n", false)]
    fn has_overload_decorator_recognises_canonical_shapes(
        #[case] src: &str,
        #[case] expected: bool,
    ) {
        let func = first_function(src);
        assert_eq!(has_overload_decorator(&func.decorator_list), expected);
    }

    #[rstest]
    #[case::pass_only("def foo(): pass\n", true)]
    #[case::ellipsis_only("def foo(): ...\n", true)]
    #[case::docstring_only("def foo():\n    \"\"\"docs\"\"\"\n", true)]
    #[case::raise_not_implemented("def foo():\n    raise NotImplementedError\n", true)]
    #[case::raise_not_implemented_called("def foo():\n    raise NotImplementedError()\n", true)]
    #[case::raise_not_implemented_with_msg(
        "def foo():\n    raise NotImplementedError(\"msg\")\n",
        true
    )]
    #[case::docstring_then_ellipsis("def foo():\n    \"\"\"docs\"\"\"\n    ...\n", true)]
    #[case::docstring_then_raise(
        "def foo():\n    \"\"\"docs\"\"\"\n    raise NotImplementedError\n",
        true
    )]
    #[case::real_return("def foo():\n    return 1\n", false)]
    #[case::docstring_then_real_return("def foo():\n    \"\"\"docs\"\"\"\n    return 1\n", false)]
    // Raising a different exception is real behaviour, not a stub.
    #[case::raise_value_error("def foo():\n    raise ValueError\n", false)]
    fn is_stub_body_recognises_stub_shapes(#[case] src: &str, #[case] expected: bool) {
        let func = first_function(src);
        assert_eq!(is_stub_body(&func.body), expected);
    }

    #[test]
    fn is_stub_function_combines_decorator_and_body_signals() {
        // Decorator alone is enough — even with a real body. Mirrors how
        // `@overload`-decorated declarations occasionally carry an
        // assertion or assignment that exists only to satisfy the
        // type-checker.
        let with_real_body = first_function("@overload\ndef foo(x):\n    return x\n");
        assert!(is_stub_function(&with_real_body));

        // Body alone is enough — no decorator required.
        let body_only = first_function("def foo():\n    ...\n");
        assert!(is_stub_function(&body_only));

        // Neither decorator nor stub body → kept.
        let real = first_function("def foo():\n    return 1\n");
        assert!(!is_stub_function(&real));
    }
}
