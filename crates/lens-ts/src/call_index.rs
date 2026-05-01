//! TypeScript / JavaScript function and call-shape extraction for the
//! function graph analyzer.
//!
//! This stays syntax-only: imports are resolved to lexical module paths
//! when they are relative, receiver calls are kept unresolved unless they
//! look like namespace/static calls, and no type inference is attempted.

use std::collections::HashSet;

use lens_domain::{
    BodyShape, CallShape, FunctionShape, ImportShape, LexicalResolutionStatus, OwnerKind,
    OwnerShape, ReceiverExprKind, SourceSpan, SyntaxFact, VisibilityShape,
};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_parser::Parser;

use crate::attrs::name_looks_like_test_function;
use crate::line_index::LineIndex;
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
    if !ret.errors.is_empty() {
        return Err(TsParseError::from_diagnostics(
            ret.errors.iter().map(|e| e.message.as_ref().to_owned()),
        ));
    }

    let line_index = LineIndex::new(source);
    let mut collector = FunctionShapeCollector {
        module: module.to_owned(),
        out: Vec::new(),
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
    if !ret.errors.is_empty() {
        return Err(TsParseError::from_diagnostics(
            ret.errors.iter().map(|e| e.message.as_ref().to_owned()),
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
}

impl FunctionVisitor for FunctionShapeCollector {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        let (owner, display_name) = split_owner(&item.name);
        let qualified_name = qualify(&self.module, &item.name);
        self.out.push(FunctionShape {
            display_name,
            qualified_name: SyntaxFact::Known(qualified_name),
            module_path: SyntaxFact::Known(self.module.clone()),
            owner: SyntaxFact::Known(owner.map(|owner| OwnerShape {
                display_name: owner,
                kind: OwnerKind::Class,
            })),
            visibility: SyntaxFact::Known(VisibilityShape::Unexported),
            signature: SyntaxFact::Unknown,
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
        let caller = qualify(&self.module, &item.name);
        let mut visitor = FunctionBodyCallVisitor {
            module: &self.module,
            caller_qualified_name: caller,
            caller_owner: owner,
            line_index: self.line_index,
            imports: &self.imports,
            namespace_aliases: &self.namespace_aliases,
            out: Vec::new(),
        };
        visitor.visit_function_body(item.body);
        self.out.extend(visitor.out);
    }
}

struct FunctionBodyCallVisitor<'a> {
    module: &'a str,
    caller_qualified_name: String,
    caller_owner: Option<String>,
    line_index: &'a LineIndex,
    imports: &'a [ImportShape],
    namespace_aliases: &'a HashSet<String>,
    out: Vec<CallShape>,
}

impl<'a> Visit<'a> for FunctionBodyCallVisitor<'_> {
    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        self.out.push(call_shape(
            &it.callee,
            self.line_index.line(it.span.start),
            self.module,
            &self.caller_qualified_name,
            self.caller_owner.clone(),
            self.imports,
            self.namespace_aliases,
        ));
        walk::walk_call_expression(self, it);
    }
}

fn call_shape(
    callee: &Expression,
    line: usize,
    module: &str,
    caller_qualified_name: &str,
    caller_owner: Option<String>,
    imports: &[ImportShape],
    namespace_aliases: &HashSet<String>,
) -> CallShape {
    let callee = callee_facts(callee, namespace_aliases);
    CallShape {
        caller_qualified_name: SyntaxFact::Known(Some(caller_qualified_name.to_owned())),
        caller_module: SyntaxFact::Known(module.to_owned()),
        caller_owner: SyntaxFact::Known(caller_owner),
        callee_display_name: SyntaxFact::Known(callee.name),
        callee_path_segments: callee
            .path_segments
            .map_or(SyntaxFact::Unknown, SyntaxFact::Known),
        receiver_expr_kind: SyntaxFact::Known(callee.receiver),
        lexical_resolution: LexicalResolutionStatus::NotAttempted,
        visible_imports: imports.to_vec(),
        line,
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
                        |name| qualify(&target_module, name),
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

fn import_shape(
    local_alias: Option<String>,
    imported_module: String,
    exported_symbol: Option<String>,
) -> ImportShape {
    ImportShape {
        imported_module: SyntaxFact::Known(imported_module),
        local_alias: SyntaxFact::Known(local_alias),
        exported_symbol: SyntaxFact::Known(exported_symbol),
    }
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

fn qualify(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{module}::{name}")
    }
}

fn split_owner(name: &str) -> (Option<String>, String) {
    name.rsplit_once("::").map_or_else(
        || (None, name.to_owned()),
        |(owner, name)| (Some(owner.to_owned()), name.to_owned()),
    )
}

fn starts_uppercase(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
