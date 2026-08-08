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
//! Functions defined *inside* another function body — callbacks, event
//! handlers, closures — are extracted too, each as a `<parent>::closure#N`
//! unit minted by [`crate::walk`], as are the callbacks a module-scope
//! call registers (`describe("…", () => …)`, named by [`crate::harness`]).
//!
//! The actual AST traversal lives in [`crate::walk`]; this module is the
//! [`LanguageParser`]-shaped adapter that converts each visited
//! [`crate::walk::FunctionItem`] into a [`FunctionDef`].

use std::path::Path;

use lens_domain::{
    FunctionDef, FunctionSignature, LanguageParseError, LanguageParser, LineIndex, ReceiverShape,
    TreeNode, identifier_tokens,
};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::attrs::{name_looks_like_test_class, name_looks_like_test_function};
use crate::harness::{is_harness_segment, is_synthetic_segment};
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
    fn language(&self) -> &'static str {
        "typescript"
    }

    fn parse(&mut self, source: &str) -> Result<TreeNode, LanguageParseError> {
        let alloc = Allocator::default();
        let ret = Parser::new(&alloc, source, self.dialect.source_type()).parse();
        if !ret.diagnostics.is_empty() {
            let err = TsParseError::from_diagnostics(
                ret.diagnostics
                    .iter()
                    .map(|e| e.message.as_ref().to_owned()),
            );
            return Err(LanguageParseError::new(self.language(), err));
        }
        let mut root = TreeNode::new("Program", "");
        for stmt in &ret.program.body {
            root.push_child(statement_tree(stmt));
        }
        Ok(root)
    }

    fn extract_functions(&mut self, source: &str) -> Result<Vec<FunctionDef>, LanguageParseError> {
        extract_with(source, self.dialect)
            .map_err(|err| LanguageParseError::new(self.language(), err))
    }
}

fn extract_with(source: &str, dialect: Dialect) -> Result<Vec<FunctionDef>, TsParseError> {
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, source, dialect.source_type()).parse();
    if !ret.diagnostics.is_empty() {
        return Err(TsParseError::from_diagnostics(
            ret.diagnostics
                .iter()
                .map(|e| e.message.as_ref().to_owned()),
        ));
    }
    let line_index = LineIndex::new(source);
    let mut visitor = FunctionDefCollector {
        out: Vec::new(),
        jsdoc_by_attach: jsdoc_by_attach_offset(source, &ret.program.comments),
    };
    walk_program(&ret.program, &line_index, &mut visitor);
    Ok(visitor.out)
}

struct FunctionDefCollector {
    out: Vec<FunctionDef>,
    /// JSDoc text keyed by the byte offset of the token the comment is
    /// attached to, matched against [`FunctionItem::doc_attach_start`].
    jsdoc_by_attach: std::collections::HashMap<u32, String>,
}

impl FunctionVisitor for FunctionDefCollector {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        let is_test = is_test_item(&item.name);
        let doc = item
            .doc_attach_start
            .and_then(|attach| self.jsdoc_by_attach.get(&attach).cloned());
        let signature = signature_info(&item.name, item.params);
        self.out.push(FunctionDef {
            name: item.name,
            start_line: item.start_line,
            end_line: item.end_line,
            is_test,
            signature: Some(signature),
            doc,
            implements: None,
            tree: function_body_tree(item.body),
        });
    }
}

/// Project a function's parameters into the language-neutral
/// [`FunctionSignature`]. Return type and generics live on the AST
/// `Function` node, which the walker does not thread through
/// [`FunctionItem`]; signature-aware similarity still gets name tokens,
/// parameter names / count, and parameter type paths. TS/JS have no
/// syntactic receiver, so [`ReceiverShape::None`] is always correct.
fn signature_info(name: &str, params: &FormalParameters) -> FunctionSignature {
    let mut parameter_names = Vec::new();
    let mut parameter_type_paths = Vec::new();
    let mut parameter_count = 0usize;

    for param in &params.items {
        parameter_count += 1;
        collect_binding_names(&param.pattern, &mut parameter_names);
        if let Some(annotation) = &param.type_annotation {
            ts_type_paths(&annotation.type_annotation, &mut parameter_type_paths);
        }
    }
    if let Some(rest) = &params.rest {
        parameter_count += 1;
        collect_binding_names(&rest.rest.argument, &mut parameter_names);
        if let Some(annotation) = &rest.type_annotation {
            ts_type_paths(&annotation.type_annotation, &mut parameter_type_paths);
        }
    }

    FunctionSignature {
        name_tokens: identifier_tokens(bare_name(name)),
        parameter_count,
        parameter_names,
        parameter_type_paths,
        return_type_paths: Vec::new(),
        generics: Vec::new(),
        receiver: ReceiverShape::None,
    }
}

/// Last `::`-separated segment of a qualified name (`Foo::bar` -> `bar`),
/// so name tokens describe the function itself, not its owner.
fn bare_name(name: &str) -> &str {
    name.rsplit_once("::").map_or(name, |(_, last)| last)
}

/// Collect the binding identifiers introduced by a parameter pattern,
/// unwinding object / array destructuring and defaults.
fn collect_binding_names(pat: &BindingPattern, out: &mut Vec<String>) {
    match pat {
        BindingPattern::BindingIdentifier(id) => out.push(id.name.to_string()),
        BindingPattern::ObjectPattern(o) => {
            for prop in &o.properties {
                collect_binding_names(&prop.value, out);
            }
            if let Some(rest) = &o.rest {
                collect_binding_names(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(a) => {
            for elem in a.elements.iter().flatten() {
                collect_binding_names(elem, out);
            }
            if let Some(rest) = &a.rest {
                collect_binding_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(a) => collect_binding_names(&a.left, out),
    }
}

/// Flatten a TS type annotation to the head identifiers it references
/// (`Map<string, User>` -> `Map`, `string`, `User`). Shapes we don't
/// model contribute nothing, mirroring the Rust adapter's path flatten.
pub(crate) fn ts_type_paths(ty: &TSType, out: &mut Vec<String>) {
    match ty {
        TSType::TSTypeReference(r) => {
            type_name_head(&r.type_name, out);
            if let Some(args) = &r.type_arguments {
                for arg in &args.params {
                    ts_type_paths(arg, out);
                }
            }
        }
        TSType::TSArrayType(a) => ts_type_paths(&a.element_type, out),
        TSType::TSUnionType(u) => {
            for t in &u.types {
                ts_type_paths(t, out);
            }
        }
        TSType::TSIntersectionType(i) => {
            for t in &i.types {
                ts_type_paths(t, out);
            }
        }
        TSType::TSParenthesizedType(p) => ts_type_paths(&p.type_annotation, out),
        // A callback's own parameter and return types are the only thing
        // distinguishing `(id: UserId) => Article` from `() => void`, and
        // `--target types` renders every interface method as one of these.
        TSType::TSFunctionType(f) => {
            formal_parameter_type_paths(&f.params, out);
            ts_type_paths(&f.return_type.type_annotation, out);
        }
        TSType::TSNumberKeyword(_) => out.push("number".to_owned()),
        TSType::TSStringKeyword(_) => out.push("string".to_owned()),
        TSType::TSBooleanKeyword(_) => out.push("boolean".to_owned()),
        _ => {}
    }
}

/// Collect the type paths every parameter slot annotates, rest slot
/// included. Shared by function-type descent here and by the interface
/// method members `--target types` renders.
pub(crate) fn formal_parameter_type_paths(params: &FormalParameters, out: &mut Vec<String>) {
    let annotations = params
        .items
        .iter()
        .map(|param| &param.type_annotation)
        .chain(params.rest.iter().map(|rest| &rest.type_annotation));
    for annotation in annotations.flatten() {
        ts_type_paths(&annotation.type_annotation, out);
    }
}

fn type_name_head(name: &TSTypeName, out: &mut Vec<String>) {
    match name {
        TSTypeName::IdentifierReference(id) => out.push(id.name.to_string()),
        TSTypeName::QualifiedName(q) => out.push(q.right.name.to_string()),
        TSTypeName::ThisExpression(_) => {}
    }
}

/// Index every leading JSDoc block (`/** ... */`) by the byte offset of
/// the token it attaches to — oxc computes that attachment during
/// parsing. Multiple JSDoc blocks on one token keep the closest (last)
/// one, matching how TypeScript tooling resolves stacked doc blocks.
pub(crate) fn jsdoc_by_attach_offset(
    source: &str,
    comments: &[oxc_ast::ast::Comment],
) -> std::collections::HashMap<u32, String> {
    let mut out = std::collections::HashMap::new();
    for comment in comments {
        if !comment.is_jsdoc() {
            continue;
        }
        let span = comment.content_span();
        let Some(raw) = source.get(span.start as usize..span.end as usize) else {
            continue;
        };
        if let Some(text) = jsdoc_text(raw) {
            out.insert(comment.attached_to, text);
        }
    }
    out
}

/// Strip JSDoc decoration from the comment body: the leading `*` that
/// opens `/**` is already outside `content_span`, so this trims each
/// line's leading `*` gutter and drops blank edges. Returns `None` when
/// nothing but decoration remains.
fn jsdoc_text(raw: &str) -> Option<String> {
    let text = raw
        .lines()
        .map(|line| {
            let line = line.trim();
            line.strip_prefix('*').map_or(line, str::trim_start)
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    (!text.is_empty()).then_some(text)
}

/// True iff a [`FunctionItem`] qualified name belongs to test
/// scaffolding. Class methods come through as `ClassName::method`, so
/// we split on the last `::` to recover the immediate owner; namespaces
/// don't propagate as owners (the walker passes `None` through
/// `walk_module_body`) so a namespaced free function shows up bare
/// here.
///
/// A callback registered with a test harness (`it#1("adds")`) is test
/// code by construction, wherever it sits and whatever the file is
/// called. A plain nested function (`<parent>::closure#N`) carries no
/// marker of its own — it inherits the classification of the enclosing
/// named function or method, so synthetic segments are peeled off first.
pub(crate) fn is_test_item(qualified: &str) -> bool {
    if qualified.split("::").any(is_harness_segment) {
        return true;
    }
    let base = strip_nested_segments(qualified);
    match base.rsplit_once("::") {
        Some((owner, method)) => {
            name_looks_like_test_class(owner) || name_looks_like_test_function(method)
        }
        None => name_looks_like_test_function(base),
    }
}

/// Peel any trailing synthetic segments minted by [`crate::walk`] for
/// nested functions, returning the qualified name of the enclosing named
/// function or method (e.g. `test_foo::closure#1` → `test_foo`).
fn strip_nested_segments(name: &str) -> &str {
    let mut base = name;
    while let Some((head, tail)) = base.rsplit_once("::") {
        if is_synthetic_segment(tail) {
            base = head;
        } else {
            break;
        }
    }
    base
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
    fn function_signature_captures_params_and_types() {
        let src = "function loadUser(id: number, name: string): void {}\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert_eq!(sig.name_tokens, vec!["load".to_owned(), "user".to_owned()]);
        assert_eq!(sig.parameter_count, 2);
        assert_eq!(
            sig.parameter_names,
            vec!["id".to_owned(), "name".to_owned()],
        );
        assert!(sig.parameter_type_paths.contains(&"number".to_owned()));
        assert!(sig.parameter_type_paths.contains(&"string".to_owned()));
        assert_eq!(sig.receiver, ReceiverShape::None);
    }

    #[test]
    fn signature_flattens_generic_type_arguments() {
        let src = "function index(m: Map<string, User>): void {}\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        for expected in ["Map", "string", "User"] {
            assert!(
                sig.parameter_type_paths.contains(&expected.to_owned()),
                "missing {expected} in {:?}",
                sig.parameter_type_paths,
            );
        }
    }

    #[test]
    fn method_signature_uses_bare_name_tokens() {
        // A class method's name is qualified (`Svc::handle`); the name
        // tokens must describe the method, not the owner.
        let src = "class Svc { handleRequest(x: number) { return x; } }\n";
        let funcs = parse_functions(src);
        let method = funcs
            .iter()
            .find(|f| f.name.ends_with("::handleRequest"))
            .expect("method missing");
        let sig = method.signature.as_ref().expect("signature populated");
        assert_eq!(
            sig.name_tokens,
            vec!["handle".to_owned(), "request".to_owned()],
        );
        assert_eq!(sig.parameter_names, vec!["x".to_owned()]);
    }

    #[test]
    fn ts_type_paths_flatten_union_intersection_paren_and_keyword() {
        // Each parameter isolates one `ts_type_paths` arm: union
        // (`UnA`/`UnB`), intersection (`InA`/`InB`), parenthesized
        // (`ParenT`), and the boolean keyword.
        let src = "\
function f(
  a: UnA | UnB,
  b: InA & InB,
  c: (ParenT),
  d: boolean,
): void {}
";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        for expected in ["UnA", "UnB", "InA", "InB", "ParenT", "boolean"] {
            assert!(
                sig.parameter_type_paths.contains(&expected.to_owned()),
                "missing {expected} in {:?}",
                sig.parameter_type_paths,
            );
        }
    }

    /// A callback parameter's inner types are the only thing that tells
    /// two callbacks apart; without descending into the function type,
    /// `(cb: (id: UserId) => Article) => void` carries no type paths at
    /// all.
    #[test]
    fn ts_type_paths_descend_into_function_types() {
        let src = "function f(cb: (id: UserId, ...rest: Flag[]) => Article): void {}\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert_eq!(sig.parameter_type_paths, ["UserId", "Flag", "Article"]);
    }

    #[test]
    fn qualified_type_name_uses_rightmost_segment() {
        // `ns.Thing` should contribute `Thing`, exercising the
        // `QualifiedName` head.
        let src = "function f(a: ns.Thing): void {}\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert!(sig.parameter_type_paths.contains(&"Thing".to_owned()));
    }

    #[test]
    fn rest_parameter_is_counted() {
        let src = "function f(a: number, ...rest: string[]): void {}\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert_eq!(sig.parameter_count, 2);
        assert_eq!(sig.parameter_names, vec!["a".to_owned(), "rest".to_owned()]);
        assert!(sig.parameter_type_paths.contains(&"string".to_owned()));
    }

    #[rstest]
    #[case::function_decl(
        "/** Parse the user id. */\nfunction f() { let x = 1; }\n",
        Some("Parse the user id.")
    )]
    #[case::multiline_jsdoc(
        "/**\n * Parse the user id.\n * @returns the id\n */\nfunction f() { let x = 1; }\n",
        Some("Parse the user id.\n@returns the id")
    )]
    #[case::exported_function(
        "/** Parse the user id. */\nexport function f() { let x = 1; }\n",
        Some("Parse the user id.")
    )]
    #[case::const_arrow(
        "/** Parse the user id. */\nconst f = () => { let x = 1; };\n",
        Some("Parse the user id.")
    )]
    #[case::line_comment_is_not_jsdoc("// plain comment\nfunction f() { let x = 1; }\n", None)]
    #[case::plain_block_is_not_jsdoc("/* plain block */\nfunction f() { let x = 1; }\n", None)]
    #[case::no_comment("function f() { let x = 1; }\n", None)]
    fn extracts_jsdoc_text(#[case] src: &str, #[case] expected: Option<&str>) {
        let funcs = parse_functions(src);
        assert_eq!(funcs[0].doc.as_deref(), expected);
    }

    #[test]
    fn extracts_jsdoc_on_class_method_but_not_nested_closure() {
        let src = r#"
class Service {
    /** Handle the request. */
    handle(req: string) {
        const inner = () => { return req; };
        return inner();
    }
}
"#;
        let funcs = parse_functions(src);
        assert_eq!(funcs[0].name, "Service::handle");
        assert_eq!(funcs[0].doc.as_deref(), Some("Handle the request."));
        assert_eq!(funcs[1].name, "Service::handle::closure#1");
        assert_eq!(funcs[1].doc, None);
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
        assert_eq!(names, expected, "default extraction must keep every item");
        assert!(funcs.iter().all(|f| f.is_test));
    }

    #[test]
    fn extraction_marks_xunit_named_scaffolding() {
        // Production code surrounded by every shape the analyzer later
        // filters for TypeScript: an xUnit-style `test_*` free function,
        // a `Test*` class with helper methods, and a `test_*` method on
        // a regular class.
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
        let mut parser = TypeScriptParser::with_dialect(Dialect::Ts);
        let funcs = parser.extract_functions(src).unwrap();
        let flags: Vec<_> = funcs.iter().map(|f| (f.name.as_str(), f.is_test)).collect();
        assert_eq!(
            flags,
            [
                ("production", false),
                ("test_unit", true),
                ("Service::compute", false),
                ("Service::test_internal", true),
                ("TestThing::helper", true),
            ]
        );
    }

    #[test]
    fn extraction_marks_functions_without_test_markers_as_production() {
        let src = "function a(): void {}\nfunction b(): void {}\n";
        let funcs = parse_functions(src);
        assert!(funcs.iter().all(|f| !f.is_test));
    }

    #[test]
    fn extraction_surfaces_parse_errors() {
        let mut parser = TypeScriptParser::with_dialect(Dialect::Ts);
        let err = parser.extract_functions("function ??? {").unwrap_err();
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

    fn parse_tsx_functions(src: &str) -> Vec<FunctionDef> {
        let mut parser = TypeScriptParser::with_dialect(Dialect::Tsx);
        parser.extract_functions(src).unwrap()
    }

    /// Regression for issue #65: every TSX component used to collapse to a
    /// single `Expr` leaf during AST normalisation, so a tiny wrapper and a
    /// page bristling with markup both scored 1.0 against each other. With
    /// JSX subtrees lowered structurally, the small wrapper and the large
    /// page must not be reported as clones.
    #[test]
    fn small_and_large_tsx_components_do_not_score_as_clones() {
        let src = r#"
function Checkbox(props: { checked: boolean }) {
    return <input type="checkbox" checked={props.checked} />;
}

function MethodologyPage() {
    return (
        <article>
            <header><h1>Methodology</h1><p>An overview.</p></header>
            <section><h2>Step one</h2><p>First we look at inputs.</p></section>
            <section><h2>Step two</h2><p>Then we score them.</p></section>
            <section><h2>Step three</h2><ul><li>case a</li><li>case b</li></ul></section>
            <footer><p>End.</p></footer>
        </article>
    );
}
"#;
        let funcs = parse_tsx_functions(src);
        assert_eq!(funcs.len(), 2);
        let pairs = find_similar_functions(&funcs, 0.85, &TSEDOptions::default());
        assert!(
            pairs.is_empty(),
            "small wrapper must not cluster with a large page: {:?}",
            pairs
                .iter()
                .map(|p| (p.a.name.as_str(), p.b.name.as_str(), p.similarity))
                .collect::<Vec<_>>(),
        );
    }

    /// Before the fix, every `function () { return <X />; }` body lowered
    /// to `FunctionBody → Return → Expr`, so two unrelated components
    /// scored an exact 1.0 against each other. Pin the regression: even
    /// small components must not score a perfect 1.0 unless their JSX is
    /// genuinely identical.
    #[test]
    fn distinct_minimal_tsx_components_are_not_perfect_clones() {
        let src = r#"
function Checkbox() { return <input type="checkbox" />; }
function Spinner() { return <svg><circle r={4} /></svg>; }
"#;
        let funcs = parse_tsx_functions(src);
        let pairs = find_similar_functions(&funcs, 0.99, &TSEDOptions::default());
        assert!(
            pairs.is_empty(),
            "structurally different JSX bodies must not be reported as 1.0 clones: {:?}",
            pairs
                .iter()
                .map(|p| (p.a.name.as_str(), p.b.name.as_str(), p.similarity))
                .collect::<Vec<_>>(),
        );
    }

    /// Two React components whose JSX really is identical apart from
    /// identifier values (a fair clone) should still be flagged.
    #[test]
    fn structurally_identical_tsx_components_are_still_clones() {
        let src = r#"
function CardA(props: { title: string }) {
    return <div className="card"><h1>{props.title}</h1><p>body</p></div>;
}

function CardB(props: { title: string }) {
    return <div className="card"><h1>{props.title}</h1><p>body</p></div>;
}
"#;
        let funcs = parse_tsx_functions(src);
        let pairs = find_similar_functions(&funcs, 0.85, &TSEDOptions::default());
        assert!(
            !pairs.is_empty(),
            "structurally identical components should still be reported as similar",
        );
    }

    /// JSX fragments and elements lower to different labels, so a
    /// fragment-bodied component must not score 1.0 against one wrapped
    /// in a real element.
    #[test]
    fn jsx_fragment_does_not_match_element_with_same_children() {
        let src = r#"
function Frag() { return <><span>a</span><span>b</span></>; }
function Wrap() { return <div><span>a</span><span>b</span></div>; }
"#;
        let funcs = parse_tsx_functions(src);
        let pairs = find_similar_functions(&funcs, 0.99, &TSEDOptions::default());
        assert!(pairs.is_empty(), "fragment vs element must differ");
    }

    /// Search the lowered tree depth-first for the first node whose label
    /// matches. Lets the JSX-shape tests below assert on what was emitted
    /// without depending on the exact path through the AST.
    fn find_node<'a>(node: &'a TreeNode, label: &str) -> Option<&'a TreeNode> {
        if node.label == label {
            return Some(node);
        }
        node.children.iter().find_map(|c| find_node(c, label))
    }

    /// JSX element names must round-trip into the lowered node's value so
    /// downstream value-aware comparisons can tell `<Foo />` from `<Bar />`
    /// — pins `jsx_element_name`'s return string against mutation.
    #[test]
    fn jsx_element_name_is_preserved_on_lowered_tree() {
        let src = "function f() { return <Foo />; }\n";
        let funcs = parse_tsx_functions(src);
        let tree = &funcs[0].tree;
        let el = find_node(tree, "JSXElement").expect("JSXElement node");
        assert_eq!(el.value, "Foo");
    }

    /// HTML-style lowercase tag names must come through verbatim too —
    /// covers the `JSXElementName::Identifier` arm of `jsx_element_name`,
    /// which the `Foo` case (an `IdentifierReference`) misses.
    #[test]
    fn jsx_lowercase_element_name_is_preserved() {
        let src = "function f() { return <div />; }\n";
        let funcs = parse_tsx_functions(src);
        let el = find_node(&funcs[0].tree, "JSXElement").expect("JSXElement node");
        assert_eq!(el.value, "div");
    }

    /// Member-style element names (`<Foo.Bar />`, `<Foo.Bar.Baz />`) must
    /// be flattened with dots so the lowered value uniquely identifies the
    /// component path — pins `jsx_member_expression_name`'s output.
    #[test]
    fn jsx_member_element_name_uses_dotted_path() {
        let src = "function f() { return <Foo.Bar.Baz />; }\n";
        let funcs = parse_tsx_functions(src);
        let el = find_node(&funcs[0].tree, "JSXElement").expect("JSXElement node");
        assert_eq!(el.value, "Foo.Bar.Baz");
    }

    /// Fragments must lower to a `JSXFragment` node carrying their child
    /// arity rather than collapsing to the catch-all `Expr` leaf — pins
    /// the dedicated `Expression::JSXFragment` arm in `expr_tree`.
    #[test]
    fn jsx_fragment_lowers_to_dedicated_node_with_children() {
        let src = "function f() { return <><span>a</span><span>b</span></>; }\n";
        let funcs = parse_tsx_functions(src);
        let tree = &funcs[0].tree;
        let frag = find_node(tree, "JSXFragment").expect("JSXFragment node");
        assert_eq!(
            frag.children.len(),
            2,
            "fragment must carry its two child elements as structural children",
        );
        assert!(
            find_node(tree, "Expr").is_none(),
            "fragment must not fall through to the generic `Expr` leaf",
        );
    }

    fn names(funcs: &[FunctionDef]) -> Vec<&str> {
        funcs.iter().map(|f| f.name.as_str()).collect()
    }

    /// Every shape of function nested inside another function's body is
    /// extracted as a `<parent>::closure#N` unit alongside its parent —
    /// callbacks, event handlers and plain closures alike.
    #[rstest]
    #[case::arrow_binding("function setup() {\n    const handler = () => {};\n}\n")]
    #[case::event_handler_property(
        "function setup(btn: any) {\n    btn.onclick = () => doThing();\n}\n"
    )]
    #[case::callback_argument(
        "function setup(el: any) {\n    el.addEventListener(\"click\", () => run());\n}\n"
    )]
    #[case::nested_function_declaration("function setup() {\n    function inner() {}\n}\n")]
    #[case::nested_function_expression("function setup() {\n    const f = function () {};\n}\n")]
    fn extracts_nested_function_as_closure_unit(#[case] src: &str) {
        let funcs = parse_functions(src);
        assert_eq!(names(&funcs), ["setup", "setup::closure#1"]);
    }

    #[test]
    fn jsx_event_handler_is_extracted_as_a_nested_function() {
        // The canonical `onClick={() => …}` handler — a nested function
        // buried in a JSX attribute value.
        let src =
            "function App() {\n    return <button onClick={() => helper()}>Run</button>;\n}\n";
        let funcs = parse_tsx_functions(src);
        assert_eq!(names(&funcs), ["App", "App::closure#1"]);
    }

    #[test]
    fn sibling_nested_functions_are_numbered_in_source_order() {
        let src =
            "function setup() {\n    const first = () => {};\n    const second = () => {};\n}\n";
        let funcs = parse_functions(src);
        assert_eq!(
            names(&funcs),
            ["setup", "setup::closure#1", "setup::closure#2"],
        );
    }

    #[test]
    fn deeply_nested_functions_qualify_under_their_parent() {
        // A closure inside a closure resets the index and qualifies under
        // the immediate parent rather than the outermost function.
        let src = "function setup() {\n    const outer = () => {\n        const inner = () => {};\n    };\n}\n";
        let funcs = parse_functions(src);
        assert_eq!(
            names(&funcs),
            ["setup", "setup::closure#1", "setup::closure#1::closure#1"],
        );
    }

    #[test]
    fn nested_function_in_a_method_qualifies_under_the_method() {
        let src = "class Widget {\n    render() {\n        const onClick = () => this.update();\n    }\n}\n";
        let funcs = parse_functions(src);
        assert_eq!(
            names(&funcs),
            ["Widget::render", "Widget::render::closure#1"]
        );
    }

    #[test]
    fn nested_function_inside_a_test_function_inherits_the_test_flag() {
        // A closure has no test marker of its own; it must inherit the
        // classification of the enclosing `test_*` function so
        // `--exclude-tests` drops it alongside its parent.
        let src = "function test_setup() {\n    const handler = () => {};\n}\n";
        let funcs = parse_functions(src);
        assert_eq!(names(&funcs), ["test_setup", "test_setup::closure#1"]);
        assert!(
            funcs.iter().all(|f| f.is_test),
            "closure inside a test function must be marked test",
        );
    }

    #[test]
    fn nested_function_inside_production_function_stays_production() {
        let src = "function compute() {\n    const handler = () => {};\n}\n";
        let funcs = parse_functions(src);
        assert!(funcs.iter().all(|f| !f.is_test));
    }

    #[rstest]
    #[case::closure_in_test_function("test_run::closure#1", true)]
    #[case::deep_closure_in_test_function("test_run::closure#1::closure#2", true)]
    #[case::closure_in_test_class_method("TestSuite::check::closure#1", true)]
    #[case::closure_in_production_method("Service::compute::closure#1", false)]
    #[case::closure_in_production_function("compute::closure#1", false)]
    #[case::private_method_is_not_a_closure_segment("Service::#closure", false)]
    #[case::harness_callback("describe#1(\"groupFor\")::it#2(\"maps\")", true)]
    #[case::closure_inside_harness_callback("it#1(\"maps\")::closure#1", true)]
    #[case::harness_callback_in_production_function("mount::it#1(\"maps\")", true)]
    fn is_test_item_peels_closure_segments(#[case] qualified: &str, #[case] expected: bool) {
        assert_eq!(is_test_item(qualified), expected);
    }

    /// Peeling stops at the first segment the source actually named, so a
    /// closure's classification comes from its enclosing function.
    #[rstest]
    #[case::closure("test_run::closure#1", "test_run")]
    #[case::deep_closure("Svc::check::closure#1::closure#2", "Svc::check")]
    #[case::titled_callback("mount::it#1(\"maps\")", "mount")]
    #[case::nothing_to_peel("Service::compute", "Service::compute")]
    #[case::private_method_is_not_synthetic("Service::#closure", "Service::#closure")]
    fn strip_nested_segments_stops_at_the_first_real_name(
        #[case] qualified: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(strip_nested_segments(qualified), expected);
    }

    /// The shape of essentially every vitest / jest suite: a module-level
    /// call whose callbacks hold the whole file. Before these were
    /// walked, such a file extracted zero functions.
    #[test]
    fn module_level_harness_callbacks_are_extracted_and_named_after_the_call() {
        let src = "import { describe, it } from \"vitest\";\n\
             describe(\"checkConsistency\", () => {\n\
                 it(\"accepts numbers that agree\", () => {\n\
                     expect(checkConsistency(counted)).toEqual([]);\n\
                 });\n\
                 it(\"rejects a mismatch\", () => {\n\
                     expect(checkConsistency(other)).toHaveLength(1);\n\
                 });\n\
             });\n";
        let funcs = parse_functions(src);
        assert_eq!(
            names(&funcs),
            [
                "describe#1(\"checkConsistency\")",
                "describe#1(\"checkConsistency\")::it#1(\"accepts numbers that agree\")",
                "describe#1(\"checkConsistency\")::it#2(\"rejects a mismatch\")",
            ],
        );
        assert!(
            funcs.iter().all(|f| f.is_test),
            "a harness callback is test code by construction",
        );
    }

    #[test]
    fn sibling_module_level_suites_get_distinct_names() {
        // Both suites are the first callback of their own statement; only
        // a file-wide counter keeps them apart.
        let src = "describe(\"a\", () => {});\ndescribe(\"b\", () => {});\n";
        let funcs = parse_functions(src);
        assert_eq!(names(&funcs), ["describe#1(\"a\")", "describe#2(\"b\")"]);
    }

    #[rstest]
    #[case::hook("beforeEach(() => {\n    reset();\n});\n", "beforeEach#1")]
    #[case::modifier("it.skip(\"pends\", () => {\n    run();\n});\n", "it#1(\"pends\")")]
    #[case::table(
        "it.each([1, 2])(\"adds %i\", (n) => {\n    run(n);\n});\n",
        "it#1(\"adds %i\")"
    )]
    #[case::template_title(
        "it(`adds numbers`, () => {\n    run();\n});\n",
        "it#1(\"adds numbers\")"
    )]
    #[case::tagged_template_table(
        "it.each`\n  a\n  ${1}\n`(\"adds $a\", ({ a }) => {\n    run(a);\n});\n",
        "it#1(\"adds $a\")"
    )]
    #[case::function_expression_callback(
        "it(\"adds\", function () {\n    run();\n});\n",
        "it#1(\"adds\")"
    )]
    #[case::computed_title("it(caseName, () => {\n    run();\n});\n", "it#1")]
    // An interpolated title has no value until the suite runs, so half of
    // it would be a worse name than none at all.
    #[case::interpolated_title("it(`adds ${n}`, () => {\n    run();\n});\n", "it#1")]
    #[case::playwright(
        "test.describe(\"suite\", () => {\n    run();\n});\n",
        "test#1(\"suite\")"
    )]
    fn harness_callback_names_follow_the_registering_call(
        #[case] src: &str,
        #[case] expected: &str,
    ) {
        let funcs = parse_functions(src);
        assert_eq!(names(&funcs), [expected]);
    }

    /// The descent is general, not test-only: a statement-level call is
    /// walked whatever it registers. Without a harness callee the unit
    /// keeps the positional `closure#N` name and stays production code.
    #[rstest]
    #[case::server_callback("app.listen(3000, () => {\n    log(\"up\");\n});\n")]
    #[case::iife("(() => {\n    log(\"up\");\n})();\n")]
    fn module_level_callbacks_outside_a_harness_stay_closures(#[case] src: &str) {
        let funcs = parse_functions(src);
        assert_eq!(names(&funcs), ["closure#1"]);
        assert!(!funcs[0].is_test);
    }

    #[test]
    fn a_test_title_cannot_split_a_qualified_name() {
        // `::` in a title would otherwise mint extra name segments and
        // make the unit look like it lives under an owner.
        let src = "it(\"handles Foo::bar() well\", () => {\n    run();\n});\n";
        let funcs = parse_functions(src);
        assert_eq!(names(&funcs), ["it#1(\"handles Foo bar well\")"]);
    }

    #[test]
    fn identical_nested_callbacks_are_detected_as_clones() {
        // Two functions register a structurally identical callback. The
        // callbacks are their own units, so the duplication surfaces even
        // though the registering functions differ.
        let src = r#"
function setupA(el: any): void {
    el.addEventListener("click", () => {
        let total = 0;
        for (const x of [1, 2, 3]) {
            total += x;
        }
        report(total);
    });
}

function setupB(el: any): void {
    el.on("press", () => {
        let sum = 0;
        for (const y of [1, 2, 3]) {
            sum += y;
        }
        report(sum);
    });
}
"#;
        let funcs = parse_functions(src);
        let pairs = find_similar_functions(&funcs, 0.85, &TSEDOptions::default());
        assert!(
            pairs.iter().any(|p| {
                let names = [p.a.name.as_str(), p.b.name.as_str()];
                names.contains(&"setupA::closure#1") && names.contains(&"setupB::closure#1")
            }),
            "expected the two callbacks to cluster: {:?}",
            pairs
                .iter()
                .map(|p| (p.a.name.as_str(), p.b.name.as_str()))
                .collect::<Vec<_>>(),
        );
    }
}
