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
    BodyShape, CallShape, FunctionShape, ImportShape, LexicalResolutionStatus, LineIndex,
    OwnerKind, OwnerShape, ReceiverExprKind, SourceSpan, SyntaxFact, callee_names_local_binding,
    qualify_module, starts_uppercase,
};
use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{Expr, ExprAttribute, ExprCall, ExprName, Stmt, StmtFunctionDef, StmtImport};
use ruff_python_parser::parse_module;

use crate::parser::{PythonParseError, function_body_tree};
use crate::walk::{FnSite, walk_module_fns};

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
    walk_module_fns(&parsed.body, &mut |site| {
        out.push(function_shape(&site, module, &lines));
    });
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
    walk_module_fns(&parsed.body, &mut |site| {
        collect_calls_in_function(
            &site,
            module,
            &imports,
            &namespace_aliases,
            &lines,
            &mut out,
        );
    });
    Ok(out)
}

fn function_shape(site: &FnSite<'_>, module: &str, lines: &LineIndex) -> FunctionShape {
    let func = site.func;
    let display_name = func.name.as_str().to_owned();
    let qualified = match site.owner {
        Some(class) => qualify_module(module, &format!("{class}::{display_name}")),
        None => qualify_module(module, &display_name),
    };
    let owner_shape = site.owner.map(|class_name| OwnerShape {
        display_name: class_name.to_owned(),
        kind: OwnerKind::Class,
    });
    FunctionShape {
        display_name,
        qualified_name: SyntaxFact::Known(qualified),
        module_path: SyntaxFact::Known(module.to_owned()),
        owner: SyntaxFact::Known(owner_shape),
        // Export status is not extracted yet, so stay honest
        // with `Unknown` instead of hardcoding `Unexported`.
        visibility: SyntaxFact::Unknown,
        signature: SyntaxFact::Unknown,
        doc: crate::parser::docstring_text(func),
        // Decorators are not extracted yet; `Unknown` keeps a
        // framework-registered function from reading as unannotated.
        attributes: SyntaxFact::Unknown,
        body: BodyShape {
            tree: function_body_tree(func),
        },
        span: function_span(func, lines),
        is_test: site.is_test,
    }
}

fn collect_calls_in_function(
    site: &FnSite<'_>,
    module: &str,
    imports: &[ImportShape],
    namespace_aliases: &HashSet<String>,
    lines: &LineIndex,
    out: &mut Vec<CallShape>,
) {
    let display_name = site.func.name.as_str();
    let caller_qualified = match site.owner {
        Some(class) => qualify_module(module, &format!("{class}::{display_name}")),
        None => qualify_module(module, display_name),
    };
    let locally_bound = local_callable_bindings(site.func);
    let mut visitor = FunctionBodyCallVisitor {
        module,
        caller_qualified_name: caller_qualified,
        caller_owner: site.owner.map(ToOwned::to_owned),
        line_index: lines,
        imports,
        namespace_aliases,
        locally_bound,
        out: Vec::new(),
    };
    for body_stmt in &site.func.body {
        visitor.visit_stmt(body_stmt);
    }
    out.extend(visitor.out);
}

/// Names bound to a callable inside `func`'s own scope: nested `def`s and
/// `lambda`s held in a local, plus parameters annotated as `Callable`.
///
/// The walker keeps a function body atomic, so a nested `def` is never a
/// graph node of its own and a call to it has no workspace target. Left
/// to the resolver's name fallback, that call would instead land on
/// whichever unrelated module happens to define the same name.
///
/// Bindings are collected body-wide rather than from the point of
/// definition: a call that precedes its binding and means the outer name
/// is rare, and losing an edge beats fabricating one.
fn local_callable_bindings(func: &StmtFunctionDef) -> HashSet<String> {
    let mut names = HashSet::new();
    for param in func.parameters.iter() {
        if param.annotation().is_some_and(annotation_is_callable) {
            names.insert(param.name().as_str().to_owned());
        }
    }
    let mut collector = LocalBindingCollector { out: &mut names };
    for stmt in &func.body {
        collector.visit_stmt(stmt);
    }
    names
}

/// `Callable`, `typing.Callable[[int], None]`, `collections.abc.Callable`.
fn annotation_is_callable(annotation: &Expr) -> bool {
    match annotation {
        Expr::Name(ExprName { id, .. }) => id.as_str() == "Callable",
        Expr::Attribute(ExprAttribute { attr, .. }) => attr.as_str() == "Callable",
        Expr::Subscript(subscript) => annotation_is_callable(&subscript.value),
        // `"Callable[[int], None]"` as a string annotation.
        Expr::StringLiteral(literal) => literal.value.to_str().starts_with("Callable"),
        _ => false,
    }
}

struct LocalBindingCollector<'a> {
    out: &'a mut HashSet<String>,
}

impl<'ast> Visitor<'ast> for LocalBindingCollector<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(nested) => {
                self.out.insert(nested.name.as_str().to_owned());
                // A nested `def`'s own body binds names in its scope, not
                // in this one, so stop here.
                return;
            }
            Stmt::Assign(assign) => {
                if matches!(assign.value.as_ref(), Expr::Lambda(_)) {
                    for target in &assign.targets {
                        if let Expr::Name(name) = target {
                            self.out.insert(name.id.to_string());
                        }
                    }
                }
            }
            Stmt::AnnAssign(assign) => {
                let binds_callable = assign
                    .value
                    .as_ref()
                    .is_some_and(|value| matches!(value.as_ref(), Expr::Lambda(_)))
                    || annotation_is_callable(&assign.annotation);
                if binds_callable && let Expr::Name(name) = assign.target.as_ref() {
                    self.out.insert(name.id.to_string());
                }
            }
            _ => {}
        }
        ruff_python_ast::visitor::walk_stmt(self, stmt);
    }

    // Lambda bodies are expressions in this scope's statements, but their
    // parameters are not; nothing in an expression binds a name here.
    fn visit_expr(&mut self, _expr: &'ast Expr) {}
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

impl<'a, 'ast> Visitor<'ast> for FunctionBodyCallVisitor<'a> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            let shape = self.call_shape(call);
            self.out.push(shape);
        }
        walk_expr(self, expr);
    }
}

impl FunctionBodyCallVisitor<'_> {
    /// The visitor already owns every caller-side fact a call shape
    /// needs, so this reads them off `self` instead of taking them as a
    /// parameter list.
    fn call_shape(&self, call: &ExprCall) -> CallShape {
        let facts = callee_facts(&call.func, self.namespace_aliases);
        let line = self.line_index.line(call.range.start().to_u32());
        let callee_is_locally_bound = callee_names_local_binding(
            facts.receiver,
            facts.path_segments.as_deref(),
            &self.locally_bound,
        );
        CallShape {
            caller_qualified_name: SyntaxFact::Known(Some(self.caller_qualified_name.clone())),
            caller_module: SyntaxFact::Known(self.module.to_owned()),
            caller_owner: SyntaxFact::Known(self.caller_owner.clone()),
            callee_display_name: SyntaxFact::Known(facts.name),
            callee_path_segments: facts
                .path_segments
                .map_or(SyntaxFact::Unknown, SyntaxFact::Known),
            receiver_expr_kind: SyntaxFact::Known(facts.receiver),
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

fn function_span(func: &StmtFunctionDef, lines: &LineIndex) -> SourceSpan {
    let start_line = lines.line(func.range.start().to_u32());
    // `range.end()` lands at the position just past the last byte of the
    // body; the line that byte sits on is the closing line.
    let end_offset = func.range.end().to_u32().saturating_sub(1);
    let end_line = lines.line(end_offset);
    SourceSpan {
        start_line,
        end_line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn shapes(src: &str, module: &str) -> Vec<FunctionShape> {
        extract_function_shapes_with_module(src, module).unwrap()
    }

    fn calls(src: &str, module: &str) -> Vec<CallShape> {
        extract_call_shapes_with_module(src, module).unwrap()
    }

    /// A callee bound to a nested `def`, a `lambda`, or a `Callable`
    /// parameter in the caller's own scope is shadowed: the resolver must
    /// be told so it does not fall back to a same-named module function.
    #[rstest]
    #[case::nested_def("def caller():\n    def emit(x):\n        pass\n    emit(1)\n", true)]
    #[case::lambda_local("def caller():\n    emit = lambda x: x\n    emit(1)\n", true)]
    #[case::annotated_lambda(
        "def caller():\n    emit: Callable[[int], None] = lambda x: x\n    emit(1)\n",
        true
    )]
    #[case::callable_param("def caller(emit: Callable[[int], None]):\n    emit(1)\n", true)]
    #[case::qualified_callable_param("def caller(emit: typing.Callable):\n    emit(1)\n", true)]
    #[case::string_annotated_param(
        "def caller(emit: \"Callable[[int], None]\"):\n    emit(1)\n",
        true
    )]
    #[case::annotated_only("def caller():\n    emit: Callable[[int], None]\n    emit(1)\n", true)]
    #[case::binding_in_nested_block(
        "def caller(flag):\n    if flag:\n        emit = lambda x: x\n    emit(1)\n",
        true
    )]
    #[case::plain_local("def caller():\n    emit = compute()\n    emit(1)\n", false)]
    #[case::value_param("def caller(emit: int):\n    emit(1)\n", false)]
    #[case::unbound_name("def caller():\n    emit(1)\n", false)]
    fn local_callable_bindings_shadow_bare_calls(#[case] src: &str, #[case] expected: bool) {
        let call = calls(src, "m")
            .into_iter()
            .find(|call| call.callee_name() == Some("emit"))
            .expect("emit call site");
        assert_eq!(call.callee_is_locally_bound(), expected);
    }

    /// A `lambda`'s own parameters bind in the lambda's scope, not the
    /// enclosing function's, so they must not shadow calls around it.
    #[test]
    fn lambda_parameters_do_not_shadow_the_enclosing_scope() {
        let src = "def caller():\n    run(lambda emit: emit(1))\n    emit(2)\n";
        let outer = calls(src, "m")
            .into_iter()
            .filter(|call| call.callee_name() == Some("emit"))
            .collect::<Vec<_>>();
        assert!(
            outer.iter().all(|call| !call.callee_is_locally_bound()),
            "got {outer:?}",
        );
    }

    /// Only the shadowed name is affected.
    #[test]
    fn other_calls_in_a_shadowing_function_are_untouched() {
        let src = "def caller():\n    emit = lambda x: x\n    emit(1)\n    helper()\n";
        let flags: Vec<_> = calls(src, "m")
            .iter()
            .map(|call| {
                (
                    call.callee_name().map(ToOwned::to_owned),
                    call.callee_is_locally_bound(),
                )
            })
            .collect();
        assert_eq!(
            flags,
            [
                (Some("emit".to_owned()), true),
                (Some("helper".to_owned()), false),
            ]
        );
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
