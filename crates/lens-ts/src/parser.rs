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

use std::path::Path;

use lens_domain::{FunctionDef, LanguageParser, TreeNode};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::attrs::{name_looks_like_test_class, name_looks_like_test_function};
use crate::line_index::LineIndex;
use crate::tree::{expr_tree, function_body_tree};
use crate::walk::{FunctionItem, FunctionVisitor, walk_program};

/// Source dialect handed to the oxc parser.
///
/// Picking the right dialect matters because the JSX-flavoured variants
/// (`Tsx`, `Jsx`) tell the parser to accept `<Foo />` as an expression;
/// passing a plain `Ts` source type to a `.tsx` file errors out. The
/// JavaScript variants additionally carry the right module-kind (script
/// vs ESM vs CommonJS) so analyses don't trip over `module.exports = ...`
/// in `.cjs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    /// `.ts` — TypeScript without JSX.
    #[default]
    Ts,
    /// `.tsx` — TypeScript with JSX.
    Tsx,
    /// `.mts` — TypeScript ES module.
    Mts,
    /// `.cts` — TypeScript CommonJS module.
    Cts,
    /// `.js` — JavaScript without JSX.
    Js,
    /// `.jsx` — JavaScript with JSX.
    Jsx,
    /// `.mjs` — JavaScript ES module.
    Mjs,
    /// `.cjs` — JavaScript CommonJS module.
    Cjs,
}

impl Dialect {
    /// Resolve a [`Dialect`] from a bare file extension (no leading dot).
    /// Returns `None` for anything outside the TS/JS family.
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext {
            "ts" => Some(Self::Ts),
            "tsx" => Some(Self::Tsx),
            "mts" => Some(Self::Mts),
            "cts" => Some(Self::Cts),
            "js" => Some(Self::Js),
            "jsx" => Some(Self::Jsx),
            "mjs" => Some(Self::Mjs),
            "cjs" => Some(Self::Cjs),
            _ => None,
        }
    }

    /// Resolve a [`Dialect`] from a file path's extension.
    pub fn from_path(path: &Path) -> Option<Self> {
        path.extension()
            .and_then(|e| e.to_str())
            .and_then(Self::from_extension)
    }

    /// Convert to the oxc parser's [`SourceType`]. Each arm is spelled out
    /// rather than calling `SourceType::from_extension` so the mapping is
    /// total and infallible at compile time.
    pub(crate) fn source_type(self) -> SourceType {
        match self {
            Self::Ts => SourceType::ts(),
            Self::Tsx => SourceType::tsx(),
            Self::Mts => SourceType::ts().with_module(true),
            Self::Cts => SourceType::ts().with_commonjs(true),
            // `.js` and `.mjs` are both ESM under oxc's `from_path` rules;
            // we keep `.js` as plain JavaScript without JSX so a stray
            // `<` is parsed as a comparison, not a JSX element. Files
            // that need JSX should be named `.jsx`.
            Self::Js => SourceType::mjs().with_jsx(false),
            Self::Jsx => SourceType::jsx(),
            Self::Mjs => SourceType::mjs(),
            Self::Cjs => SourceType::cjs(),
        }
    }
}

/// TypeScript / JavaScript parser.
///
/// The parser carries its [`Dialect`] so a single instance always feeds
/// the same `SourceType` to oxc. Use [`TypeScriptParser::new`] for the
/// default `.ts` dialect, or [`TypeScriptParser::with_dialect`] when the
/// caller already knows the file's extension.
#[derive(Debug, Default, Clone, Copy)]
pub struct TypeScriptParser {
    dialect: Dialect,
}

impl TypeScriptParser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_dialect(dialect: Dialect) -> Self {
        Self { dialect }
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
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
        let ret = Parser::new(&alloc, source, self.dialect.source_type()).parse();
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
        extract_with(source, self.dialect, ExtractOptions::default())
    }
}

/// Like [`TypeScriptParser::extract_functions`] but drops items that
/// look like xUnit-style test scaffolding: declaration-level functions
/// whose name matches the `test` / `test_*` convention, and methods of
/// classes whose name starts with `Test`.
///
/// Anonymous arrow callbacks passed to `describe()` / `it()` / `test()`
/// are already invisible to the walker (they aren't bound to a name),
/// so this filter only catches the named, declaration-level shapes
/// that slip through. Used by analysers (similarity, future cohesion /
/// complexity reports) that want to look at production code only.
pub fn extract_functions_excluding_tests(
    source: &str,
    dialect: Dialect,
) -> Result<Vec<FunctionDef>, TsParseError> {
    extract_with(
        source,
        dialect,
        ExtractOptions {
            exclude_tests: true,
        },
    )
}

#[derive(Default, Clone, Copy)]
struct ExtractOptions {
    exclude_tests: bool,
}

fn extract_with(
    source: &str,
    dialect: Dialect,
    opts: ExtractOptions,
) -> Result<Vec<FunctionDef>, TsParseError> {
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, source, dialect.source_type()).parse();
    if !ret.errors.is_empty() {
        return Err(TsParseError::from_diagnostics(
            ret.errors.iter().map(|e| e.message.as_ref().to_owned()),
        ));
    }
    let line_index = LineIndex::new(source);
    let mut visitor = FunctionDefCollector {
        opts,
        out: Vec::new(),
    };
    walk_program(&ret.program, &line_index, &mut visitor);
    Ok(visitor.out)
}

struct FunctionDefCollector {
    opts: ExtractOptions,
    out: Vec<FunctionDef>,
}

impl FunctionVisitor for FunctionDefCollector {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        if self.opts.exclude_tests && is_test_item(&item.name) {
            return;
        }
        self.out.push(FunctionDef {
            name: item.name,
            start_line: item.start_line,
            end_line: item.end_line,
            tree: function_body_tree(item.body),
        });
    }
}

/// True iff a [`FunctionItem`] qualified name belongs to test
/// scaffolding. Class methods come through as `ClassName::method`, so
/// we split on the last `::` to recover the immediate owner; namespaces
/// don't propagate as owners (the walker passes `None` through
/// `walk_module_body`) so a namespaced free function shows up bare
/// here.
fn is_test_item(qualified: &str) -> bool {
    match qualified.rsplit_once("::") {
        Some((owner, method)) => {
            name_looks_like_test_class(owner) || name_looks_like_test_function(method)
        }
        None => name_looks_like_test_function(qualified),
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
    fn extracts_functions_inside_exported_namespace() {
        // `export namespace foo { ... }` wraps the inner namespace in
        // an `ExportNamedDeclaration` whose `declaration` is the
        // `Declaration::TSModuleDeclaration` arm of `walk_decl`. The
        // top-level `namespace foo` form goes through `walk_stmt` —
        // only `export namespace` reaches the analogous arm in
        // `walk_decl`, so it needs its own coverage.
        let src = r#"
export namespace outer {
    export function exported_inner(): void {}
}
"#;
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "exported_inner");
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

    /// Default `extract_functions` keeps every item — even what
    /// `--exclude-tests` would drop. If the boolean guards in the
    /// collector ever degrade to constants the default contract would
    /// silently break, so each test-flavoured shape gets a default-mode
    /// case here.
    #[rstest]
    #[case::xunit_test_function("function test_foo(): void {}\n", &["test_foo"][..])]
    #[case::just_test("function test(): void {}\n", &["test"][..])]
    #[case::test_class(
        "class TestThing {\n    helper(): number { return 1; }\n}\n",
        &["TestThing::helper"][..],
    )]
    #[case::test_class_method(
        "class Foo {\n    test_a(): void {}\n}\n",
        &["Foo::test_a"][..],
    )]
    fn default_extraction_includes_test_flavoured_items(
        #[case] src: &str,
        #[case] expected: &[&str],
    ) {
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names, expected,
            "default extraction must keep every item; only --exclude-tests should drop them",
        );
    }

    #[test]
    fn excluding_tests_drops_xunit_named_scaffolding() {
        // Production code surrounded by every shape `--exclude-tests`
        // is supposed to filter for TypeScript: an xUnit-style
        // `test_*` free function, a `Test*` class with helper
        // methods, and a `test_*` method on a regular class. The
        // production class method (`Service::compute`) should
        // survive — covers the negative branch of the test-class
        // check (a mutant that always returns `true` would drop it
        // and fail the assertion).
        let src = r#"
function production(x: number): number {
    return x + 1;
}

function test_unit(): void {
    if (production(0) !== 1) throw new Error("bad");
}

class Service {
    compute(x: number): number {
        return production(x);
    }
    test_internal(): void {
        // xUnit-style method on a production class.
    }
}

class TestThing {
    helper(): number {
        return production(0);
    }
}
"#;
        let funcs = extract_functions_excluding_tests(src, Dialect::Ts).unwrap();
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["production", "Service::compute"]);
    }

    #[test]
    fn excluding_tests_keeps_default_extraction_with_no_test_markers() {
        // No test-shaped items — the filter should be a no-op so the
        // public surface still reports every production function.
        let src = "function a(): void {}\nfunction b(): void {}\n";
        let baseline = parse_functions(src);
        let filtered = extract_functions_excluding_tests(src, Dialect::Ts).unwrap();
        assert_eq!(
            baseline.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            filtered.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn excluding_tests_surfaces_parse_errors() {
        let err = extract_functions_excluding_tests("function ??? {", Dialect::Ts).unwrap_err();
        assert!(format!("{err}").contains("failed to parse TypeScript source"));
    }

    #[test]
    fn tsx_dialect_accepts_jsx_syntax() {
        // Plain `Dialect::Ts` rejects `<Foo />` because the `<` is read
        // as a less-than. `Dialect::Tsx` flips the JSX flag on the oxc
        // parser, so the same source must round-trip.
        let src = "function Comp(): JSX.Element { return <div />; }\n";
        let mut parser = TypeScriptParser::with_dialect(Dialect::Tsx);
        let funcs =
            <TypeScriptParser as LanguageParser>::extract_functions(&mut parser, src).unwrap();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "Comp");
    }

    #[test]
    fn jsx_dialect_accepts_jsx_in_javascript() {
        // `.jsx` files have no type annotations but do use JSX.
        let src = "function Comp() { return <div className=\"x\">hi</div>; }\n";
        let mut parser = TypeScriptParser::with_dialect(Dialect::Jsx);
        let funcs =
            <TypeScriptParser as LanguageParser>::extract_functions(&mut parser, src).unwrap();
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "Comp");
    }

    #[test]
    fn ts_dialect_rejects_jsx_syntax() {
        // Negative case — without TSX, the same input must not silently
        // succeed (a regression here would mean the dialect is ignored).
        let src = "function Comp(): JSX.Element { return <div />; }\n";
        let mut parser = TypeScriptParser::with_dialect(Dialect::Ts);
        assert!(<TypeScriptParser as LanguageParser>::extract_functions(&mut parser, src).is_err());
    }

    #[test]
    fn dialect_resolves_from_extensions() {
        for (ext, expected) in [
            ("ts", Dialect::Ts),
            ("tsx", Dialect::Tsx),
            ("mts", Dialect::Mts),
            ("cts", Dialect::Cts),
            ("js", Dialect::Js),
            ("jsx", Dialect::Jsx),
            ("mjs", Dialect::Mjs),
            ("cjs", Dialect::Cjs),
        ] {
            assert_eq!(Dialect::from_extension(ext), Some(expected));
        }
        assert_eq!(Dialect::from_extension("rs"), None);
    }

    #[test]
    fn dialect_resolves_from_path() {
        assert_eq!(
            Dialect::from_path(Path::new("src/App.tsx")),
            Some(Dialect::Tsx),
        );
        assert_eq!(Dialect::from_path(Path::new("Makefile")), None);
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
