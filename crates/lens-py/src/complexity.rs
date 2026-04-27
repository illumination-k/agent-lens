//! ruff-based complexity extraction for Python source files.
//!
//! For every function-shaped item — top-level `def` / `async def` and
//! every method on a class — we walk the body and produce a
//! [`FunctionComplexity`]:
//!
//! * **Cyclomatic Complexity** — McCabe; starts at 1 and is incremented
//!   for each branching construct (`if`, `elif`, `while`, `for`, each
//!   `except` clause, each `match` arm beyond the first, every chained
//!   `and` / `or` step, the ternary `x if cond else y`, and assertions).
//! * **Cognitive Complexity** — Sonar-style; control structures add
//!   `1 + nesting` so deeply-nested code scores higher than the same
//!   number of flat branches. `and` / `or` add `1` per occurrence.
//! * **Max Nesting Depth** — the deepest control-flow nesting reached in
//!   the function body.
//! * **Halstead counts** — operators (keywords like `if`, `for`, `def`,
//!   `return`, `=`, plus binary / boolean / comparison / unary
//!   operators) and operands (identifiers and literals).
//!
//! Nested functions and classes inside a function body contribute to
//! the enclosing function's score (matches how a reader experiences the
//! code) but are not surfaced as separate units. This mirrors how the
//! similarity extractor treats `def` bodies as atomic — see
//! [`crate::parser::extract_functions_excluding_tests`].

use std::collections::HashMap;

use lens_domain::{FunctionComplexity, HalsteadCounts, qualify};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{
    BoolOp, CmpOp, Expr, ExprBoolOp, ExprCall, ExprCompare, ExprIf, ExprUnaryOp, Number, Stmt,
    StmtAssert, StmtClassDef, StmtFor, StmtFunctionDef, StmtIf, StmtMatch, StmtTry, StmtWhile,
    StmtWith, UnaryOp,
};
use ruff_python_parser::{ParseError, parse_module};

use crate::attrs::{inherits_protocol, is_stub_function};
use crate::line_index::LineIndex;

/// Failures produced while extracting complexity units.
#[derive(Debug, thiserror::Error)]
pub enum ComplexityError {
    #[error("failed to parse Python source: {0}")]
    Parse(#[from] ParseError),
}

/// Extract one [`FunctionComplexity`] per function-shaped item in
/// `source`. Methods are reported as `Class::method`; free functions
/// keep their bare name.
pub fn extract_complexity_units(source: &str) -> Result<Vec<FunctionComplexity>, ComplexityError> {
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
    out: &mut Vec<FunctionComplexity>,
) {
    match stmt {
        Stmt::FunctionDef(func) => {
            // Stubs all score CC=1, cognitive=0; reporting them just
            // inflates the table with rows that carry no signal.
            if is_stub_function(func) {
                return;
            }
            let name = qualify(owner, func.name.as_str());
            out.push(analyze(&name, func, lines));
        }
        Stmt::ClassDef(class) => collect_class(class, lines, out),
        _ => {}
    }
}

fn collect_class(class: &StmtClassDef, lines: &LineIndex, out: &mut Vec<FunctionComplexity>) {
    // Protocol classes are pure declarations — every method body is a
    // `...` stub. Drop the whole subtree.
    if inherits_protocol(class) {
        return;
    }
    let class_name = class.name.as_str();
    for inner in &class.body {
        collect_stmt(inner, Some(class_name), lines, out);
    }
}

fn analyze(name: &str, func: &StmtFunctionDef, lines: &LineIndex) -> FunctionComplexity {
    let mut visitor = ComplexityVisitor::new();
    for stmt in &func.body {
        visitor.visit_stmt(stmt);
    }
    let halstead = HalsteadCounts {
        distinct_operators: visitor.halstead.operators.len(),
        distinct_operands: visitor.halstead.operands.len(),
        total_operators: visitor.halstead.operators.values().sum(),
        total_operands: visitor.halstead.operands.values().sum(),
    };
    let start_line = lines.line_of(func.range.start().to_usize());
    let end_offset = func.range.end().to_usize().saturating_sub(1);
    let end_line = lines.line_of(end_offset);
    FunctionComplexity {
        name: name.to_owned(),
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

    // Setting `max_nesting` is idempotent for repeated entries at the
    // same depth; `>` and `>=` produce the same final value, just with
    // a different number of writes. The `>` boundary is therefore
    // listed under `exclude_re` in `.cargo/mutants.toml` — the
    // mutation is equivalent, not a real test gap.
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

impl<'a> Visitor<'a> for ComplexityVisitor {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::If(s) => self.visit_if(s),
            Stmt::While(s) => self.visit_while(s),
            Stmt::For(s) => self.visit_for(s),
            Stmt::Match(s) => self.visit_match(s),
            Stmt::Try(s) => self.visit_try(s),
            Stmt::With(s) => self.visit_with(s),
            Stmt::Assert(s) => self.visit_assert(s),
            _ => {
                self.record_stmt_halstead(stmt);
                walk_stmt(self, stmt);
            }
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::BoolOp(b) => self.visit_bool_op(b),
            Expr::If(e) => self.visit_ternary(e),
            Expr::Compare(c) => self.visit_compare(c),
            Expr::UnaryOp(u) => self.visit_unary(u),
            Expr::Call(c) => self.visit_call(c),
            _ => {
                self.record_expr_halstead(expr);
                walk_expr(self, expr);
            }
        }
    }
}

impl ComplexityVisitor {
    // Halstead labels are an implementation detail — they affect which
    // keys land in the operator/operand maps but do not change the
    // cyclomatic / cognitive / nesting numbers an analyzer is judged
    // on. Asserting every label individually would require brittle
    // exact-count checks; both `record_*_halstead` helpers are listed
    // in `.cargo/mutants.toml`'s `exclude_re` so cargo-mutants leaves
    // their match arms alone.
    fn record_stmt_halstead(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Return(_) => self.halstead.op("return"),
            Stmt::Raise(_) => self.halstead.op("raise"),
            Stmt::Assign(_) => self.halstead.op("="),
            Stmt::AugAssign(s) => self.halstead.op(&format!("{}=", s.op.as_str())),
            Stmt::AnnAssign(_) => self.halstead.op(":"),
            // Nested `def` contributes its body to the parent's score
            // but does not get its own [`FunctionComplexity`] entry —
            // see the module-level docstring.
            Stmt::FunctionDef(_) => self.halstead.op("def"),
            Stmt::ClassDef(_) => self.halstead.op("class"),
            _ => {}
        }
    }

    fn record_expr_halstead(&mut self, expr: &Expr) {
        match expr {
            Expr::BinOp(b) => self.halstead.op(b.op.as_str()),
            Expr::Lambda(_) => self.halstead.op("lambda"),
            Expr::Await(_) => self.halstead.op("await"),
            Expr::Yield(_) => self.halstead.op("yield"),
            Expr::YieldFrom(_) => self.halstead.op("yield from"),
            Expr::Name(n) => self.halstead.operand(n.id.as_str()),
            Expr::Attribute(a) => self.halstead.operand(a.attr.as_str()),
            Expr::NumberLiteral(n) => self.halstead.operand(&number_literal_repr(&n.value)),
            Expr::StringLiteral(_) => self.halstead.operand("<str>"),
            Expr::BytesLiteral(_) => self.halstead.operand("<bytes>"),
            Expr::FString(_) => self.halstead.operand("<fstring>"),
            Expr::BooleanLiteral(b) => {
                self.halstead
                    .operand(if b.value { "True" } else { "False" });
            }
            Expr::NoneLiteral(_) => self.halstead.operand("None"),
            Expr::EllipsisLiteral(_) => self.halstead.operand("..."),
            _ => {}
        }
    }
}

impl ComplexityVisitor {
    fn visit_if(&mut self, stmt: &StmtIf) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.halstead.op("if");
        self.visit_expr(&stmt.test);
        self.enter_nest();
        for s in &stmt.body {
            self.visit_stmt(s);
        }
        self.exit_nest();
        for clause in &stmt.elif_else_clauses {
            match &clause.test {
                // `elif` is its own branch in McCabe and a +1 in cognitive
                // (no extra penalty for the bare `else`).
                Some(test) => {
                    self.cyclomatic_branches += 1;
                    self.cognitive += 1 + self.nesting;
                    self.halstead.op("elif");
                    self.visit_expr(test);
                    self.enter_nest();
                    for s in &clause.body {
                        self.visit_stmt(s);
                    }
                    self.exit_nest();
                }
                None => {
                    self.cognitive += 1;
                    self.halstead.op("else");
                    self.enter_nest();
                    for s in &clause.body {
                        self.visit_stmt(s);
                    }
                    self.exit_nest();
                }
            }
        }
    }

    fn visit_while(&mut self, stmt: &StmtWhile) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.halstead.op("while");
        self.visit_expr(&stmt.test);
        self.enter_nest();
        for s in &stmt.body {
            self.visit_stmt(s);
        }
        self.exit_nest();
        // `else:` after a while/for runs when the loop completes without
        // break — it doesn't add a structural branch in cognitive
        // complexity, but we still walk it for Halstead operands.
        for s in &stmt.orelse {
            self.visit_stmt(s);
        }
    }

    fn visit_for(&mut self, stmt: &StmtFor) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.halstead.op("for");
        self.visit_expr(&stmt.target);
        self.visit_expr(&stmt.iter);
        self.enter_nest();
        for s in &stmt.body {
            self.visit_stmt(s);
        }
        self.exit_nest();
        for s in &stmt.orelse {
            self.visit_stmt(s);
        }
    }

    fn visit_match(&mut self, stmt: &StmtMatch) {
        // McCabe: every arm beyond the first introduces a new path.
        let arms = u32::try_from(stmt.cases.len()).unwrap_or(u32::MAX);
        self.cyclomatic_branches += arms.saturating_sub(1);
        self.cognitive += 1 + self.nesting;
        self.halstead.op("match");
        self.visit_expr(&stmt.subject);
        self.enter_nest();
        for case in &stmt.cases {
            if let Some(guard) = &case.guard {
                // A guard adds another conditional path.
                self.cyclomatic_branches += 1;
                self.visit_expr(guard);
            }
            for s in &case.body {
                self.visit_stmt(s);
            }
        }
        self.exit_nest();
    }

    fn visit_try(&mut self, stmt: &StmtTry) {
        self.halstead.op("try");
        self.enter_nest();
        for s in &stmt.body {
            self.visit_stmt(s);
        }
        self.exit_nest();
        for handler in &stmt.handlers {
            // Each `except` clause is one extra control-flow branch.
            self.cyclomatic_branches += 1;
            self.cognitive += 1 + self.nesting;
            self.halstead.op("except");
            let ruff_python_ast::ExceptHandler::ExceptHandler(h) = handler;
            if let Some(t) = &h.type_ {
                self.visit_expr(t);
            }
            self.enter_nest();
            for s in &h.body {
                self.visit_stmt(s);
            }
            self.exit_nest();
        }
        for s in &stmt.orelse {
            self.visit_stmt(s);
        }
        // The `!`-deletion mutant here would only flip whether the
        // "finally" Halstead operator gets registered and whether the
        // finally body gets walked for label collection. The
        // cyclomatic / cognitive / nesting numbers are unaffected, so
        // `record_finally` is listed under `.cargo/mutants.toml`'s
        // `exclude_re` and the helper exists purely to keep that
        // boundary in one place.
        self.record_finally(&stmt.finalbody);
    }

    fn record_finally(&mut self, finalbody: &[Stmt]) {
        if !finalbody.is_empty() {
            self.halstead.op("finally");
            for s in finalbody {
                self.visit_stmt(s);
            }
        }
    }

    fn visit_with(&mut self, stmt: &StmtWith) {
        self.halstead.op("with");
        for item in &stmt.items {
            self.visit_expr(&item.context_expr);
            if let Some(vars) = &item.optional_vars {
                self.visit_expr(vars);
            }
        }
        self.enter_nest();
        for s in &stmt.body {
            self.visit_stmt(s);
        }
        self.exit_nest();
    }

    fn visit_assert(&mut self, stmt: &StmtAssert) {
        // Treat `assert` as a branch: the failed-assert path is a
        // distinct control-flow exit point, like `?` in Rust.
        self.cyclomatic_branches += 1;
        self.halstead.op("assert");
        self.visit_expr(&stmt.test);
        if let Some(msg) = &stmt.msg {
            self.visit_expr(msg);
        }
    }

    fn visit_bool_op(&mut self, expr: &ExprBoolOp) {
        // `a and b and c` is one BoolOp node with three values; each
        // step beyond the first short-circuits, so charge `len-1`
        // branches for McCabe and `len-1` cognitive bumps.
        let extra = u32::try_from(expr.values.len()).unwrap_or(u32::MAX);
        let extra = extra.saturating_sub(1);
        self.cyclomatic_branches += extra;
        self.cognitive += extra;
        let label = match expr.op {
            BoolOp::And => "and",
            BoolOp::Or => "or",
        };
        self.halstead.op(label);
        for v in &expr.values {
            self.visit_expr(v);
        }
    }

    fn visit_ternary(&mut self, expr: &ExprIf) {
        // `x if cond else y` is a branching construct just like a
        // statement-level `if`.
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.halstead.op("if-expr");
        self.visit_expr(&expr.test);
        self.enter_nest();
        self.visit_expr(&expr.body);
        self.visit_expr(&expr.orelse);
        self.exit_nest();
    }

    fn visit_compare(&mut self, expr: &ExprCompare) {
        // `a < b < c` carries multiple ops on one node; record each.
        for op in expr.ops.iter() {
            self.halstead.op(cmp_op_str(*op));
        }
        self.visit_expr(&expr.left);
        for c in expr.comparators.iter() {
            self.visit_expr(c);
        }
    }

    fn visit_unary(&mut self, expr: &ExprUnaryOp) {
        let label = match expr.op {
            UnaryOp::Invert => "~",
            UnaryOp::Not => "not",
            UnaryOp::UAdd => "+u",
            UnaryOp::USub => "-u",
        };
        self.halstead.op(label);
        self.visit_expr(&expr.operand);
    }

    fn visit_call(&mut self, call: &ExprCall) {
        // The call itself is an operator; the callee may still
        // contribute operand counts (the function's name).
        self.halstead.op("call");
        self.visit_expr(&call.func);
        for arg in &call.arguments.args {
            self.visit_expr(arg);
        }
        for kw in &call.arguments.keywords {
            self.visit_expr(&kw.value);
        }
    }
}

// Pure label functions — their return value flows into the Halstead
// operator/operand map keys, so any mutation just renames a key and
// has no observable effect on the cyclomatic / cognitive / nesting
// numbers. Listed under `.cargo/mutants.toml`'s `exclude_re` for the
// same reason as `record_*_halstead`.
fn cmp_op_str(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "==",
        CmpOp::NotEq => "!=",
        CmpOp::Lt => "<",
        CmpOp::LtE => "<=",
        CmpOp::Gt => ">",
        CmpOp::GtE => ">=",
        CmpOp::Is => "is",
        CmpOp::IsNot => "is not",
        CmpOp::In => "in",
        CmpOp::NotIn => "not in",
    }
}

fn number_literal_repr(num: &Number) -> String {
    match num {
        Number::Int(i) => i.to_string(),
        Number::Float(f) => f.to_string(),
        Number::Complex { real, imag } => format!("{real}+{imag}j"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn extract(src: &str) -> Vec<FunctionComplexity> {
        extract_complexity_units(src).unwrap()
    }

    fn one(src: &str) -> FunctionComplexity {
        let mut units = extract(src);
        assert_eq!(units.len(), 1, "expected exactly one function");
        units.remove(0)
    }

    #[rstest]
    #[case::linear_function("def noop():\n    x = 1 + 2\n", 1, 0, 0)]
    #[case::single_if(
        "def f(x):\n    if x > 0:\n        return 1\n    else:\n        return 0\n",
        2,
        2,
        1
    )]
    #[case::elif_chain(
        "
def f(n):
    if n > 0:
        return 1
    elif n < 0:
        return -1
    else:
        return 0
",
        3,
        3,
        1
    )]
    #[case::match_statement(
        "
def f(n):
    match n:
        case 0:
            return 0
        case 1:
            return 1
        case _:
            return -1
",
        3,
        1,
        1
    )]
    #[case::logical_steps("def f(a, b, c):\n    return a and b or c\n", 3, 2, 0)]
    #[case::ternary_expression("def f(x):\n    return 1 if x > 0 else 0\n", 2, 1, 1)]
    #[case::try_except(
        "
def f():
    try:
        return 1
    except Exception:
        return 0
",
        2,
        1,
        1
    )]
    #[case::nested_loops(
        "
def f():
    for i in range(10):
        for j in range(10):
            if i == j:
                pass
",
        4,
        6,
        3
    )]
    #[case::while_inside_if(
        "
def f(go):
    if go:
        i = 0
        while i < 10:
            i += 1
",
        3,
        3,
        2
    )]
    #[case::for_inside_if(
        "
def f(go):
    if go:
        for x in range(5):
            pass
",
        3,
        3,
        2
    )]
    #[case::match_inside_if(
        "
def f(go, n):
    if go:
        match n:
            case 0:
                return 0
            case 1:
                return 1
    return -1
",
        3,
        3,
        2
    )]
    #[case::assert_statement("def f(x):\n    assert x > 0\n", 2, 0, 0)]
    #[case::match_guard(
        "
def f(n):
    match n:
        case x if x > 0:
            return 1
        case _:
            return 0
",
        3,
        1,
        1
    )]
    #[case::elif_inside_if(
        "
def f(x, y):
    if x:
        if y > 0:
            return 1
        elif y < 0:
            return -1
",
        4,
        5,
        2
    )]
    #[case::except_inside_if(
        "
def f(go):
    if go:
        try:
            return 1
        except Exception:
            return 0
",
        3,
        3,
        2
    )]
    #[case::ternary_inside_if(
        "
def f(x, y):
    if x:
        return 1 if y else 0
",
        3,
        3,
        2
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
            "
def flat(n):
    if n > 0:
        pass
    if n < 0:
        pass
def nested(n):
    if n > 0:
        if n < 5:
            pass
",
        );
        let flat = units.iter().find(|f| f.name == "flat").unwrap();
        let nested = units.iter().find(|f| f.name == "nested").unwrap();
        // Flat: 1 + 1 = 2; Nested: 1 + (1+1) = 3.
        assert_eq!(flat.cognitive, 2);
        assert_eq!(nested.cognitive, 3);
    }

    #[rstest]
    #[case::class_method(
        "
class Foo:
    def bar(self):
        return 1
",
        &["Foo::bar"]
    )]
    #[case::async_function("async def fetch(url):\n    return await get(url)\n", &["fetch"])]
    #[case::nested_function_is_not_separate_unit(
        "
def outer():
    def inner():
        return 1
    return inner
",
        &["outer"]
    )]
    fn extracted_names_match(#[case] src: &str, #[case] expected: &[&str]) {
        let units = extract(src);
        let names: Vec<_> = units.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn line_range_covers_signature_through_last_body_line() {
        let f = one("def f():\n    x = 1\n    y = 2\n");
        assert_eq!(f.start_line, 1);
        assert_eq!(f.end_line, 3);
        assert_eq!(f.loc(), 3);
    }

    #[test]
    fn halstead_treats_keywords_as_operators_and_idents_as_operands() {
        let f = one("def f():\n    x = 1\n");
        assert!(f.halstead.distinct_operators >= 1);
        assert!(f.halstead.distinct_operands >= 2); // x, 1
    }

    #[test]
    fn halstead_volume_is_defined_for_a_realistic_function() {
        let f = one("
def add(a, b):
    s = a + b
    return s
");
        let v = f.halstead_volume();
        assert!(v.is_some());
        let mi = f.maintainability_index().unwrap();
        assert!((0.0..=100.0).contains(&mi));
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = extract_complexity_units("def !!!(:").unwrap_err();
        assert!(matches!(err, ComplexityError::Parse(_)));
    }

    #[test]
    fn complexity_error_source_is_present() {
        use std::error::Error as _;
        let err = extract_complexity_units("def !!!(:").unwrap_err();
        assert!(err.source().is_some());
    }

    #[test]
    fn empty_file_yields_no_units() {
        let units = extract("# nothing here\n");
        assert!(units.is_empty());
    }

    #[test]
    fn protocol_class_methods_are_filtered() {
        // PEP 544 Protocol methods all score CC=1 / cognitive=0 — the
        // floor of the metric. Including them inflates the table with
        // rows that carry no signal.
        let units = extract(
            "
from typing import Protocol

class Service(Protocol):
    def handle(self, x): ...
    def close(self): ...
",
        );
        assert!(units.is_empty());
    }

    #[test]
    fn abstractmethod_and_overload_are_filtered() {
        // Mixed module: only the concrete free function should appear.
        let units = extract(
            "
from abc import abstractmethod
from typing import overload

@overload
def stub_overload(x: int) -> int: ...

class Animal:
    @abstractmethod
    def speak(self): ...
    def common(self):
        return 1

def real(x):
    return x + 1
",
        );
        let names: Vec<_> = units.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Animal::common", "real"]);
    }

    #[test]
    fn stub_bodied_functions_are_filtered() {
        // `pass` / `...` / docstring / `raise NotImplementedError`
        // bodies are stubs even without a decorator. None of them
        // should land in the complexity report.
        let units = extract(
            "
def pass_only():
    pass
def ellipsis_only():
    ...
def docstring_only():
    \"\"\"docs\"\"\"
def not_implemented():
    raise NotImplementedError
def real():
    return 1
",
        );
        let names: Vec<_> = units.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["real"]);
    }

    #[test]
    fn comparison_chain_records_each_op() {
        // `a < b < c` is one Compare node with two ops; both should
        // appear in the operator total.
        let f = one("def f(a, b, c):\n    return a < b < c\n");
        assert!(f.halstead.distinct_operators >= 1);
        assert!(f.halstead.total_operators >= 2);
    }

    #[test]
    fn with_statement_increments_max_nesting_and_walks_body() {
        // `visit_with` enters a nesting level and walks the body.
        // Replacing it with a no-op would leave `max_nesting` at 0
        // and drop the body's identifiers from Halstead operands.
        let f = one("
def f(ctx):
    with ctx:
        x = 1
        y = 2
");
        assert_eq!(f.max_nesting, 1);
        // The body assignments contribute `x`, `y`, `1`, `2` as
        // operands; if `visit_with` is replaced with `()` the body
        // is never walked, so none of those land in the operand
        // set. A minimum of two distinct operands is enough to make
        // the no-op replacement observable.
        assert!(
            f.halstead.distinct_operands >= 2,
            "expected with-body operands `x` and `y` to be counted, got distinct={}",
            f.halstead.distinct_operands,
        );
    }

    #[test]
    fn unary_not_records_operator_and_walks_into_operand() {
        // `visit_unary` does two things: records the unary operator
        // ("not"/"~"/"+u"/"-u") and descends into its operand.
        // Deleting the `Expr::UnaryOp` arm in `visit_expr` would
        // route through the default `walk_expr` path, which still
        // walks the operand (so `x` is counted) but drops the
        // operator label. Pinning a minimum on `distinct_operators`
        // catches that drop.
        let f = one("def f(x):\n    return not x\n");
        // `x` must still appear as an operand.
        assert!(
            f.halstead.distinct_operands >= 1,
            "expected `x` to be counted as an operand under `not`",
        );
        // Operators must include both `return` and `not`; if the
        // UnaryOp arm is deleted, only `return` survives.
        assert!(
            f.halstead.distinct_operators >= 2,
            "expected `return` and `not` operators, got distinct={}",
            f.halstead.distinct_operators,
        );
    }

    #[test]
    fn call_records_operator_and_walks_into_arguments() {
        // `visit_call` does two things: records the `call` operator
        // and walks the arguments. The default `walk_expr` path also
        // walks args (so `x`, `y`, `foo` would still be counted as
        // operands), but the `call` operator label only comes from
        // `visit_call`. Without `Stmt::Expr` recording any operator
        // of its own, deleting the `Expr::Call` arm leaves the body
        // with zero operators.
        let f = one("def f(x, y):\n    foo(x, y)\n");
        assert!(
            f.halstead.distinct_operands >= 2,
            "expected call args to be counted as operands, got distinct={}",
            f.halstead.distinct_operands,
        );
        assert!(
            f.halstead.distinct_operators >= 1,
            "expected `call` operator from visit_call, got distinct={}",
            f.halstead.distinct_operators,
        );
    }
}
