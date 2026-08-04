//! Go function and call-shape extraction for the function-graph
//! analyzer.
//!
//! Stays syntax-only. Free functions and methods become
//! [`FunctionShape`]s qualified at the lexical module path the caller
//! supplies. Each call expression inside a function body becomes a
//! [`CallShape`] tagged with the imports visible at its file.
//!
//! Go's `import "path/to/pkg"` declarations create a namespace alias on
//! the last path segment (or on the explicit alias when the import
//! reads `import foo "path/to/pkg"`). This module mirrors `lens-py` and
//! `lens-ts`: namespace-aliased member calls (`pkg.Func()`) read as path
//! calls, while plain receiver calls (`obj.Method()`) stay as receiver
//! calls so the resolver leaves them unresolved unless the receiver is
//! a namespace alias or an uppercase identifier.

use std::collections::HashSet;

use lens_domain::{
    BodyShape, CallShape, FunctionShape, ImportShape, LexicalResolutionStatus, OwnerKind,
    OwnerShape, ParameterShape, ReceiverExprKind, SignatureShape, SourceSpan, SyntaxFact,
    VisibilityShape, callee_names_local_binding, qualify_module, starts_uppercase,
};
use tree_sitter::Node;

use crate::attrs::name_looks_like_test_function;
use crate::node_text::node_str;
use crate::parser::{GoParseError, parse_tree, unquote_go_string_literal};
use crate::walk::{FnSite, walk_top_level_fns};

/// Extract neutral function-shape facts for Go.
pub fn extract_function_shapes_with_module(
    source: &str,
    module: &str,
) -> Result<Vec<FunctionShape>, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    walk_top_level_fns(tree.root_node(), bytes, &mut |site| {
        out.push(function_shape(&site, bytes, module));
    });
    Ok(out)
}

/// Extract neutral call-shape facts for Go.
pub fn extract_call_shapes_with_module(
    source: &str,
    module: &str,
) -> Result<Vec<CallShape>, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let imports = collect_imports(tree.root_node(), bytes);
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
    walk_top_level_fns(tree.root_node(), bytes, &mut |site| {
        collect_calls_in_function(&site, bytes, module, &imports, &namespace_aliases, &mut out);
    });
    Ok(out)
}

fn function_shape(site: &FnSite<'_, '_>, source: &[u8], module: &str) -> FunctionShape {
    let node = site.node;
    let body = site.body;
    let raw_name = site.name;
    let owner = site.owner.as_deref();
    let display_name = raw_name.to_owned();
    let qualified = match owner {
        Some(class) => qualify_module(module, &format!("{class}::{display_name}")),
        None => qualify_module(module, &display_name),
    };
    let owner_shape = owner.map(|class_name| OwnerShape {
        display_name: class_name.to_owned(),
        kind: OwnerKind::Receiver,
    });
    let is_test = owner.is_none() && name_looks_like_test_function(raw_name);
    let span = SourceSpan {
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
    };
    let visibility = if starts_uppercase(raw_name) {
        VisibilityShape::Exported
    } else {
        VisibilityShape::Unexported
    };
    FunctionShape {
        display_name,
        qualified_name: SyntaxFact::Known(qualified),
        module_path: SyntaxFact::Known(module.to_owned()),
        owner: SyntaxFact::Known(owner_shape),
        visibility: SyntaxFact::Known(visibility),
        signature: SyntaxFact::Known(parameter_signature(node, source)),
        doc: crate::parser::doc_comment_text(node, source),
        attributes: SyntaxFact::Known(crate::parser::directive_names(node, source)),
        body: BodyShape {
            tree: crate::parser::function_body_tree(body, source),
        },
        span,
        is_test,
    }
}

/// Project a declaration's parameter list into a [`SignatureShape`]
/// that carries only the parameter slots (with their names, where
/// declared — the receiver is not a slot). The call graph reads the
/// slot count to match methods against interface method sets by arity;
/// every other signature fact stays [`SyntaxFact::Unknown`] rather than
/// being half-extracted here.
fn parameter_signature(node: Node<'_>, source: &[u8]) -> SignatureShape {
    let params = node
        .child_by_field_name("parameters")
        .map(|params| crate::parser::parameter_slot_names(params, source))
        .unwrap_or_default()
        .into_iter()
        .map(|name| ParameterShape {
            name: SyntaxFact::Known(name),
            type_annotation: SyntaxFact::Unknown,
            type_paths: Vec::new(),
        })
        .collect();
    SignatureShape {
        name_tokens: SyntaxFact::Unknown,
        params,
        return_type: SyntaxFact::Unknown,
        return_type_paths: Vec::new(),
        receiver: SyntaxFact::Unknown,
        generics: SyntaxFact::Unknown,
        bounds: SyntaxFact::Unknown,
    }
}

fn collect_calls_in_function(
    site: &FnSite<'_, '_>,
    source: &[u8],
    module: &str,
    imports: &[ImportShape],
    namespace_aliases: &HashSet<String>,
    out: &mut Vec<CallShape>,
) {
    let owner = site.owner.as_deref();
    let caller_qualified = match owner {
        Some(class) => qualify_module(module, &format!("{class}::{}", site.name)),
        None => qualify_module(module, site.name),
    };
    let locally_bound = local_callable_bindings(site, source);
    let ctx = CallContext {
        source,
        module,
        caller_qualified_name: &caller_qualified,
        caller_owner: owner.map(ToOwned::to_owned),
        imports,
        namespace_aliases,
        locally_bound,
    };
    visit_calls(site.body, &ctx, out);
}

/// Names bound to a callable inside `site`'s own scope: closures held in
/// a local (`emit := func(...) {...}`, `var emit = func...`, `emit =
/// func...`) and function-typed parameters (`func pump(emit func(Event))`).
///
/// A call to one of these targets the local binding, so the resolver must
/// not attribute it to a package-level function of the same name. Go
/// scopes `:=` from its declaration to the end of the block; tracking
/// that position would only matter for a call that precedes the binding
/// and means the outer name, which is rare enough that whole-body scope
/// is the better trade — dropping an edge beats fabricating one.
fn local_callable_bindings(site: &FnSite<'_, '_>, source: &[u8]) -> HashSet<String> {
    let mut names = HashSet::new();
    if let Some(params) = site.node.child_by_field_name("parameters") {
        collect_function_typed_params(params, source, &mut names);
    }
    collect_func_literal_bindings(site.body, source, &mut names);
    names
}

/// Parameters whose declared type is a `func(...)` type.
fn collect_function_typed_params(params: Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    let mut cursor = params.walk();
    for param in params.named_children(&mut cursor) {
        if param.kind() != "parameter_declaration" {
            continue;
        }
        if !param
            .child_by_field_name("type")
            .is_some_and(|ty| ty.kind() == "function_type")
        {
            continue;
        }
        // A single declaration can name several parameters
        // (`func(f, g func())`), each an `identifier` in the `name` field.
        let mut inner = param.walk();
        for child in param.named_children(&mut inner) {
            if child.kind() == "identifier"
                && let Some(name) = node_str(child, source)
            {
                out.insert(name.to_owned());
            }
        }
    }
}

/// Identifiers on the left of a binding whose right-hand side is a
/// `func_literal`, anywhere in the function body (nested blocks included).
fn collect_func_literal_bindings(node: Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    if matches!(
        node.kind(),
        "short_var_declaration" | "assignment_statement" | "var_spec" | "const_spec"
    ) {
        collect_func_literal_binding_names(node, source, out);
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_func_literal_bindings(child, source, out);
    }
}

fn collect_func_literal_binding_names(node: Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    // `var emit func(Event)` declares a callable with no initialiser, so
    // the type is the only evidence — the name/value pairing below would
    // find nothing.
    if node
        .child_by_field_name("type")
        .is_some_and(|ty| ty.kind() == "function_type")
        && let Some(names) = node.child_by_field_name("name")
    {
        for name in expression_list_children(names) {
            if name.kind() == "identifier"
                && let Some(text) = node_str(name, source)
            {
                out.insert(text.to_owned());
            }
        }
    }
    // `left`/`right` for `:=` and `=`; `name`/`value` for `var`/`const`.
    // Both sides are positional lists, so pair them by index: only the
    // names whose own initialiser is a closure are bound to a callable.
    let left = node
        .child_by_field_name("left")
        .or_else(|| node.child_by_field_name("name"));
    let right = node
        .child_by_field_name("right")
        .or_else(|| node.child_by_field_name("value"));
    let (Some(left), Some(right)) = (left, right) else {
        return;
    };
    let names = expression_list_children(left);
    let values = expression_list_children(right);
    for (name, value) in names.iter().zip(values.iter()) {
        if value.kind() == "func_literal"
            && name.kind() == "identifier"
            && let Some(text) = node_str(*name, source)
        {
            out.insert(text.to_owned());
        }
    }
}

/// Flatten an `expression_list` / `identifier_list` into its elements; a
/// bare node (single-element list) stands for itself.
fn expression_list_children(node: Node<'_>) -> Vec<Node<'_>> {
    if matches!(node.kind(), "expression_list" | "identifier_list") {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).collect()
    } else {
        vec![node]
    }
}

/// Bundle the caller-side facts shared by every recursive `visit_calls`
/// step. Without this struct the call recursion would push an
/// 8-argument signature past clippy's threshold and obscure that the
/// inner walk only mutates `out`.
struct CallContext<'a> {
    source: &'a [u8],
    module: &'a str,
    caller_qualified_name: &'a str,
    caller_owner: Option<String>,
    imports: &'a [ImportShape],
    namespace_aliases: &'a HashSet<String>,
    /// Callable names bound in this function's own scope — see
    /// [`local_callable_bindings`].
    locally_bound: HashSet<String>,
}

fn visit_calls(node: Node<'_>, ctx: &CallContext<'_>, out: &mut Vec<CallShape>) {
    if node.kind() == "call_expression"
        && let Some(callee) = node.child_by_field_name("function")
    {
        let facts = callee_facts(callee, ctx.source, ctx.namespace_aliases);
        let locally_bound = callee_names_local_binding(
            facts.receiver,
            facts.path_segments.as_deref(),
            &ctx.locally_bound,
        );
        out.push(CallShape {
            caller_qualified_name: SyntaxFact::Known(Some(ctx.caller_qualified_name.to_owned())),
            caller_module: SyntaxFact::Known(ctx.module.to_owned()),
            caller_owner: SyntaxFact::Known(ctx.caller_owner.clone()),
            callee_display_name: SyntaxFact::Known(facts.name),
            callee_path_segments: facts
                .path_segments
                .map_or(SyntaxFact::Unknown, SyntaxFact::Known),
            receiver_expr_kind: SyntaxFact::Known(facts.receiver),
            callee_is_locally_bound: SyntaxFact::Known(locally_bound),
            lexical_resolution: LexicalResolutionStatus::NotAttempted,
            visible_imports: ctx.imports.to_vec(),
            line: node.start_position().row + 1,
        });
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        // Don't descend into nested function/method declarations or
        // closures: their calls belong to the inner unit. Mirrors the
        // Python adapter, which scopes `FunctionBodyCallVisitor` to a
        // single `def`.
        if matches!(
            child.kind(),
            "function_declaration" | "method_declaration" | "func_literal"
        ) {
            continue;
        }
        visit_calls(child, ctx, out);
    }
}

struct CalleeFacts {
    name: Option<String>,
    path_segments: Option<Vec<String>>,
    receiver: ReceiverExprKind,
}

fn callee_facts(
    callee: Node<'_>,
    source: &[u8],
    namespace_aliases: &HashSet<String>,
) -> CalleeFacts {
    match callee.kind() {
        "identifier" => {
            let Some(name) = node_str(callee, source) else {
                return CalleeFacts {
                    name: None,
                    path_segments: None,
                    receiver: ReceiverExprKind::None,
                };
            };
            CalleeFacts {
                name: Some(name.to_owned()),
                path_segments: Some(vec![name.to_owned()]),
                receiver: ReceiverExprKind::None,
            }
        }
        "selector_expression" => {
            let Some(field) = callee.child_by_field_name("field") else {
                return CalleeFacts {
                    name: None,
                    path_segments: None,
                    receiver: ReceiverExprKind::Expression,
                };
            };
            let Some(field_name) = node_str(field, source).map(str::to_owned) else {
                return CalleeFacts {
                    name: None,
                    path_segments: None,
                    receiver: ReceiverExprKind::Expression,
                };
            };
            let mut segments = callee
                .child_by_field_name("operand")
                .and_then(|operand| expression_path(operand, source))
                .unwrap_or_default();
            segments.push(field_name.clone());
            // Two operand shapes are namespace-aliased path calls in Go:
            // a known import alias (`pkg.Func()`) and a Go type-style
            // identifier that starts with an uppercase letter
            // (`Foo.Method()` where Foo is a type / package). Everything
            // else is a receiver call (`obj.method()`).
            let receiver = if segments
                .first()
                .is_some_and(|first| namespace_aliases.contains(first) || starts_uppercase(first))
            {
                ReceiverExprKind::None
            } else {
                ReceiverExprKind::Expression
            };
            CalleeFacts {
                name: Some(field_name),
                path_segments: (!segments.is_empty()).then_some(segments),
                receiver,
            }
        }
        "parenthesized_expression" => {
            let mut cursor = callee.walk();
            let inner = callee.named_children(&mut cursor).next();
            inner.map_or(
                CalleeFacts {
                    name: None,
                    path_segments: None,
                    receiver: ReceiverExprKind::Expression,
                },
                |inner| callee_facts(inner, source, namespace_aliases),
            )
        }
        _ => CalleeFacts {
            name: None,
            path_segments: None,
            receiver: ReceiverExprKind::Expression,
        },
    }
}

fn expression_path(node: Node<'_>, source: &[u8]) -> Option<Vec<String>> {
    match node.kind() {
        "identifier" | "type_identifier" | "package_identifier" => {
            Some(vec![node_str(node, source)?.to_owned()])
        }
        "selector_expression" => {
            let operand = node.child_by_field_name("operand")?;
            let field = node.child_by_field_name("field")?;
            let mut segments = expression_path(operand, source)?;
            segments.push(node_str(field, source)?.to_owned());
            Some(segments)
        }
        "parenthesized_expression" => {
            let mut cursor = node.walk();
            let inner = node.named_children(&mut cursor).next()?;
            expression_path(inner, source)
        }
        _ => None,
    }
}

fn collect_imports(root: Node<'_>, source: &[u8]) -> Vec<ImportShape> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "import_declaration" {
            collect_import_specs(child, source, &mut out);
        }
    }
    out
}

fn collect_import_specs(node: Node<'_>, source: &[u8], out: &mut Vec<ImportShape>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "import_spec" => push_import_spec(child, source, out),
            "import_spec_list" => {
                let mut inner = child.walk();
                for spec in child.named_children(&mut inner) {
                    if spec.kind() == "import_spec" {
                        push_import_spec(spec, source, out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn push_import_spec(spec: Node<'_>, source: &[u8], out: &mut Vec<ImportShape>) {
    let Some(path_node) = spec.child_by_field_name("path") else {
        return;
    };
    let Some(raw_path) = node_str(path_node, source) else {
        return;
    };
    let path = unquote_go_string_literal(raw_path);
    let target = path
        .split('/')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("::");
    if target.is_empty() {
        return;
    }
    // Default alias: the last segment of the import path. `import foo
    // "path/to/pkg"` overrides this; `import . "..."` (dot) and `import
    // _ "..."` (blank) drop the alias entirely so the import only
    // contributes a visible-imports entry without polluting the
    // namespace-alias set.
    let default_alias = path
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let local_alias = match spec.child_by_field_name("name") {
        Some(name) => match name.kind() {
            "blank_identifier" | "dot" => None,
            _ => node_str(name, source).map(str::to_owned).or(default_alias),
        },
        None => default_alias,
    };
    out.push(ImportShape {
        imported_module: SyntaxFact::Known(target),
        local_alias: SyntaxFact::Known(local_alias),
        // Whole-package imports — the imported entity is the package
        // itself, accessed through the alias. Mirrors how `lens-py`
        // models `import os` (vs. `from os import path`, which would
        // set `exported_symbol = Some("path")`).
        exported_symbol: SyntaxFact::Known(None),
    });
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

    #[test]
    fn extracts_module_qualified_names_for_free_and_method_functions() {
        let src = r#"
package main

func Helper() int { return 1 }

type Service struct{}

func (s *Service) Run() int { return Helper() }
"#;
        let funcs = shapes(src, "pkg::main");
        let names: Vec<&str> = funcs
            .iter()
            .map(|f| f.qualified_name.known_value().unwrap().as_str())
            .collect();
        assert_eq!(names, ["pkg::main::Helper", "pkg::main::Service::Run"]);

        let owner = funcs[1]
            .owner
            .known_value()
            .unwrap()
            .as_ref()
            .map(|o| (o.display_name.clone(), o.kind));
        assert_eq!(owner, Some(("Service".to_owned(), OwnerKind::Receiver)));
    }

    #[test]
    fn empty_module_qualifies_with_bare_name() {
        let funcs = shapes("package main\nfunc f() int { return 1 }\n", "");
        assert_eq!(
            funcs[0].qualified_name.known_value().map(String::as_str),
            Some("f"),
        );
    }

    #[test]
    fn bare_call_records_caller_module_and_imports() {
        let src = r#"
package main

import "github.com/x/proj/helper"

func caller() { helper.Run() }
"#;
        let calls = calls(src, "main");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].caller_qualified_name(), Some("main::caller"));
        assert_eq!(calls[0].callee_name(), Some("Run"));
        assert_eq!(
            calls[0].callee_path().as_deref(),
            Some("helper::Run"),
            "namespace-aliased member call must be a path call",
        );
        assert!(!calls[0].has_receiver_expression());

        let imports = &calls[0].visible_imports;
        assert_eq!(
            imports[0].imported_module.known_value().map(String::as_str),
            Some("github.com::x::proj::helper"),
            "module path keeps dotted segments (e.g. github.com) intact",
        );
        assert_eq!(
            imports[0]
                .local_alias
                .known_value()
                .and_then(Option::as_ref)
                .map(String::as_str),
            Some("helper"),
        );
    }

    #[test]
    fn aliased_imports_use_the_explicit_alias() {
        let src = r#"
package main

import foo "github.com/x/proj/helper"

func caller() { foo.Run() }
"#;
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_path().as_deref(), Some("foo::Run"));
        assert!(!call.has_receiver_expression());
        let import = &call.visible_imports[0];
        assert_eq!(
            import.local_alias.known_value().and_then(Option::as_ref),
            Some(&"foo".to_owned()),
        );
    }

    #[test]
    fn dot_and_blank_imports_carry_no_alias() {
        let src = r#"
package main

import (
    . "github.com/x/proj/dot"
    _ "github.com/x/proj/blank"
)

func caller() { run() }
"#;
        let call = &calls(src, "main")[0];
        let aliases: Vec<_> = call
            .visible_imports
            .iter()
            .filter_map(|imp| {
                imp.local_alias
                    .known_value()
                    .and_then(Option::as_ref)
                    .cloned()
            })
            .collect();
        assert!(aliases.is_empty(), "got {aliases:?}");
    }

    #[test]
    fn lowercase_member_calls_remain_receiver_calls() {
        let src = r#"
package p

func caller(client *Client) { client.connect() }
"#;
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("connect"));
        assert_eq!(call.callee_path().as_deref(), Some("client::connect"));
        assert!(call.has_receiver_expression());
    }

    #[test]
    fn uppercase_receiver_calls_are_path_calls() {
        // `Foo.Method()` looks like a static call on a type/package and
        // is treated as a path call, mirroring lens-py / lens-ts.
        let src = r#"
package p

func caller() { Foo.Bar() }
"#;
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_path().as_deref(), Some("Foo::Bar"));
        assert!(!call.has_receiver_expression());
    }

    #[test]
    fn closures_inside_functions_do_not_steal_outer_calls() {
        // Closures (`func_literal`) stay attached to their parent
        // function: their inner calls should still be attributed to the
        // outer caller, mirroring `lens-rust` and `lens-py`.
        let src = r#"
package p

func outer() {
    helper := func() { Inner() }
    helper()
}

func Inner() {}
"#;
        let calls = calls(src, "main");
        // Outer should record the call to `helper` (and not `Inner`,
        // because that one belongs to the closure body which we skip).
        let names: Vec<_> = calls
            .iter()
            .map(|c| (c.caller_qualified_name(), c.callee_name()))
            .collect();
        assert!(
            names
                .iter()
                .any(|(caller, callee)| *caller == Some("main::outer") && *callee == Some("helper")),
            "expected outer→helper call, got {names:?}",
        );
    }

    #[test]
    fn parenthesized_callees_keep_inner_name() {
        let src = r#"
package p

func caller() { (helper)() }
"#;
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("helper"));
        assert_eq!(call.callee_path().as_deref(), Some("helper"));
        assert!(!call.has_receiver_expression());
    }

    #[test]
    fn nested_selector_calls_preserve_full_path() {
        let src = r#"
package p

func caller() { a.b.c() }
"#;
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("c"));
        assert_eq!(call.callee_path().as_deref(), Some("a::b::c"));
    }

    #[test]
    fn anonymous_callee_expressions_have_no_name() {
        let src = r#"
package p

func caller() { (func() {})() }
"#;
        let call = &calls(src, "main")[0];
        assert!(call.callee_name().is_none());
        assert!(call.callee_path().is_none());
    }

    #[test]
    fn shape_visibility_reflects_uppercase_export_rule() {
        let src = "package p\nfunc Public() {}\nfunc private() {}\n";
        let funcs = shapes(src, "m");
        let visibilities: Vec<_> = funcs
            .iter()
            .map(|f| f.visibility.known_value().cloned())
            .collect();
        assert_eq!(
            visibilities,
            [
                Some(VisibilityShape::Exported),
                Some(VisibilityShape::Unexported),
            ]
        );
    }

    /// The graph matches methods against interface method sets by
    /// arity, so the call-index shapes carry parameter slots: grouped
    /// names expand, unnamed types count one each, a variadic slot is
    /// one, and the receiver is not a slot.
    #[rstest]
    #[case::grouped("func f(a, b int) {}", 2)]
    #[case::unnamed("func f(int, string) {}", 2)]
    #[case::variadic("func f(xs ...int) {}", 1)]
    #[case::receiver_not_counted("func (s *S) f(x int) {}", 1)]
    #[case::niladic("func f() {}", 0)]
    fn shapes_carry_parameter_slot_counts(#[case] decl: &str, #[case] expected: usize) {
        let funcs = shapes(&format!("package p\n{decl}\n"), "m");
        assert_eq!(
            funcs[0]
                .signature_shape()
                .map(SignatureShape::parameter_count),
            Some(expected),
        );
    }

    #[test]
    fn test_named_functions_are_marked_is_test() {
        let src = "package p\n\nimport \"testing\"\n\nfunc TestThing(t *testing.T) {}\nfunc helper() {}\n";
        let funcs = shapes(src, "m");
        let flags: Vec<_> = funcs
            .iter()
            .map(|f| (f.display_name.clone(), f.is_test))
            .collect();
        assert_eq!(
            flags,
            [("TestThing".to_owned(), true), ("helper".to_owned(), false)]
        );
    }

    /// Lines are 1-based. tree-sitter reports 0-based row numbers, so
    /// the `+ 1` is what converts them; a mutant that swaps `+ 1` for
    /// `* 1` or `- 1` would collapse line 1 to line 0 and shift every
    /// later line by 2. Pin start and end of a function so those
    /// mutations surface.
    #[test]
    fn function_shape_records_one_based_line_numbers() {
        let funcs = shapes("package p\nfunc f() {\n}\n", "m");
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].span.start_line, 2, "starts on physical line 2");
        assert_eq!(funcs[0].span.end_line, 3, "ends on physical line 3");
    }

    /// Same logic for call lines: the call site sits inside a function
    /// body, so a `+ 1` swap to `- 1` or `* 1` collapses or shifts the
    /// reported line.
    #[test]
    fn call_shape_records_one_based_line_numbers() {
        let calls = calls("package p\nfunc f() {\n  helper()\n}\n", "m");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].line, 3, "call is on physical line 3");
    }

    /// Parenthesised member access (`(api).create()`) keeps its inner
    /// segment chain. Without the `parenthesized_expression` arm in
    /// `expression_path`, the path collapses to just `create`.
    #[test]
    fn parenthesised_object_in_member_call_keeps_full_path() {
        let call = &calls("package p\nfunc caller() { (api).create() }\n", "m")[0];
        assert_eq!(call.callee_name(), Some("create"));
        assert_eq!(call.callee_path().as_deref(), Some("api::create"));
    }

    /// Imports written as a parenthesised list (`import (...)`) must be
    /// flattened by walking the `import_spec_list` arm. Without that
    /// arm, the alias map is empty and `foo.Run()` falls back to a
    /// receiver call instead of a path call.
    #[test]
    fn block_imports_register_namespace_aliases() {
        let src = concat!(
            "package p\n\n",
            "import (\n    \"github.com/x/foo\"\n)\n\n",
            "func caller() { foo.Run() }\n",
        );
        let call = &calls(src, "m")[0];
        assert_eq!(call.callee_path().as_deref(), Some("foo::Run"));
        assert!(
            !call.has_receiver_expression(),
            "block-imported package alias should be a path call",
        );
    }

    /// A callee bound to a closure or a function-typed parameter in the
    /// caller's own scope is shadowed: the resolver must be told so it
    /// does not fall back to a same-named package-level function.
    #[rstest]
    #[case::short_var_closure("func caller() {\n  emit := func(x int) {}\n  emit(1)\n}\n", true)]
    #[case::var_closure("func caller() {\n  var emit = func(x int) {}\n  emit(1)\n}\n", true)]
    #[case::var_function_type("func caller() {\n  var emit func(int)\n  emit(1)\n}\n", true)]
    #[case::assignment_closure(
        "func caller() {\n  var emit func(int)\n  emit = func(x int) {}\n  emit(1)\n}\n",
        true
    )]
    #[case::function_typed_param("func caller(emit func(int)) {\n  emit(1)\n}\n", true)]
    #[case::binding_in_nested_block(
        "func caller() {\n  if true {\n    emit := func(x int) {}\n    emit(1)\n  }\n}\n",
        true
    )]
    #[case::plain_local("func caller() {\n  emit := compute()\n  emit(1)\n}\n", false)]
    #[case::value_typed_param("func caller(emit int) {\n  emit(1)\n}\n", false)]
    #[case::unbound_name("func caller() {\n  emit(1)\n}\n", false)]
    fn local_callable_bindings_shadow_bare_calls(#[case] body: &str, #[case] expected: bool) {
        let calls = calls(&format!("package p\n\n{body}"), "m");
        let call = calls
            .iter()
            .find(|call| call.callee_name() == Some("emit"))
            .expect("emit call site");
        assert_eq!(call.callee_is_locally_bound(), expected);
    }

    /// Only the shadowed name is affected: other calls in the same body
    /// keep resolving normally.
    #[test]
    fn other_calls_in_a_shadowing_function_are_untouched() {
        let src = concat!(
            "package p\n\n",
            "func caller() {\n",
            "  emit := func(x int) {}\n",
            "  emit(1)\n",
            "  helper()\n",
            "}\n",
        );
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

    /// A local `emit` does not shadow `pkg.emit()` or `obj.emit()` —
    /// those are anchored by their prefix / receiver.
    #[rstest]
    #[case::namespace_path("helper.emit()")]
    #[case::receiver_call("obj.emit()")]
    fn prefixed_calls_are_not_shadowed_by_a_local_binding(#[case] call_expr: &str) {
        let src = format!(
            concat!(
                "package p\n\n",
                "import \"github.com/x/proj/helper\"\n\n",
                "func caller(obj *Thing) {{\n",
                "  emit := func(x int) {{}}\n",
                "  _ = emit\n",
                "  {call_expr}\n",
                "}}\n",
            ),
            call_expr = call_expr,
        );
        let call = calls(&src, "m")
            .into_iter()
            .find(|call| {
                call.callee_name() == Some("emit")
                    && call.callee_path().is_some_and(|p| p.contains("::"))
            })
            .expect("prefixed emit call site");
        assert!(!call.callee_is_locally_bound());
    }

    /// Calls inside a method body must be extracted with the receiver
    /// type as the caller's `caller_owner`. Without the
    /// `method_declaration` match arm, the call would be dropped
    /// entirely and the resolver would lose the link from `Caller` to
    /// `helper`.
    #[test]
    fn method_bodies_record_calls_qualified_to_their_receiver() {
        let src = concat!(
            "package p\n\n",
            "type Service struct{}\n\n",
            "func (s *Service) Caller() int { return helper() }\n",
            "func helper() int { return 1 }\n",
        );
        let calls = calls(src, "m");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].caller_qualified_name(), Some("m::Service::Caller"));
        assert_eq!(calls[0].caller_owner(), Some("Service"));
        assert_eq!(calls[0].callee_name(), Some("helper"));
    }
}
