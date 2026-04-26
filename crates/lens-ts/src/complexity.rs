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

use std::collections::HashMap;

use lens_domain::{FunctionComplexity, HalsteadCounts};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::Visit;
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::line_index::LineIndex;
use crate::parser::TsParseError;

/// Failures produced while extracting complexity units.
#[derive(Debug)]
pub enum ComplexityError {
    Parse(TsParseError),
}

impl std::fmt::Display for ComplexityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ComplexityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
        }
    }
}

impl From<TsParseError> for ComplexityError {
    fn from(value: TsParseError) -> Self {
        Self::Parse(value)
    }
}

/// Extract one [`FunctionComplexity`] per function-shaped item in `source`.
pub fn extract_complexity_units(source: &str) -> Result<Vec<FunctionComplexity>, ComplexityError> {
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, source, SourceType::ts()).parse();
    if !ret.errors.is_empty() {
        return Err(ComplexityError::Parse(TsParseError::from_diagnostics(
            ret.errors.iter().map(|e| e.message.as_ref().to_owned()),
        )));
    }
    let line_index = LineIndex::new(source);
    let mut out = Vec::new();
    for stmt in &ret.program.body {
        collect_stmt(stmt, None, &line_index, &mut out);
    }
    Ok(out)
}

fn collect_stmt(
    stmt: &Statement,
    owner: Option<&str>,
    line_index: &LineIndex,
    out: &mut Vec<FunctionComplexity>,
) {
    match stmt {
        Statement::FunctionDeclaration(f) => {
            push_function(f, owner, line_index, out);
        }
        Statement::ClassDeclaration(c) => collect_class(c, line_index, out),
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                push_variable_declarator(d, line_index, out);
            }
        }
        Statement::ExportNamedDeclaration(e) => {
            if let Some(decl) = &e.declaration {
                collect_decl(decl, owner, line_index, out);
            }
        }
        Statement::ExportDefaultDeclaration(e) => match &e.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                push_function(f, owner, line_index, out);
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

fn collect_decl(
    decl: &Declaration,
    owner: Option<&str>,
    line_index: &LineIndex,
    out: &mut Vec<FunctionComplexity>,
) {
    match decl {
        Declaration::FunctionDeclaration(f) => push_function(f, owner, line_index, out),
        Declaration::ClassDeclaration(c) => collect_class(c, line_index, out),
        Declaration::VariableDeclaration(v) => {
            for d in &v.declarations {
                push_variable_declarator(d, line_index, out);
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
    out: &mut Vec<FunctionComplexity>,
) {
    match body {
        TSModuleDeclarationBody::TSModuleBlock(block) => {
            for stmt in &block.body {
                collect_stmt(stmt, None, line_index, out);
            }
        }
        TSModuleDeclarationBody::TSModuleDeclaration(nested) => {
            if let Some(body) = &nested.body {
                collect_module_body(body, line_index, out);
            }
        }
    }
}

fn collect_class(class: &Class, line_index: &LineIndex, out: &mut Vec<FunctionComplexity>) {
    let class_name = class
        .id
        .as_ref()
        .map(|i| i.name.as_str())
        .unwrap_or("anonymous");
    for elem in &class.body.body {
        if let ClassElement::MethodDefinition(m) = elem
            && let Some(body) = &m.value.body
            && let Some(name) = method_key_name(&m.key)
        {
            let qualified = format!("{class_name}::{name}");
            let start_line = line_index.line(m.span.start);
            let end_line = line_index.line(m.span.end);
            out.push(analyze(qualified, start_line, end_line, body));
        }
    }
}

fn method_key_name(key: &PropertyKey) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.to_string()),
        PropertyKey::PrivateIdentifier(id) => Some(format!("#{}", id.name)),
        PropertyKey::StringLiteral(s) => Some(s.value.to_string()),
        _ => None,
    }
}

fn push_function(
    func: &Function,
    owner: Option<&str>,
    line_index: &LineIndex,
    out: &mut Vec<FunctionComplexity>,
) {
    let Some(body) = &func.body else { return };
    let raw_name = func
        .id
        .as_ref()
        .map(|i| i.name.as_str())
        .unwrap_or("anonymous");
    let name = match owner {
        Some(o) => format!("{o}::{raw_name}"),
        None => raw_name.to_owned(),
    };
    let start_line = line_index.line(func.span.start);
    let end_line = line_index.line(body.span.end);
    out.push(analyze(name, start_line, end_line, body));
}

fn push_variable_declarator(
    decl: &VariableDeclarator,
    line_index: &LineIndex,
    out: &mut Vec<FunctionComplexity>,
) {
    let Some(init) = &decl.init else { return };
    let BindingPattern::BindingIdentifier(id) = &decl.id else {
        return;
    };
    let name = id.name.to_string();
    match init {
        Expression::ArrowFunctionExpression(arrow) => {
            let start_line = line_index.line(decl.span.start);
            let end_line = line_index.line(arrow.body.span.end);
            out.push(analyze(name, start_line, end_line, &arrow.body));
        }
        Expression::FunctionExpression(f) => {
            if let Some(body) = &f.body {
                let start_line = line_index.line(decl.span.start);
                let end_line = line_index.line(body.span.end);
                out.push(analyze(name, start_line, end_line, body));
            }
        }
        _ => {}
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
        *self.operators.entry(s.to_owned()).or_insert(0) += 1;
    }
    fn operand(&mut self, s: &str) {
        *self.operands.entry(s.to_owned()).or_insert(0) += 1;
    }
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

    fn extract(src: &str) -> Vec<FunctionComplexity> {
        extract_complexity_units(src).unwrap()
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

    #[test]
    fn linear_function_has_cc_one() {
        let f = one("function noop() { const _ = 1 + 2; }");
        assert_eq!(f.cyclomatic, 1);
        assert_eq!(f.cognitive, 0);
        assert_eq!(f.max_nesting, 0);
    }

    #[test]
    fn single_if_adds_one_to_cyclomatic() {
        let f = one(r#"
function f(x: number): number {
    if (x > 0) { return 1; } else { return 0; }
}
"#);
        assert_eq!(f.cyclomatic, 2);
    }

    #[test]
    fn switch_adds_arms_minus_one_to_cyclomatic() {
        let f = one(r#"
function f(n: number): number {
    switch (n) {
        case 0: return 0;
        case 1: return 1;
        case 2: return 2;
        default: return 3;
    }
}
"#);
        // base 1 + (3 case arms - 1) = 3
        assert_eq!(f.cyclomatic, 3);
    }

    #[test]
    fn logical_operators_each_add_one() {
        let f = one(r#"
function f(a: boolean, b: boolean, c: boolean): boolean { return a && b || c; }
"#);
        // base 1 + 1 (&&) + 1 (||) = 3
        assert_eq!(f.cyclomatic, 3);
    }

    #[test]
    fn conditional_expression_adds_one_to_cyclomatic() {
        let f = one("function f(x: number): number { return x > 0 ? 1 : 0; }");
        assert_eq!(f.cyclomatic, 2);
    }

    #[test]
    fn try_catch_adds_one_to_cyclomatic() {
        let f = one(r#"
function f(): number {
    try { return 1; } catch (e) { return 0; }
}
"#);
        assert_eq!(f.cyclomatic, 2);
    }

    #[test]
    fn nested_loops_track_max_nesting() {
        let f = one(r#"
function f(): void {
    for (let i = 0; i < 10; i++) {
        for (let j = 0; j < 10; j++) {
            if (i === j) {}
        }
    }
}
"#);
        assert_eq!(f.max_nesting, 3);
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

    #[test]
    fn class_methods_get_qualified_names() {
        let units = extract(
            r#"
class Foo {
    bar(): void {}
}
"#,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "Foo::bar");
    }

    #[test]
    fn arrow_function_binding_records_name() {
        let units = extract("const add = (a: number, b: number): number => a + b;");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "add");
    }

    #[test]
    fn nested_namespace_functions_are_picked_up() {
        let units = extract(
            r#"
namespace inner {
    export function hidden(n: number): number { return n > 0 ? 1 : 0; }
}
"#,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "hidden");
        // base 1 + ?: = 2
        assert_eq!(units[0].cyclomatic, 2);
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
        let v = f.halstead_volume();
        assert!(v.is_some(), "expected Volume to be defined");
        let mi = f.maintainability_index().unwrap();
        assert!((0.0..=100.0).contains(&mi), "MI out of bounds: {mi}");
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = extract_complexity_units("function ??? {").unwrap_err();
        assert!(matches!(err, ComplexityError::Parse(_)));
    }

    #[test]
    fn empty_file_yields_no_units() {
        let units = extract("// just a comment\n");
        assert!(units.is_empty());
    }

    #[test]
    fn complexity_error_display_includes_inner() {
        let err = extract_complexity_units("function ??? {").unwrap_err();
        let msg = err.to_string();
        assert!(!msg.is_empty(), "expected non-empty error message");
    }

    #[test]
    fn complexity_error_source_is_present() {
        use std::error::Error as _;
        let err = extract_complexity_units("function ??? {").unwrap_err();
        assert!(err.source().is_some());
    }

    #[test]
    fn while_statement_adds_one_to_cyclomatic_and_cognitive() {
        let f = one(r#"
function f(): void {
    let i = 0;
    while (i < 10) { i++; }
}
"#);
        assert_eq!(f.cyclomatic, 2, "1 base + 1 while");
        assert_eq!(f.cognitive, 1, "while at nest 0 contributes 1");
        assert_eq!(f.max_nesting, 1);
    }

    #[test]
    fn while_inside_if_pays_nesting_penalty() {
        let f = one(r#"
function f(go: boolean): void {
    if (go) {
        let i = 0;
        while (i < 10) { i++; }
    }
}
"#);
        assert_eq!(f.cyclomatic, 3);
        // if: +1 at nest 0; while: +(1+1)=2 at nest 1; total 3.
        assert_eq!(f.cognitive, 3);
        assert_eq!(f.max_nesting, 2);
    }

    #[test]
    fn do_while_adds_one_to_cyclomatic_and_cognitive() {
        let f = one(r#"
function f(): void {
    let i = 0;
    do { i++; } while (i < 10);
}
"#);
        assert_eq!(f.cyclomatic, 2);
        assert_eq!(f.cognitive, 1);
        assert_eq!(f.max_nesting, 1);
    }

    #[test]
    fn for_statement_adds_one_to_cyclomatic_and_cognitive() {
        let f = one(r#"
function f(): void {
    for (let i = 0; i < 5; i++) {}
}
"#);
        assert_eq!(f.cyclomatic, 2);
        assert_eq!(f.cognitive, 1);
        assert_eq!(f.max_nesting, 1);
    }

    #[test]
    fn for_inside_if_pays_nesting_penalty() {
        let f = one(r#"
function f(go: boolean): void {
    if (go) {
        for (let i = 0; i < 5; i++) {}
    }
}
"#);
        assert_eq!(f.cyclomatic, 3);
        // 1 (if) + 2 (for at nest 1) = 3
        assert_eq!(f.cognitive, 3);
    }

    #[test]
    fn for_in_adds_one_to_cyclomatic_and_cognitive() {
        let f = one(r#"
function f(o: Record<string, number>): void {
    for (const k in o) {}
}
"#);
        assert_eq!(f.cyclomatic, 2);
        assert_eq!(f.cognitive, 1);
        assert_eq!(f.max_nesting, 1);
    }

    #[test]
    fn for_in_inside_if_pays_nesting_penalty() {
        let f = one(r#"
function f(o: Record<string, number>, go: boolean): void {
    if (go) {
        for (const k in o) {}
    }
}
"#);
        assert_eq!(f.cyclomatic, 3);
        assert_eq!(f.cognitive, 3);
    }

    #[test]
    fn for_of_adds_one_to_cyclomatic_and_cognitive() {
        let f = one(r#"
function f(xs: number[]): void {
    for (const x of xs) {}
}
"#);
        assert_eq!(f.cyclomatic, 2);
        assert_eq!(f.cognitive, 1);
        assert_eq!(f.max_nesting, 1);
    }

    #[test]
    fn each_logical_operator_bumps_cognitive_by_one() {
        let f = one(r#"
function f(a: boolean, b: boolean, c: boolean): boolean { return a && b || c; }
"#);
        // && / || each contribute +1 in cognitive (no nesting penalty).
        assert_eq!(f.cognitive, 2);
    }

    #[test]
    fn plain_else_bumps_cognitive_by_one_else_if_does_not() {
        let f = one(r#"
function f(n: number): number {
    if (n > 0) { return 1; } else { return 0; }
}
"#);
        // outer if: +1 at nest 0; plain else: +1; total 2.
        assert_eq!(f.cognitive, 2);
    }

    #[test]
    fn if_without_else_does_not_pay_else_penalty() {
        let f = one(r#"
function f(n: number): number {
    if (n > 0) { return 1; }
    return 0;
}
"#);
        assert_eq!(f.cognitive, 1);
    }

    #[test]
    fn else_if_chain_pays_else_penalty_only_for_trailing_bare_else() {
        let f = one(r#"
function f(n: number): number {
    if (n > 0) { return 1; } else if (n < 0) { return -1; } else { return 0; }
}
"#);
        // outer if: 1 at nest 0
        // inner else-if: 1 + 1 (nest=1) = 2
        // trailing bare else: +1
        // total: 4
        assert_eq!(f.cognitive, 4);
    }

    #[test]
    fn switch_at_top_level_charges_one_for_cognitive_regardless_of_arms() {
        let f = one(r#"
function f(n: number): number {
    switch (n) {
        case 0: return 0;
        case 1: return 1;
        case 2: return 2;
        default: return 3;
    }
}
"#);
        assert_eq!(f.cyclomatic, 3);
        assert_eq!(f.cognitive, 1);
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

    #[test]
    fn export_default_function_is_extracted() {
        let units = extract("export default function defaulted(): void {}");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "defaulted");
    }

    #[test]
    fn export_default_class_methods_are_extracted() {
        let units = extract(
            r#"
export default class Foo {
    bar(): void {}
}
"#,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "Foo::bar");
    }

    #[test]
    fn exported_class_methods_are_extracted() {
        let units = extract(
            r#"
export class Foo {
    bar(): void {}
}
"#,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "Foo::bar");
    }

    #[test]
    fn exported_variable_declaration_is_extracted() {
        let units = extract("export const adder = (a: number, b: number): number => a + b;");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "adder");
    }

    #[test]
    fn function_expression_assigned_to_const_is_extracted() {
        let units = extract("const fe = function () { return 1; };");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "fe");
    }

    #[test]
    fn private_class_methods_are_extracted_with_hash_prefix() {
        let units = extract(
            r#"
class Foo {
    #secret(): void {}
}
"#,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "Foo::#secret");
    }

    #[test]
    fn class_methods_with_string_literal_keys_are_extracted() {
        let units = extract(
            r#"
class Foo {
    "weird name"(): void {}
}
"#,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "Foo::weird name");
    }

    #[test]
    fn ts_namespace_module_declaration_is_walked() {
        let units = extract(
            r#"
namespace outer {
    export function inner(): void {}
}
"#,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "inner");
    }

    #[test]
    fn switch_arms_increase_cyclomatic_correctly() {
        // Two arms beyond the first should add exactly 2 to cyclomatic.
        let f = one(r#"
function f(n: number): number {
    switch (n) {
        case 0: return 0;
        case 1: return 1;
        case 2: return 2;
    }
    return -1;
}
"#);
        // base 1 + (3 case arms - 1) = 3
        assert_eq!(f.cyclomatic, 3);
        // The switch itself contributes a single +1 to cognitive.
        assert_eq!(f.cognitive, 1);
    }

    #[test]
    fn switch_inside_if_pays_nesting_penalty() {
        let f = one(r#"
function f(go: boolean, n: number): number {
    if (go) {
        switch (n) {
            case 0: return 0;
            case 1: return 1;
        }
    }
    return -1;
}
"#);
        // 1 (if) + 2 (switch at nest 1) = 3
        assert_eq!(f.cognitive, 3);
    }

    #[test]
    fn try_catch_increments_cognitive_only_for_catch_clause() {
        let f = one(r#"
function f(): number {
    try { return 1; } catch (e) { return 0; }
}
"#);
        // try itself does NOT add to cognitive in our visitor; only the
        // `catch` handler adds +1. With nesting 0, total cognitive = 1.
        assert_eq!(f.cognitive, 1);
        assert_eq!(f.cyclomatic, 2);
    }

    #[test]
    fn try_catch_inside_if_pays_nesting_penalty() {
        let f = one(r#"
function f(go: boolean): number {
    if (go) {
        try { return 1; } catch (e) { return 0; }
    }
    return -1;
}
"#);
        // 1 (if at nest 0) + (1 + 1 nest) (catch at nest 1) = 3
        assert_eq!(f.cognitive, 3);
    }

    #[test]
    fn logical_expression_each_step_bumps_cognitive_by_one() {
        // `a && b || c && d` parses as ((a && b) || (c && d)) — three
        // logical-expression nodes total.
        let f = one(r#"
function f(a: boolean, b: boolean, c: boolean, d: boolean): boolean {
    return a && b || c && d;
}
"#);
        // Three logicals each contribute +1 in cognitive.
        assert_eq!(f.cognitive, 3);
        // Cyclomatic: base 1 + 3 logicals = 4
        assert_eq!(f.cyclomatic, 4);
    }
}
