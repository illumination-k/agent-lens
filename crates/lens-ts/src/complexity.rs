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
//! * **Cognitive Complexity** — SonarSource's rules: control structures
//!   add `1 + nesting`; `else if`, `else`, labelled `break` / `continue`,
//!   and each run of like `&&` / `||` / `??` operators add a flat `1`;
//!   direct recursion adds `1` once. `try` and `finally` add nothing;
//!   nested functions and arrows raise the nesting of their bodies.
//! * **Max Nesting Depth** — the deepest control-flow nesting reached in
//!   the function body.
//! * **Halstead counts** — operators and operands are derived from the
//!   AST. Identifiers and literals are operands; binary / logical /
//!   unary / update / assignment operators are operators; control-flow
//!   keywords (`if`, `for`, `while`, `return`, …) are operators.
//!
//! Closures and inner functions defined *inside* a function body
//! contribute to the enclosing function's score one nesting level
//! deeper, mirroring how a reader
//! actually experiences the code. They are *additionally* reported as
//! their own `<parent>::closure#N` units (see [`crate::walk`]), so a
//! callback that is complex in its own right is still visible directly.
//! The same holds for the callbacks a module-scope call registers, which
//! is what gives a `describe`/`it` suite any units at all.
//!
//! The traversal that finds function-shaped items lives in
//! [`crate::walk`]; this module only converts each [`FunctionItem`] into
//! a [`FunctionComplexity`] by running [`ComplexityVisitor`] against its
//! body.

use lens_domain::{ComplexityCounters, FunctionComplexity, HalsteadAcc, LineIndex};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_syntax::scope::ScopeFlags;

use crate::parser::{Dialect, TsParseError};
use crate::walk::{FnBody, FunctionItem, FunctionVisitor, walk_program};

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
    let ret = dialect.parse(&alloc, source);
    if !ret.diagnostics.is_empty() {
        return Err(ComplexityError::Parse(TsParseError::from_diagnostics(
            ret.diagnostics
                .iter()
                .map(|e| e.message.as_ref().to_owned()),
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
    body: FnBody<'_>,
) -> FunctionComplexity {
    // Recursion is a call to the unit's own name: its last `::` segment
    // (`Class::method` → `method`; a `closure#N` never matches).
    let own_name = name.rsplit("::").next().unwrap_or(&name).to_owned();
    let mut visitor = ComplexityVisitor::new(own_name);
    body.visit(&mut visitor);
    let is_test = crate::parser::is_test_item(&name);
    FunctionComplexity {
        name,
        start_line,
        end_line,
        cyclomatic: visitor.counters.cyclomatic(),
        cognitive: visitor.counters.cognitive(),
        max_nesting: visitor.counters.max_nesting(),
        halstead: visitor.halstead.counts(),
        is_test,
    }
}

/// TypeScript-specific half of the complexity walk: which oxc node counts
/// as a branch and which Halstead label it carries. The scoring rules live
/// in [`ComplexityCounters`] / [`HalsteadAcc`].
struct ComplexityVisitor {
    counters: ComplexityCounters,
    halstead: HalsteadAcc,
    /// The unit's own name, for spotting direct recursion (`name(..)`,
    /// `this.name(..)`).
    name: String,
}

impl ComplexityVisitor {
    fn new(name: String) -> Self {
        Self {
            counters: ComplexityCounters::default(),
            halstead: HalsteadAcc::default(),
            name,
        }
    }

    /// Walk one `if`'s test, body, and `else` chain. An `else if` is a
    /// flat `+1` read at the same depth as the `if` it continues; a plain
    /// `else` is `+1` with no McCabe path.
    fn visit_if_chain(&mut self, it: &IfStatement<'_>) {
        self.visit_expression(&it.test);
        self.visit_nested(&it.consequent);
        match &it.alternate {
            Some(Statement::IfStatement(next)) => {
                self.counters.add_flat_branch();
                self.halstead.op("if");
                self.visit_if_chain(next);
            }
            Some(alt) => {
                self.counters.add_cognitive(1);
                self.halstead.op("else");
                self.visit_nested(alt);
            }
            None => {}
        }
    }

    fn visit_nested(&mut self, stmt: &Statement<'_>) {
        self.counters.enter_nest();
        self.visit_statement(stmt);
        self.counters.exit_nest();
    }

    fn is_own_call(&self, callee: &Expression<'_>) -> bool {
        match callee {
            Expression::Identifier(id) => id.name == self.name.as_str(),
            Expression::StaticMemberExpression(member) => {
                matches!(member.object, Expression::ThisExpression(_))
                    && member.property.name == self.name.as_str()
            }
            _ => false,
        }
    }

    /// Score one loop: `+1` McCabe branch, `+(1 + nesting)` cognitive,
    /// one Halstead operator, then the optional header expression
    /// (`while`'s condition, `for-in` / `for-of`'s right-hand side)
    /// outside the nesting and the body inside it.
    ///
    /// `while`, `do`, `for-in` and `for-of` differ only in `label` and
    /// which node is the header; mirrors `lens-rust`'s `visit_loop`.
    fn visit_loop<'a>(
        &mut self,
        label: &str,
        header: Option<&Expression<'a>>,
        body: &Statement<'a>,
    ) {
        self.counters.add_branch();
        self.halstead.op(label);
        if let Some(header) = header {
            self.visit_expression(header);
        }
        self.counters.enter_nest();
        self.visit_statement(body);
        self.counters.exit_nest();
    }
}

impl<'a> Visit<'a> for ComplexityVisitor {
    fn visit_if_statement(&mut self, it: &IfStatement<'a>) {
        self.counters.add_branch();
        self.halstead.op("if");
        self.visit_if_chain(it);
    }

    fn visit_while_statement(&mut self, it: &WhileStatement<'a>) {
        self.visit_loop("while", Some(&it.test), &it.body);
    }

    fn visit_do_while_statement(&mut self, it: &DoWhileStatement<'a>) {
        // The header runs *after* the body here, so it is walked last
        // rather than through `visit_loop`'s header slot.
        self.visit_loop("do", None, &it.body);
        self.visit_expression(&it.test);
    }

    fn visit_for_statement(&mut self, it: &ForStatement<'a>) {
        self.counters.add_branch();
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
        self.counters.enter_nest();
        self.visit_statement(&it.body);
        self.counters.exit_nest();
    }

    fn visit_for_in_statement(&mut self, it: &ForInStatement<'a>) {
        self.visit_loop("for-in", Some(&it.right), &it.body);
    }

    fn visit_for_of_statement(&mut self, it: &ForOfStatement<'a>) {
        self.visit_loop("for-of", Some(&it.right), &it.body);
    }

    fn visit_switch_statement(&mut self, it: &SwitchStatement<'a>) {
        // McCabe: every case beyond the first introduces a new path. We
        // count cases that have a `test` (default arms don't add a path).
        let arms = it.cases.iter().filter(|c| c.test.is_some()).count();
        let arms = u32::try_from(arms).unwrap_or(u32::MAX);
        self.counters.add_branches(arms.saturating_sub(1));
        self.halstead.op("switch");

        self.visit_expression(&it.discriminant);
        self.counters.enter_nest();
        for case in &it.cases {
            if let Some(t) = &case.test {
                self.visit_expression(t);
            }
            for stmt in &case.consequent {
                self.visit_statement(stmt);
            }
        }
        self.counters.exit_nest();
    }

    fn visit_try_statement(&mut self, it: &TryStatement<'a>) {
        // Sonar: `try` and `finally` neither cost nor nest; only the
        // `catch` is a branch.
        self.halstead.op("try");
        self.visit_block_statement(&it.block);
        if let Some(handler) = &it.handler {
            self.counters.add_branch();
            self.halstead.op("catch");
            self.counters.enter_nest();
            self.visit_block_statement(&handler.body);
            self.counters.exit_nest();
        }
        if let Some(finalizer) = &it.finalizer {
            self.halstead.op("finally");
            self.visit_block_statement(finalizer);
        }
    }

    fn visit_logical_expression(&mut self, it: &LogicalExpression<'a>) {
        // A whole chain is scored at its root: one cognitive point per
        // run of like operators. Operands are walked afterwards and start
        // chains of their own.
        let mut ops = Vec::new();
        let mut operands = Vec::new();
        flatten_logical(it, &mut ops, &mut operands);
        self.counters.add_logical_chain(&ops);
        for op in &ops {
            self.halstead.op(op.as_str());
        }
        for operand in operands {
            self.visit_expression(operand);
        }
    }

    fn visit_function(&mut self, it: &Function<'a>, flags: ScopeFlags) {
        // A nested function is read inside its parent; Sonar nests it.
        self.counters.enter_nest();
        walk::walk_function(self, it, flags);
        self.counters.exit_nest();
    }

    fn visit_arrow_function_expression(&mut self, it: &ArrowFunctionExpression<'a>) {
        self.counters.enter_nest();
        walk::walk_arrow_function_expression(self, it);
        self.counters.exit_nest();
    }

    fn visit_break_statement(&mut self, it: &BreakStatement<'a>) {
        // A labelled jump is a goto in disguise: `+1`, no nesting.
        if it.label.is_some() {
            self.counters.add_cognitive(1);
        }
    }

    fn visit_continue_statement(&mut self, it: &ContinueStatement<'a>) {
        if it.label.is_some() {
            self.counters.add_cognitive(1);
        }
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        if self.is_own_call(&it.callee) {
            self.counters.add_recursion();
        }
        walk::walk_call_expression(self, it);
    }

    fn visit_conditional_expression(&mut self, it: &ConditionalExpression<'a>) {
        // `cond ? a : b` is a branching construct just like `if`.
        self.counters.add_branch();
        self.halstead.op("?:");
        self.visit_expression(&it.test);
        self.counters.enter_nest();
        self.visit_expression(&it.consequent);
        self.visit_expression(&it.alternate);
        self.counters.exit_nest();
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

/// Collect a logical chain's operators in source order, looking through
/// parentheses, and the operands that end it.
fn flatten_logical<'b, 'a>(
    it: &'b LogicalExpression<'a>,
    ops: &mut Vec<LogicalOperator>,
    operands: &mut Vec<&'b Expression<'a>>,
) {
    flatten_logical_side(&it.left, ops, operands);
    ops.push(it.operator);
    flatten_logical_side(&it.right, ops, operands);
}

fn flatten_logical_side<'b, 'a>(
    expr: &'b Expression<'a>,
    ops: &mut Vec<LogicalOperator>,
    operands: &mut Vec<&'b Expression<'a>>,
) {
    let mut inner = expr;
    while let Expression::ParenthesizedExpression(paren) = inner {
        inner = &paren.expression;
    }
    match inner {
        Expression::LogicalExpression(logical) => flatten_logical(logical, ops, operands),
        _ => operands.push(expr),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn extract(src: &str) -> Vec<FunctionComplexity> {
        extract_complexity_units(src, Dialect::Ts).unwrap()
    }

    fn test_flags(units: &[FunctionComplexity]) -> Vec<(&str, bool)> {
        units.iter().map(|f| (f.name.as_str(), f.is_test)).collect()
    }

    #[test]
    fn harness_callbacks_are_flagged_and_production_functions_are_not() {
        let src = "function prod() {}\ndescribe(\"prod\", () => {\n  it(\"works\", () => { prod(); });\n});\n";
        let units = extract(src);
        let flags = test_flags(&units);
        assert!(flags.contains(&("prod", false)), "{flags:?}");
        assert!(
            flags
                .iter()
                .filter(|(name, _)| *name != "prod")
                .all(|&(_, t)| t),
            "{flags:?}"
        );
        assert!(flags.len() > 1, "{flags:?}");
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
        3,
        1
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

    /// SonarSource's Cognitive Complexity rules, case by case. The first
    /// three are the white paper's own worked examples, ported to TS.
    #[rstest]
    #[case::sum_of_primes(
        "function sumOfPrimes(max: number): number {
  let total = 0;
  OUT: for (let i = 1; i <= max; ++i) {   // +1
    for (let j = 2; j < i; ++j) {         // +2
      if (i % j === 0) {                  // +3
        continue OUT;                     // +1
      }
    }
    total += i;
  }
  return total;
}",
        7
    )]
    #[case::switch_counts_once(
        "function getWords(n: number): string {
  switch (n) {                            // +1
    case 1: return 'one';
    case 2: return 'a couple';
    case 3: return 'a few';
    default: return 'lots';
  }
}",
        1
    )]
    #[case::try_does_not_nest_but_catch_does(
        "function f(a: boolean, b: boolean): void {
  try {
    if (a) {                              // +1
      for (let i = 0; i < 10; i++) {      // +2
        while (b) {}                      // +3
      }
    }
  } catch (e) {                           // +1
    if (b) {}                             // +2
  } finally {
    if (a) {}                             // +1
  }
}",
        10
    )]
    #[case::arrow_body_is_nested(
        "function f(a: boolean): void {
  const r = () => {
    if (a) {}                             // +2
  };
}",
        2
    )]
    #[case::else_if_is_flat_and_does_not_nest_deeper(
        "function f(xs: boolean[], b: boolean): void {
  for (const a of xs) {                   // +1
    if (a) {                              // +2
    } else if (b) {                       // +1
      if (b) {}                           // +3
    } else {                              // +1
    }
  }
}",
        8
    )]
    #[case::like_operators_form_one_run(
        "function f(a: boolean, b: boolean, c: boolean): boolean { return a && b && c; }",
        1
    )]
    #[case::each_operator_switch_is_a_new_run(
        "function f(a: any, b: any, c: any, d: any): any { return a && b || c && d; }",
        3
    )]
    #[case::nullish_coalescing_is_a_logical_run(
        "function f(a: any, b: any, c: any): any { return a ?? b ?? c; }",
        1
    )]
    #[case::parentheses_do_not_break_a_run(
        "function f(a: boolean, b: boolean, c: boolean): boolean { return a && (b && c); }",
        1
    )]
    #[case::negation_starts_a_new_run(
        "function f(a: boolean, b: boolean, c: boolean): boolean { return a && !(b && c); }",
        2
    )]
    #[case::direct_recursion_counts_once(
        "function fact(n: number): number {
  if (n === 0) { return 1; }              // +1
  return n * fact(n - 1) + fact(0);       // recursion +1
}",
        2
    )]
    #[case::method_recursion_through_this(
        "class T {
  walk(n: number): void {
    if (n > 0) { this.walk(n - 1); }      // +1, recursion +1
  }
}",
        2
    )]
    #[case::calls_that_only_look_like_recursion(
        "class T {
  walk(other: T): void {
    other.walk(this);
    this.run(other);
    walker(other);
  }
}",
        0
    )]
    #[case::nested_function_body_is_nested(
        "function f(a: boolean) {
  function inner() {
    if (a) {}                             // +2
  }
  return inner;
}",
        2
    )]
    #[case::labelled_break_counts_flat(
        "function f(xs: number[]): void {
  scan: while (true) {                    // +1
    for (const x of xs) {                 // +2
      if (x > 0) { break scan; }          // +3, +1
    }
    break;
  }
}",
        7
    )]
    fn cognitive_follows_the_sonar_rules(#[case] src: &str, #[case] cognitive: u32) {
        let units = extract(src);
        let f = units
            .iter()
            .find(|f| !f.name.contains("closure#"))
            .expect("one named unit");
        assert_eq!(f.cognitive, cognitive, "{}", f.name);
    }

    #[test]
    fn logical_chain_keeps_one_mccabe_path_per_operator() {
        let f = one("function f(a: any, b: any, c: any, d: any): any { return a && b || c && d; }");
        assert_eq!(f.cyclomatic, 4);
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
        "inner::hidden",
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
        "outer::inner",
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

    #[test]
    fn nested_function_is_reported_as_its_own_complexity_unit() {
        // A callback with real control flow gets its own unit so its
        // complexity is visible directly, not just folded into `setup`.
        let units = extract(
            r#"
function setup(x: number): void {
    const handler = () => {
        if (x > 0) {
            return;
        }
    };
}
"#,
        );
        let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(names, ["setup", "setup::closure#1"]);
        let handler = units
            .iter()
            .find(|u| u.name == "setup::closure#1")
            .expect("the callback must be its own unit");
        // The `if` inside the callback gives it a cyclomatic of 2.
        assert_eq!(handler.cyclomatic, 2);
    }

    #[test]
    fn a_vitest_suite_reports_its_cases_rather_than_no_functions() {
        // A test file holds its logic in harness callbacks, never in
        // declarations; reporting "no functions found" for one is the
        // failure this covers.
        let units = extract(
            r#"
describe("classify", () => {
    it("branches on the threshold", () => {
        if (classify(1) === "low") {
            expect(classify(9)).toBe("high");
        }
    });
});
"#,
        );
        let names: Vec<&str> = units.iter().map(|u| u.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "describe#1(\"classify\")",
                "describe#1(\"classify\")::it#1(\"branches on the threshold\")",
            ],
        );
        let case = units
            .iter()
            .find(|u| u.name.ends_with("it#1(\"branches on the threshold\")"))
            .expect("the case must be its own unit");
        assert_eq!(case.cyclomatic, 2);
    }
}
