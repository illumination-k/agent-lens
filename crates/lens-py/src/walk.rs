//! Single-pass walk over the function-shaped statements in a Python
//! module: module-level `def`s and the methods of the classes they
//! contain.
//!
//! The parser, complexity, wrapper, and call-index extractors each used to
//! carry the same `Stmt::FunctionDef` / `Stmt::ClassDef` ladder, and with
//! it the same two policy decisions: stub-shaped functions
//! (`@overload`, `@abstractmethod`, `pass` / `...` / docstring-only /
//! `raise NotImplementedError`) carry no analysable content, and PEP 544
//! `Protocol` classes are pure declarations whose whole subtree is stubs.
//! Both filters now live here, so a change of policy is a change in one
//! place rather than five.
//!
//! Nested `def`s and classes *inside a function body* are deliberately not
//! emitted: a function body is the atomic unit of analysis, matching
//! `lens-rust` (closures stay inside their parent fn) and `lens-ts`
//! (inner functions contribute to their parent's score). Classes nested
//! inside a class body are walked, with `owner` naming the innermost one.
//!
//! This is the Python counterpart of `lens-golang`'s `walk.rs`,
//! `lens-ts`'s `walk.rs`, and `lens-rust`'s `common::walk_fn_items`.

use ruff_python_ast::{Stmt, StmtClassDef, StmtFunctionDef};

use crate::attrs::{inherits_protocol, is_stub_function, is_test_class, is_test_function};

/// One function-shaped statement found by [`walk_module_fns`].
pub(crate) struct FnSite<'a> {
    pub func: &'a StmtFunctionDef,
    /// Name of the enclosing class, or `None` for a module-level `def`.
    pub owner: Option<&'a str>,
    /// True when the function is test-shaped, or when any enclosing class
    /// is. Consumers that report test functions separately (parser,
    /// call index) or drop them outright (wrapper) share this one rule.
    pub is_test: bool,
}

/// Walk `body` — a module's top-level statements — and emit one
/// [`FnSite`] per non-stub function, in source order.
pub(crate) fn walk_module_fns<'a, F>(body: &'a [Stmt], visit: &mut F)
where
    F: FnMut(FnSite<'a>),
{
    walk_stmts(body, None, false, visit);
}

fn walk_stmts<'a, F>(body: &'a [Stmt], owner: Option<&'a str>, owner_is_test: bool, visit: &mut F)
where
    F: FnMut(FnSite<'a>),
{
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(func) => {
                if is_stub_function(func) {
                    continue;
                }
                visit(FnSite {
                    func,
                    owner,
                    is_test: owner_is_test || is_test_function(func),
                });
            }
            Stmt::ClassDef(class) => walk_class(class, owner_is_test, visit),
            _ => {}
        }
    }
}

fn walk_class<'a, F>(class: &'a StmtClassDef, owner_is_test: bool, visit: &mut F)
where
    F: FnMut(FnSite<'a>),
{
    if inherits_protocol(class) {
        return;
    }
    walk_stmts(
        &class.body,
        Some(class.name.as_str()),
        owner_is_test || is_test_class(class),
        visit,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_parser::parse_module;

    fn sites(source: &str) -> Vec<(String, Option<String>, bool)> {
        let module = parse_module(source).expect("parses").into_syntax();
        let mut out = Vec::new();
        walk_module_fns(&module.body, &mut |site| {
            out.push((
                site.func.name.as_str().to_owned(),
                site.owner.map(ToOwned::to_owned),
                site.is_test,
            ));
        });
        out
    }

    #[test]
    fn emits_module_functions_and_methods_with_their_owner() {
        let source =
            "def alpha():\n    return 1\n\n\nclass C:\n    def beta(self):\n        return 2\n";
        assert_eq!(
            sites(source),
            [
                ("alpha".to_owned(), None, false),
                ("beta".to_owned(), Some("C".to_owned()), false),
            ]
        );
    }

    #[test]
    fn skips_stub_functions_and_protocol_subtrees() {
        let source = "from typing import Protocol\n\n\ndef stub():\n    ...\n\n\nclass P(Protocol):\n    def contract(self):\n        ...\n\n\ndef real():\n    return 1\n";
        assert_eq!(sites(source), [("real".to_owned(), None, false)]);
    }

    #[test]
    fn test_shape_comes_from_the_function_or_any_enclosing_class() {
        let source = "def test_alpha():\n    assert True\n\n\nclass TestSuite:\n    def helper(self):\n        return 1\n\n    class Inner:\n        def nested(self):\n            return 2\n";
        assert_eq!(
            sites(source),
            [
                ("test_alpha".to_owned(), None, true),
                ("helper".to_owned(), Some("TestSuite".to_owned()), true),
                ("nested".to_owned(), Some("Inner".to_owned()), true),
            ]
        );
    }

    #[test]
    fn nested_definitions_inside_a_body_are_left_to_their_parent() {
        let source = "def outer():\n    def inner():\n        return 1\n\n    class Local:\n        def method(self):\n            return 2\n\n    return inner\n";
        assert_eq!(sites(source), [("outer".to_owned(), None, false)]);
    }
}
