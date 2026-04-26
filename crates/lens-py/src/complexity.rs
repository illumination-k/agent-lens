//! ruff-based complexity extraction for Python source files.

use std::collections::HashSet;

use lens_domain::{FunctionComplexity, HalsteadCounts};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{
    BoolOp, CmpOp, Expr, Operator, Stmt, StmtClassDef, StmtFunctionDef, UnaryOp,
};
use ruff_python_parser::{ParseError, parse_module};

/// Failures produced while extracting complexity units.
#[derive(Debug)]
pub enum ComplexityError {
    Parse(ParseError),
}

impl std::fmt::Display for ComplexityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "failed to parse Python source: {e}"),
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

impl From<ParseError> for ComplexityError {
    fn from(value: ParseError) -> Self {
        Self::Parse(value)
    }
}

/// Extract one [`FunctionComplexity`] per top-level function and class method.
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
        Stmt::FunctionDef(f) => out.push(analyze_function(owner, f, lines)),
        Stmt::ClassDef(class) => collect_class(class, lines, out),
        _ => {}
    }
}

fn collect_class(class: &StmtClassDef, lines: &LineIndex, out: &mut Vec<FunctionComplexity>) {
    let owner = class.name.as_str();
    for inner in &class.body {
        collect_stmt(inner, Some(owner), lines, out);
    }
}

fn analyze_function(
    owner: Option<&str>,
    f: &StmtFunctionDef,
    lines: &LineIndex,
) -> FunctionComplexity {
    let mut visitor = ComplexityVisitor::default();
    for stmt in &f.body {
        visitor.visit_stmt(stmt);
    }

    let start_line = lines.line_of(f.range.start().to_usize());
    let end_line = lines.line_of(f.range.end().to_usize().saturating_sub(1));
    FunctionComplexity {
        name: qualify(owner, f.name.as_str()),
        start_line,
        end_line,
        cyclomatic: 1 + visitor.cyclomatic_branches,
        cognitive: visitor.cognitive,
        max_nesting: visitor.max_nesting,
        halstead: visitor.halstead_counts(),
    }
}

fn qualify(owner: Option<&str>, method: &str) -> String {
    match owner {
        Some(owner) => format!("{owner}::{method}"),
        None => method.to_owned(),
    }
}

#[derive(Default)]
struct ComplexityVisitor {
    cyclomatic_branches: u32,
    cognitive: u32,
    nesting: u32,
    max_nesting: u32,
    operators_total: usize,
    operands_total: usize,
    operators_distinct: HashSet<String>,
    operands_distinct: HashSet<String>,
}

impl ComplexityVisitor {
    fn enter_nest(&mut self) {
        self.nesting += 1;
        self.max_nesting = self.max_nesting.max(self.nesting);
    }

    fn exit_nest(&mut self) {
        self.nesting = self.nesting.saturating_sub(1);
    }

    fn push_operator(&mut self, op: impl Into<String>) {
        self.operators_total += 1;
        self.operators_distinct.insert(op.into());
    }

    fn push_operand(&mut self, operand: impl Into<String>) {
        self.operands_total += 1;
        self.operands_distinct.insert(operand.into());
    }

    fn branch(&mut self) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
    }

    fn halstead_counts(&self) -> HalsteadCounts {
        HalsteadCounts {
            distinct_operators: self.operators_distinct.len(),
            distinct_operands: self.operands_distinct.len(),
            total_operators: self.operators_total,
            total_operands: self.operands_total,
        }
    }
}

impl<'a> Visitor<'a> for ComplexityVisitor {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::If(_)
            | Stmt::For(_)
            | Stmt::While(_)
            | Stmt::Try(_)
            | Stmt::With(_)
            | Stmt::Match(_) => {
                self.push_operator(stmt_kind(stmt));
                self.branch();
                self.enter_nest();
                walk_stmt(self, stmt);
                self.exit_nest();
                return;
            }
            Stmt::Return(_)
            | Stmt::Raise(_)
            | Stmt::Assert(_)
            | Stmt::Assign(_)
            | Stmt::AnnAssign(_)
            | Stmt::AugAssign(_)
            | Stmt::Delete(_)
            | Stmt::Pass(_)
            | Stmt::Break(_)
            | Stmt::Continue(_)
            | Stmt::Import(_)
            | Stmt::ImportFrom(_)
            | Stmt::Global(_)
            | Stmt::Nonlocal(_)
            | Stmt::TypeAlias(_) => {
                self.push_operator(stmt_kind(stmt));
            }
            Stmt::FunctionDef(_)
            | Stmt::ClassDef(_)
            | Stmt::Expr(_)
            | Stmt::IpyEscapeCommand(_) => {}
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::BoolOp(bool_op) => {
                let terms = bool_op.values.len();
                let ops = u32::try_from(terms.saturating_sub(1)).unwrap_or(u32::MAX);
                self.cyclomatic_branches = self.cyclomatic_branches.saturating_add(ops);
                self.cognitive = self.cognitive.saturating_add(ops);
                for _ in 0..ops {
                    self.push_operator(match bool_op.op {
                        BoolOp::And => "and",
                        BoolOp::Or => "or",
                    });
                }
            }
            Expr::If(_) => {
                self.push_operator("ifexpr");
                self.branch();
            }
            Expr::BinOp(bin) => self.push_operator(binop_name(bin.op)),
            Expr::UnaryOp(unary) => self.push_operator(unaryop_name(unary.op)),
            Expr::Compare(compare) => {
                for op in &compare.ops {
                    self.push_operator(cmpop_name(*op));
                }
            }
            Expr::Call(_) => self.push_operator("call"),
            Expr::Name(name) => self.push_operand(name.id.as_str()),
            Expr::NumberLiteral(_) => self.push_operand("num"),
            Expr::StringLiteral(_) => self.push_operand("str"),
            Expr::BooleanLiteral(_) => self.push_operand("bool"),
            Expr::NoneLiteral(_) => self.push_operand("none"),
            _ => {}
        }
        walk_expr(self, expr);
    }
}

fn stmt_kind(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::FunctionDef(_) => "def",
        Stmt::ClassDef(_) => "class",
        Stmt::Return(_) => "return",
        Stmt::Delete(_) => "delete",
        Stmt::Assign(_) => "assign",
        Stmt::AugAssign(_) => "aug_assign",
        Stmt::AnnAssign(_) => "ann_assign",
        Stmt::TypeAlias(_) => "type_alias",
        Stmt::For(_) => "for",
        Stmt::While(_) => "while",
        Stmt::If(_) => "if",
        Stmt::With(_) => "with",
        Stmt::Match(_) => "match",
        Stmt::Raise(_) => "raise",
        Stmt::Try(_) => "try",
        Stmt::Assert(_) => "assert",
        Stmt::Import(_) => "import",
        Stmt::ImportFrom(_) => "import_from",
        Stmt::Global(_) => "global",
        Stmt::Nonlocal(_) => "nonlocal",
        Stmt::Expr(_) => "expr",
        Stmt::Pass(_) => "pass",
        Stmt::Break(_) => "break",
        Stmt::Continue(_) => "continue",
        Stmt::IpyEscapeCommand(_) => "ipy_escape",
    }
}

fn binop_name(op: Operator) -> &'static str {
    match op {
        Operator::Add => "+",
        Operator::Sub => "-",
        Operator::Mult => "*",
        Operator::MatMult => "@",
        Operator::Div => "/",
        Operator::Mod => "%",
        Operator::Pow => "**",
        Operator::LShift => "<<",
        Operator::RShift => ">>",
        Operator::BitOr => "|",
        Operator::BitXor => "^",
        Operator::BitAnd => "&",
        Operator::FloorDiv => "//",
    }
}

fn unaryop_name(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Invert => "~",
        UnaryOp::Not => "not",
        UnaryOp::UAdd => "+u",
        UnaryOp::USub => "-u",
    }
}

fn cmpop_name(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "==",
        CmpOp::NotEq => "!=",
        CmpOp::Lt => "<",
        CmpOp::LtE => "<=",
        CmpOp::Gt => ">",
        CmpOp::GtE => ">=",
        CmpOp::Is => "is",
        CmpOp::IsNot => "is_not",
        CmpOp::In => "in",
        CmpOp::NotIn => "not_in",
    }
}

/// Maps byte offsets in the source to 1-based line numbers.
struct LineIndex {
    line_starts: Vec<u32>,
}

impl LineIndex {
    fn new(source: &str) -> Self {
        let mut line_starts = Vec::with_capacity(source.len() / 32 + 1);
        line_starts.push(0u32);
        for (i, byte) in source.bytes().enumerate() {
            if byte == b'\n' {
                let next = u32::try_from(i + 1).unwrap_or(u32::MAX);
                line_starts.push(next);
            }
        }
        Self { line_starts }
    }

    fn line_of(&self, offset: usize) -> usize {
        let target = u32::try_from(offset).unwrap_or(u32::MAX);
        match self.line_starts.binary_search(&target) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn extracts_top_level_and_method_complexity() {
        let src = "def a(x):\n    if x:\n        return 1\n    return 0\n\nclass C:\n    def m(self, y):\n        for i in y:\n            pass\n";
        let out = extract_complexity_units(src).expect("parse");
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].name, "a");
        assert_eq!(out[1].name, "C::m");
    }

    #[test]
    fn control_flow_increases_complexity() {
        let src = "def f(x):\n    if x and x > 0:\n        return 1\n    return 0\n";
        let out = extract_complexity_units(src).expect("parse");
        let f = &out[0];
        assert!(f.cyclomatic >= 3, "got {f:?}");
        assert!(f.cognitive >= 1, "got {f:?}");
        assert!(f.max_nesting >= 1, "got {f:?}");
    }

    #[test]
    fn halstead_is_populated() {
        let src = "def f(a, b):\n    return a + b\n";
        let out = extract_complexity_units(src).expect("parse");
        let h = out[0].halstead;
        assert!(h.total_operators > 0, "got {h:?}");
        assert!(h.total_operands > 0, "got {h:?}");
    }

    #[test]
    fn invalid_python_returns_parse_error() {
        let err = extract_complexity_units("def !!!(:").expect_err("must fail");
        assert!(format!("{err}").contains("failed to parse Python source"));
    }

    #[test]
    fn line_range_is_inclusive() {
        let src = "def f():\n    x = 1\n    return x\n";
        let out = extract_complexity_units(src).expect("parse");
        assert_eq!(out[0].start_line, 1);
        assert_eq!(out[0].end_line, 3);
    }

    #[test]
    fn nested_function_is_not_reported_as_top_level_unit() {
        let src = "def outer():\n    def inner():\n        return 1\n    return inner()\n";
        let out = extract_complexity_units(src).expect("parse");
        let names: HashMap<_, _> = out
            .iter()
            .map(|f| (f.name.as_str(), f.cyclomatic))
            .collect();
        assert!(names.contains_key("outer"));
        assert!(!names.contains_key("inner"));
    }
}
