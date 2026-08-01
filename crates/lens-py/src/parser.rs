//! ruff-based implementation of [`lens_domain::LanguageParser`] for Python.

use lens_domain::{
    FunctionDef, FunctionSignature, LanguageParseError, LanguageParser, LineIndex, ReceiverShape,
    TreeNode, identifier_tokens, qualify as qualify_name,
};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtFunctionDef};
use ruff_python_parser::{ParseError, parse_module};

use crate::walk::walk_module_fns;

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
#[derive(Debug, thiserror::Error)]
pub enum PythonParseError {
    #[error("failed to parse Python source: {0}")]
    Parse(#[from] ParseError),
}

impl LanguageParser for PythonParser {
    fn language(&self) -> &'static str {
        "python"
    }

    fn parse(&mut self, source: &str) -> Result<TreeNode, LanguageParseError> {
        let module = parse_module(source)
            .map_err(PythonParseError::from)
            .map_err(|err| LanguageParseError::new(self.language(), err))?
            .into_syntax();
        let mut builder = TreeBuilder::new("Module");
        for stmt in &module.body {
            builder.visit_stmt(stmt);
        }
        Ok(builder.finish())
    }

    fn extract_functions(&mut self, source: &str) -> Result<Vec<FunctionDef>, LanguageParseError> {
        extract_with(source).map_err(|err| LanguageParseError::new(self.language(), err))
    }
}

fn extract_with(source: &str) -> Result<Vec<FunctionDef>, PythonParseError> {
    let module = parse_module(source)?.into_syntax();
    let lines = LineIndex::new(source);
    let mut out = Vec::new();
    walk_module_fns(&module.body, &mut |site| {
        let qualified = qualify_name(site.owner, site.func.name.as_str());
        out.push(function_def_from(
            site.func,
            &qualified,
            site.is_test,
            site.owner.is_some(),
            &lines,
        ));
    });
    Ok(out)
}

fn function_def_from(
    func: &StmtFunctionDef,
    name: &str,
    is_test: bool,
    is_method: bool,
    lines: &LineIndex,
) -> FunctionDef {
    let start_line = lines.line(func.range.start().to_u32());
    // `range.end()` lands at the position just past the last byte of the
    // body; we want the line that byte sits on.
    let end_offset = func.range.end().to_u32().saturating_sub(1);
    let end_line = lines.line(end_offset);
    FunctionDef {
        name: name.to_owned(),
        start_line,
        end_line,
        is_test,
        signature: Some(signature_info(func, is_method)),
        doc: docstring_text(func),
        tree: function_body_tree(func),
    }
}

/// Project a `def` into the language-neutral [`FunctionSignature`]. A
/// leading `self` / `cls` on a method is treated as the receiver (and
/// dropped from the parameter list) so instance methods compare against
/// each other rather than being skewed by the implicit first argument.
/// Type annotations are flattened to their head identifiers, mirroring
/// how the Rust adapter reduces types to path segments.
fn signature_info(func: &StmtFunctionDef, is_method: bool) -> FunctionSignature {
    let params = &func.parameters;
    let mut parameter_names = Vec::new();
    let mut parameter_type_paths = Vec::new();
    let mut parameter_count = 0usize;
    let mut receiver = ReceiverShape::None;

    let mut non_variadic = params.iter_non_variadic_params();
    if is_method && let Some(first) = non_variadic.next() {
        let first_name = first.parameter.name.as_str();
        if first_name == "self" || first_name == "cls" {
            receiver = ReceiverShape::Value;
        } else {
            // Not a conventional receiver — keep it as a real parameter.
            record_param(
                first,
                &mut parameter_count,
                &mut parameter_names,
                &mut parameter_type_paths,
            );
        }
    }
    for param in non_variadic {
        record_param(
            param,
            &mut parameter_count,
            &mut parameter_names,
            &mut parameter_type_paths,
        );
    }
    for variadic in [params.vararg.as_deref(), params.kwarg.as_deref()]
        .into_iter()
        .flatten()
    {
        parameter_count += 1;
        parameter_names.push(variadic.name.as_str().to_owned());
        if let Some(annotation) = &variadic.annotation {
            annotation_paths(annotation, &mut parameter_type_paths);
        }
    }

    let mut return_type_paths = Vec::new();
    if let Some(returns) = &func.returns {
        annotation_paths(returns, &mut return_type_paths);
    }

    FunctionSignature {
        name_tokens: identifier_tokens(func.name.as_str()),
        parameter_count,
        parameter_names,
        parameter_type_paths,
        return_type_paths,
        // PEP 695 type parameters are not surfaced yet.
        generics: Vec::new(),
        receiver,
    }
}

fn record_param(
    param: &ruff_python_ast::ParameterWithDefault,
    count: &mut usize,
    names: &mut Vec<String>,
    type_paths: &mut Vec<String>,
) {
    *count += 1;
    names.push(param.parameter.name.as_str().to_owned());
    if let Some(annotation) = &param.parameter.annotation {
        annotation_paths(annotation, type_paths);
    }
}

/// Flatten a type annotation to the head identifiers it references:
/// `list[int]` contributes `list` and `int`, `a.B` contributes `B`,
/// `X | Y` contributes both. Best-effort — shapes we don't model
/// contribute nothing rather than raw text.
pub(crate) fn annotation_paths(expr: &Expr, out: &mut Vec<String>) {
    match expr {
        Expr::Name(name) => out.push(name.id.as_str().to_owned()),
        Expr::Attribute(attr) => out.push(attr.attr.as_str().to_owned()),
        Expr::Subscript(sub) => {
            annotation_paths(&sub.value, out);
            annotation_paths(&sub.slice, out);
        }
        Expr::Tuple(tuple) => {
            for elt in &tuple.elts {
                annotation_paths(elt, out);
            }
        }
        Expr::List(list) => {
            for elt in &list.elts {
                annotation_paths(elt, out);
            }
        }
        Expr::BinOp(binop) => {
            annotation_paths(&binop.left, out);
            annotation_paths(&binop.right, out);
        }
        _ => {}
    }
}

/// PEP 257 docstring: a string-literal expression statement as the first
/// statement of the body. Implicit concatenation is folded by ruff's AST,
/// so `to_str` sees the full text. Returns `None` when absent or blank.
pub(crate) fn docstring_text(func: &StmtFunctionDef) -> Option<String> {
    let Some(Stmt::Expr(first)) = func.body.first() else {
        return None;
    };
    let Expr::StringLiteral(literal) = first.value.as_ref() else {
        return None;
    };
    let text = literal.value.to_str().trim().to_owned();
    (!text.is_empty()).then_some(text)
}

/// Lower a function body into a generic [`TreeNode`] rooted at `Block`,
/// matching the shape similarity / wrapper analyzers expect.
pub(crate) fn function_body_tree(func: &StmtFunctionDef) -> TreeNode {
    let mut builder = TreeBuilder::new("Block");
    for stmt in &func.body {
        builder.visit_stmt(stmt);
    }
    builder.finish()
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
        Expr::TString(_) => "TString",
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
    use rstest::rstest;

    fn parse_functions(src: &str) -> Vec<FunctionDef> {
        let mut parser = PythonParser::new();
        parser.extract_functions(src).unwrap()
    }

    #[test]
    fn free_function_signature_captures_annotations() {
        let src = "def parse_user(name: str, count: int) -> list[int]:\n    return []\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert_eq!(sig.name_tokens, vec!["parse".to_owned(), "user".to_owned()]);
        assert_eq!(sig.parameter_count, 2);
        assert_eq!(
            sig.parameter_names,
            vec!["name".to_owned(), "count".to_owned()],
        );
        assert!(sig.parameter_type_paths.contains(&"str".to_owned()));
        assert!(sig.parameter_type_paths.contains(&"int".to_owned()));
        assert!(sig.return_type_paths.contains(&"list".to_owned()));
        assert!(sig.return_type_paths.contains(&"int".to_owned()));
        assert_eq!(sig.receiver, ReceiverShape::None);
    }

    #[test]
    fn method_drops_self_receiver() {
        let src = "class C:\n    def handle(self, x: int) -> int:\n        return x\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert_eq!(sig.receiver, ReceiverShape::Value);
        // `self` is the receiver, not a parameter.
        assert_eq!(sig.parameter_count, 1);
        assert_eq!(sig.parameter_names, vec!["x".to_owned()]);
    }

    #[test]
    fn staticmethod_without_self_has_no_receiver() {
        // A method whose first parameter is not self/cls keeps every
        // parameter and reports no receiver.
        let src = "class C:\n    def make(value: int) -> int:\n        return value\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert_eq!(sig.receiver, ReceiverShape::None);
        assert_eq!(sig.parameter_count, 1);
        assert_eq!(sig.parameter_names, vec!["value".to_owned()]);
    }

    #[test]
    fn annotation_paths_flatten_qualified_union_and_containers() {
        // Each parameter isolates one `annotation_paths` arm via unique
        // type names: attribute (`t.Path`), tuple slice (`KeyT`/`ValT`),
        // list inside a subscript (`ArgT`), and union (`bytes`/`float`).
        let src = "\
def f(
    a: t.Path,
    b: dict[KeyT, ValT],
    c: Callable[[ArgT], RetT],
) -> bytes | float:
    return b
";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        // Attribute arm.
        assert!(sig.parameter_type_paths.contains(&"Path".to_owned()));
        // Tuple-slice arm (`KeyT`/`ValT` appear only inside the tuple).
        assert!(sig.parameter_type_paths.contains(&"KeyT".to_owned()));
        assert!(sig.parameter_type_paths.contains(&"ValT".to_owned()));
        // List arm (`ArgT` appears only inside the nested `[...]`).
        assert!(sig.parameter_type_paths.contains(&"ArgT".to_owned()));
        // BinOp (union) arm.
        assert!(sig.return_type_paths.contains(&"bytes".to_owned()));
        assert!(sig.return_type_paths.contains(&"float".to_owned()));
    }

    #[test]
    fn variadic_parameters_are_counted() {
        let src = "def f(a, *args, **kwargs):\n    return a\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert_eq!(sig.parameter_count, 3);
        assert_eq!(
            sig.parameter_names,
            vec!["a".to_owned(), "args".to_owned(), "kwargs".to_owned()],
        );
    }

    #[rstest]
    #[case::docstring(
        "def f():\n    \"\"\"Parse the user id.\"\"\"\n    return 1\n",
        Some("Parse the user id.")
    )]
    #[case::multiline_docstring(
        "def f():\n    \"\"\"Parse the id.\n\n    Returns None on failure.\n    \"\"\"\n    return 1\n",
        Some("Parse the id.\n\n    Returns None on failure.")
    )]
    #[case::no_docstring("def f():\n    return 1\n", None)]
    #[case::string_not_first("def f():\n    x = 1\n    \"\"\"not a docstring\"\"\"\n", None)]
    fn extracts_docstring_text(#[case] src: &str, #[case] expected: Option<&str>) {
        let funcs = parse_functions(src);
        assert_eq!(funcs[0].doc.as_deref(), expected);
    }

    #[test]
    fn extracts_top_level_function_name_and_lines() {
        // Real bodies for both functions: a stub like `pass` would now
        // be filtered as Protocol/abstract noise, defeating the test.
        let src = "def first():\n    return 1\ndef second():\n    x = 1\n";
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
    fn nested_def_inside_function_is_not_a_separate_unit() {
        // Function bodies are atomic: nested `def`s contribute to the
        // parent's body but don't surface as their own entries. Matches
        // the documented `lens-ts` behaviour and `lens-rust`'s decision
        // to keep closures inside their parent fn.
        let src = "def outer():\n    def inner():\n        return 1\n    return inner\n";
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["outer"]);
    }

    #[test]
    fn nested_class_inside_function_is_not_a_separate_unit() {
        // Same rule as nested `def`: an inner class only shows up
        // through its outer function's tree.
        let src =
            "def outer():\n    class Inner:\n        def m(self): return 1\n    return Inner\n";
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["outer"]);
    }

    /// Default `extract_functions` keeps every item — even what
    /// `--exclude-tests` would drop. If the boolean guards in
    /// `collect_*` ever degrade to constants the default contract would
    /// silently break, so each test-flavoured shape gets a default-mode
    /// case here.
    // Bodies use `assert True` / `return 1` rather than `pass` so that
    // the stub filter (Protocol / abstract / `pass`-only) doesn't drop
    // these items before the test-flavour filter has a chance to be
    // observed. The test is about the test-flavour filter; the stub
    // filter is orthogonal and applies always.
    #[rstest]
    #[case::pytest_test_function("def test_foo():\n    assert True\n", &["test_foo"][..])]
    #[case::pytest_fixture(
        "import pytest\n@pytest.fixture\ndef sample():\n    return 1\n",
        &["sample"][..],
    )]
    #[case::pytest_mark_skip(
        "import pytest\n@pytest.mark.skip\ndef test_skip():\n    assert True\n",
        &["test_skip"][..],
    )]
    #[case::test_class(
        "class TestThing:\n    def helper(self):\n        return 1\n",
        &["TestThing::helper"][..],
    )]
    #[case::unittest_testcase(
        "import unittest\nclass Foo(unittest.TestCase):\n    def test_a(self):\n        assert True\n",
        &["Foo::test_a"][..],
    )]
    fn default_extraction_includes_test_flavoured_items(
        #[case] src: &str,
        #[case] expected: &[&str],
    ) {
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, expected, "default extraction must keep every item");
        assert!(funcs.iter().all(|f| f.is_test));
    }

    #[test]
    fn extraction_marks_pytest_and_unittest_scaffolding() {
        // Production code surrounded by every shape the analyzer later
        // filters: a pytest test fn, a pytest fixture, a `pytest.mark.*`
        // test, a `Test*` class, and a `unittest.TestCase` subclass.
        let src = "
import pytest
import unittest

def production(x):
    return x + 1

def test_unit():
    assert production(0) == 1

@pytest.fixture
def sample():
    return 1

@pytest.mark.parametrize(\"x\", [1, 2])
def test_param(x):
    assert production(x) > 0

class Service:
    def compute(self, x):
        return production(x)

class TestThing:
    def helper(self):
        return production(0)

class Other(unittest.TestCase):
    def test_method(self):
        self.assertEqual(production(0), 1)

@unittest.skip(\"flaky\")
def disabled():
    return 0
";
        let mut parser = PythonParser::new();
        let funcs = parser.extract_functions(src).unwrap();
        let flags: Vec<_> = funcs.iter().map(|f| (f.name.as_str(), f.is_test)).collect();
        assert_eq!(
            flags,
            [
                ("production", false),
                ("test_unit", true),
                ("sample", true),
                ("test_param", true),
                ("Service::compute", false),
                ("TestThing::helper", true),
                ("Other::test_method", true),
                ("disabled", true),
            ]
        );
    }

    #[test]
    fn extraction_marks_functions_without_test_markers_as_production() {
        let src = "def a():\n    return 1\ndef b():\n    return 2\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 2);
        assert!(funcs.iter().all(|f| !f.is_test));
    }

    #[test]
    fn parse_returns_error_for_invalid_python() {
        let mut parser = PythonParser::new();
        let err = parser.parse("def !!!(:").unwrap_err();
        assert!(format!("{err}").contains("failed to parse Python source"));
    }

    /// Protocol / abstract / overload / stub-bodied functions carry no
    /// analysable content; every Protocol method body collapses to the
    /// same one-node tree, which would dominate similarity reports.
    /// These cases must be dropped by the parser before any analyser
    /// sees them.
    #[rstest]
    #[case::protocol_class(
        "from typing import Protocol\nclass Foo(Protocol):\n    def f(self, x): ...\n    def g(self, x): ...\n"
    )]
    #[case::generic_protocol_class(
        "from typing import Protocol\nclass Foo(Protocol[T]):\n    def f(self, x): ...\n"
    )]
    #[case::abstractmethod(
        "from abc import abstractmethod\nclass Foo:\n    @abstractmethod\n    def f(self): ...\n"
    )]
    #[case::overload("from typing import overload\n@overload\ndef f(x: int) -> int: ...\n")]
    #[case::ellipsis_body("def f(x): ...\n")]
    #[case::pass_body("def f(x):\n    pass\n")]
    #[case::docstring_only("def f(x):\n    \"\"\"docstring\"\"\"\n")]
    #[case::raise_not_implemented("def f(x):\n    raise NotImplementedError\n")]
    fn stub_shaped_functions_are_dropped(#[case] src: &str) {
        let funcs = parse_functions(src);
        assert!(
            funcs.is_empty(),
            "stub-shaped function should be filtered, got {:?}",
            funcs.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn abc_subclass_keeps_concrete_methods_and_drops_abstract_ones() {
        // Mixed ABC: only the `@abstractmethod` method is a stub. The
        // concrete sibling must survive — a class-level filter that
        // dropped the whole subtree would over-reach the same way
        // skipping every Protocol class would not.
        let src = "
from abc import ABC, abstractmethod

class Animal(ABC):
    @abstractmethod
    def speak(self): ...

    def common(self):
        return 'common'
";
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Animal::common"]);
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
    fn parse_records_template_string_expression_label() {
        let tree = parse_tree("x = t'hello {name}'\n");
        assert!(
            find_label(&tree, "TString").is_some(),
            "template string expression should be present in the tree",
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
