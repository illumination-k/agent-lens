//! Python function and call-shape extraction for the function-graph
//! analyzer.
//!
//! This stays syntax-only. Free functions and class methods become
//! [`FunctionShape`]s qualified at the lexical module path the caller
//! supplies. Each call expression inside a function body becomes a
//! [`CallShape`] tagged with the imports visible at its file. No type
//! inference is attempted: `self.method(...)` and `obj.method(...)` are
//! left as receiver calls so the resolver keeps them unresolved.
//!
//! The treatment of imports mirrors `lens-ts`: aliases imported as a whole
//! module (`import os`, `import os as o`) participate as namespace aliases
//! so `os.path()` reads as a path call, while `from pkg import name`
//! aliases are treated as value imports.

use std::collections::HashSet;

use lens_domain::{
    BodyShape, CallShape, FunctionShape, ImportShape, LexicalResolutionStatus, OwnerKind,
    OwnerShape, ReceiverExprKind, SourceSpan, SyntaxFact, VisibilityShape,
};
use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{Expr, ExprAttribute, ExprCall, ExprName, Stmt, StmtFunctionDef, StmtImport};
use ruff_python_parser::parse_module;

use crate::attrs::{inherits_protocol, is_stub_function, is_test_class, is_test_function};
use crate::line_index::LineIndex;
use crate::parser::{PythonParseError, function_body_tree};

/// Extract neutral function-shape facts for Python.
///
/// `module` is the lexical module path the file lives at, in `::`-separated
/// form (e.g. `pkg::sub::main`). It is used to qualify each function name
/// so cross-file resolution in the function-graph analyzer can match them.
pub fn extract_function_shapes_with_module(
    source: &str,
    module: &str,
) -> Result<Vec<FunctionShape>, PythonParseError> {
    let parsed = parse_module(source)?.into_syntax();
    let lines = LineIndex::new(source);
    let mut out = Vec::new();
    for stmt in &parsed.body {
        collect_function_shapes(stmt, None, false, module, &lines, &mut out);
    }
    Ok(out)
}

/// Extract neutral call-shape facts for Python.
///
/// Calls outside any `def` (top-level statements, class-body expressions)
/// are skipped: the function-graph analyzer only attributes calls to
/// callers it can name.
pub fn extract_call_shapes_with_module(
    source: &str,
    module: &str,
) -> Result<Vec<CallShape>, PythonParseError> {
    let parsed = parse_module(source)?.into_syntax();
    let lines = LineIndex::new(source);
    let imports = collect_imports(&parsed.body, module);
    let namespace_aliases: HashSet<String> = imports
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
    let mut out = Vec::new();
    for stmt in &parsed.body {
        collect_call_shapes(
            stmt,
            None,
            module,
            &imports,
            &namespace_aliases,
            &lines,
            &mut out,
        );
    }
    Ok(out)
}

fn collect_function_shapes(
    stmt: &Stmt,
    owner: Option<&str>,
    owner_is_test: bool,
    module: &str,
    lines: &LineIndex,
    out: &mut Vec<FunctionShape>,
) {
    match stmt {
        Stmt::FunctionDef(func) => {
            // Drop stub-shaped functions exactly like the similarity / cohesion
            // / complexity extractors do — a `pass`-bodied or `@overload`
            // function carries no analysable graph signal.
            if is_stub_function(func) {
                return;
            }
            let display_name = func.name.as_str().to_owned();
            let qualified = match owner {
                Some(class) => qualify(module, &format!("{class}::{display_name}")),
                None => qualify(module, &display_name),
            };
            let owner_shape = owner.map(|class_name| OwnerShape {
                display_name: class_name.to_owned(),
                kind: OwnerKind::Class,
            });
            let is_test = owner_is_test || is_test_function(func);
            let span = function_span(func, lines);
            out.push(FunctionShape {
                display_name,
                qualified_name: SyntaxFact::Known(qualified),
                module_path: SyntaxFact::Known(module.to_owned()),
                owner: SyntaxFact::Known(owner_shape),
                visibility: SyntaxFact::Known(VisibilityShape::Unexported),
                signature: SyntaxFact::Unknown,
                body: BodyShape {
                    tree: function_body_tree(func),
                },
                span,
                is_test,
            });
        }
        Stmt::ClassDef(class) => {
            // Protocol classes are pure declarations; every method body is a
            // `...` stub. Drop the whole subtree, matching the similarity
            // / cohesion / complexity passes.
            if inherits_protocol(class) {
                return;
            }
            let class_is_test = owner_is_test || is_test_class(class);
            let class_name = class.name.as_str();
            for inner in &class.body {
                collect_function_shapes(inner, Some(class_name), class_is_test, module, lines, out);
            }
        }
        _ => {}
    }
}

fn collect_call_shapes(
    stmt: &Stmt,
    owner: Option<&str>,
    module: &str,
    imports: &[ImportShape],
    namespace_aliases: &HashSet<String>,
    lines: &LineIndex,
    out: &mut Vec<CallShape>,
) {
    match stmt {
        Stmt::FunctionDef(func) => {
            if is_stub_function(func) {
                return;
            }
            let display_name = func.name.as_str();
            let caller_qualified = match owner {
                Some(class) => qualify(module, &format!("{class}::{display_name}")),
                None => qualify(module, display_name),
            };
            let mut visitor = FunctionBodyCallVisitor {
                module,
                caller_qualified_name: caller_qualified,
                caller_owner: owner.map(ToOwned::to_owned),
                line_index: lines,
                imports,
                namespace_aliases,
                out: Vec::new(),
            };
            for body_stmt in &func.body {
                visitor.visit_stmt(body_stmt);
            }
            out.extend(visitor.out);
        }
        Stmt::ClassDef(class) => {
            if inherits_protocol(class) {
                return;
            }
            let class_name = class.name.as_str();
            for inner in &class.body {
                collect_call_shapes(
                    inner,
                    Some(class_name),
                    module,
                    imports,
                    namespace_aliases,
                    lines,
                    out,
                );
            }
        }
        _ => {}
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

impl<'a, 'ast> Visitor<'ast> for FunctionBodyCallVisitor<'a> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            self.out.push(call_shape(
                call,
                self.line_index,
                self.module,
                &self.caller_qualified_name,
                self.caller_owner.clone(),
                self.imports,
                self.namespace_aliases,
            ));
        }
        walk_expr(self, expr);
    }
}

fn call_shape(
    call: &ExprCall,
    line_index: &LineIndex,
    module: &str,
    caller_qualified_name: &str,
    caller_owner: Option<String>,
    imports: &[ImportShape],
    namespace_aliases: &HashSet<String>,
) -> CallShape {
    let facts = callee_facts(&call.func, namespace_aliases);
    let line = line_index.line_of(call.range.start().to_usize());
    CallShape {
        caller_qualified_name: SyntaxFact::Known(Some(caller_qualified_name.to_owned())),
        caller_module: SyntaxFact::Known(module.to_owned()),
        caller_owner: SyntaxFact::Known(caller_owner),
        callee_display_name: SyntaxFact::Known(facts.name),
        callee_path_segments: facts
            .path_segments
            .map_or(SyntaxFact::Unknown, SyntaxFact::Known),
        receiver_expr_kind: SyntaxFact::Known(facts.receiver),
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

fn callee_facts(callee: &Expr, namespace_aliases: &HashSet<String>) -> CalleeFacts {
    match callee {
        Expr::Name(ExprName { id, .. }) => CalleeFacts {
            name: Some(id.to_string()),
            path_segments: Some(vec![id.to_string()]),
            receiver: ReceiverExprKind::None,
        },
        Expr::Attribute(ExprAttribute { value, attr, .. }) => {
            let mut segments = expression_path(value).unwrap_or_default();
            segments.push(attr.as_str().to_owned());
            let receiver = if matches!(
                value.as_ref(),
                Expr::Name(name) if name.id.as_str() == "self"
            ) {
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
                name: Some(attr.as_str().to_owned()),
                path_segments: (!segments.is_empty()).then_some(segments),
                receiver,
            }
        }
        _ => CalleeFacts {
            name: None,
            path_segments: None,
            receiver: ReceiverExprKind::Expression,
        },
    }
}

fn expression_path(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::Name(name) => Some(vec![name.id.to_string()]),
        Expr::Attribute(attr) => {
            let mut segments = expression_path(&attr.value)?;
            segments.push(attr.attr.as_str().to_owned());
            Some(segments)
        }
        _ => None,
    }
}

fn collect_imports(body: &[Stmt], module: &str) -> Vec<ImportShape> {
    let mut out = Vec::new();
    for stmt in body {
        match stmt {
            Stmt::Import(StmtImport { names, .. }) => {
                for alias in names {
                    let imported = alias.name.as_str();
                    let local = alias
                        .asname
                        .as_ref()
                        .map(|n| n.as_str().to_owned())
                        .unwrap_or_else(|| top_segment(imported).to_owned());
                    out.push(import_shape(
                        Some(local),
                        dotted_to_module_path(imported),
                        None,
                    ));
                }
            }
            Stmt::ImportFrom(from) => {
                let Some(base) =
                    resolve_from_base(module, from.level, from.module.as_ref().map(|m| m.as_str()))
                else {
                    continue;
                };
                for alias in &from.names {
                    let imported = alias.name.as_str();
                    if imported == "*" {
                        continue;
                    }
                    let local = alias
                        .asname
                        .as_ref()
                        .map(|n| n.as_str().to_owned())
                        .unwrap_or_else(|| imported.to_owned());
                    let target = if base.is_empty() {
                        imported.to_owned()
                    } else {
                        format!("{base}::{imported}")
                    };
                    out.push(import_shape(Some(local), target, Some(imported.to_owned())));
                }
            }
            _ => {}
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

/// Resolve the lexical base module of a `from X import ...` statement.
///
/// Returns `None` when a relative import outruns the available depth
/// (e.g. `from ... import x` in a top-level file).
fn resolve_from_base(current: &str, level: u32, module: Option<&str>) -> Option<String> {
    let mut segments: Vec<String> = if level == 0 || current.is_empty() {
        Vec::new()
    } else {
        current.split("::").map(ToOwned::to_owned).collect()
    };
    if level != 0 {
        let pops = level as usize;
        if pops > segments.len() {
            return None;
        }
        segments.truncate(segments.len() - pops);
    }
    if let Some(module) = module
        && !module.is_empty()
    {
        segments.extend(module.split('.').map(ToOwned::to_owned));
    }
    Some(segments.join("::"))
}

fn dotted_to_module_path(dotted: &str) -> String {
    dotted
        .split('.')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("::")
}

fn top_segment(dotted: &str) -> &str {
    dotted.split('.').next().unwrap_or(dotted)
}

fn qualify(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{module}::{name}")
    }
}

fn starts_uppercase(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
}

fn function_span(func: &StmtFunctionDef, lines: &LineIndex) -> SourceSpan {
    let start_line = lines.line_of(func.range.start().to_usize());
    // `range.end()` lands at the position just past the last byte of the
    // body; the line that byte sits on is the closing line.
    let end_offset = func.range.end().to_usize().saturating_sub(1);
    let end_line = lines.line_of(end_offset);
    SourceSpan {
        start_line,
        end_line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shapes(src: &str, module: &str) -> Vec<FunctionShape> {
        extract_function_shapes_with_module(src, module).unwrap()
    }

    fn calls(src: &str, module: &str) -> Vec<CallShape> {
        extract_call_shapes_with_module(src, module).unwrap()
    }

    #[test]
    fn extracts_module_qualified_names_for_free_and_class_methods() {
        let src = "
def helper():
    return 1

class Service:
    def run(self):
        return helper()
";
        let funcs = shapes(src, "pkg::main");
        let names: Vec<_> = funcs
            .iter()
            .map(|f| f.qualified_name.known_value().unwrap().as_str())
            .collect();
        assert_eq!(names, ["pkg::main::helper", "pkg::main::Service::run"]);

        let owner = funcs[1]
            .owner
            .known_value()
            .unwrap()
            .as_ref()
            .map(|o| (o.display_name.clone(), o.kind));
        assert_eq!(owner, Some(("Service".to_owned(), OwnerKind::Class)));
    }

    #[test]
    fn empty_module_qualifies_with_bare_name() {
        let funcs = shapes("def f():\n    return 1\n", "");
        assert_eq!(
            funcs[0].qualified_name.known_value().map(String::as_str),
            Some("f"),
        );
    }

    #[test]
    fn drops_stub_and_protocol_subtrees() {
        let src = "
from typing import Protocol

class P(Protocol):
    def f(self): ...

def stub(): ...

def real():
    return 1
";
        let funcs = shapes(src, "m");
        let names: Vec<_> = funcs.iter().map(|f| f.display_name.clone()).collect();
        assert_eq!(names, ["real"]);
    }

    #[test]
    fn bare_call_shape_records_caller_module_and_imports() {
        let src = "
from helper import helper

def caller():
    helper()
";
        let calls = calls(src, "main");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].caller_qualified_name(), Some("main::caller"));
        assert_eq!(calls[0].callee_name(), Some("helper"));
        assert_eq!(
            calls[0].visible_imports[0]
                .imported_module
                .known_value()
                .map(String::as_str),
            Some("helper::helper"),
        );
    }

    #[test]
    fn namespace_import_member_calls_are_path_calls() {
        let src = "
import graph

def caller():
    graph.create_view()
";
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("create_view"));
        assert_eq!(call.callee_path().as_deref(), Some("graph::create_view"));
        assert!(!call.has_receiver_expression());
    }

    #[test]
    fn aliased_namespace_import_keeps_alias_segment() {
        let src = "
import graph as g

def caller():
    g.create_view()
";
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_path().as_deref(), Some("g::create_view"));
        assert!(!call.has_receiver_expression());
        let import = &call.visible_imports[0];
        assert_eq!(
            import.local_alias.known_value().and_then(Option::as_ref),
            Some(&"g".to_owned()),
        );
        assert_eq!(
            import.imported_module.known_value().map(String::as_str),
            Some("graph"),
        );
    }

    #[test]
    fn self_method_calls_remain_unresolved_self_value() {
        let src = "
class Service:
    def helper(self):
        return 1
    def caller(self):
        return self.helper()
";
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("helper"));
        assert!(call.has_receiver_expression());
    }

    #[test]
    fn class_static_calls_are_path_calls() {
        let src = "
class Helper:
    @staticmethod
    def run():
        return 1

def caller():
    Helper.run()
";
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("run"));
        assert_eq!(call.callee_path().as_deref(), Some("Helper::run"));
        assert!(!call.has_receiver_expression());
    }

    #[test]
    fn lowercase_member_calls_remain_receiver_calls() {
        let src = "
def caller(client):
    client.connect()
";
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("connect"));
        assert_eq!(call.callee_path().as_deref(), Some("client::connect"));
        assert!(call.has_receiver_expression());
    }

    #[test]
    fn from_import_targets_are_qualified_to_module_segment() {
        let src = "
from pkg.sub import name

def caller():
    name()
";
        let call = &calls(src, "main")[0];
        let import = &call.visible_imports[0];
        assert_eq!(
            import.imported_module.known_value().map(String::as_str),
            Some("pkg::sub::name"),
        );
        assert_eq!(
            import
                .exported_symbol
                .known_value()
                .and_then(Option::as_ref)
                .map(String::as_str),
            Some("name"),
        );
    }

    #[test]
    fn relative_from_import_climbs_module_segments() {
        let src = "
from .. import util

def caller():
    util.run()
";
        let call = &calls(src, "pkg::sub::main")[0];
        let import = &call.visible_imports[0];
        assert_eq!(
            import.imported_module.known_value().map(String::as_str),
            Some("pkg::util"),
        );
    }

    #[test]
    fn relative_import_outrunning_depth_is_dropped() {
        let src = "
from ... import util

def caller():
    util.run()
";
        let call = &calls(src, "main")[0];
        assert!(call.visible_imports.is_empty());
    }

    #[test]
    fn star_imports_are_skipped() {
        let src = "
from helpers import *

def caller():
    helper()
";
        let call = &calls(src, "main")[0];
        assert!(call.visible_imports.is_empty());
    }

    #[test]
    fn anonymous_callee_expressions_are_recorded_without_a_name() {
        let src = "
def caller():
    (lambda: 1)()
";
        let call = &calls(src, "main")[0];
        assert!(call.callee_name().is_none());
        assert!(call.callee_path().is_none());
    }

    #[test]
    fn nested_attribute_call_preserves_full_path_segments() {
        // Catches a regression in `expression_path` where dropping the
        // recursive `Attribute` arm would shorten `a.b.c()` to just `c`.
        let src = "
def caller():
    a.b.c()
";
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("c"));
        assert_eq!(call.callee_path().as_deref(), Some("a::b::c"));
    }

    #[test]
    fn is_test_propagates_from_test_class_to_inner_methods() {
        // `helper` is not test-named on its own, but the enclosing
        // `TestThing` class is — the `||` between owner and self must
        // surface that, otherwise mutation testing flags the propagation
        // as a no-op.
        let src = "
class TestThing:
    def helper(self):
        assert True
";
        let funcs = shapes(src, "pkg::main");
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].display_name, "helper");
        assert!(
            funcs[0].is_test,
            "method on a Test* class must inherit is_test=true",
        );
    }

    #[test]
    fn single_dot_import_at_module_root_yields_bare_target() {
        // `from . import util` at a top-level file — `pops == segments.len()`
        // must still resolve (`>` not `>=`), producing a bare `util` target.
        let src = "
from . import util

def caller():
    util.run()
";
        let call = &calls(src, "main")[0];
        let import = &call.visible_imports[0];
        assert_eq!(
            import.imported_module.known_value().map(String::as_str),
            Some("util"),
        );
    }

    #[test]
    fn single_dot_import_in_nested_module_keeps_parent_path() {
        // `from . import util` from `pkg::sub::main` resolves to
        // `pkg::sub::util` (drop one segment, then append the import).
        // Differentiates `len - pops` from `len / pops`, which would
        // collapse to `pkg::sub::main::util` here.
        let src = "
from . import util

def caller():
    util.run()
";
        let call = &calls(src, "pkg::sub::main")[0];
        let import = &call.visible_imports[0];
        assert_eq!(
            import.imported_module.known_value().map(String::as_str),
            Some("pkg::sub::util"),
        );
    }
}
