//! ruff-based cohesion extraction for Python classes and modules.
//!
//! For every `class` body we collect each instance method's referenced
//! `self.<field>` accesses and its `self.<sibling>(...)` calls. Static
//! methods (decorated with `@staticmethod`), class methods (decorated
//! with `@classmethod`), and the `__init__` constructor are ignored:
//!
//! * `@staticmethod` cannot reach `self` and has no field cohesion;
//! * `@classmethod` only sees the type, not the instance, so it does not
//!   participate in instance-field cohesion either;
//! * `__init__`'s job is to initialise fields, so it would over-link
//!   every method through every field it assigns.
//!
//! In addition to class-level units we emit a **module-level** unit per
//! file: top-level `def`s play the role of methods, top-level variable
//! assignments play the role of fields, and direct sibling calls form
//! edges. Locally-bound names inside a function (parameters, local
//! assignments) shadow module names, so a `counter = 0` inside a function
//! body is not counted as a reference to the module-level `counter`.
//! `global x` re-promotes a name back to the module scope.
//!
//! Calls are matched verbatim against the names of *sibling* methods on
//! the same class (or sibling functions on the same module). Calls to
//! anything else are dropped — the cohesion graph only ever sees
//! in-unit edges, mirroring `lens-rust` and `lens-ts`.
//!
//! Test classes (`Test*` names or `unittest.TestCase` subclasses) are
//! filtered out: their cohesion is dictated by the test framework's
//! lifecycle hooks, not by a real production responsibility split. Test
//! functions (`test_*` / `test`, pytest fixture / mark decorators,
//! `@unittest.skip*`) are filtered from the module unit for the same
//! reason.

use std::collections::HashSet;

use lens_domain::{CohesionUnit, CohesionUnitKind, MethodCohesion};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{
    Decorator, ExceptHandler, Expr, Stmt, StmtAnnAssign, StmtAssign, StmtAugAssign, StmtClassDef,
    StmtFor, StmtFunctionDef, StmtIf, StmtImport, StmtImportFrom, StmtMatch, StmtReturn, StmtTry,
    StmtWhile, StmtWith,
};
use ruff_python_parser::{ParseError, parse_module};
use ruff_text_size::Ranged;

use crate::attrs::{inherits_protocol, is_stub_function, is_test_class, is_test_function};
use crate::line_index::LineIndex;

/// Failures produced while extracting cohesion units.
#[derive(Debug, thiserror::Error)]
pub enum CohesionError {
    #[error("failed to parse Python source: {0}")]
    Parse(#[from] ParseError),
}

/// Placeholder name used for the module-level cohesion unit. The CLI
/// report already prints the file path, so a constant placeholder keeps
/// the unit identifiable without requiring the path to be threaded down.
const MODULE_UNIT_NAME: &str = "<module>";

/// Extract one [`CohesionUnit`] per class in `source` that has at least
/// one instance method, plus one module-level unit when the file has at
/// least one top-level production function.
pub fn extract_cohesion_units(source: &str) -> Result<Vec<CohesionUnit>, CohesionError> {
    let module = parse_module(source)?.into_syntax();
    let lines = LineIndex::new(source);
    let mut out = Vec::new();
    for stmt in &module.body {
        if let Stmt::ClassDef(class) = stmt
            && let Some(unit) = unit_from_class(class, &lines)
        {
            out.push(unit);
        }
    }
    if let Some(unit) = unit_from_module(&module.body, &lines) {
        out.push(unit);
    }
    Ok(out)
}

fn unit_from_class(class: &StmtClassDef, lines: &LineIndex) -> Option<CohesionUnit> {
    // Protocol classes describe a structural contract; their cohesion
    // is meaningless and every method body is a `...` stub. Drop the
    // unit the same way test classes are dropped below.
    if inherits_protocol(class) {
        return None;
    }
    if is_test_class(class) {
        return None;
    }
    let class_name = class.name.as_str();

    let methods: Vec<&StmtFunctionDef> = class
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(f) if is_instance_method(f) && !is_stub_function(f) => Some(f),
            _ => None,
        })
        .collect();
    if methods.is_empty() {
        return None;
    }

    let sibling_names: HashSet<String> =
        methods.iter().map(|m| m.name.as_str().to_owned()).collect();

    let cohesions: Vec<MethodCohesion> = methods
        .iter()
        .map(|m| method_cohesion(m, &sibling_names, lines))
        .collect();

    let start_line = lines.line_of(class.range.start().to_usize());
    let end_offset = class.range.end().to_usize().saturating_sub(1);
    let end_line = lines.line_of(end_offset);
    Some(CohesionUnit::build(
        CohesionUnitKind::Inherent,
        class_name,
        start_line,
        end_line,
        cohesions,
    ))
}

/// `__init__` initialises every field it touches, so including it would
/// link every method through every field it assigned. `@staticmethod`
/// and `@classmethod` cannot reach instance state, so they have no
/// cohesion to measure. Anything else (including dunder methods like
/// `__repr__` and properties) participates normally.
fn is_instance_method(func: &StmtFunctionDef) -> bool {
    if func.name.as_str() == "__init__" {
        return false;
    }
    !decorator_list_marks_non_instance(&func.decorator_list)
}

fn decorator_list_marks_non_instance(decorators: &[Decorator]) -> bool {
    decorators.iter().any(|d| {
        matches!(
            decorator_name(&d.expression),
            Some("staticmethod" | "classmethod")
        )
    })
}

/// Last segment of a decorator path. We deliberately accept either bare
/// `staticmethod` (the builtin) or fully-qualified shapes — the walker
/// only cares about the leaf name. `Call` nodes unwrap to their callee
/// so `@staticmethod()` would also match (no real codebase writes this,
/// but it costs nothing to handle).
fn decorator_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(attr) => Some(attr.attr.as_str()),
        Expr::Call(call) => decorator_name(&call.func),
        _ => None,
    }
}

fn method_cohesion(
    method: &StmtFunctionDef,
    siblings: &HashSet<String>,
    lines: &LineIndex,
) -> MethodCohesion {
    let mut visitor = SelfRefVisitor::default();
    for stmt in &method.body {
        visitor.visit_stmt(stmt);
    }

    let mut fields = visitor.fields;
    fields.sort();
    fields.dedup();

    let mut calls: Vec<String> = visitor
        .calls
        .into_iter()
        .filter(|name| siblings.contains(name))
        .collect();
    calls.sort();
    calls.dedup();

    let start_line = lines.line_of(method.range.start().to_usize());
    let end_offset = method.range.end().to_usize().saturating_sub(1);
    let end_line = lines.line_of(end_offset);
    MethodCohesion::new(method.name.as_str(), start_line, end_line, fields, calls)
}

#[derive(Default)]
struct SelfRefVisitor {
    fields: Vec<String>,
    calls: Vec<String>,
    /// We need to know when an attribute access is the callee of a
    /// `Call` so we count `self.bar()` as a call rather than as a field
    /// access. Visiting an `ExprCall` flips this to `true` for the
    /// descent into its `func`, then back.
    in_callee: bool,
}

impl<'a> Visitor<'a> for SelfRefVisitor {
    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Attribute(attr) if is_self_expr(&attr.value) => {
                if self.in_callee {
                    self.calls.push(attr.attr.as_str().to_owned());
                } else {
                    self.fields.push(attr.attr.as_str().to_owned());
                }
                // Don't recurse into the `self` value: it would just
                // produce an `ExprName` for `self`, which we don't track.
                return;
            }
            Expr::Call(call) => {
                let prev = self.in_callee;
                self.in_callee = true;
                self.visit_expr(&call.func);
                self.in_callee = prev;
                for arg in &call.arguments.args {
                    self.visit_expr(arg);
                }
                for kw in &call.arguments.keywords {
                    self.visit_expr(&kw.value);
                }
                return;
            }
            _ => {}
        }
        let prev = self.in_callee;
        self.in_callee = false;
        walk_expr(self, expr);
        self.in_callee = prev;
    }

    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        // Statements never sit at the receiver position of a call, so
        // reset the flag before descending.
        let prev = self.in_callee;
        self.in_callee = false;
        walk_stmt(self, stmt);
        self.in_callee = prev;
    }
}

fn is_self_expr(expr: &Expr) -> bool {
    matches!(expr, Expr::Name(n) if n.id.as_str() == "self")
}

/// Build the module-level cohesion unit, if any. Returns `None` when the
/// file has no production top-level function — class-only modules and
/// pure-test modules don't get a unit.
fn unit_from_module(body: &[Stmt], lines: &LineIndex) -> Option<CohesionUnit> {
    let functions: Vec<&StmtFunctionDef> = body
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(f) if !is_stub_function(f) && !is_test_function(f) => Some(f),
            _ => None,
        })
        .collect();
    if functions.is_empty() {
        return None;
    }

    let module_fields = collect_module_fields(body);
    let sibling_names: HashSet<String> = functions
        .iter()
        .map(|f| f.name.as_str().to_owned())
        .collect();

    let cohesions: Vec<MethodCohesion> = functions
        .iter()
        .map(|f| module_function_cohesion(f, &module_fields, &sibling_names, lines))
        .collect();

    let (start_line, end_line) = module_line_range(body, lines);
    Some(CohesionUnit::build(
        CohesionUnitKind::Module,
        MODULE_UNIT_NAME,
        start_line,
        end_line,
        cohesions,
    ))
}

/// Names of every top-level binding that participates in cohesion as a
/// "field": plain assignments, annotated assignments, augmented
/// assignments, walrus targets, and `for` / `with` / `except` loop
/// variables. Imports and nested `def` / `class` definitions are
/// excluded — those are scoped names, not shared state. Tuple
/// destructuring is unwound so `A, B = 1, 2` contributes both names.
fn collect_module_fields(body: &[Stmt]) -> HashSet<String> {
    let mut fields = HashSet::new();
    for stmt in body {
        match stmt {
            Stmt::Assign(a) => {
                for target in &a.targets {
                    collect_assign_targets(target, &mut fields);
                }
            }
            Stmt::AnnAssign(a) => {
                collect_assign_targets(&a.target, &mut fields);
            }
            Stmt::AugAssign(a) => {
                collect_assign_targets(&a.target, &mut fields);
            }
            _ => {}
        }
    }
    fields
}

/// Recursively pull out every plain `Name` target from an assignment LHS.
/// Tuple / list destructuring is unwound; subscript and attribute
/// targets are skipped because they do not introduce new bindings.
fn collect_assign_targets(target: &Expr, out: &mut HashSet<String>) {
    match target {
        Expr::Name(n) => {
            out.insert(n.id.as_str().to_owned());
        }
        Expr::Tuple(t) => {
            for elt in &t.elts {
                collect_assign_targets(elt, out);
            }
        }
        Expr::List(l) => {
            for elt in &l.elts {
                collect_assign_targets(elt, out);
            }
        }
        Expr::Starred(s) => {
            collect_assign_targets(&s.value, out);
        }
        _ => {}
    }
}

fn module_line_range(body: &[Stmt], lines: &LineIndex) -> (usize, usize) {
    if body.is_empty() {
        return (1, 1);
    }
    let first = body
        .first()
        .map(|s| s.range().start().to_usize())
        .unwrap_or(0);
    let last = body
        .last()
        .map(|s| s.range().end().to_usize().saturating_sub(1))
        .unwrap_or(0);
    (lines.line_of(first), lines.line_of(last))
}

fn module_function_cohesion(
    func: &StmtFunctionDef,
    module_fields: &HashSet<String>,
    siblings: &HashSet<String>,
    lines: &LineIndex,
) -> MethodCohesion {
    let locals = collect_local_names(func);
    let mut visitor = ModuleRefVisitor {
        module_fields,
        siblings,
        locals: &locals,
        fields: Vec::new(),
        calls: Vec::new(),
        in_callee: false,
    };
    for stmt in &func.body {
        visitor.visit_stmt(stmt);
    }
    let mut fields = visitor.fields;
    fields.sort();
    fields.dedup();
    let mut calls = visitor.calls;
    calls.sort();
    calls.dedup();

    let start_line = lines.line_of(func.range.start().to_usize());
    let end_offset = func.range.end().to_usize().saturating_sub(1);
    let end_line = lines.line_of(end_offset);
    MethodCohesion::new(func.name.as_str(), start_line, end_line, fields, calls)
}

/// Names that resolve to function-local bindings (parameters and any
/// in-body assignment / loop / `with` / `except` target / nested
/// `def` / `class`), with `global` declarations subtracted back out so
/// `global counter; counter += 1` correctly references the module-level
/// `counter`. `nonlocal` is treated like `global` — at the top level
/// there's no enclosing function, but the name still doesn't bind locally.
fn collect_local_names(func: &StmtFunctionDef) -> HashSet<String> {
    let mut locals = HashSet::new();
    let mut globals = HashSet::new();
    for param in func.parameters.iter_non_variadic_params() {
        locals.insert(param.parameter.name.as_str().to_owned());
    }
    if let Some(vararg) = &func.parameters.vararg {
        locals.insert(vararg.name.as_str().to_owned());
    }
    if let Some(kwarg) = &func.parameters.kwarg {
        locals.insert(kwarg.name.as_str().to_owned());
    }
    let mut walker = LocalNameWalker {
        locals: &mut locals,
        globals: &mut globals,
    };
    for stmt in &func.body {
        walker.walk_stmt(stmt);
    }
    for name in &globals {
        locals.remove(name);
    }
    locals
}

struct LocalNameWalker<'a> {
    locals: &'a mut HashSet<String>,
    globals: &'a mut HashSet<String>,
}

impl LocalNameWalker<'_> {
    fn walk_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Assign(a) => self.walk_assign(a),
            Stmt::AnnAssign(a) => self.walk_ann_assign(a),
            Stmt::AugAssign(a) => self.walk_aug_assign(a),
            Stmt::For(f) => self.walk_for(f),
            Stmt::While(w) => self.walk_while(w),
            Stmt::If(i) => self.walk_if(i),
            Stmt::With(w) => self.walk_with(w),
            Stmt::Try(t) => self.walk_try(t),
            Stmt::Match(m) => self.walk_match(m),
            Stmt::FunctionDef(f) => self.record_binding(f.name.as_str()),
            Stmt::ClassDef(c) => self.record_binding(c.name.as_str()),
            Stmt::Global(g) => self.record_globals(g.names.iter().map(|name| name.as_str())),
            Stmt::Nonlocal(n) => self.record_globals(n.names.iter().map(|name| name.as_str())),
            Stmt::Import(i) => self.walk_import(i),
            Stmt::ImportFrom(i) => self.walk_import_from(i),
            Stmt::Expr(e) => self.walk_expr(&e.value),
            Stmt::Return(r) => self.walk_return(r),
            _ => {}
        }
    }

    fn walk_assign(&mut self, stmt: &StmtAssign) {
        for target in &stmt.targets {
            self.collect_target(target);
        }
        self.walk_expr(&stmt.value);
    }

    fn walk_ann_assign(&mut self, stmt: &StmtAnnAssign) {
        self.collect_target(&stmt.target);
        if let Some(value) = &stmt.value {
            self.walk_expr(value);
        }
    }

    fn walk_aug_assign(&mut self, stmt: &StmtAugAssign) {
        self.collect_target(&stmt.target);
        self.walk_expr(&stmt.value);
    }

    fn walk_for(&mut self, stmt: &StmtFor) {
        self.collect_target(&stmt.target);
        self.walk_expr(&stmt.iter);
        self.walk_suite(&stmt.body);
        self.walk_suite(&stmt.orelse);
    }

    fn walk_while(&mut self, stmt: &StmtWhile) {
        self.walk_expr(&stmt.test);
        self.walk_suite(&stmt.body);
        self.walk_suite(&stmt.orelse);
    }

    fn walk_if(&mut self, stmt: &StmtIf) {
        self.walk_expr(&stmt.test);
        self.walk_suite(&stmt.body);
        for clause in &stmt.elif_else_clauses {
            if let Some(test) = &clause.test {
                self.walk_expr(test);
            }
            self.walk_suite(&clause.body);
        }
    }

    fn walk_with(&mut self, stmt: &StmtWith) {
        for item in &stmt.items {
            self.walk_expr(&item.context_expr);
            if let Some(opt) = &item.optional_vars {
                self.collect_target(opt);
            }
        }
        self.walk_suite(&stmt.body);
    }

    fn walk_try(&mut self, stmt: &StmtTry) {
        self.walk_suite(&stmt.body);
        for handler in &stmt.handlers {
            self.walk_except_handler(handler);
        }
        self.walk_suite(&stmt.orelse);
        self.walk_suite(&stmt.finalbody);
    }

    fn walk_except_handler(&mut self, handler: &ExceptHandler) {
        let ExceptHandler::ExceptHandler(handler) = handler;
        if let Some(name) = &handler.name {
            self.record_binding(name.as_str());
        }
        if let Some(typ) = &handler.type_ {
            self.walk_expr(typ);
        }
        self.walk_suite(&handler.body);
    }

    fn walk_match(&mut self, stmt: &StmtMatch) {
        self.walk_expr(&stmt.subject);
        for case in &stmt.cases {
            self.walk_suite(&case.body);
        }
    }

    fn walk_import(&mut self, stmt: &StmtImport) {
        for alias in &stmt.names {
            let name = alias.asname.as_ref().map_or_else(
                || {
                    alias
                        .name
                        .as_str()
                        .split('.')
                        .next()
                        .unwrap_or(alias.name.as_str())
                },
                |asname| asname.as_str(),
            );
            self.record_binding(name);
        }
    }

    fn walk_import_from(&mut self, stmt: &StmtImportFrom) {
        for alias in &stmt.names {
            let name = alias
                .asname
                .as_ref()
                .map_or_else(|| alias.name.as_str(), |asname| asname.as_str());
            self.record_binding(name);
        }
    }

    fn walk_return(&mut self, stmt: &StmtReturn) {
        if let Some(value) = &stmt.value {
            self.walk_expr(value);
        }
    }

    fn walk_suite(&mut self, body: &[Stmt]) {
        for stmt in body {
            self.walk_stmt(stmt);
        }
    }

    fn record_binding(&mut self, name: &str) {
        self.locals.insert(name.to_owned());
    }

    fn record_globals<'a>(&mut self, names: impl Iterator<Item = &'a str>) {
        for name in names {
            self.globals.insert(name.to_owned());
        }
    }

    fn walk_expr(&mut self, expr: &Expr) {
        if let Expr::Named(n) = expr {
            self.collect_target(&n.target);
            self.walk_expr(&n.value);
            return;
        }
        // We only need to find walrus operators; any deeper traversal is
        // handled by the statement walker. Recurse through structural
        // children so nested expressions still get visited.
        match expr {
            Expr::BoolOp(b) => {
                for v in &b.values {
                    self.walk_expr(v);
                }
            }
            Expr::BinOp(b) => {
                self.walk_expr(&b.left);
                self.walk_expr(&b.right);
            }
            Expr::UnaryOp(u) => self.walk_expr(&u.operand),
            Expr::If(i) => {
                self.walk_expr(&i.test);
                self.walk_expr(&i.body);
                self.walk_expr(&i.orelse);
            }
            Expr::Compare(c) => {
                self.walk_expr(&c.left);
                for e in &c.comparators {
                    self.walk_expr(e);
                }
            }
            Expr::Call(c) => {
                self.walk_expr(&c.func);
                for arg in &c.arguments.args {
                    self.walk_expr(arg);
                }
                for kw in &c.arguments.keywords {
                    self.walk_expr(&kw.value);
                }
            }
            Expr::Attribute(a) => self.walk_expr(&a.value),
            Expr::Subscript(s) => {
                self.walk_expr(&s.value);
                self.walk_expr(&s.slice);
            }
            Expr::Tuple(t) => {
                for e in &t.elts {
                    self.walk_expr(e);
                }
            }
            Expr::List(l) => {
                for e in &l.elts {
                    self.walk_expr(e);
                }
            }
            _ => {}
        }
    }

    fn collect_target(&mut self, target: &Expr) {
        match target {
            Expr::Name(n) => {
                self.locals.insert(n.id.as_str().to_owned());
            }
            Expr::Tuple(t) => {
                for e in &t.elts {
                    self.collect_target(e);
                }
            }
            Expr::List(l) => {
                for e in &l.elts {
                    self.collect_target(e);
                }
            }
            Expr::Starred(s) => self.collect_target(&s.value),
            _ => {}
        }
    }
}

/// Visitor that records (a) references to module-level fields not
/// shadowed by a function-local binding and (b) calls to sibling
/// top-level functions. Mirrors [`SelfRefVisitor`] but tracks free
/// names instead of `self.x`.
struct ModuleRefVisitor<'a> {
    module_fields: &'a HashSet<String>,
    siblings: &'a HashSet<String>,
    locals: &'a HashSet<String>,
    fields: Vec<String>,
    calls: Vec<String>,
    in_callee: bool,
}

impl<'a, 'ast> Visitor<'ast> for ModuleRefVisitor<'a> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Name(n) => {
                let id = n.id.as_str();
                if !self.locals.contains(id) {
                    if self.in_callee && self.siblings.contains(id) {
                        self.calls.push(id.to_owned());
                    } else if !self.in_callee && self.module_fields.contains(id) {
                        self.fields.push(id.to_owned());
                    }
                }
                return;
            }
            Expr::Call(call) => {
                let prev = self.in_callee;
                self.in_callee = true;
                self.visit_expr(&call.func);
                self.in_callee = prev;
                for arg in &call.arguments.args {
                    self.visit_expr(arg);
                }
                for kw in &call.arguments.keywords {
                    self.visit_expr(&kw.value);
                }
                return;
            }
            _ => {}
        }
        let prev = self.in_callee;
        self.in_callee = false;
        walk_expr(self, expr);
        self.in_callee = prev;
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // Don't recurse into nested function / class bodies — their
        // scopes are independent, and the cohesion-graph contract is
        // "what does *this* function reach", not its inner closures.
        match stmt {
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => return,
            _ => {}
        }
        let prev = self.in_callee;
        self.in_callee = false;
        walk_stmt(self, stmt);
        self.in_callee = prev;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn unit(src: &str) -> CohesionUnit {
        let mut units = extract_cohesion_units(src).unwrap();
        assert_eq!(units.len(), 1, "expected exactly one class");
        units.remove(0)
    }

    fn module_unit(src: &str) -> CohesionUnit {
        extract_cohesion_units(src)
            .unwrap()
            .into_iter()
            .find(|u| matches!(u.kind, CohesionUnitKind::Module))
            .expect("expected a module-level cohesion unit")
    }

    #[test]
    fn cohesive_class_collapses_to_one_component() {
        let src = "
class Counter:
    def inc(self):
        self.n += 1
    def get(self):
        return self.n
";
        let u = unit(src);
        assert_eq!(u.type_name, "Counter");
        assert_eq!(u.components.len(), 1);
        assert_eq!(u.methods.len(), 2);
    }

    #[test]
    fn split_responsibilities_show_multiple_components() {
        let src = "
class Thing:
    def bump(self):
        self.counter += 1
    def current(self):
        return self.counter
    def record(self, s):
        self.log += s
    def dump(self):
        return self.log
";
        let u = unit(src);
        assert_eq!(u.components.len(), 2);
        assert_eq!(u.components, vec![vec![0, 1], vec![2, 3]]);
    }

    #[test]
    fn calls_via_self_method_form_an_edge() {
        // `helper` needs a real body: a `pass`-only method now reads
        // as a stub and is dropped before cohesion is measured, which
        // would also strip it from `outer`'s set of in-unit calls.
        let src = "
class Foo:
    def outer(self):
        self.helper()
    def helper(self):
        return 1
";
        let u = unit(src);
        assert_eq!(u.components.len(), 1);
        let outer = u.methods.iter().find(|m| m.name == "outer").unwrap();
        assert_eq!(outer.calls, vec!["helper"]);
    }

    #[test]
    fn static_methods_are_excluded() {
        let src = "
class Foo:
    @staticmethod
    def make():
        return Foo()
    def get(self):
        return self.n
    def set(self, n):
        self.n = n
";
        let u = unit(src);
        assert_eq!(u.methods.len(), 2);
        assert_eq!(u.components.len(), 1);
        for m in &u.methods {
            assert_ne!(m.name, "make", "static methods should be filtered");
        }
    }

    #[test]
    fn qualified_staticmethod_decorator_is_recognised() {
        // `@some_alias.staticmethod` is the `Expr::Attribute` arm of
        // `decorator_name`. If that arm is ever deleted, the alias
        // path is no longer recognised and the decorated method
        // sneaks back into the cohesion unit.
        let src = "
class Foo:
    @abc.staticmethod
    def make():
        return Foo()
    def get(self):
        return self.n
    def set(self, n):
        self.n = n
";
        let u = unit(src);
        assert_eq!(u.methods.len(), 2);
        for m in &u.methods {
            assert_ne!(m.name, "make");
        }
    }

    #[test]
    fn called_decorator_is_recognised_via_callee() {
        // `@staticmethod()` is the `Expr::Call` arm of
        // `decorator_name`: it unwraps to the callee `staticmethod`
        // before checking the leaf name. Deleting that arm would
        // leave the decoration unrecognised and the method would
        // appear in the cohesion unit.
        let src = "
class Foo:
    @staticmethod()
    def make():
        return Foo()
    def get(self):
        return self.n
    def set(self, n):
        self.n = n
";
        let u = unit(src);
        assert_eq!(u.methods.len(), 2);
        for m in &u.methods {
            assert_ne!(m.name, "make");
        }
    }

    #[test]
    fn classmethods_are_excluded() {
        let src = "
class Foo:
    @classmethod
    def of(cls):
        return cls()
    def get(self):
        return self.n
    def set(self, n):
        self.n = n
";
        let u = unit(src);
        assert_eq!(u.methods.len(), 2);
        for m in &u.methods {
            assert_ne!(m.name, "of", "classmethods should be filtered");
        }
    }

    #[test]
    fn dunder_init_is_excluded() {
        // Including __init__ would over-link every method through every
        // field it assigns, defeating LCOM4.
        let src = "
class Foo:
    def __init__(self):
        self.a = 0
        self.b = 0
    def getA(self):
        return self.a
    def getB(self):
        return self.b
";
        let u = unit(src);
        assert_eq!(u.methods.len(), 2);
        assert_eq!(u.components.len(), 2);
    }

    #[test]
    fn external_calls_do_not_create_edges() {
        let src = "
class Foo:
    def a(self):
        other()
    def b(self):
        another()
";
        let u = unit(src);
        assert_eq!(u.components.len(), 2);
    }

    #[test]
    fn field_access_on_other_object_is_not_self_field() {
        let src = "
class Foo:
    def external(self, o):
        return o.tag
    def local(self):
        return self.tag
";
        let u = unit(src);
        let external = u.methods.iter().find(|m| m.name == "external").unwrap();
        let local = u.methods.iter().find(|m| m.name == "local").unwrap();
        assert!(external.fields.is_empty());
        assert_eq!(local.fields, vec!["tag"]);
        assert_eq!(u.components.len(), 2);
    }

    #[rstest]
    #[case::test_class(
        "
class TestThing:
    def helper(self):
        return self.tag
    def use(self):
        return self.helper()
"
    )]
    #[case::unittest_testcase_subclass(
        "
import unittest
class Foo(unittest.TestCase):
    def test_a(self):
        return self.tag
    def use(self):
        return self.tag
"
    )]
    #[case::protocol_class(
        "
from typing import Protocol

class Service(Protocol):
    def handle(self, x): ...
    def close(self): ...
"
    )]
    #[case::class_with_only_dunder_init(
        "
class Foo:
    def __init__(self, n):
        self.n = n
"
    )]
    #[case::pure_test_module(
        "
def test_a():
    assert True

def test_b():
    assert True
"
    )]
    fn extracts_no_units(#[case] src: &str) {
        let units = extract_cohesion_units(src).unwrap();
        assert!(units.is_empty());
    }

    #[test]
    fn abstract_methods_are_excluded_from_cohesion_unit() {
        // `@abstractmethod`-decorated methods inside a regular class
        // (e.g. an ABC subclass with mixed concrete + abstract methods)
        // must not pollute the cohesion graph. Only `concrete` and
        // `also_concrete` should be visible to LCOM4.
        let src = "
from abc import abstractmethod

class Service:
    @abstractmethod
    def stub(self): ...

    def concrete(self):
        return self.x
    def also_concrete(self):
        return self.x
";
        let u = unit(src);
        let names: Vec<&str> = u.methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["concrete", "also_concrete"]);
        assert_eq!(u.components.len(), 1);
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = extract_cohesion_units("class !!!:\n").unwrap_err();
        assert!(matches!(err, CohesionError::Parse(_)));
    }

    #[test]
    fn cohesion_error_source_is_present() {
        use std::error::Error as _;
        let err = extract_cohesion_units("class !!!:\n").unwrap_err();
        assert!(err.source().is_some());
    }

    #[test]
    fn line_range_covers_class_through_last_method() {
        let src = "class Foo:\n    def get(self):\n        return self.n\n";
        let u = unit(src);
        assert_eq!(u.start_line, 1);
        assert_eq!(u.end_line, 3);
    }

    #[test]
    fn nested_attribute_access_does_not_leak_self_field() {
        // `obj.tag` inside a method that has no `self.<x>` access should
        // not record `tag` as a self-field. Without the visitor's
        // `is_self_expr` guard the in_callee tracker would still mark
        // attribute accesses as fields verbatim.
        let src = "
class Foo:
    def a(self, obj):
        return obj.tag
    def b(self):
        return self.tag
";
        let u = unit(src);
        let a = u.methods.iter().find(|m| m.name == "a").unwrap();
        assert!(a.fields.is_empty(), "a should not see obj.tag as self.tag");
        let b = u.methods.iter().find(|m| m.name == "b").unwrap();
        assert_eq!(b.fields, vec!["tag"]);
    }

    #[test]
    fn self_method_call_with_kwargs_still_counts_as_call() {
        let src = "
class Foo:
    def outer(self):
        self.helper(x=1, y=2)
    def helper(self, x, y):
        pass
";
        let u = unit(src);
        assert_eq!(u.components.len(), 1);
    }

    // ---------- Module-level cohesion ----------

    #[test]
    fn module_unit_collapses_when_all_functions_share_a_global() {
        let src = "
counter = 0

def bump():
    global counter
    counter += 1

def get():
    return counter
";
        let u = module_unit(src);
        assert!(matches!(u.kind, CohesionUnitKind::Module));
        assert_eq!(u.type_name, "<module>");
        assert_eq!(u.methods.len(), 2);
        assert_eq!(u.components.len(), 1);
    }

    #[test]
    fn module_split_responsibilities_show_multiple_components() {
        let src = "
counter = 0
log = []

def bump():
    global counter
    counter += 1

def current():
    return counter

def record(s):
    log.append(s)

def dump():
    return log
";
        let u = module_unit(src);
        assert_eq!(u.components.len(), 2);
        assert_eq!(u.components, vec![vec![0, 1], vec![2, 3]]);
    }

    #[test]
    fn module_sibling_call_forms_an_edge() {
        let src = "
def outer():
    helper()

def helper():
    return 1
";
        let u = module_unit(src);
        assert_eq!(u.components.len(), 1);
        let outer = u.methods.iter().find(|m| m.name == "outer").unwrap();
        assert_eq!(outer.calls, vec!["helper"]);
    }

    #[test]
    fn module_local_assignment_shadows_module_field() {
        // The function's local `counter` rebinds the name, so the
        // reference is NOT to the module-level `counter`. Without
        // shadow-tracking the two functions would spuriously share a
        // field and collapse to one component.
        let src = "
counter = 0

def reads_global():
    return counter

def shadowed():
    counter = 99
    return counter
";
        let u = module_unit(src);
        assert_eq!(u.components.len(), 2);
    }

    #[test]
    fn module_global_declaration_re_exposes_module_field() {
        // `global counter` makes `counter` resolve to the module-level
        // binding even though the function also writes to it. Without
        // the `global` -> "remove from locals" subtraction, both
        // functions would look local-only and miss the cohesion edge.
        let src = "
counter = 0

def reads():
    return counter

def writes():
    global counter
    counter = 1
";
        let u = module_unit(src);
        assert_eq!(u.components.len(), 1);
    }

    #[test]
    fn module_test_functions_are_excluded() {
        // `test_*` functions are scaffolding; their cohesion is
        // dictated by the test framework, not by a real production
        // responsibility split. The single production function leaves
        // the unit with one method.
        let src = "
counter = 0

def production():
    return counter

def test_smoke():
    assert production() == 0
";
        let u = module_unit(src);
        let names: Vec<&str> = u.methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["production"]);
    }

    #[test]
    fn module_unit_is_skipped_when_no_top_level_functions() {
        let src = "
class Counter:
    def inc(self):
        self.n += 1
    def get(self):
        return self.n
";
        let units = extract_cohesion_units(src).unwrap();
        // One class unit, no module unit.
        assert_eq!(units.len(), 1);
        assert!(matches!(units[0].kind, CohesionUnitKind::Inherent));
    }

    #[test]
    fn module_calls_to_non_siblings_do_not_create_edges() {
        // `print` is a builtin, not a sibling — so two functions that
        // both call `print` must not collapse into one component on
        // that basis.
        let src = "
def a():
    print(1)

def b():
    print(2)
";
        let u = module_unit(src);
        assert_eq!(u.components.len(), 2);
    }

    #[test]
    fn module_nested_def_is_not_a_separate_unit_or_field_reference() {
        // The nested `inner` is a local binding inside `outer`; it
        // must NOT register as a module-level field reference, and
        // must NOT be picked up as its own sibling. Without this
        // guard, `outer` would record a reference to `helper` via
        // its nested call, polluting the cohesion graph.
        let src = "
def outer():
    def inner():
        return helper()
    return inner

def helper():
    return 1
";
        let u = module_unit(src);
        let outer = u.methods.iter().find(|m| m.name == "outer").unwrap();
        // outer's body only contains a `def inner` and a `return inner`;
        // the call inside `inner` belongs to `inner`'s own scope.
        assert!(
            outer.calls.is_empty(),
            "outer.calls should be empty, got {:?}",
            outer.calls
        );
    }
}
