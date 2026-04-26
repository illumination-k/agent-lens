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

use lens_domain::{FunctionDef, LanguageParser, TreeNode};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::line_index::LineIndex;
use crate::tree::{expr_tree, function_body_tree};

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
        Self::Parse { message }
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
        let mut out = Vec::new();
        for stmt in &ret.program.body {
            collect_stmt(stmt, None, &line_index, &mut out);
        }
        Ok(out)
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

fn collect_stmt(
    stmt: &Statement,
    owner: Option<&str>,
    line_index: &LineIndex,
    out: &mut Vec<FunctionDef>,
) {
    match stmt {
        Statement::FunctionDeclaration(f) => {
            if let Some(def) = function_def_from_function(f, owner, line_index) {
                out.push(def);
            }
        }
        Statement::ClassDeclaration(c) => collect_class(c, line_index, out),
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                collect_variable_declarator(d, line_index, out);
            }
        }
        Statement::ExportNamedDeclaration(e) => {
            if let Some(decl) = &e.declaration {
                collect_decl(decl, owner, line_index, out);
            }
        }
        Statement::ExportDefaultDeclaration(e) => match &e.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                if let Some(def) = function_def_from_function(f, owner, line_index) {
                    out.push(def);
                }
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
    out: &mut Vec<FunctionDef>,
) {
    match decl {
        Declaration::FunctionDeclaration(f) => {
            if let Some(def) = function_def_from_function(f, owner, line_index) {
                out.push(def);
            }
        }
        Declaration::ClassDeclaration(c) => collect_class(c, line_index, out),
        Declaration::VariableDeclaration(v) => {
            for d in &v.declarations {
                collect_variable_declarator(d, line_index, out);
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
    out: &mut Vec<FunctionDef>,
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

fn collect_class(class: &Class, line_index: &LineIndex, out: &mut Vec<FunctionDef>) {
    let class_name = class
        .id
        .as_ref()
        .map(|i| i.name.as_str())
        .unwrap_or("anonymous");
    for elem in &class.body.body {
        if let ClassElement::MethodDefinition(m) = elem
            && let Some(name) = method_key_name(&m.key)
        {
            let qualified = format!("{class_name}::{name}");
            if let Some(body) = &m.value.body {
                let start_line = line_index.line(m.span.start);
                let end_line = line_index.line(m.span.end);
                out.push(FunctionDef {
                    name: qualified,
                    start_line,
                    end_line,
                    tree: function_body_tree(body),
                });
            }
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

fn collect_variable_declarator(
    decl: &VariableDeclarator,
    line_index: &LineIndex,
    out: &mut Vec<FunctionDef>,
) {
    let Some(init) = &decl.init else {
        return;
    };
    let BindingPattern::BindingIdentifier(id) = &decl.id else {
        return;
    };
    match init {
        Expression::ArrowFunctionExpression(arrow) => {
            let start_line = line_index.line(decl.span.start);
            let end_line = line_index.line(arrow.body.span.end);
            out.push(FunctionDef {
                name: id.name.to_string(),
                start_line,
                end_line,
                tree: function_body_tree(&arrow.body),
            });
        }
        Expression::FunctionExpression(f) => {
            if let Some(body) = &f.body {
                let start_line = line_index.line(decl.span.start);
                let end_line = line_index.line(body.span.end);
                out.push(FunctionDef {
                    name: id.name.to_string(),
                    start_line,
                    end_line,
                    tree: function_body_tree(body),
                });
            }
        }
        _ => {}
    }
}

fn function_def_from_function(
    func: &Function,
    owner: Option<&str>,
    line_index: &LineIndex,
) -> Option<FunctionDef> {
    let body = func.body.as_ref()?;
    let raw_name = func
        .id
        .as_ref()
        .map(|i| i.name.as_str())
        .unwrap_or("anonymous");
    let name = match owner {
        Some(o) => format!("{o}::{raw_name}"),
        None => raw_name.to_owned(),
    };
    Some(FunctionDef {
        name,
        start_line: line_index.line(func.span.start),
        end_line: line_index.line(body.span.end),
        tree: function_body_tree(body),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lens_domain::{TSEDOptions, calculate_tsed, find_similar_functions};

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

    #[test]
    fn extracts_arrow_const_binding() {
        let src = "const add = (a: number, b: number): number => a + b;\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "add");
    }

    #[test]
    fn extracts_function_expression_let_binding() {
        let src = "let f = function () { return 1; };\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "f");
    }

    #[test]
    fn extracts_functions_inside_namespace() {
        let src = r#"
namespace inner {
    export function hidden(): number { return 0; }
}
"#;
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "hidden");
    }

    #[test]
    fn extracts_exported_function_declaration() {
        let src = "export function exported(): number { return 1; }\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "exported");
    }

    #[test]
    fn skips_function_overload_signatures() {
        // Overload signatures have no body and should be ignored; only
        // the implementation function is reported.
        let src = r#"
function f(x: number): number;
function f(x: string): string;
function f(x: any): any { return x; }
"#;
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "f");
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
