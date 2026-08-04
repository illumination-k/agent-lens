//! TypeScript / JavaScript function and call-shape extraction for the
//! function graph analyzer.
//!
//! This stays syntax-only: imports are resolved to lexical module paths
//! when they are relative, receiver calls are kept unresolved unless they
//! look like namespace/static calls, and no type inference is attempted.

use std::collections::HashSet;

use lens_domain::{
    BodyShape, CallShape, FunctionShape, ImportShape, LexicalResolutionStatus, LineIndex,
    OwnerKind, OwnerShape, ReceiverExprKind, SourceSpan, SyntaxFact, callee_names_local_binding,
    qualify_module, starts_uppercase,
};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;
use oxc_syntax::scope::ScopeFlags;

use crate::attrs::name_looks_like_test_function;
use crate::parser::{Dialect, TsParseError};
use crate::tree::function_body_tree;
use crate::walk::{FunctionItem, FunctionVisitor, walk_program};

/// Extract neutral function-shape facts for TypeScript / JavaScript.
pub fn extract_function_shapes_with_module(
    source: &str,
    dialect: Dialect,
    module: &str,
) -> Result<Vec<FunctionShape>, TsParseError> {
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
    let mut collector = FunctionShapeCollector {
        module: module.to_owned(),
        out: Vec::new(),
        jsdoc_by_attach: crate::parser::jsdoc_by_attach_offset(source, &ret.program.comments),
    };
    walk_program(&ret.program, &line_index, &mut collector);
    Ok(collector.out)
}

/// Extract neutral call-shape facts for TypeScript / JavaScript.
pub fn extract_call_shapes_with_module(
    source: &str,
    dialect: Dialect,
    module: &str,
) -> Result<Vec<CallShape>, TsParseError> {
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
    let imports = collect_imports(&ret.program, module);
    let namespace_aliases = imports
        .iter()
        .filter_map(|import| {
            let alias = import.local_alias.known_value().and_then(Option::as_ref)?;
            let exported = import
                .exported_symbol
                .known_value()
                .and_then(Option::as_ref);
            (exported.is_none()).then(|| alias.clone())
        })
        .collect();
    let mut collector = CallShapeCollector {
        module: module.to_owned(),
        line_index: &line_index,
        imports,
        namespace_aliases,
        out: Vec::new(),
    };
    walk_program(&ret.program, &line_index, &mut collector);
    Ok(collector.out)
}

struct FunctionShapeCollector {
    module: String,
    out: Vec<FunctionShape>,
    jsdoc_by_attach: std::collections::HashMap<u32, String>,
}

impl FunctionVisitor for FunctionShapeCollector {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        let (owner, display_name) = split_owner(&item.name);
        let qualified_name = qualify_module(&self.module, &item.name);
        let doc = item
            .doc_attach_start
            .and_then(|attach| self.jsdoc_by_attach.get(&attach).cloned());
        self.out.push(FunctionShape {
            display_name,
            qualified_name: SyntaxFact::Known(qualified_name),
            module_path: SyntaxFact::Known(self.module.clone()),
            owner: SyntaxFact::Known(owner.map(|owner| OwnerShape {
                display_name: owner,
                kind: OwnerKind::Class,
            })),
            // Export status is not extracted yet, so stay honest with
            // `Unknown` instead of hardcoding `Unexported`.
            visibility: SyntaxFact::Unknown,
            signature: SyntaxFact::Unknown,
            doc,
            // Decorators are not extracted yet; `Unknown` keeps a
            // framework-registered function from reading as unannotated.
            attributes: SyntaxFact::Unknown,
            body: BodyShape {
                tree: function_body_tree(item.body),
            },
            span: SourceSpan {
                start_line: item.start_line,
                end_line: item.end_line,
            },
            is_test: name_looks_like_test_function(&item.name),
        });
    }
}

struct CallShapeCollector<'a> {
    module: String,
    line_index: &'a LineIndex,
    imports: Vec<ImportShape>,
    namespace_aliases: HashSet<String>,
    out: Vec<CallShape>,
}

impl FunctionVisitor for CallShapeCollector<'_> {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        let (owner, _) = split_owner(&item.name);
        let caller = qualify_module(&self.module, &item.name);
        let mut visitor = FunctionBodyCallVisitor {
            module: &self.module,
            caller_qualified_name: caller,
            caller_owner: owner,
            line_index: self.line_index,
            imports: &self.imports,
            namespace_aliases: &self.namespace_aliases,
            locally_bound: local_callable_bindings(&item),
            out: Vec::new(),
        };
        visitor.visit_function_body(item.body);
        self.out.extend(visitor.out);
    }
}

/// Names bound to a callable inside `item`'s own scope: arrow functions,
/// function expressions and nested `function` declarations held in a
/// local, plus parameters whose type annotation or default value is a
/// function.
///
/// The walker gives a nested function the synthetic name
/// `<parent>::closure#N`, never the local it is assigned to, so a call to
/// that local has no workspace target. Left to the resolver's name
/// fallback, it would instead land on whichever unrelated module happens
/// to export the same name.
///
/// Bindings are collected body-wide rather than from the point of
/// declaration: a call that precedes its binding and means an outer name
/// is rare, and losing an edge beats fabricating one.
fn local_callable_bindings(item: &FunctionItem<'_>) -> HashSet<String> {
    let mut out = HashSet::new();
    for param in &item.params.items {
        if !binds_a_callable(param) {
            continue;
        }
        if let Some(id) = param.pattern.get_binding_identifier() {
            out.insert(id.name.to_string());
        }
    }
    let mut collector = LocalBindingCollector { out };
    collector.visit_function_body(item.body);
    collector.out
}

/// A parameter is callable when its annotation is a function type
/// (`emit: (e: Event) => void`) or its default value is a function
/// (`emit = () => {}`) — the two shapes JS/TS syntax can decide alone.
fn binds_a_callable(param: &FormalParameter) -> bool {
    let annotated = param
        .type_annotation
        .as_ref()
        .is_some_and(|annotation| matches!(annotation.type_annotation, TSType::TSFunctionType(_)));
    let defaulted = param
        .initializer
        .as_deref()
        .is_some_and(is_function_expression);
    annotated || defaulted
}

fn is_function_expression(expr: &Expression) -> bool {
    matches!(
        expr,
        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)
    )
}

struct LocalBindingCollector {
    out: HashSet<String>,
}

impl<'a> Visit<'a> for LocalBindingCollector {
    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        if it.init.as_ref().is_some_and(is_function_expression)
            && let Some(id) = it.id.get_binding_identifier()
        {
            self.out.insert(id.name.to_string());
        }
        // The initialiser's body is a scope of its own — nothing it
        // declares binds here.
    }

    fn visit_function(&mut self, it: &Function<'a>, _flags: ScopeFlags) {
        // A `function inner() {}` statement binds `inner` in this scope;
        // a named function *expression* binds its name only inside
        // itself, so the declaration check is not just an id check.
        if it.r#type == FunctionType::FunctionDeclaration
            && let Some(id) = &it.id
        {
            self.out.insert(id.name.to_string());
        }
    }

    fn visit_arrow_function_expression(&mut self, _it: &ArrowFunctionExpression<'a>) {}
}

struct FunctionBodyCallVisitor<'a> {
    module: &'a str,
    caller_qualified_name: String,
    caller_owner: Option<String>,
    line_index: &'a LineIndex,
    imports: &'a [ImportShape],
    namespace_aliases: &'a HashSet<String>,
    /// Callable names bound in this function's own scope — see
    /// [`local_callable_bindings`].
    locally_bound: HashSet<String>,
    out: Vec<CallShape>,
}

impl<'a> Visit<'a> for FunctionBodyCallVisitor<'_> {
    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        let shape = self.call_shape(&it.callee, self.line_index.line(it.span.start));
        self.out.push(shape);
        walk::walk_call_expression(self, it);
    }

    // A call site belongs to exactly one function: its nearest enclosing
    // one. The walker already emits each nested function as its own
    // `<parent>::closure#N` unit, so stop the descent here rather than
    // re-attributing a callback's calls to the function that registered
    // it (`el.onclick = () => help()` is a call by the closure, not by
    // the enclosing function).
    fn visit_function(&mut self, _it: &Function<'a>, _flags: ScopeFlags) {}

    fn visit_arrow_function_expression(&mut self, _it: &ArrowFunctionExpression<'a>) {}
}

impl FunctionBodyCallVisitor<'_> {
    /// The visitor already owns every caller-side fact a call shape
    /// needs, so this reads them off `self` instead of taking them as a
    /// parameter list.
    fn call_shape(&self, callee: &Expression, line: usize) -> CallShape {
        let callee = callee_facts(callee, self.namespace_aliases);
        let callee_is_locally_bound = callee_names_local_binding(
            callee.receiver,
            callee.path_segments.as_deref(),
            &self.locally_bound,
        );
        CallShape {
            caller_qualified_name: SyntaxFact::Known(Some(self.caller_qualified_name.clone())),
            caller_module: SyntaxFact::Known(self.module.to_owned()),
            caller_owner: SyntaxFact::Known(self.caller_owner.clone()),
            callee_display_name: SyntaxFact::Known(callee.name),
            callee_path_segments: callee
                .path_segments
                .map_or(SyntaxFact::Unknown, SyntaxFact::Known),
            receiver_expr_kind: SyntaxFact::Known(callee.receiver),
            callee_is_locally_bound: SyntaxFact::Known(callee_is_locally_bound),
            lexical_resolution: LexicalResolutionStatus::NotAttempted,
            visible_imports: self.imports.to_vec(),
            line,
        }
    }
}

struct CalleeFacts {
    name: Option<String>,
    path_segments: Option<Vec<String>>,
    receiver: ReceiverExprKind,
}

fn callee_facts(callee: &Expression, namespace_aliases: &HashSet<String>) -> CalleeFacts {
    match callee {
        Expression::Identifier(id) => CalleeFacts {
            name: Some(id.name.to_string()),
            path_segments: Some(vec![id.name.to_string()]),
            receiver: ReceiverExprKind::None,
        },
        Expression::StaticMemberExpression(member) => {
            let mut segments = expression_path(&member.object).unwrap_or_default();
            segments.push(member.property.name.to_string());
            let receiver = if matches!(member.object, Expression::ThisExpression(_)) {
                ReceiverExprKind::SelfValue
            } else if segments
                .first()
                .is_some_and(|first| namespace_aliases.contains(first) || starts_uppercase(first))
            {
                ReceiverExprKind::None
            } else {
                ReceiverExprKind::Expression
            };
            CalleeFacts {
                name: Some(member.property.name.to_string()),
                path_segments: (!segments.is_empty()).then_some(segments),
                receiver,
            }
        }
        Expression::ParenthesizedExpression(expr) => {
            callee_facts(&expr.expression, namespace_aliases)
        }
        _ => CalleeFacts {
            name: None,
            path_segments: None,
            receiver: ReceiverExprKind::Expression,
        },
    }
}

fn expression_path(expr: &Expression) -> Option<Vec<String>> {
    match expr {
        Expression::Identifier(id) => Some(vec![id.name.to_string()]),
        Expression::StaticMemberExpression(member) => {
            let mut segments = expression_path(&member.object)?;
            segments.push(member.property.name.to_string());
            Some(segments)
        }
        Expression::ParenthesizedExpression(expr) => expression_path(&expr.expression),
        _ => None,
    }
}

fn collect_imports(program: &Program, module: &str) -> Vec<ImportShape> {
    let mut out = Vec::new();
    for stmt in &program.body {
        let Statement::ImportDeclaration(import) = stmt else {
            continue;
        };
        let Some(target_module) = resolve_import_module(module, import.source.value.as_str())
        else {
            continue;
        };
        let Some(specifiers) = &import.specifiers else {
            continue;
        };
        for specifier in specifiers {
            match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => {
                    let imported = module_export_name(&specifier.imported);
                    let local = specifier.local.name.to_string();
                    let target = imported.as_deref().map_or_else(
                        || target_module.clone(),
                        |name| qualify_module(&target_module, name),
                    );
                    out.push(import_shape(
                        Some(local),
                        target,
                        imported.map(Some).unwrap_or(None),
                    ));
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => {
                    out.push(import_shape(
                        Some(specifier.local.name.to_string()),
                        target_module.clone(),
                        None,
                    ));
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                    out.push(import_shape(
                        Some(specifier.local.name.to_string()),
                        target_module.clone(),
                        None,
                    ));
                }
            }
        }
    }
    out
}

/// Every import in this language names its module, alias, and symbol
/// outright, so the three facts are always known.
fn import_shape(
    local_alias: Option<String>,
    imported_module: String,
    exported_symbol: Option<String>,
) -> ImportShape {
    ImportShape::known(imported_module, local_alias, exported_symbol)
}

fn module_export_name(name: &ModuleExportName) -> Option<String> {
    match name {
        ModuleExportName::IdentifierName(id) => Some(id.name.to_string()),
        ModuleExportName::IdentifierReference(id) => Some(id.name.to_string()),
        ModuleExportName::StringLiteral(s) => Some(s.value.to_string()),
    }
}

fn resolve_import_module(module: &str, source: &str) -> Option<String> {
    if !source.starts_with('.') {
        return None;
    }
    let mut segments = module
        .split("::")
        .filter(|segment| !segment.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    segments.pop();
    for raw in source.split('/') {
        match raw {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(strip_ts_extension(segment).to_owned()),
        }
    }
    (!segments.is_empty()).then(|| segments.join("::"))
}

fn strip_ts_extension(segment: &str) -> &str {
    for ext in [".tsx", ".ts", ".jsx", ".js", ".mts", ".cts", ".mjs", ".cjs"] {
        if let Some(stripped) = segment.strip_suffix(ext) {
            return stripped;
        }
    }
    segment
}

fn split_owner(name: &str) -> (Option<String>, String) {
    name.rsplit_once("::").map_or_else(
        || (None, name.to_owned()),
        |(owner, name)| (Some(owner.to_owned()), name.to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// A callee bound to a closure, a nested `function`, or a
    /// function-shaped parameter in the caller's own scope is shadowed:
    /// the resolver must be told so it does not fall back to a same-named
    /// function exported elsewhere. The walker names such a closure
    /// `pump::closure#N`, never `emit`, so nothing legitimate resolves.
    #[rstest]
    #[case::arrow_local("function pump() { const emit = (e: number) => {}; emit(1); }", true)]
    #[case::function_expression_local(
        "function pump() { const emit = function () {}; emit(1); }",
        true
    )]
    #[case::let_arrow_local("function pump() { let emit = () => {}; emit(1); }", true)]
    #[case::nested_declaration("function pump() { function emit() {} emit(1); }", true)]
    #[case::function_typed_param("function pump(emit: (e: number) => void) { emit(1); }", true)]
    #[case::defaulted_param("function pump(emit = () => {}) { emit(1); }", true)]
    #[case::binding_in_nested_block(
        "function pump(flag: boolean) { if (flag) { const emit = () => {}; emit(1); } }",
        true
    )]
    #[case::plain_local("function pump() { const emit = compute(); emit(1); }", false)]
    #[case::value_param("function pump(emit: number) { emit(1); }", false)]
    #[case::unbound_name("function pump() { emit(1); }", false)]
    fn local_callable_bindings_shadow_bare_calls(#[case] source: &str, #[case] expected: bool) {
        let call = extract_call_shapes_with_module(source, Dialect::Ts, "src::m")
            .unwrap()
            .into_iter()
            .find(|call| {
                call.callee_name() == Some("emit")
                    && call.caller_qualified_name() == Some("src::m::pump")
            })
            .expect("emit call site in pump");
        assert_eq!(call.callee_is_locally_bound(), expected);
    }

    /// A binding in one function does not shadow the same name in
    /// another, and a top-level `const emit = () => {}` is a real module
    /// node that must keep resolving.
    #[test]
    fn bindings_do_not_leak_across_functions() {
        let source = concat!(
            "const emit = (e: number) => {};\n",
            "function pump() { const emit = () => {}; emit(1); }\n",
            "function drain() { emit(2); }\n",
        );
        let flags: Vec<_> = extract_call_shapes_with_module(source, Dialect::Ts, "src::m")
            .unwrap()
            .into_iter()
            .filter(|call| call.callee_name() == Some("emit"))
            .map(|call| {
                (
                    call.caller_qualified_name().map(ToOwned::to_owned),
                    call.callee_is_locally_bound(),
                )
            })
            .collect();
        assert_eq!(
            flags,
            [
                (Some("src::m::pump".to_owned()), true),
                (Some("src::m::drain".to_owned()), false),
            ]
        );
    }

    /// A closure's own parameters bind in the closure, which the walker
    /// emits as its own unit — they must not shadow the parent's calls.
    #[test]
    fn closure_parameters_do_not_shadow_the_enclosing_scope() {
        let source = "function pump() { run((emit: () => void) => emit()); emit(); }";
        let call = extract_call_shapes_with_module(source, Dialect::Ts, "src::m")
            .unwrap()
            .into_iter()
            .find(|call| {
                call.callee_name() == Some("emit")
                    && call.caller_qualified_name() == Some("src::m::pump")
            })
            .expect("emit call site in pump");
        assert!(!call.callee_is_locally_bound());
    }

    #[test]
    fn extracts_functions_with_module_qualified_names() {
        let source = "class Service { run() { helper(); } }\nfunction helper() {}\n";

        let functions =
            extract_function_shapes_with_module(source, Dialect::Ts, "src::service").unwrap();

        assert_eq!(functions[0].display_name, "run");
        assert_eq!(
            functions[0]
                .qualified_name
                .known_value()
                .map(String::as_str),
            Some("src::service::Service::run"),
        );
        assert_eq!(functions[1].display_name, "helper");
        assert_eq!(
            functions[1]
                .qualified_name
                .known_value()
                .map(String::as_str),
            Some("src::service::helper"),
        );
    }

    #[test]
    fn extracts_bare_and_imported_call_shapes() {
        let source = "import { helper } from './helper';\nfunction caller() { helper(); }\n";

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].caller_qualified_name(), Some("src::main::caller"));
        assert_eq!(calls[0].callee_name(), Some("helper"));
        assert_eq!(
            calls[0].visible_imports[0]
                .imported_module
                .known_value()
                .map(String::as_str),
            Some("src::helper::helper"),
        );
    }

    #[test]
    fn namespace_import_member_calls_are_path_calls() {
        let source =
            "import * as graph from '../graph';\nfunction caller() { graph.createGraphView(); }\n";

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "routes::index").unwrap();

        assert_eq!(calls[0].callee_name(), Some("createGraphView"));
        assert_eq!(
            calls[0].callee_path().as_deref(),
            Some("graph::createGraphView"),
        );
        assert!(!calls[0].has_receiver_expression());
        assert_eq!(
            calls[0].visible_imports[0]
                .imported_module
                .known_value()
                .map(String::as_str),
            Some("graph"),
        );
    }

    #[test]
    fn parenthesized_bare_callees_keep_the_inner_name() {
        let source = "function caller() { (helper)(); }\nfunction helper() {}\n";

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();

        assert_eq!(calls[0].callee_name(), Some("helper"));
        assert_eq!(calls[0].callee_path().as_deref(), Some("helper"));
        assert!(!calls[0].has_receiver_expression());
    }

    #[test]
    fn parenthesized_static_member_objects_keep_the_object_path() {
        let source = "function caller() { (Api).create(); }\n";

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();

        assert_eq!(calls[0].callee_name(), Some("create"));
        assert_eq!(calls[0].callee_path().as_deref(), Some("Api::create"));
        assert!(!calls[0].has_receiver_expression());
    }

    #[test]
    fn nested_static_member_paths_are_preserved() {
        let source = "function caller() { Api.Services.create(); }\n";

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();

        assert_eq!(calls[0].callee_name(), Some("create"));
        assert_eq!(
            calls[0].callee_path().as_deref(),
            Some("Api::Services::create"),
        );
        assert!(!calls[0].has_receiver_expression());
    }

    #[test]
    fn lowercase_member_calls_remain_receiver_calls() {
        let source = "function caller(client) { client.connect(); }\n";

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();

        assert_eq!(calls[0].callee_name(), Some("connect"));
        assert_eq!(calls[0].callee_path().as_deref(), Some("client::connect"));
        assert!(calls[0].has_receiver_expression());
    }

    #[test]
    fn nested_functions_get_their_own_function_shapes() {
        let source = "function setup() { const handler = () => {}; }\n";
        let functions =
            extract_function_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();
        let qualified: Vec<&str> = functions
            .iter()
            .filter_map(|f| f.qualified_name.known_value().map(String::as_str))
            .collect();
        assert!(qualified.contains(&"src::main::setup"), "got {qualified:?}");
        assert!(
            qualified.contains(&"src::main::setup::closure#1"),
            "got {qualified:?}",
        );
    }

    #[test]
    fn calls_inside_a_nested_function_are_owned_by_the_closure() {
        // The `helper()` call is made by the callback, not by the
        // function that defines it: the call shape's caller is the
        // closure, and `setup` itself contributes no calls.
        let source = "function setup() { const run = () => helper(); }\nfunction helper() {}\n";
        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].callee_name(), Some("helper"));
        assert_eq!(
            calls[0].caller_qualified_name(),
            Some("src::main::setup::closure#1"),
        );
    }
}
