//! ruff-based cohesion extraction for Python classes.
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
//! Calls are matched verbatim against the names of *sibling* methods on
//! the same class. Calls to anything else are dropped — the cohesion
//! graph only ever sees in-unit edges, mirroring `lens-rust` and
//! `lens-ts`.
//!
//! Test classes (`Test*` names or `unittest.TestCase` subclasses) are
//! filtered out: their cohesion is dictated by the test framework's
//! lifecycle hooks, not by a real production responsibility split.

use std::collections::HashSet;

use lens_domain::{CohesionUnit, CohesionUnitKind, MethodCohesion};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Decorator, Expr, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_python_parser::{ParseError, parse_module};

use crate::attrs::{class_inherits_test_case, name_looks_like_test_class};
use crate::line_index::LineIndex;

/// Failures produced while extracting cohesion units.
#[derive(Debug, thiserror::Error)]
pub enum CohesionError {
    #[error("failed to parse Python source: {0}")]
    Parse(#[from] ParseError),
}

/// Extract one [`CohesionUnit`] per class in `source` that has at least
/// one instance method.
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
    Ok(out)
}

fn unit_from_class(class: &StmtClassDef, lines: &LineIndex) -> Option<CohesionUnit> {
    if name_looks_like_test_class(&class.name) || class_inherits_test_case(class) {
        return None;
    }
    let class_name = class.name.as_str();

    let methods: Vec<&StmtFunctionDef> = class
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(f) if is_instance_method(f) => Some(f),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(src: &str) -> CohesionUnit {
        let mut units = extract_cohesion_units(src).unwrap();
        assert_eq!(units.len(), 1, "expected exactly one class");
        units.remove(0)
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
        assert_eq!(u.lcom4(), 1);
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
        assert_eq!(u.lcom4(), 2);
        assert_eq!(u.components, vec![vec![0, 1], vec![2, 3]]);
    }

    #[test]
    fn calls_via_self_method_form_an_edge() {
        let src = "
class Foo:
    def outer(self):
        self.helper()
    def helper(self):
        pass
";
        let u = unit(src);
        assert_eq!(u.lcom4(), 1);
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
        assert_eq!(u.lcom4(), 1);
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
        assert_eq!(u.lcom4(), 2);
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
        assert_eq!(u.lcom4(), 2);
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
        assert_eq!(u.lcom4(), 2);
    }

    #[test]
    fn test_classes_are_skipped() {
        // Test classes' cohesion is dictated by the framework's
        // lifecycle hooks, not by a real production responsibility split.
        let src = "
class TestThing:
    def helper(self):
        return self.tag
    def use(self):
        return self.helper()
";
        let units = extract_cohesion_units(src).unwrap();
        assert!(units.is_empty());
    }

    #[test]
    fn unittest_testcase_subclasses_are_skipped() {
        let src = "
import unittest
class Foo(unittest.TestCase):
    def test_a(self):
        return self.tag
    def use(self):
        return self.tag
";
        let units = extract_cohesion_units(src).unwrap();
        assert!(units.is_empty());
    }

    #[test]
    fn class_with_only_dunder_init_is_skipped() {
        let src = "
class Foo:
    def __init__(self, n):
        self.n = n
";
        let units = extract_cohesion_units(src).unwrap();
        assert!(units.is_empty());
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
        assert_eq!(u.lcom4(), 1);
    }
}
