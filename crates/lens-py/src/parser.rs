//! ruff-based implementation of [`lens_domain::LanguageParser`] for Python.

use lens_domain::{FunctionDef, LanguageParser, TreeNode};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_python_parser::{ParseError, parse_module};

/// A Python-language parser backed by [`ruff_python_parser`].
///
/// Stateless; all work happens inside [`LanguageParser::parse`] and
/// [`LanguageParser::extract_functions`]. The struct exists so that callers
/// can swap in a tree-sitter backend later without changing downstream code.
#[derive(Debug, Default, Clone, Copy)]
pub struct PythonParser;

impl PythonParser {
    pub fn new() -> Self {
        Self
    }
}

/// Parse failures surfaced by [`PythonParser`].
#[derive(Debug)]
pub enum PythonParseError {
    Parse(ParseError),
}

impl std::fmt::Display for PythonParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "failed to parse Python source: {e}"),
        }
    }
}

impl std::error::Error for PythonParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Parse(e) => Some(e),
        }
    }
}

impl From<ParseError> for PythonParseError {
    fn from(value: ParseError) -> Self {
        Self::Parse(value)
    }
}

impl LanguageParser for PythonParser {
    type Error = PythonParseError;

    fn language(&self) -> &'static str {
        "python"
    }

    fn parse(&mut self, source: &str) -> Result<TreeNode, Self::Error> {
        let module = parse_module(source)?.into_syntax();
        let mut builder = TreeBuilder::new("Module");
        for stmt in &module.body {
            builder.visit_stmt(stmt);
        }
        Ok(builder.finish())
    }

    fn extract_functions(&mut self, source: &str) -> Result<Vec<FunctionDef>, Self::Error> {
        let module = parse_module(source)?.into_syntax();
        let lines = LineIndex::new(source);
        let mut out = Vec::new();
        for stmt in &module.body {
            collect_stmt(stmt, None, &lines, &mut out);
        }
        Ok(out)
    }
}

fn collect_stmt(stmt: &Stmt, owner: Option<&str>, lines: &LineIndex, out: &mut Vec<FunctionDef>) {
    match stmt {
        Stmt::FunctionDef(func) => {
            let qualified = qualify_name(owner, func.name.as_str());
            out.push(function_def_from(func, &qualified, lines));
            // Recurse into the body so nested `def`s and inner classes still
            // surface as their own [`FunctionDef`] entries.
            for inner in &func.body {
                collect_stmt(inner, None, lines, out);
            }
        }
        Stmt::ClassDef(class) => collect_class(class, lines, out),
        _ => {}
    }
}

fn collect_class(class: &StmtClassDef, lines: &LineIndex, out: &mut Vec<FunctionDef>) {
    let class_name = class.name.as_str();
    for inner in &class.body {
        collect_stmt(inner, Some(class_name), lines, out);
    }
}

fn function_def_from(func: &StmtFunctionDef, name: &str, lines: &LineIndex) -> FunctionDef {
    let start_line = lines.line_of(func.range.start().to_usize());
    // `range.end()` lands at the position just past the last byte of the
    // body; we want the line that byte sits on.
    let end_offset = func.range.end().to_usize().saturating_sub(1);
    let end_line = lines.line_of(end_offset);
    let mut builder = TreeBuilder::new("Block");
    for stmt in &func.body {
        builder.visit_stmt(stmt);
    }
    FunctionDef {
        name: name.to_owned(),
        start_line,
        end_line,
        tree: builder.finish(),
    }
}

fn qualify_name(owner: Option<&str>, method: &str) -> String {
    match owner {
        Some(owner) => format!("{owner}::{method}"),
        None => method.to_owned(),
    }
}

/// Maps byte offsets in the source to 1-based line numbers.
struct LineIndex {
    /// Byte offset of the start of each line. Always begins with `0`.
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

    /// 1-based line number containing the given byte offset.
    fn line_of(&self, offset: usize) -> usize {
        let target = u32::try_from(offset).unwrap_or(u32::MAX);
        match self.line_starts.binary_search(&target) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        }
    }
}

/// Builds a [`TreeNode`] tree by walking the AST with [`Visitor`].
///
/// The stack always holds the open ancestor chain; `enter` pushes a fresh
/// node, `leave` pops the top and attaches it to the new top. Every `enter`
/// pairs with exactly one `leave`, so the root remains in place until
/// [`Self::finish`] is called.
struct TreeBuilder {
    stack: Vec<TreeNode>,
}

impl TreeBuilder {
    fn new(root_label: &str) -> Self {
        Self {
            stack: vec![TreeNode::new(root_label, "")],
        }
    }

    fn enter(&mut self, label: &'static str, value: &str) {
        self.stack.push(TreeNode::new(label, value));
    }

    fn leave(&mut self) {
        if let Some(child) = self.stack.pop() {
            if let Some(parent) = self.stack.last_mut() {
                parent.push_child(child);
            } else {
                // Underflow: re-push so we never lose the root. This branch
                // is unreachable when callers pair `enter`/`leave` correctly.
                self.stack.push(child);
            }
        }
    }

    fn finish(mut self) -> TreeNode {
        while self.stack.len() > 1 {
            self.leave();
        }
        self.stack
            .pop()
            .unwrap_or_else(|| TreeNode::new("Block", ""))
    }
}

impl<'a> Visitor<'a> for TreeBuilder {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        let label = stmt_label(stmt);
        let value = stmt_value(stmt);
        self.enter(label, value);
        walk_stmt(self, stmt);
        self.leave();
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        let label = expr_label(expr);
        let value = expr_value(expr);
        self.enter(label, &value);
        walk_expr(self, expr);
        self.leave();
    }
}

fn stmt_label(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::FunctionDef(_) => "FunctionDef",
        Stmt::ClassDef(_) => "ClassDef",
        Stmt::Return(_) => "Return",
        Stmt::Delete(_) => "Delete",
        Stmt::Assign(_) => "Assign",
        Stmt::AugAssign(_) => "AugAssign",
        Stmt::AnnAssign(_) => "AnnAssign",
        Stmt::TypeAlias(_) => "TypeAlias",
        Stmt::For(_) => "For",
        Stmt::While(_) => "While",
        Stmt::If(_) => "If",
        Stmt::With(_) => "With",
        Stmt::Match(_) => "Match",
        Stmt::Raise(_) => "Raise",
        Stmt::Try(_) => "Try",
        Stmt::Assert(_) => "Assert",
        Stmt::Import(_) => "Import",
        Stmt::ImportFrom(_) => "ImportFrom",
        Stmt::Global(_) => "Global",
        Stmt::Nonlocal(_) => "Nonlocal",
        Stmt::Expr(_) => "Expr",
        Stmt::Pass(_) => "Pass",
        Stmt::Break(_) => "Break",
        Stmt::Continue(_) => "Continue",
        Stmt::IpyEscapeCommand(_) => "IpyEscapeCommand",
    }
}

fn stmt_value(stmt: &Stmt) -> &str {
    match stmt {
        Stmt::FunctionDef(f) => f.name.as_str(),
        Stmt::ClassDef(c) => c.name.as_str(),
        _ => "",
    }
}

fn expr_label(expr: &Expr) -> &'static str {
    match expr {
        Expr::BoolOp(_) => "BoolOp",
        Expr::Named(_) => "Named",
        Expr::BinOp(_) => "BinOp",
        Expr::UnaryOp(_) => "UnaryOp",
        Expr::Lambda(_) => "Lambda",
        Expr::If(_) => "IfExpr",
        Expr::Dict(_) => "Dict",
        Expr::Set(_) => "Set",
        Expr::ListComp(_) => "ListComp",
        Expr::SetComp(_) => "SetComp",
        Expr::DictComp(_) => "DictComp",
        Expr::Generator(_) => "Generator",
        Expr::Await(_) => "Await",
        Expr::Yield(_) => "Yield",
        Expr::YieldFrom(_) => "YieldFrom",
        Expr::Compare(_) => "Compare",
        Expr::Call(_) => "Call",
        Expr::FString(_) => "FString",
        Expr::StringLiteral(_) => "Str",
        Expr::BytesLiteral(_) => "Bytes",
        Expr::NumberLiteral(_) => "Num",
        Expr::BooleanLiteral(_) => "Bool",
        Expr::NoneLiteral(_) => "None",
        Expr::EllipsisLiteral(_) => "Ellipsis",
        Expr::Attribute(_) => "Attribute",
        Expr::Subscript(_) => "Subscript",
        Expr::Starred(_) => "Starred",
        Expr::Name(_) => "Name",
        Expr::List(_) => "List",
        Expr::Tuple(_) => "Tuple",
        Expr::Slice(_) => "Slice",
        Expr::IpyEscapeCommand(_) => "IpyEscape",
    }
}

fn expr_value(expr: &Expr) -> String {
    match expr {
        Expr::Name(name) => name.id.to_string(),
        Expr::Attribute(attr) => attr.attr.as_str().to_owned(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lens_domain::{TSEDOptions, calculate_tsed, find_similar_functions};

    fn parse_functions(src: &str) -> Vec<FunctionDef> {
        let mut parser = PythonParser::new();
        parser.extract_functions(src).unwrap()
    }

    #[test]
    fn extracts_top_level_function_name_and_lines() {
        let src = "def first():\n    pass\ndef second():\n    x = 1\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 2);
        assert_eq!(funcs[0].name, "first");
        assert_eq!(funcs[1].name, "second");
        assert_eq!(funcs[0].start_line, 1);
        assert_eq!(funcs[0].end_line, 2);
        assert_eq!(funcs[1].start_line, 3);
        assert_eq!(funcs[1].end_line, 4);
    }

    #[test]
    fn end_line_tracks_last_body_line_for_multi_line_function() {
        let src = "def body():\n    x = 1\n    y = 2\n    return x + y\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].start_line, 1);
        assert_eq!(funcs[0].end_line, 4);
    }

    #[test]
    fn language_identifier_is_python() {
        let parser = PythonParser::new();
        assert_eq!(parser.language(), "python");
    }

    #[test]
    fn parse_error_exposes_underlying_ruff_error_via_source() {
        let mut parser = PythonParser::new();
        let err = parser.parse("def !!!(:").unwrap_err();
        let source = std::error::Error::source(&err).expect("source should be Some");
        assert!(!format!("{source}").is_empty());
    }

    #[test]
    fn extracts_class_methods_with_qualified_names() {
        let src = "class Foo:\n    def bar(self):\n        return 1\n    def baz(self):\n        return 2\n";
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Foo::bar", "Foo::baz"]);
    }

    #[test]
    fn extracts_async_functions() {
        let src = "async def fetch(url):\n    return await get(url)\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "fetch");
    }

    #[test]
    fn extracts_nested_def_inside_function() {
        let src = "def outer():\n    def inner():\n        return 1\n    return inner\n";
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"outer"));
        assert!(names.contains(&"inner"));
    }

    #[test]
    fn parse_returns_error_for_invalid_python() {
        let mut parser = PythonParser::new();
        let err = parser.parse("def !!!(:").unwrap_err();
        assert!(format!("{err}").contains("failed to parse Python source"));
    }

    #[test]
    fn clones_are_detected_as_highly_similar() {
        let src = "
def original(xs):
    total = 0
    for x in xs:
        total += x
    return total

def cloned(ys):
    sum_ = 0
    for y in ys:
        sum_ += y
    return sum_
";
        let funcs = parse_functions(src);
        let opts = TSEDOptions::default();
        let sim = calculate_tsed(&funcs[0].tree, &funcs[1].tree, &opts);
        assert!(
            sim > 0.9,
            "expected renamed clone to stay > 0.9 similar, got {sim}"
        );
    }

    #[test]
    fn structurally_different_functions_score_low() {
        let src = "
def loopy(xs):
    total = 0
    for x in xs:
        total += x
    return total

def recursive(n):
    if n == 0:
        return 0
    return n + recursive(n - 1)
";
        let funcs = parse_functions(src);
        let opts = TSEDOptions::default();
        let sim = calculate_tsed(&funcs[0].tree, &funcs[1].tree, &opts);
        assert!(
            sim < 0.8,
            "expected structurally different functions to score < 0.8, got {sim}"
        );
    }

    fn parse_tree(src: &str) -> TreeNode {
        let mut parser = PythonParser::new();
        parser.parse(src).unwrap()
    }

    fn find_label<'a>(node: &'a TreeNode, label: &str) -> Option<&'a TreeNode> {
        if node.label == label {
            return Some(node);
        }
        for c in &node.children {
            if let Some(found) = find_label(c, label) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn parse_records_function_def_label_and_name_value() {
        let tree = parse_tree("def hello():\n    pass\n");
        let func = find_label(&tree, "FunctionDef").expect("FunctionDef present");
        assert_eq!(
            func.value, "hello",
            "FunctionDef should expose its name as the node value",
        );
    }

    #[test]
    fn parse_records_class_def_label_and_name_value() {
        let tree = parse_tree("class Bar:\n    pass\n");
        let class = find_label(&tree, "ClassDef").expect("ClassDef present");
        assert_eq!(class.value, "Bar");
    }

    #[test]
    fn parse_records_name_expression_with_identifier() {
        let tree = parse_tree("x = y\n");
        let name = find_label(&tree, "Name").expect("Name node present");
        // `y` is a Name expression in the RHS; the identifier becomes the value.
        assert!(
            name.value == "y" || name.value == "x",
            "Name node value should be the identifier (got {:?})",
            name.value,
        );
    }

    #[test]
    fn parse_records_attribute_expression_with_attr_name() {
        let tree = parse_tree("y = obj.field\n");
        let attr = find_label(&tree, "Attribute").expect("Attribute node present");
        // The attribute name (right-hand side of the dot) is the value.
        assert_eq!(attr.value, "field");
    }

    #[test]
    fn parse_walks_into_expressions_so_call_nodes_appear() {
        // visit_expr must descend; if it short-circuits, `Call` (and its
        // children) never enter the tree.
        let tree = parse_tree("x = f(1)\n");
        assert!(
            find_label(&tree, "Call").is_some(),
            "Call expression should be present in the tree",
        );
    }

    #[test]
    fn parse_finishes_into_a_single_root_for_multi_statement_input() {
        // `finish` unwinds the stack until exactly one node remains. With
        // a multi-statement program it must still return the `Module` root,
        // not the most recently pushed child.
        let tree = parse_tree("x = 1\ny = 2\nz = 3\n");
        assert_eq!(tree.label, "Module");
        // The Module has at least the three Assign children visible.
        let assign_count = tree.children.iter().filter(|c| c.label == "Assign").count();
        assert!(
            assign_count >= 3,
            "expected at least 3 Assign children under root, got {assign_count} ({tree:?})",
        );
    }

    #[test]
    fn parse_distinguishes_for_while_and_if_labels() {
        let src = "
for x in xs:
    pass
while True:
    pass
if cond:
    pass
";
        let tree = parse_tree(src);
        assert!(find_label(&tree, "For").is_some(), "For label missing");
        assert!(find_label(&tree, "While").is_some(), "While label missing");
        assert!(find_label(&tree, "If").is_some(), "If label missing");
    }

    #[test]
    fn find_similar_functions_reports_clone_pair() {
        let src = "
def a(xs):
    t = 0
    for x in xs:
        t += x
    return t

def b(ys):
    s = 0
    for y in ys:
        s += y
    return s

def c(n):
    if n == 0:
        return 0
    return n * 2
";
        let funcs = parse_functions(src);
        let pairs = find_similar_functions(&funcs, 0.85, &TSEDOptions::default());
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a.name, "a");
        assert_eq!(pairs[0].b.name, "b");
    }
}
