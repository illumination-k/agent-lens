//! Detect "thin wrapper" TypeScript / JavaScript functions: bodies
//! that, after peeling a short chain of trivial adapters, are just a
//! forwarding call to another function with the parameters passed
//! straight through.
//!
//! Conceptually a wrapper is a function whose body adds no logic of its
//! own — it only renames, narrows visibility, or coerces types. Things
//! that DO add logic (extra statements, branching, argument
//! transformations, literal arguments) keep the function out of the
//! report.
//!
//! Mirrors [`lens_rust::find_wrappers`] in shape and intent. Adapter
//! lists differ because TS uses `await` / `as T` / `!` / `?.` rather
//! than `?` / `.unwrap()` / `.into()`.

use lens_domain::{WrapperFinding, args_pass_through_by};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;

use crate::line_index::LineIndex;
use crate::parser::{Dialect, TsParseError};
use crate::walk::{FunctionItem, FunctionVisitor, walk_program};

/// Method names with no arguments that we treat as "no semantic content":
/// type/borrow coercions and stringification.
const TRIVIAL_NULLARY_ADAPTERS: &[&str] = &[
    "toString",
    "valueOf",
    "toJSON",
    "toFixed",
    "toLocaleString",
    "asReadonly",
];

/// Walk the source and return every function whose body is just a
/// forwarding call.
pub fn find_wrappers(source: &str, dialect: Dialect) -> Result<Vec<WrapperFinding>, TsParseError> {
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, source, dialect.source_type()).parse();
    if !ret.errors.is_empty() {
        return Err(TsParseError::from_diagnostics(
            ret.errors.iter().map(|e| e.message.as_ref().to_owned()),
        ));
    }
    let line_index = LineIndex::new(source);
    let mut visitor = WrapperCollector { out: Vec::new() };
    walk_program(&ret.program, &line_index, &mut visitor);
    Ok(visitor.out)
}

struct WrapperCollector {
    out: Vec<WrapperFinding>,
}

impl FunctionVisitor for WrapperCollector {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        // Constructors that just forward `super(...)` are idiomatic
        // boilerplate; flagging them is noise, not signal.
        if item.is_constructor {
            return;
        }
        let params = collect_param_idents(item.params);
        let Some(tail) = single_block_tail(item.body) else {
            return;
        };
        if let Some(finding) =
            analyze_tail(&item.name, tail, item.start_line, item.end_line, &params)
        {
            self.out.push(finding);
        }
    }
}

fn analyze_tail(
    name: &str,
    tail: &Expression,
    start_line: usize,
    end_line: usize,
    params: &[String],
) -> Option<WrapperFinding> {
    let (core, adapters) = peel_adapters(tail);
    let (callee, args) = core_call(core)?;
    if !args_pass_through(args, params) {
        return None;
    }
    Some(WrapperFinding {
        name: name.to_owned(),
        start_line,
        end_line,
        callee,
        adapters,
    })
}

/// Extract the single tail expression from a function body. Both
/// block-bodied functions (`return EXPR;` or a bare `EXPR;`) and
/// expression-bodied arrows (`(x) => f(x)`, which oxc still wraps in a
/// `FunctionBody` carrying a single `ExpressionStatement`) flow through
/// here — the two `Statement` variants are disjoint, so a single match
/// covers both shapes.
fn single_block_tail<'a>(body: &'a FunctionBody<'a>) -> Option<&'a Expression<'a>> {
    let [stmt] = body.statements.as_slice() else {
        return None;
    };
    match stmt {
        Statement::ReturnStatement(ret) => ret.argument.as_ref(),
        Statement::ExpressionStatement(es) => Some(&es.expression),
        _ => None,
    }
}

/// Strip trivial adapters from the outside. Returns the innermost
/// expression and the list of adapter labels in source order
/// (innermost-first), matching how `lens-rust` renders them.
fn peel_adapters<'a>(expr: &'a Expression<'a>) -> (&'a Expression<'a>, Vec<String>) {
    let mut current = expr;
    let mut outer_to_inner: Vec<String> = Vec::new();
    loop {
        match current {
            Expression::ParenthesizedExpression(p) => current = &p.expression,
            Expression::TSAsExpression(a) => {
                outer_to_inner.push(" as T".to_owned());
                current = &a.expression;
            }
            Expression::TSSatisfiesExpression(s) => {
                outer_to_inner.push(" satisfies T".to_owned());
                current = &s.expression;
            }
            Expression::TSNonNullExpression(n) => {
                outer_to_inner.push("!".to_owned());
                current = &n.expression;
            }
            Expression::TSTypeAssertion(t) => {
                outer_to_inner.push(" as T".to_owned());
                current = &t.expression;
            }
            Expression::AwaitExpression(a) => {
                outer_to_inner.push("await".to_owned());
                current = &a.argument;
            }
            Expression::ChainExpression(c) => match &c.expression {
                ChainElement::CallExpression(call) => current = call_as_expression(call),
                ChainElement::TSNonNullExpression(n) => {
                    outer_to_inner.push("!".to_owned());
                    current = &n.expression;
                }
                _ => break,
            },
            Expression::CallExpression(call) if is_trivial_method_call(call) => {
                let name = method_call_name(call).unwrap_or_default();
                outer_to_inner.push(format!(".{name}()"));
                current = nested_call_receiver(call).unwrap_or(current);
                if std::ptr::eq(current, expr) {
                    break;
                }
            }
            _ => break,
        }
    }
    outer_to_inner.reverse();
    (current, outer_to_inner)
}

/// `oxc` represents a `ChainExpression` whose tail is a call by storing
/// the call directly. We only need to look through it to keep peeling,
/// so this just returns the receiver of the call as an `Expression`.
fn call_as_expression<'a>(call: &'a CallExpression<'a>) -> &'a Expression<'a> {
    &call.callee
}

fn is_trivial_method_call(call: &CallExpression) -> bool {
    if !call.arguments.is_empty() {
        return false;
    }
    let Some(name) = method_call_name(call) else {
        return false;
    };
    TRIVIAL_NULLARY_ADAPTERS.contains(&name.as_str())
}

fn method_call_name(call: &CallExpression) -> Option<String> {
    match &call.callee {
        Expression::StaticMemberExpression(m) => Some(m.property.name.to_string()),
        _ => None,
    }
}

fn nested_call_receiver<'a>(call: &'a CallExpression<'a>) -> Option<&'a Expression<'a>> {
    match &call.callee {
        Expression::StaticMemberExpression(m) => Some(&m.object),
        _ => None,
    }
}

/// If `expr` is a function call or a method call whose callee/receiver
/// is itself a "thin" path (no nested computation), return its rendered
/// callee path and the argument list.
fn core_call<'a>(
    expr: &'a Expression<'a>,
) -> Option<(String, &'a oxc_allocator::Vec<'a, Argument<'a>>)> {
    let Expression::CallExpression(call) = expr else {
        return None;
    };
    let callee = render_callee(&call.callee)?;
    Some((callee, &call.arguments))
}

/// Path-shaped expressions: a name, a member chain (`self.inner.x`), or
/// the same wrapped in parens. Method calls and function calls anywhere
/// inside are rejected — those add computation, not just navigation.
fn render_callee(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Identifier(id) => Some(id.name.to_string()),
        Expression::ThisExpression(_) => Some("this".to_owned()),
        Expression::Super(_) => Some("super".to_owned()),
        Expression::StaticMemberExpression(m) => {
            let base = render_callee(&m.object)?;
            Some(format!("{base}.{}", m.property.name))
        }
        Expression::PrivateFieldExpression(p) => {
            let base = render_callee(&p.object)?;
            Some(format!("{base}.#{}", p.field.name))
        }
        Expression::ParenthesizedExpression(p) => render_callee(&p.expression),
        _ => None,
    }
}

fn collect_param_idents(params: &FormalParameters) -> Vec<String> {
    let mut out = Vec::new();
    for item in &params.items {
        // Destructuring patterns and rest parameters are not eligible
        // for passthrough — there is no way to forward them verbatim
        // while still being a "thin" wrapper.
        let Some(id) = item.pattern.get_binding_identifier() else {
            return Vec::new();
        };
        // A defaulted parameter (`x = 0`) injects a literal at the call
        // site if the caller omits the argument; treating it as a plain
        // passthrough hides that real difference.
        if item.initializer.is_some() {
            return Vec::new();
        }
        out.push(id.name.to_string());
    }
    if params.rest.is_some() {
        return Vec::new();
    }
    out
}

/// True iff every call argument is a parameter passed through, every
/// parameter is used exactly once, and the arity matches.
fn args_pass_through(args: &[Argument], params: &[String]) -> bool {
    args_pass_through_by(args, params, passthrough_ident)
}

fn passthrough_ident(arg: &Argument) -> Option<String> {
    let expr = arg.as_expression()?;
    expr_passthrough_ident(expr)
}

fn expr_passthrough_ident(expr: &Expression) -> Option<String> {
    match expr {
        Expression::Identifier(id) => Some(id.name.to_string()),
        Expression::ParenthesizedExpression(p) => expr_passthrough_ident(&p.expression),
        Expression::TSAsExpression(a) => expr_passthrough_ident(&a.expression),
        Expression::TSNonNullExpression(n) => expr_passthrough_ident(&n.expression),
        Expression::TSSatisfiesExpression(s) => expr_passthrough_ident(&s.expression),
        Expression::TSTypeAssertion(t) => expr_passthrough_ident(&t.expression),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn run(src: &str) -> Vec<WrapperFinding> {
        find_wrappers(src, Dialect::Ts).unwrap()
    }

    fn names(findings: &[WrapperFinding]) -> Vec<&str> {
        findings.iter().map(|f| f.name.as_str()).collect()
    }

    #[rstest]
    #[case::simple_forward(
        "function a(x: number): number { return b(x); }\n",
        "a",
        "b",
        &[]
    )]
    #[case::method_delegation(
        r#"
class Service {
    inner = { handle(_x: number): number { return 0; } };
    handle(x: number): number { return this.inner.handle(x); }
}
"#,
        "Service::handle",
        "this.inner.handle",
        &[]
    )]
    #[case::as_adapter(
        "function a(x: number): bigint { return b(x) as bigint; }\n",
        "a",
        "b",
        &[" as T"]
    )]
    #[case::non_null_assertion(
        "function a(x: number): number { return b(x)!; }\n",
        "a",
        "b",
        &["!"]
    )]
    #[case::await_adapter(
        "async function a(x: number): Promise<number> { return await b(x); }\n",
        "a",
        "b",
        &["await"]
    )]
    #[case::to_string_adapter(
        "function a(x: number): string { return b(x).toString(); }\n",
        "a",
        "b",
        &[".toString()"]
    )]
    #[case::arrow_expression("const a = (x: number): number => b(x);\n", "a", "b", &[])]
    #[case::arrow_block("const a = (x: number): number => { return b(x); };\n", "a", "b", &[])]
    #[case::reordered_args(
        "function a(x: number, y: number): number { return b(y, x); }\n",
        "a",
        "b",
        &[]
    )]
    #[case::class_method(
        r#"
class Foo {
    a(x: number): number { return b(x); }
}
"#,
        "Foo::a",
        "b",
        &[]
    )]
    #[case::private_class_method(
        r#"
class Foo {
    #a(x: number): number { return b(x); }
}
"#,
        "Foo::#a",
        "b",
        &[]
    )]
    #[case::namespace_function(
        r#"
namespace inner {
    export function shim(x: number): number { return core(x); }
}
"#,
        "shim",
        "core",
        &[]
    )]
    #[case::parenthesised_adapter(
        "function a(x: number): number { return (b(x))!; }\n",
        "a",
        "b",
        &["!"]
    )]
    #[case::function_expression_const(
        "const a = function (x: number): number { return b(x); };\n",
        "a",
        "b",
        &[]
    )]
    fn detects_wrapper(
        #[case] src: &str,
        #[case] expected_name: &str,
        #[case] expected_callee: &str,
        #[case] expected_adapters: &[&str],
    ) {
        let findings = run(src);
        assert_eq!(names(&findings), [expected_name]);
        assert_eq!(findings[0].callee, expected_callee);
        let expected_adapters: Vec<String> =
            expected_adapters.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(findings[0].adapters, expected_adapters);
    }

    /// Body shapes that disqualify a function from being a wrapper. The
    /// detector must return an empty report for each.
    #[rstest]
    #[case::arg_transformation("function a(x: number): number { return b(x + 1); }\n")]
    #[case::multi_statement_body("function a(x: number): number { const y = x; return b(y); }\n")]
    #[case::literal_only_body("function a(): number { return 42; }\n")]
    #[case::empty_body("function a(): void {}\n")]
    #[case::unrelated_arg_name("function a(x: number): number { return b(y); }\n")]
    #[case::arity_mismatch_too_few("function a(x: number): number { return b(); }\n")]
    #[case::arity_mismatch_too_many("function a(x: number): number { return b(x, x); }\n")]
    #[case::branching_body(
        "function a(x: number): number { if (x > 0) { return b(x); } else { return c(x); } }\n"
    )]
    #[case::chain_receiver_call("function a(x: number): number { return foo(x).bar(x); }\n")]
    #[case::destructuring_param("function a({ x }: { x: number }): number { return b(x); }\n")]
    #[case::default_value_param("function a(x: number = 0): number { return b(x); }\n")]
    fn rejects_non_wrapper_shape(#[case] src: &str) {
        assert!(run(src).is_empty(), "expected no wrapper for: {src}");
    }

    #[test]
    fn constructor_is_not_reported() {
        // `super(x)` in a constructor body looks structurally like a
        // wrapper, but it is mandatory boilerplate, not a refactor
        // opportunity. Mirrors how lens-rust skips boilerplate trait
        // methods.
        let src = r#"
class Child extends Parent {
    constructor(x: number) { super(x); }
}
"#;
        let findings = run(src);
        assert!(findings.is_empty(), "got: {:?}", names(&findings));
    }

    #[test]
    fn records_line_numbers_from_signature_to_block_end() {
        let src = "\nfunction first(x: number): number {\n    return b(x);\n}\n";
        let findings = run(src);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].start_line, 2);
        assert_eq!(findings[0].end_line, 4);
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = find_wrappers("function ??? {", Dialect::Ts).unwrap_err();
        assert!(format!("{err}").contains("failed to parse TypeScript source"));
    }

    #[test]
    fn tsx_dialect_lets_jsx_wrappers_be_detected() {
        // A component whose body is a forwarding call should still
        // surface when the file uses JSX. Plain `Dialect::Ts` would
        // fail to parse the JSX bodies that surround it.
        let src = "function shim(x: number): number { return core(x); }\n\
                   function Comp(): JSX.Element { return <div />; }\n";
        let findings = find_wrappers(src, Dialect::Tsx).unwrap();
        assert_eq!(names(&findings), ["shim"]);
    }
}
