//! oxc-based implementation of [`lens_domain::LanguageParser`] for
//! TypeScript / JavaScript.
//!
//! Functions are extracted from:
//!
//! * `function` declarations,
//! * `class` methods (qualified as `ClassName::method`),
//! * `const` / `let` / `var` initialisers that are arrow functions or
//!   function expressions (qualified to the binding's identifier).
//!
//! Items declared inside `namespace` / `module` blocks are walked
//! recursively, mirroring how `lens-rust` walks inline `mod foo {}`.
//! Functions defined *inside* another function body are deliberately
//! left out; their containing function is the unit of analysis.
//!
//! The actual AST traversal lives in [`crate::walk`]; this module is the
//! [`LanguageParser`]-shaped adapter that converts each visited
//! [`crate::walk::FunctionItem`] into a [`FunctionDef`].

use lens_domain::{FunctionDef, LanguageParser, TreeNode};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::line_index::LineIndex;
use crate::tree::{expr_tree, function_body_tree};
use crate::walk::{FunctionItem, FunctionVisitor, walk_program};

/// TypeScript / JavaScript parser. Stateless; configurable per call via
/// `SourceType` (defaults to `.ts`).
#[derive(Debug, Default, Clone, Copy)]
pub struct TypeScriptParser;

impl TypeScriptParser {
    pub fn new() -> Self {
        Self
    }
}

/// Parse failures surfaced by [`TypeScriptParser`].
#[derive(Debug, thiserror::Error)]
pub enum TsParseError {
    /// One or more errors were emitted by `oxc_parser`.
    #[error("failed to parse TypeScript source: {message}")]
    Parse {
        /// Stringified diagnostics, joined by `\n`. We swallow the rich
        /// `oxc_diagnostics` types here to keep the public surface small —
        /// callers that want structured errors should reach for the
        /// underlying parser directly.
        message: String,
        #[source]
        source: std::io::Error,
    },
}

impl TsParseError {
    pub(crate) fn from_diagnostics<I>(errors: I) -> Self
    where
        I: IntoIterator,
        I::Item: std::fmt::Display,
    {
        let message = errors
            .into_iter()
            .map(|e| format!("{e}"))
            .collect::<Vec<_>>()
            .join("\n");
        let source = std::io::Error::other(message.clone());
        Self::Parse { message, source }
    }
}

impl LanguageParser for TypeScriptParser {
    type Error = TsParseError;

    fn language(&self) -> &'static str {
        "typescript"
    }

    fn parse(&mut self, source: &str) -> Result<TreeNode, Self::Error> {
        let alloc = Allocator::default();
        let ret = Parser::new(&alloc, source, SourceType::ts()).parse();
        if !ret.errors.is_empty() {
            return Err(TsParseError::from_diagnostics(
                ret.errors.iter().map(|e| e.message.as_ref().to_owned()),
            ));
        }
        let mut root = TreeNode::new("Program", "");
        for stmt in &ret.program.body {
            root.push_child(statement_tree(stmt));
        }
        Ok(root)
    }

    fn extract_functions(&mut self, source: &str) -> Result<Vec<FunctionDef>, Self::Error> {
        let alloc = Allocator::default();
        let ret = Parser::new(&alloc, source, SourceType::ts()).parse();
        if !ret.errors.is_empty() {
            return Err(TsParseError::from_diagnostics(
                ret.errors.iter().map(|e| e.message.as_ref().to_owned()),
            ));
        }
        let line_index = LineIndex::new(source);
        let mut visitor = FunctionDefCollector::default();
        walk_program(&ret.program, &line_index, &mut visitor);
        Ok(visitor.out)
    }
}

#[derive(Default)]
struct FunctionDefCollector {
    out: Vec<FunctionDef>,
}

impl FunctionVisitor for FunctionDefCollector {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        self.out.push(FunctionDef {
            name: item.name,
            start_line: item.start_line,
            end_line: item.end_line,
            tree: function_body_tree(item.body),
        });
    }
}

fn statement_tree(stmt: &Statement) -> TreeNode {
    // Re-using the body-tree builder for arbitrary statements keeps
    // labelling consistent across `parse` and `extract_functions`.
    let mut node = TreeNode::new("Stmt", "");
    if let Statement::ExpressionStatement(e) = stmt {
        node.push_child(expr_tree(&e.expression));
    } else if let Some(body) = stmt_block_body(stmt) {
        for s in body {
            node.push_child(statement_tree(s));
        }
    }
    node
}

fn stmt_block_body<'a>(stmt: &'a Statement<'a>) -> Option<&'a [Statement<'a>]> {
    if let Statement::BlockStatement(b) = stmt {
        Some(&b.body)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lens_domain::{TSEDOptions, calculate_tsed, find_similar_functions};
    use rstest::rstest;

    fn parse_functions(src: &str) -> Vec<FunctionDef> {
        let mut parser = TypeScriptParser::new();
        parser.extract_functions(src).unwrap()
    }

    #[test]
    fn extracts_top_level_function_name_and_lines() {
        let src = "function first() {}\nfunction second() { let x = 1; }\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 2);
        assert_eq!(funcs[0].name, "first");
        assert_eq!(funcs[1].name, "second");
        assert_eq!(funcs[0].start_line, 1);
        assert_eq!(funcs[1].start_line, 2);
    }

    #[test]
    fn end_line_tracks_closing_brace_for_multi_line_function() {
        let src = "function body() {\n    const x = 1;\n    const y = 2;\n}\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].start_line, 1);
        assert_eq!(funcs[0].end_line, 4);
    }

    #[test]
    fn language_identifier_is_typescript() {
        let parser = TypeScriptParser::new();
        assert_eq!(parser.language(), "typescript");
    }

    #[test]
    fn extracts_class_methods_with_qualified_names() {
        let src = r#"
class Foo {
    bar(): number { return 1; }
    baz(): number { return 2; }
}
"#;
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Foo::bar", "Foo::baz"]);
    }

    /// Each binding form should produce exactly one [`FunctionDef`] with
    /// the binding's identifier as its name. The cases share a body so a
    /// single rstest captures them without leaving 5 near-identical
    /// `extracts_*` tests for the similarity analyzer to flag.
    #[rstest]
    #[case::arrow_const_binding("const add = (a: number, b: number): number => a + b;\n", "add")]
    #[case::function_expression_let_binding("let f = function () { return 1; };\n", "f")]
    #[case::function_inside_namespace(
        "namespace inner {\n    export function hidden(): number { return 0; }\n}\n",
        "hidden"
    )]
    #[case::exported_function_declaration(
        "export function exported(): number { return 1; }\n",
        "exported"
    )]
    #[case::function_overload_signatures_skipped(
        "function f(x: number): number;\nfunction f(x: string): string;\nfunction f(x: any): any { return x; }\n",
        "f"
    )]
    fn extracts_single_named_function(#[case] src: &str, #[case] expected_name: &str) {
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1, "expected one function in: {src}");
        assert_eq!(funcs[0].name, expected_name);
    }

    #[test]
    fn parse_returns_error_for_invalid_typescript() {
        let mut parser = TypeScriptParser::new();
        let err = parser.parse("function ??? {").unwrap_err();
        assert!(format!("{err}").contains("failed to parse TypeScript source"));
    }

    #[test]
    fn clones_are_detected_as_highly_similar() {
        let src = r#"
function original(xs: number[]): number {
    let total = 0;
    for (const x of xs) {
        total += x;
    }
    return total;
}

function cloned(ys: number[]): number {
    let sum = 0;
    for (const y of ys) {
        sum += y;
    }
    return sum;
}
"#;
        let funcs = parse_functions(src);
        let opts = TSEDOptions::default();
        let sim = calculate_tsed(&funcs[0].tree, &funcs[1].tree, &opts);
        assert!(
            sim > 0.85,
            "expected renamed clone to stay > 0.85 similar, got {sim}"
        );
    }

    #[test]
    fn find_similar_functions_reports_clone_pair() {
        let src = r#"
function a(xs: number[]): number {
    let t = 0;
    for (const x of xs) { t += x; }
    return t;
}

function b(ys: number[]): number {
    let s = 0;
    for (const y of ys) { s += y; }
    return s;
}

function c(n: number): number {
    if (n === 0) { return 0; } else { return n * 2; }
}
"#;
        let funcs = parse_functions(src);
        let pairs = find_similar_functions(&funcs, 0.8, &TSEDOptions::default());
        assert!(!pairs.is_empty());
        let names: Vec<_> = pairs
            .iter()
            .map(|p| (p.a.name.as_str(), p.b.name.as_str()))
            .collect();
        assert!(names.contains(&("a", "b")) || names.contains(&("b", "a")));
    }
}
