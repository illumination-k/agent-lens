//! oxc-based complexity extraction for TypeScript / JavaScript source files.
//!
//! For every function-shaped item — `function` declaration, class
//! method, arrow / function expression bound to a `const`/`let`/`var` —
//! we walk the body and produce a [`FunctionComplexity`]:
//!
//! * **Cyclomatic Complexity** — McCabe; starts at 1 and is incremented
//!   for each branching construct (`if`, `else if`, `while`, `for`,
//!   `for-in`, `for-of`, `do-while`, each `case` arm beyond the first,
//!   `&&`/`||`/`??`, `?:`, `catch`).
//! * **Cognitive Complexity** — Sonar-style; control structures add
//!   `1 + nesting` so deeply-nested code scores higher than the same
//!   number of flat branches. Logical operators add `1` per occurrence.
//! * **Max Nesting Depth** — the deepest control-flow nesting reached in
//!   the function body.
//! * **Halstead counts** — operators and operands are derived from the
//!   AST. Identifiers and literals are operands; binary / logical /
//!   unary / update / assignment operators are operators; control-flow
//!   keywords (`if`, `for`, `while`, `return`, …) are operators.
//!
//! Closures and inner functions defined *inside* a function body
//! contribute to the enclosing function's score, mirroring how a reader
//! actually experiences the code.
//!
//! The traversal that finds function-shaped items lives in
//! [`crate::walk`]; this module only converts each [`FunctionItem`] into
//! a [`FunctionComplexity`] by running [`ComplexityVisitor`] against its
//! body.

use std::collections::HashMap;

use lens_domain::{FunctionComplexity, HalsteadCounts};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::Visit;
use oxc_parser::Parser;

use crate::line_index::LineIndex;
use crate::parser::{Dialect, TsParseError};
use crate::walk::{FunctionItem, FunctionVisitor, walk_program};

/// Failures produced while extracting complexity units.
#[derive(Debug, thiserror::Error)]
pub enum ComplexityError {
    #[error(transparent)]
    Parse(#[from] TsParseError),
}

/// Extract one [`FunctionComplexity`] per function-shaped item in `source`.
pub fn extract_complexity_units(
    source: &str,
    dialect: Dialect,
) -> Result<Vec<FunctionComplexity>, ComplexityError> {
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, source, dialect.source_type()).parse();
    if !ret.errors.is_empty() {
        return Err(ComplexityError::Parse(TsParseError::from_diagnostics(
            ret.errors.iter().map(|e| e.message.as_ref().to_owned()),
        )));
    }
    let line_index = LineIndex::new(source);
    let mut visitor = ComplexityCollector::default();
    walk_program(&ret.program, &line_index, &mut visitor);
    Ok(visitor.out)
}

#[derive(Default)]
struct ComplexityCollector {
    out: Vec<FunctionComplexity>,
}

impl FunctionVisitor for ComplexityCollector {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        self.out.push(analyze(
            item.name,
            item.start_line,
            item.end_line,
            item.body,
        ));
    }
}

fn analyze(
    name: String,
    start_line: usize,
    end_line: usize,
    body: &FunctionBody,
) -> FunctionComplexity {
    let mut visitor = ComplexityVisitor::new();
    visitor.visit_function_body(body);
    let halstead = HalsteadCounts {
        distinct_operators: visitor.halstead.operators.len(),
        distinct_operands: visitor.halstead.operands.len(),
        total_operators: visitor.halstead.operators.values().sum(),
        total_operands: visitor.halstead.operands.values().sum(),
    };
    FunctionComplexity {
        name,
        start_line,
        end_line,
        cyclomatic: 1 + visitor.cyclomatic_branches,
        cognitive: visitor.cognitive,
        max_nesting: visitor.max_nesting,
        halstead,
    }
}

#[derive(Default)]
struct HalsteadAcc {
    operators: HashMap<String, usize>,
    operands: HashMap<String, usize>,
}

impl HalsteadAcc {
    fn op(&mut self, s: &str) {
        bump_count(&mut self.operators, s);
    }
    fn operand(&mut self, s: &str) {
        bump_count(&mut self.operands, s);
    }
}

/// Increment the count for `s` in `map`, or insert it at 1 if absent.
/// Centralises the inner body that `op` and `operand` used to repeat
/// verbatim — the methods stay as semantic markers but no longer share
/// a structurally identical body for the similarity analyser to flag.
fn bump_count(map: &mut HashMap<String, usize>, s: &str) {
    *map.entry(s.to_owned()).or_insert(0) += 1;
}

struct ComplexityVisitor {
    cyclomatic_branches: u32,
    cognitive: u32,
    nesting: u32,
    max_nesting: u32,
    halstead: HalsteadAcc,
}

impl ComplexityVisitor {
    fn new() -> Self {
        Self {
            cyclomatic_branches: 0,
            cognitive: 0,
            nesting: 0,
            max_nesting: 0,
            halstead: HalsteadAcc::default(),
        }
    }

    fn enter_nest(&mut self) {
        self.nesting += 1;
        if self.nesting > self.max_nesting {
            self.max_nesting = self.nesting;
        }
    }

    fn exit_nest(&mut self) {
        self.nesting = self.nesting.saturating_sub(1);
    }
}

impl<'a> Visit<'a> for ComplexityVisitor {
    fn visit_if_statement(&mut self, it: &IfStatement<'a>) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.halstead.op("if");

        self.visit_expression(&it.test);

        self.enter_nest();
        self.visit_statement(&it.consequent);
        self.exit_nest();

        if let Some(alt) = &it.alternate {
            // `else if` is rendered by Sonar as the chained if's own +1
            // (no extra penalty for the bare `else`); a plain `else`
            // counts as +1.
            if !matches!(alt, Statement::IfStatement(_)) {
                self.cognitive += 1;
                self.halstead.op("else");
            }
            self.enter_nest();
            self.visit_statement(alt);
            self.exit_nest();
        }
    }

    fn visit_while_statement(&mut self, it: &WhileStatement<'a>) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.halstead.op("while");
        self.visit_expression(&it.test);
        self.enter_nest();
        self.visit_statement(&it.body);
        self.exit_nest();
    }

    fn visit_do_while_statement(&mut self, it: &DoWhileStatement<'a>) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.halstead.op("do");
        self.enter_nest();
        self.visit_statement(&it.body);
        self.exit_nest();
        self.visit_expression(&it.test);
    }

    fn visit_for_statement(&mut self, it: &ForStatement<'a>) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.halstead.op("for");
        if let Some(init) = &it.init {
            self.visit_for_statement_init(init);
        }
        if let Some(test) = &it.test {
            self.visit_expression(test);
        }
        if let Some(update) = &it.update {
            self.visit_expression(update);
        }
        self.enter_nest();
        self.visit_statement(&it.body);
        self.exit_nest();
    }

    fn visit_for_in_statement(&mut self, it: &ForInStatement<'a>) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.halstead.op("for-in");
        self.visit_expression(&it.right);
        self.enter_nest();
        self.visit_statement(&it.body);
        self.exit_nest();
    }

    fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.halstead.op("for-of");
        self.visit_expression(&it.right);
        self.enter_nest();
        self.visit_statement(&it.body);
        self.exit_nest();
    }

    fn visit_switch_statement(&mut self, it: &SwitchStatement<'a>) {
        // McCabe: every case beyond the first introduces a new path. We
        // count cases that have a `test` (default arms don't add a path).
        let arms = it.cases.iter().filter(|c| c.test.is_some()).count();
        let arms = u32::try_from(arms).unwrap_or(u32::MAX);
        self.cyclomatic_branches += arms.saturating_sub(1);
        self.cognitive += 1 + self.nesting;
        self.halstead.op("switch");

        self.visit_expression(&it.discriminant);
        self.enter_nest();
        for case in &it.cases {
            if let Some(t) = &case.test {
                self.visit_expression(t);
            }
            for stmt in &case.consequent {
                self.visit_statement(stmt);
            }
        }
        self.exit_nest();
    }

    fn visit_try_statement(&mut self, it: &TryStatement<'a>) {
        self.halstead.op("try");
        self.enter_nest();
        self.visit_block_statement(&it.block);
        self.exit_nest();
        if let Some(handler) = &it.handler {
            self.cyclomatic_branches += 1;
            self.cognitive += 1 + self.nesting;
            self.halstead.op("catch");
            self.enter_nest();
            self.visit_block_statement(&handler.body);
            self.exit_nest();
        }
        if let Some(finalizer) = &it.finalizer {
            self.halstead.op("finally");
            self.enter_nest();
            self.visit_block_statement(finalizer);
            self.exit_nest();
        }
    }

    fn visit_logical_expression(&mut self, it: &LogicalExpression<'a>) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1;
        self.halstead.op(it.operator.as_str());
        self.visit_expression(&it.left);
        self.visit_expression(&it.right);
    }

    fn visit_conditional_expression(&mut self, it: &ConditionalExpression<'a>) {
        // `cond ? a : b` is a branching construct just like `if`.
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.halstead.op("?:");
        self.visit_expression(&it.test);
        self.enter_nest();
        self.visit_expression(&it.consequent);
        self.visit_expression(&it.alternate);
        self.exit_nest();
    }

    fn visit_binary_expression(&mut self, it: &BinaryExpression<'a>) {
        self.halstead.op(it.operator.as_str());
        self.visit_expression(&it.left);
        self.visit_expression(&it.right);
    }

    fn visit_unary_expression(&mut self, it: &UnaryExpression<'a>) {
        self.halstead.op(it.operator.as_str());
        self.visit_expression(&it.argument);
    }

    fn visit_update_expression(&mut self, it: &UpdateExpression<'a>) {
        self.halstead.op(it.operator.as_str());
    }

    fn visit_assignment_expression(&mut self, it: &AssignmentExpression<'a>) {
        self.halstead.op(it.operator.as_str());
        self.visit_expression(&it.right);
    }

    fn visit_variable_declaration(&mut self, it: &VariableDeclaration<'a>) {
        // `const` / `let` / `var` is a binding operator in Halstead's
        // sense; the initializer's `=` carries no AssignmentExpression
        // node, so without this hook short bodies produce zero operators.
        self.halstead.op(it.kind.as_str());
        for d in &it.declarations {
            self.visit_variable_declarator(d);
        }
    }

    fn visit_return_statement(&mut self, it: &ReturnStatement<'a>) {
        self.halstead.op("return");
        if let Some(arg) = &it.argument {
            self.visit_expression(arg);
        }
    }

    fn visit_throw_statement(&mut self, it: &ThrowStatement<'a>) {
        self.halstead.op("throw");
        self.visit_expression(&it.argument);
    }

    fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
        self.halstead.operand(it.name.as_str());
    }

    fn visit_identifier_name(&mut self, it: &IdentifierName<'a>) {
        self.halstead.operand(it.name.as_str());
    }

    fn visit_binding_identifier(&mut self, it: &BindingIdentifier<'a>) {
        self.halstead.operand(it.name.as_str());
    }

    fn visit_string_literal(&mut self, it: &StringLiteral<'a>) {
        self.halstead.operand(it.value.as_str());
    }

    fn visit_numeric_literal(&mut self, it: &NumericLiteral<'a>) {
        self.halstead.operand(&it.raw_str());
    }

    fn visit_boolean_literal(&mut self, it: &BooleanLiteral) {
        self.halstead
            .operand(if it.value { "true" } else { "false" });
    }

    fn visit_null_literal(&mut self, _it: &NullLiteral) {
        self.halstead.operand("null");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn extract(src: &str) -> Vec<FunctionComplexity> {
        extract_complexity_units(src, Dialect::Ts).unwrap()
    }

    fn one(src: &str) -> FunctionComplexity {
        let mut units = extract(src);
        assert_eq!(
            units.len(),
            1,
            "expected exactly one function, got {}",
            units.len()
        );
        units.remove(0)
    }

    #[rstest]
    #[case::linear_function("function noop() { const _ = 1 + 2; }", 1, 0, 0)]
    #[case::single_if(
        r#"
function f(x: number): number {
    if (x > 0) { return 1; } else { return 0; }
}
"#,
        2,
        2,
        1
    )]
    #[case::switch_at_top_level(
        r#"
function f(n: number): number {
    switch (n) {
        case 0: return 0;
        case 1: return 1;
        case 2: return 2;
        default: return 3;
    }
}
"#,
        3,
        1,
        1
    )]
    #[case::logical_operators(
        r#"
function f(a: boolean, b: boolean, c: boolean): boolean { return a && b || c; }
"#,
        3,
        2,
        0
    )]
    #[case::conditional_expression(
        "function f(x: number): number { return x > 0 ? 1 : 0; }",
        2,
        1,
        1
    )]
    #[case::try_catch(
        r#"
function f(): number {
    try { return 1; } catch (e) { return 0; }
}
"#,
        2,
        1,
        1
    )]
    #[case::nested_loops(
        r#"
function f(): void {
    for (let i = 0; i < 10; i++) {
        for (let j = 0; j < 10; j++) {
            if (i === j) {}
        }
    }
}
"#,
        4,
        6,
        3
    )]
    #[case::while_statement(
        r#"
function f(): void {
    let i = 0;
    while (i < 10) { i++; }
}
"#,
        2,
        1,
        1
    )]
    #[case::while_inside_if(
        r#"
function f(go: boolean): void {
    if (go) {
        let i = 0;
        while (i < 10) { i++; }
    }
}
"#,
        3,
        3,
        2
    )]
    #[case::do_while_statement(
        r#"
function f(): void {
    let i = 0;
    do { i++; } while (i < 10);
}
"#,
        2,
        1,
        1
    )]
    #[case::for_statement(
        r#"
function f(): void {
    for (let i = 0; i < 5; i++) {}
}
"#,
        2,
        1,
        1
    )]
    #[case::for_inside_if(
        r#"
function f(go: boolean): void {
    if (go) {
        for (let i = 0; i < 5; i++) {}
    }
}
"#,
        3,
        3,
        2
    )]
    #[case::for_in_statement(
        r#"
function f(o: Record<string, number>): void {
    for (const k in o) {}
}
"#,
        2,
        1,
        1
    )]
    #[case::for_in_inside_if(
        r#"
function f(o: Record<string, number>, go: boolean): void {
    if (go) {
        for (const k in o) {}
    }
}
"#,
        3,
        3,
        2
    )]
    #[case::for_of_statement(
        r#"
function f(xs: number[]): void {
    for (const x of xs) {}
}
"#,
        2,
        1,
        1
    )]
    #[case::if_without_else(
        r#"
function f(n: number): number {
    if (n > 0) { return 1; }
    return 0;
}
"#,
        2,
        1,
        1
    )]
    #[case::else_if_chain(
        r#"
function f(n: number): number {
    if (n > 0) { return 1; } else if (n < 0) { return -1; } else { return 0; }
}
"#,
        3,
        4,
        2
    )]
    #[case::switch_inside_if(
        r#"
function f(go: boolean, n: number): number {
    if (go) {
        switch (n) {
            case 0: return 0;
            case 1: return 1;
        }
    }
    return -1;
}
"#,
        3,
        3,
        2
    )]
    #[case::try_catch_inside_if(
        r#"
function f(go: boolean): number {
    if (go) {
        try { return 1; } catch (e) { return 0; }
    }
    return -1;
}
"#,
        3,
        3,
        2
    )]
    #[case::logical_expression_steps(
        r#"
function f(a: boolean, b: boolean, c: boolean, d: boolean): boolean {
    return a && b || c && d;
}
"#,
        4,
        3,
        0
    )]
    fn complexity_metrics_match(
        #[case] src: &str,
        #[case] cyclomatic: u32,
        #[case] cognitive: u32,
        #[case] max_nesting: u32,
    ) {
        let f = one(src);
        assert_eq!(f.cyclomatic, cyclomatic);
        assert_eq!(f.cognitive, cognitive);
        assert_eq!(f.max_nesting, max_nesting);
    }

    #[test]
    fn cognitive_grows_with_nesting() {
        let units = extract(
            r#"
function flat(n: number): void {
    if (n > 0) {}
    if (n < 0) {}
}
function nested(n: number): void {
    if (n > 0) {
        if (n < 5) {}
    }
}
"#,
        );
        let flat = units.iter().find(|f| f.name == "flat").unwrap();
        let nested = units.iter().find(|f| f.name == "nested").unwrap();
        // Flat: 1 + 1 = 2; Nested: 1 + (1+1) = 3
        assert_eq!(flat.cognitive, 2);
        assert_eq!(nested.cognitive, 3);
    }

    #[rstest]
    #[case::class_method(
        r#"
class Foo {
    bar(): void {}
}
"#,
        "Foo::bar",
        None
    )]
    #[case::arrow_binding("const add = (a: number, b: number): number => a + b;", "add", None)]
    #[case::nested_namespace_function(
        r#"
namespace inner {
    export function hidden(n: number): number { return n > 0 ? 1 : 0; }
}
"#,
        "hidden",
        Some(2)
    )]
    #[case::export_default_function(
        "export default function defaulted(): void {}",
        "defaulted",
        None
    )]
    #[case::export_default_class_method(
        r#"
export default class Foo {
    bar(): void {}
}
"#,
        "Foo::bar",
        None
    )]
    #[case::exported_class_method(
        r#"
export class Foo {
    bar(): void {}
}
"#,
        "Foo::bar",
        None
    )]
    #[case::exported_variable(
        "export const adder = (a: number, b: number): number => a + b;",
        "adder",
        None
    )]
    #[case::function_expression_const("const fe = function () { return 1; };", "fe", None)]
    #[case::private_class_method(
        r#"
class Foo {
    #secret(): void {}
}
"#,
        "Foo::#secret",
        None
    )]
    #[case::string_literal_class_method(
        r#"
class Foo {
    "weird name"(): void {}
}
"#,
        "Foo::weird name",
        None
    )]
    #[case::namespace_module_declaration(
        r#"
namespace outer {
    export function inner(): void {}
}
"#,
        "inner",
        None
    )]
    fn extracted_function_matches(
        #[case] src: &str,
        #[case] expected_name: &str,
        #[case] expected_cyclomatic: Option<u32>,
    ) {
        let units = extract(src);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, expected_name);
        if let Some(expected) = expected_cyclomatic {
            assert_eq!(units[0].cyclomatic, expected);
        }
    }

    #[test]
    fn line_range_covers_signature_through_closing_brace() {
        let f = one("function f() {\n    const x = 1;\n    const y = 2;\n}\n");
        assert_eq!(f.start_line, 1);
        assert_eq!(f.end_line, 4);
        assert_eq!(f.loc(), 4);
    }

    #[test]
    fn halstead_treats_keywords_as_operators_and_idents_as_operands() {
        let f = one("function f() { const x = 1; }");
        assert!(f.halstead.distinct_operators >= 1);
        assert!(f.halstead.distinct_operands >= 2);
    }

    #[test]
    fn halstead_volume_is_defined_for_a_realistic_function() {
        let f = one(r#"
function add(a: number, b: number): number {
    const s = a + b;
    return s;
}
"#);
        let v = f.halstead.volume();
        assert!(v.is_some(), "expected Volume to be defined");
        let mi = f.maintainability_index().unwrap();
        assert!((0.0..=100.0).contains(&mi), "MI out of bounds: {mi}");
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = extract_complexity_units("function ??? {", Dialect::Ts).unwrap_err();
        assert!(matches!(err, ComplexityError::Parse(_)));
    }

    #[test]
    fn empty_file_yields_no_units() {
        let units = extract("// just a comment\n");
        assert!(units.is_empty());
    }

    #[test]
    fn complexity_error_display_includes_inner() {
        let err = extract_complexity_units("function ??? {", Dialect::Ts).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.is_empty(), "expected non-empty error message");
    }

    #[test]
    fn complexity_error_source_is_present() {
        use std::error::Error as _;
        let err = extract_complexity_units("function ??? {", Dialect::Ts).unwrap_err();
        assert!(err.source().is_some());
    }

    #[test]
    fn halstead_total_operator_count_grows_per_occurrence() {
        // Multiple bindings — each `const` is one operator occurrence.
        let f = one(r#"
function f(): void {
    const a = 1;
    const b = 2;
    const c = 3;
}
"#);
        // Three `const` plus literals/identifiers — totals must be > 1
        // to catch HalsteadAcc::op being mutated to a no-op (`*= 1`).
        assert!(
            f.halstead.total_operators >= 3,
            "expected total_operators >= 3, got {}",
            f.halstead.total_operators,
        );
    }

    #[test]
    fn halstead_total_operand_count_grows_per_occurrence() {
        // Three identifiers and three literals → operands appear repeatedly.
        let f = one(r#"
function f(): void {
    const a = 1;
    const b = 2;
    const c = 3;
}
"#);
        assert!(
            f.halstead.total_operands >= 6,
            "expected total_operands >= 6, got {}",
            f.halstead.total_operands,
        );
    }
}
