//! Detect "thin wrapper" Python functions: bodies that, after peeling
//! a short chain of trivial adapters, are just a forwarding call to
//! another function with the parameters passed straight through.
//!
//! Conceptually a wrapper is a function whose body adds no logic of
//! its own — it only renames, narrows visibility, or coerces types.
//! Things that DO add logic (extra statements, branching, argument
//! transformations, literal arguments) keep the function out of the
//! report.
//!
//! Mirrors [`lens_rust::find_wrappers`] and [`lens_ts::find_wrappers`]
//! in shape and intent. The adapter list differs because Python uses
//! `await` and stringification methods rather than `?` / `.unwrap()`.
//!
//! Tests are filtered the same way [`crate::extract_functions_excluding_tests`]
//! filters them: pytest-flavoured functions and `unittest.TestCase`
//! methods are forwarding by design and would only add noise.

use lens_domain::{WrapperFinding, args_pass_through_by, qualify};
use ruff_python_ast::{
    Expr, ExprAttribute, ExprCall, Parameters, Stmt, StmtClassDef, StmtFunctionDef, StmtReturn,
};
use ruff_python_parser::{ParseError, parse_module};

use crate::attrs::{
    class_inherits_test_case, has_pytest_decorator, has_unittest_skip_decorator, inherits_protocol,
    is_stub_function, name_looks_like_test_class, name_looks_like_test_function,
};
use crate::line_index::LineIndex;

/// Method names with no arguments that we treat as "no semantic
/// content": stringification / coercion shims.
const TRIVIAL_NULLARY_METHOD_ADAPTERS: &[&str] = &[
    "encode", "decode", "strip", "lstrip", "rstrip", "lower", "upper", "casefold", "copy", "items",
    "keys", "values",
];

/// Builtins that take a single argument and act as type coercions —
/// `str(x)`, `list(x)`, etc. The adapter list mirrors what the Rust
/// adapter calls "into" / "to_string": pure shape-changers with no
/// semantic content.
const TRIVIAL_UNARY_BUILTIN_ADAPTERS: &[&str] = &[
    "str",
    "int",
    "float",
    "bool",
    "list",
    "tuple",
    "set",
    "dict",
    "bytes",
    "bytearray",
];

/// Failures produced while extracting wrappers.
#[derive(Debug, thiserror::Error)]
pub enum WrapperError {
    #[error("failed to parse Python source: {0}")]
    Parse(#[from] ParseError),
}

/// Walk the source and return every function whose body is just a
/// forwarding call.
pub fn find_wrappers(source: &str) -> Result<Vec<WrapperFinding>, WrapperError> {
    let module = parse_module(source)?.into_syntax();
    let lines = LineIndex::new(source);
    let mut out = Vec::new();
    for stmt in &module.body {
        collect_stmt(stmt, None, &lines, &mut out);
    }
    Ok(out)
}

fn collect_stmt(
    stmt: &Stmt,
    owner: Option<&str>,
    lines: &LineIndex,
    out: &mut Vec<WrapperFinding>,
) {
    match stmt {
        Stmt::FunctionDef(func) => {
            // Stub-shaped functions (Protocol / abstract / overload /
            // pass / ... / docstring / raise NotImplementedError) carry
            // no body to forward — flagging them is noise.
            if is_stub_function(func) {
                return;
            }
            if is_test_function(func) {
                return;
            }
            if let Some(finding) = analyze(func, owner, lines) {
                out.push(finding);
            }
        }
        Stmt::ClassDef(class) => collect_class(class, lines, out),
        _ => {}
    }
}

fn collect_class(class: &StmtClassDef, lines: &LineIndex, out: &mut Vec<WrapperFinding>) {
    // Protocol classes are pure declarations; every method is a stub by
    // definition. Drop the whole subtree the same way test classes are
    // dropped above.
    if inherits_protocol(class) {
        return;
    }
    if name_looks_like_test_class(&class.name) || class_inherits_test_case(class) {
        return;
    }
    let class_name = class.name.as_str();
    for inner in &class.body {
        collect_stmt(inner, Some(class_name), lines, out);
    }
}

fn is_test_function(func: &StmtFunctionDef) -> bool {
    name_looks_like_test_function(&func.name)
        || has_pytest_decorator(&func.decorator_list)
        || has_unittest_skip_decorator(&func.decorator_list)
}

fn analyze(
    func: &StmtFunctionDef,
    owner: Option<&str>,
    lines: &LineIndex,
) -> Option<WrapperFinding> {
    // `__init__` is ceremonial: returning early via `super().__init__(*args)`
    // is idiomatic boilerplate, not a refactoring opportunity. Mirrors
    // how `lens-ts` skips constructors.
    if func.name.as_str() == "__init__" {
        return None;
    }
    let tail = single_tail_expr(&func.body)?;
    let (core, adapters) = peel_adapters(tail);
    let (callee, args, kwargs_empty) = core_call(core)?;
    if !kwargs_empty {
        return None;
    }
    let mut params = collect_param_idents(&func.parameters)?;
    // When the callee chain is rooted at one of the function's
    // parameters (e.g. `self.inner.handle` in `def handle(self, x)`),
    // that parameter is consumed via the receiver path. Drop it from
    // the pass-through set so the remaining params still need to line
    // up exactly with the call's positional args.
    if let Some(root) = callee_root(&callee)
        && let Some(pos) = params.iter().position(|p| p == root)
    {
        params.remove(pos);
    }
    if !args_pass_through(args, &params) {
        return None;
    }
    let start_line = lines.line_of(func.range.start().to_usize());
    let end_offset = func.range.end().to_usize().saturating_sub(1);
    let end_line = lines.line_of(end_offset);
    Some(WrapperFinding {
        name: qualify(owner, func.name.as_str()),
        start_line,
        end_line,
        callee,
        adapters,
    })
}

/// Return the single tail expression of a function body: either the
/// argument of a `return` or a single bare expression statement.
fn single_tail_expr(body: &[Stmt]) -> Option<&Expr> {
    let [stmt] = body else {
        return None;
    };
    match stmt {
        Stmt::Return(StmtReturn { value, .. }) => value.as_deref(),
        Stmt::Expr(expr_stmt) => Some(&expr_stmt.value),
        _ => None,
    }
}

/// Strip trivial adapters from the outside. Returns the innermost
/// expression and the list of adapter labels in source order
/// (innermost-first), matching how `lens-rust` and `lens-ts` render
/// them.
fn peel_adapters(expr: &Expr) -> (&Expr, Vec<String>) {
    let mut current = expr;
    let mut outer_to_inner: Vec<String> = Vec::new();
    loop {
        match current {
            Expr::Await(a) => {
                outer_to_inner.push("await".to_owned());
                current = &a.value;
            }
            Expr::Call(call) if is_trivial_unary_builtin(call) => {
                outer_to_inner.push(format!("{}()", builtin_name(call).unwrap_or_default()));
                let inner = call
                    .arguments
                    .args
                    .first()
                    .filter(|_| call.arguments.keywords.is_empty());
                if let Some(arg) = inner {
                    current = arg;
                } else {
                    break;
                }
            }
            Expr::Call(call) if is_trivial_method_call(call) => {
                let name = method_call_name(call).unwrap_or_default();
                outer_to_inner.push(format!(".{name}()"));
                if let Some(receiver) = method_call_receiver(call) {
                    current = receiver;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }
    outer_to_inner.reverse();
    (current, outer_to_inner)
}

fn is_trivial_method_call(call: &ExprCall) -> bool {
    if !call.arguments.args.is_empty() || !call.arguments.keywords.is_empty() {
        return false;
    }
    let Some(name) = method_call_name(call) else {
        return false;
    };
    TRIVIAL_NULLARY_METHOD_ADAPTERS.contains(&name)
}

fn method_call_name(call: &ExprCall) -> Option<&str> {
    if let Expr::Attribute(attr) = call.func.as_ref() {
        Some(attr.attr.as_str())
    } else {
        None
    }
}

fn method_call_receiver(call: &ExprCall) -> Option<&Expr> {
    if let Expr::Attribute(attr) = call.func.as_ref() {
        Some(&attr.value)
    } else {
        None
    }
}

fn is_trivial_unary_builtin(call: &ExprCall) -> bool {
    if call.arguments.args.len() != 1 || !call.arguments.keywords.is_empty() {
        return false;
    }
    let Some(name) = builtin_name(call) else {
        return false;
    };
    TRIVIAL_UNARY_BUILTIN_ADAPTERS.contains(&name)
}

fn builtin_name(call: &ExprCall) -> Option<&str> {
    if let Expr::Name(n) = call.func.as_ref() {
        Some(n.id.as_str())
    } else {
        None
    }
}

/// If `expr` is a function call or a method call whose callee /
/// receiver is itself a "thin" path (no nested computation), return
/// its rendered callee path, the positional argument list, and a flag
/// marking whether the call has no keyword arguments.
fn core_call(expr: &Expr) -> Option<(String, &[Expr], bool)> {
    let Expr::Call(call) = expr else {
        return None;
    };
    let callee = render_callee(&call.func)?;
    Some((
        callee,
        &call.arguments.args,
        call.arguments.keywords.is_empty(),
    ))
}

/// First identifier in a rendered callee path. `self.inner.handle`
/// has root `self`; a bare `b` has root `b`.
fn callee_root(callee: &str) -> Option<&str> {
    callee.split('.').next().filter(|s| !s.is_empty())
}

/// Path-shaped expressions: a name, an attribute chain
/// (`self.inner.x`). Calls or arbitrary expressions inside are
/// rejected — those would add real computation, not just navigation.
fn render_callee(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(n) => Some(n.id.as_str().to_owned()),
        Expr::Attribute(ExprAttribute { value, attr, .. }) => {
            let base = render_callee(value)?;
            Some(format!("{base}.{}", attr.as_str()))
        }
        _ => None,
    }
}

/// Collect parameter names that can be forwarded verbatim. Returns
/// `None` if any parameter has a default value, is variadic
/// (`*args` / `**kwargs`), or is a positional-only / keyword-only
/// parameter — none of those can be passed through cleanly while
/// keeping the function a "thin" wrapper.
fn collect_param_idents(params: &Parameters) -> Option<Vec<String>> {
    if params.vararg.is_some() || params.kwarg.is_some() {
        return None;
    }
    if !params.kwonlyargs.is_empty() || !params.posonlyargs.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(params.args.len());
    for p in &params.args {
        // A defaulted parameter (`x=0`) injects a literal at the call
        // site if the caller omits the argument; treating it as a
        // plain passthrough hides that real difference.
        if p.default.is_some() {
            return None;
        }
        out.push(p.parameter.name.as_str().to_owned());
    }
    Some(out)
}

/// True iff every call argument is a parameter passed through, every
/// parameter is used exactly once, and the arity matches.
fn args_pass_through(args: &[Expr], params: &[String]) -> bool {
    args_pass_through_by(args, params, passthrough_ident)
}

fn passthrough_ident(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Name(n) => Some(n.id.as_str().to_owned()),
        // `*xs` / `**kw` at the call site is not a plain passthrough.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn run(src: &str) -> Vec<WrapperFinding> {
        find_wrappers(src).unwrap()
    }

    fn names(findings: &[WrapperFinding]) -> Vec<&str> {
        findings.iter().map(|f| f.name.as_str()).collect()
    }

    #[test]
    fn detects_simple_forward() {
        let src = "def a(x):\n    return b(x)\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].callee, "b");
        assert!(findings[0].adapters.is_empty());
    }

    #[test]
    fn detects_method_delegation() {
        let src = "
class Service:
    def handle(self, x):
        return self.inner.handle(x)
";
        let findings = run(src);
        assert_eq!(names(&findings), ["Service::handle"]);
        assert_eq!(findings[0].callee, "self.inner.handle");
    }

    #[test]
    fn detects_with_await() {
        let src = "async def a(x):\n    return await b(x)\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].adapters, vec!["await".to_owned()]);
    }

    #[test]
    fn detects_with_str_coercion() {
        let src = "def a(x):\n    return str(b(x))\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].callee, "b");
        assert_eq!(findings[0].adapters, vec!["str()".to_owned()]);
    }

    #[test]
    fn detects_with_method_adapter() {
        let src = "def a(x):\n    return b(x).encode()\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].callee, "b");
        assert_eq!(findings[0].adapters, vec![".encode()".to_owned()]);
    }

    #[test]
    fn detects_with_chained_adapters() {
        let src = "async def a(x):\n    return await b(x).decode()\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        // outer-to-inner: await wraps decode wraps b(x); rendered
        // inner-first as [".decode()", "await"].
        assert_eq!(
            findings[0].adapters,
            vec![".decode()".to_owned(), "await".to_owned()]
        );
    }

    #[test]
    fn detects_passthrough_with_reordered_args() {
        let src = "def a(x, y):\n    return b(y, x)\n";
        assert_eq!(names(&run(src)), ["a"]);
    }

    #[test]
    fn detects_bare_call_expression_body() {
        // No `return` — a bare expression statement carrying a call is
        // still a wrapper (e.g. logging shim that returns None).
        let src = "def a(x):\n    b(x)\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].callee, "b");
    }

    /// Body shapes that disqualify a function from being a wrapper.
    #[rstest]
    #[case::arg_transformation("def a(x):\n    return b(x + 1)\n")]
    #[case::multi_statement_body("def a(x):\n    y = x\n    return b(y)\n")]
    #[case::literal_only_body("def a():\n    return 42\n")]
    #[case::empty_body("def a():\n    pass\n")]
    #[case::unrelated_arg_name("def a(x):\n    return b(y)\n")]
    #[case::arity_mismatch_too_few("def a(x):\n    return b()\n")]
    #[case::arity_mismatch_too_many("def a(x):\n    return b(x, x)\n")]
    #[case::branching_body("def a(x):\n    if x > 0:\n        return b(x)\n    return c(x)\n")]
    #[case::default_value_param("def a(x=0):\n    return b(x)\n")]
    #[case::vararg("def a(*xs):\n    return b(xs)\n")]
    #[case::kwarg("def a(**kw):\n    return b(kw)\n")]
    #[case::kwonly_arg("def a(*, x):\n    return b(x)\n")]
    #[case::keyword_arg_at_call("def a(x):\n    return b(x=x)\n")]
    #[case::chain_receiver_call("def a(x):\n    return foo(x).bar(x)\n")]
    fn rejects_non_wrapper_shape(#[case] src: &str) {
        assert!(run(src).is_empty(), "expected no wrapper for: {src}");
    }

    #[test]
    fn class_method_gets_qualified_name() {
        let src = "
class Foo:
    def a(self, x):
        return b(self, x)
";
        let findings = run(src);
        assert_eq!(names(&findings), ["Foo::a"]);
    }

    #[test]
    fn dunder_init_is_not_reported() {
        // __init__'s super() forwarding is mandatory boilerplate; the
        // detector skips it the same way TS / Rust skip constructors.
        let src = "
class Child(Parent):
    def __init__(self, x):
        super().__init__(x)
";
        let findings = run(src);
        assert!(findings.is_empty());
    }

    #[test]
    fn pytest_test_function_is_not_reported() {
        // Test functions are forwarding by design; flagging them is
        // noise, not signal — same rationale as `lens-rust`'s
        // `#[cfg(test)]` skip.
        let src = "
def production(x):
    return helper(x)

def test_thing(x):
    return helper(x)
";
        let findings = run(src);
        assert_eq!(names(&findings), ["production"]);
    }

    #[test]
    fn methods_in_test_class_are_not_reported() {
        let src = "
class TestThing:
    def helper(self, x):
        return underlying(self, x)
";
        let findings = run(src);
        assert!(findings.is_empty());
    }

    #[test]
    fn methods_in_unittest_testcase_subclass_are_not_reported() {
        let src = "
import unittest
class Foo(unittest.TestCase):
    def helper(self, x):
        return underlying(self, x)
";
        let findings = run(src);
        assert!(findings.is_empty());
    }

    #[test]
    fn methods_inside_protocol_class_are_not_reported() {
        // PEP 544 Protocol classes describe a structural contract; the
        // bodies are stubs by design. Even if a body looks superficially
        // like a forwarding call, reporting it as a wrapper is noise.
        let src = "
from typing import Protocol

class Service(Protocol):
    def handle(self, x):
        return self.inner.handle(x)
";
        let findings = run(src);
        assert!(findings.is_empty());
    }

    #[test]
    fn abstractmethod_decorated_methods_are_not_reported() {
        // A method decorated with `@abstractmethod` is a stub even if
        // it carries a real-looking body. Mirrors how `lens-rust` skips
        // trait method declarations without a `default` body.
        let src = "
from abc import abstractmethod

class Service:
    @abstractmethod
    def handle(self, x):
        return self.inner.handle(x)
";
        let findings = run(src);
        assert!(findings.is_empty());
    }

    #[test]
    fn overload_decorated_functions_are_not_reported() {
        // `@overload` declarations are typing-only stubs. Even if the
        // body is a forwarding call (sometimes added to satisfy the
        // type-checker), they must not be reported as wrappers.
        let src = "
from typing import overload

@overload
def f(x):
    return b(x)
";
        let findings = run(src);
        assert!(findings.is_empty());
    }

    #[test]
    fn pytest_fixture_is_not_reported() {
        let src = "
import pytest
@pytest.fixture
def sample(x):
    return build(x)
";
        let findings = run(src);
        assert!(findings.is_empty());
    }

    #[test]
    fn records_line_numbers_from_signature_to_block_end() {
        let src = "\ndef first(x):\n    return b(x)\n";
        let findings = run(src);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].start_line, 2);
        assert_eq!(findings[0].end_line, 3);
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = find_wrappers("def !!!(:").unwrap_err();
        assert!(matches!(err, WrapperError::Parse(_)));
    }

    #[test]
    fn wrapper_error_source_is_present() {
        use std::error::Error as _;
        let err = find_wrappers("def !!!(:").unwrap_err();
        assert!(err.source().is_some());
    }

    #[test]
    fn does_not_flag_function_with_extra_logic_inside_call_args() {
        // `b(x, x.lower())` performs work in the second argument; even
        // though both args reference parameters, only one is a plain
        // passthrough, so this must not be reported.
        let src = "def a(x):\n    return b(x, x.lower())\n";
        assert!(run(src).is_empty());
    }

    #[test]
    fn does_not_flag_function_with_keyword_call() {
        // A keyword passes a literal-equivalent label that the wrapper
        // is injecting — not a plain passthrough.
        let src = "def a(x):\n    return b(x, mode='r')\n";
        assert!(run(src).is_empty());
    }

    #[test]
    fn method_call_with_positional_arg_is_not_treated_as_trivial_adapter() {
        // `is_trivial_method_call` rejects calls that have *either*
        // positional args or keyword args. If the `||` were silently
        // tightened to `&&` the method `.encode("utf-8")` would slip
        // through (positional only, no kwargs) and `a` would be
        // reported as a wrapper with adapter `.encode()`.
        let src = "def a(x):\n    return b(x).encode(\"utf-8\")\n";
        assert!(
            run(src).is_empty(),
            ".encode(arg) is not a trivial nullary adapter and a should not be flagged",
        );
    }

    #[test]
    fn method_call_with_keyword_arg_is_not_treated_as_trivial_adapter() {
        // Mirror of the positional-arg case; keyword-only args must
        // also disqualify a call from the trivial-adapter list.
        let src = "def a(x):\n    return b(x).encode(encoding=\"utf-8\")\n";
        assert!(run(src).is_empty());
    }

    #[test]
    fn unary_builtin_with_extra_positional_arg_is_not_treated_as_adapter() {
        // `is_trivial_unary_builtin` rejects unless arity is exactly
        // 1 *and* keywords are empty. Wrap an inner forwarding call
        // so the mutation is observable: with the gate intact,
        // `int(b(x), 16)` is left whole and `core_call` sees
        // arity-2 args that don't pass through `def a(x)`. With the
        // `||` silently tightened to `&&`, the gate misfires, the
        // adapter peels down to `b(x)`, and `a` would be reported
        // as a wrapper over `b`.
        let src = "def a(x):\n    return int(b(x), 16)\n";
        assert!(
            run(src).is_empty(),
            "int(b(x), extra) is not a trivial unary builtin and a should not be flagged",
        );
    }

    #[test]
    fn unary_builtin_with_keyword_arg_is_not_treated_as_adapter() {
        // Same gate, keyword-arg side. Mirror of the positional
        // test: the inner forwarding call must be present so the
        // mutation has somewhere to fall through to.
        let src = "def a(x):\n    return int(b(x), base=16)\n";
        assert!(run(src).is_empty());
    }

    #[test]
    fn vararg_alongside_passthrough_param_is_rejected() {
        // `def a(x, *xs): return b(x)` looks like a passthrough on
        // `x`, but the presence of `*xs` means the function is not a
        // pure forwarder — extra arguments are silently dropped.
        // `collect_param_idents` rejects on either `vararg` or
        // `kwarg`; tightening that `||` to `&&` would let `*xs`
        // alone slip through and incorrectly report `a`.
        let src = "def a(x, *xs):\n    return b(x)\n";
        assert!(
            run(src).is_empty(),
            "vararg-bearing function must not be reported as a wrapper",
        );
    }

    #[test]
    fn kwarg_alongside_passthrough_param_is_rejected() {
        // Mirror of the vararg case for `**kw`.
        let src = "def a(x, **kw):\n    return b(x)\n";
        assert!(run(src).is_empty());
    }

    #[test]
    fn kwonly_alongside_passthrough_param_is_rejected() {
        // `collect_param_idents` rejects on either `kwonlyargs` or
        // `posonlyargs`; tightening that `||` to `&&` would let a
        // kwonly-only signature slip through.
        let src = "def a(x, *, y):\n    return b(x)\n";
        assert!(run(src).is_empty());
    }

    #[test]
    fn posonly_alongside_passthrough_param_is_rejected() {
        // Mirror of the kwonly case for positional-only params.
        let src = "def a(x, /, y):\n    return b(y)\n";
        assert!(run(src).is_empty());
    }
}
