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
use oxc_span::SourceType;

use crate::line_index::LineIndex;
use crate::parser::TsParseError;

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
pub fn find_wrappers(source: &str) -> Result<Vec<WrapperFinding>, TsParseError> {
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, source, SourceType::ts()).parse();
    if !ret.errors.is_empty() {
        return Err(TsParseError::from_diagnostics(
            ret.errors.iter().map(|e| e.message.as_ref().to_owned()),
        ));
    }
    let line_index = LineIndex::new(source);
    let mut out = Vec::new();
    for stmt in &ret.program.body {
        collect_stmt(stmt, &line_index, &mut out);
    }
    Ok(out)
}

fn collect_stmt(stmt: &Statement, line_index: &LineIndex, out: &mut Vec<WrapperFinding>) {
    match stmt {
        Statement::FunctionDeclaration(f) => analyze_function(f, None, line_index, out),
        Statement::ClassDeclaration(c) => collect_class(c, line_index, out),
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                analyze_variable_declarator(d, line_index, out);
            }
        }
        Statement::ExportNamedDeclaration(e) => {
            if let Some(decl) = &e.declaration {
                collect_decl(decl, line_index, out);
            }
        }
        Statement::ExportDefaultDeclaration(e) => match &e.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                analyze_function(f, None, line_index, out);
            }
            ExportDefaultDeclarationKind::ClassDeclaration(c) => collect_class(c, line_index, out),
            _ => {}
        },
        Statement::TSModuleDeclaration(m) => {
            if let Some(body) = &m.body {
                collect_module_body(body, line_index, out);
            }
        }
        _ => {}
    }
}

fn collect_decl(decl: &Declaration, line_index: &LineIndex, out: &mut Vec<WrapperFinding>) {
    match decl {
        Declaration::FunctionDeclaration(f) => analyze_function(f, None, line_index, out),
        Declaration::ClassDeclaration(c) => collect_class(c, line_index, out),
        Declaration::VariableDeclaration(v) => {
            for d in &v.declarations {
                analyze_variable_declarator(d, line_index, out);
            }
        }
        Declaration::TSModuleDeclaration(m) => {
            if let Some(body) = &m.body {
                collect_module_body(body, line_index, out);
            }
        }
        _ => {}
    }
}

fn collect_module_body(
    body: &TSModuleDeclarationBody,
    line_index: &LineIndex,
    out: &mut Vec<WrapperFinding>,
) {
    match body {
        TSModuleDeclarationBody::TSModuleBlock(block) => {
            for stmt in &block.body {
                collect_stmt(stmt, line_index, out);
            }
        }
        TSModuleDeclarationBody::TSModuleDeclaration(nested) => {
            if let Some(body) = &nested.body {
                collect_module_body(body, line_index, out);
            }
        }
    }
}

fn collect_class(class: &Class, line_index: &LineIndex, out: &mut Vec<WrapperFinding>) {
    let class_name = class
        .id
        .as_ref()
        .map(|i| i.name.as_str())
        .unwrap_or("anonymous");
    for elem in &class.body.body {
        let ClassElement::MethodDefinition(m) = elem else {
            continue;
        };
        // Constructors that just forward `super(...)` are idiomatic
        // boilerplate; flagging them is noise, not signal.
        if matches!(m.kind, MethodDefinitionKind::Constructor) {
            continue;
        }
        let Some(name) = method_name(m) else {
            continue;
        };
        analyze_function_value(
            &format!("{class_name}::{name}"),
            &m.value,
            m.span.start,
            line_index,
            out,
        );
    }
}

fn analyze_function(
    func: &Function,
    owner: Option<&str>,
    line_index: &LineIndex,
    out: &mut Vec<WrapperFinding>,
) {
    let Some(id) = &func.id else { return };
    let raw_name = id.name.as_str();
    let qualified = match owner {
        Some(o) => format!("{o}::{raw_name}"),
        None => raw_name.to_owned(),
    };
    analyze_function_value(&qualified, func, func.span.start, line_index, out);
}

fn analyze_function_value(
    name: &str,
    func: &Function,
    start_offset: u32,
    line_index: &LineIndex,
    out: &mut Vec<WrapperFinding>,
) {
    let Some(body) = &func.body else { return };
    let params = collect_param_idents(&func.params);
    let Some(finding) = analyze(name, body, start_offset, &params, line_index) else {
        return;
    };
    out.push(finding);
}

fn analyze_variable_declarator(
    decl: &VariableDeclarator,
    line_index: &LineIndex,
    out: &mut Vec<WrapperFinding>,
) {
    let Some(init) = &decl.init else { return };
    let Some(id) = decl.id.get_binding_identifier() else {
        return;
    };
    let name = id.name.to_string();
    match init {
        Expression::ArrowFunctionExpression(arrow) => {
            let params = collect_param_idents(&arrow.params);
            let body = if arrow.expression {
                expression_body_tail(&arrow.body)
            } else {
                single_block_tail(&arrow.body)
            };
            let Some(tail) = body else { return };
            if let Some(finding) = analyze_tail(
                &name,
                tail,
                decl.span.start,
                arrow.body.span.end,
                &params,
                line_index,
            ) {
                out.push(finding);
            }
        }
        Expression::FunctionExpression(f) => {
            let Some(body) = &f.body else { return };
            let params = collect_param_idents(&f.params);
            if let Some(finding) = analyze(&name, body, decl.span.start, &params, line_index) {
                out.push(finding);
            }
        }
        _ => {}
    }
}

fn analyze(
    name: &str,
    body: &FunctionBody,
    start_offset: u32,
    params: &[String],
    line_index: &LineIndex,
) -> Option<WrapperFinding> {
    let tail = single_block_tail(body)?;
    analyze_tail(name, tail, start_offset, body.span.end, params, line_index)
}

fn analyze_tail(
    name: &str,
    tail: &Expression,
    start_offset: u32,
    end_offset: u32,
    params: &[String],
    line_index: &LineIndex,
) -> Option<WrapperFinding> {
    let (core, adapters) = peel_adapters(tail);
    let (callee, args) = core_call(core)?;
    if !args_pass_through(args, params) {
        return None;
    }
    Some(WrapperFinding {
        name: name.to_owned(),
        start_line: line_index.line(start_offset),
        end_line: line_index.line(end_offset),
        callee,
        adapters,
    })
}

fn method_name(method: &MethodDefinition) -> Option<String> {
    crate::walk::method_key_name(&method.key)
}

/// Extract the single tail expression from a block-bodied function:
/// either `return EXPR;` or a single `EXPR;` expression statement.
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

/// Extract the tail expression from an arrow function with `expression: true`
/// (e.g. `(x) => f(x)`). oxc still wraps the body in a `FunctionBody`; the
/// inner statement is an `ExpressionStatement` carrying the expression.
fn expression_body_tail<'a>(body: &'a FunctionBody<'a>) -> Option<&'a Expression<'a>> {
    let [stmt] = body.statements.as_slice() else {
        return None;
    };
    match stmt {
        Statement::ExpressionStatement(es) => Some(&es.expression),
        Statement::ReturnStatement(ret) => ret.argument.as_ref(),
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
        find_wrappers(src).unwrap()
    }

    fn names(findings: &[WrapperFinding]) -> Vec<&str> {
        findings.iter().map(|f| f.name.as_str()).collect()
    }

    #[test]
    fn detects_simple_forward() {
        let src = "function a(x: number): number { return b(x); }\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].callee, "b");
        assert!(findings[0].adapters.is_empty());
    }

    #[test]
    fn detects_method_delegation() {
        let src = r#"
class Service {
    inner = { handle(_x: number): number { return 0; } };
    handle(x: number): number { return this.inner.handle(x); }
}
"#;
        let findings = run(src);
        assert_eq!(names(&findings), ["Service::handle"]);
        assert_eq!(findings[0].callee, "this.inner.handle");
    }

    #[test]
    fn detects_with_as_adapter() {
        let src = "function a(x: number): bigint { return b(x) as bigint; }\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].callee, "b");
        assert_eq!(findings[0].adapters, vec![" as T".to_owned()]);
    }

    #[test]
    fn detects_with_non_null_assertion() {
        let src = "function a(x: number): number { return b(x)!; }\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].adapters, vec!["!".to_owned()]);
    }

    #[test]
    fn detects_with_await() {
        let src = "async function a(x: number): Promise<number> { return await b(x); }\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].adapters, vec!["await".to_owned()]);
    }

    #[test]
    fn detects_with_to_string_adapter() {
        let src = "function a(x: number): string { return b(x).toString(); }\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].adapters, vec![".toString()".to_owned()]);
    }

    #[test]
    fn detects_arrow_expression_body() {
        let src = "const a = (x: number): number => b(x);\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].callee, "b");
    }

    #[test]
    fn detects_arrow_block_body_with_return() {
        let src = "const a = (x: number): number => { return b(x); };\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
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
    fn detects_passthrough_with_reordered_args() {
        let src = "function a(x: number, y: number): number { return b(y, x); }\n";
        assert_eq!(names(&run(src)), ["a"]);
    }

    #[test]
    fn class_method_gets_qualified_name() {
        let src = r#"
class Foo {
    a(x: number): number { return b(x); }
}
"#;
        let findings = run(src);
        assert_eq!(names(&findings), ["Foo::a"]);
    }

    #[test]
    fn private_class_method_keeps_hash_prefix() {
        let src = r#"
class Foo {
    #a(x: number): number { return b(x); }
}
"#;
        let findings = run(src);
        assert_eq!(names(&findings), ["Foo::#a"]);
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
    fn finds_wrappers_in_namespace() {
        let src = r#"
namespace inner {
    export function shim(x: number): number { return core(x); }
}
"#;
        let findings = run(src);
        assert_eq!(names(&findings), ["shim"]);
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
    fn parenthesised_call_with_adapter_is_peeled() {
        let src = "function a(x: number): number { return (b(x))!; }\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
        assert_eq!(findings[0].callee, "b");
        assert_eq!(findings[0].adapters, vec!["!".to_owned()]);
    }

    #[test]
    fn function_expression_assigned_to_const_is_detected() {
        let src = "const a = function (x: number): number { return b(x); };\n";
        let findings = run(src);
        assert_eq!(names(&findings), ["a"]);
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = find_wrappers("function ??? {").unwrap_err();
        assert!(format!("{err}").contains("failed to parse TypeScript source"));
    }
}
