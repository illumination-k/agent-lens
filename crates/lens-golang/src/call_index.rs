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
    OwnerShape, ReceiverExprKind, SourceSpan, SyntaxFact, VisibilityShape,
};
use tree_sitter::Node;

use crate::attrs::name_looks_like_test_function;
use crate::parser::{
    GoParseError, function_name_text, method_receiver_type, parse_tree, unquote_go_string_literal,
};

/// Extract neutral function-shape facts for Go.
pub fn extract_function_shapes_with_module(
    source: &str,
    module: &str,
) -> Result<Vec<FunctionShape>, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut cursor = tree.root_node().walk();
    for child in tree.root_node().named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(shape) = function_shape(child, bytes, module, None) {
                    out.push(shape);
                }
            }
            "method_declaration" => {
                let owner = method_receiver_type(child, bytes);
                if let Some(shape) = function_shape(child, bytes, module, owner.as_deref()) {
                    out.push(shape);
                }
            }
            _ => {}
        }
    }
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
    let mut cursor = tree.root_node().walk();
    for child in tree.root_node().named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" => collect_calls_in_function(
                child,
                bytes,
                module,
                None,
                &imports,
                &namespace_aliases,
                &mut out,
            ),
            "method_declaration" => {
                let owner = method_receiver_type(child, bytes);
                collect_calls_in_function(
                    child,
                    bytes,
                    module,
                    owner.as_deref(),
                    &imports,
                    &namespace_aliases,
                    &mut out,
                );
            }
            _ => {}
        }
    }
    Ok(out)
}

fn function_shape(
    node: Node<'_>,
    source: &[u8],
    module: &str,
    owner: Option<&str>,
) -> Option<FunctionShape> {
    let body = node.child_by_field_name("body")?;
    let raw_name = function_name_text(node, source)?;
    let display_name = raw_name.to_owned();
    let qualified = match owner {
        Some(class) => qualify(module, &format!("{class}::{display_name}")),
        None => qualify(module, &display_name),
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
    Some(FunctionShape {
        display_name,
        qualified_name: SyntaxFact::Known(qualified),
        module_path: SyntaxFact::Known(module.to_owned()),
        owner: SyntaxFact::Known(owner_shape),
        visibility: SyntaxFact::Known(visibility),
        signature: SyntaxFact::Unknown,
        body: BodyShape {
            tree: crate::parser::function_body_tree(body, source),
        },
        span,
        is_test,
    })
}

fn collect_calls_in_function(
    node: Node<'_>,
    source: &[u8],
    module: &str,
    owner: Option<&str>,
    imports: &[ImportShape],
    namespace_aliases: &HashSet<String>,
    out: &mut Vec<CallShape>,
) {
    let Some(body) = node.child_by_field_name("body") else {
        return;
    };
    let Some(raw_name) = function_name_text(node, source) else {
        return;
    };
    let caller_qualified = match owner {
        Some(class) => qualify(module, &format!("{class}::{raw_name}")),
        None => qualify(module, raw_name),
    };
    let ctx = CallContext {
        source,
        module,
        caller_qualified_name: &caller_qualified,
        caller_owner: owner.map(ToOwned::to_owned),
        imports,
        namespace_aliases,
    };
    visit_calls(body, &ctx, out);
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
}

fn visit_calls(node: Node<'_>, ctx: &CallContext<'_>, out: &mut Vec<CallShape>) {
    if node.kind() == "call_expression"
        && let Some(callee) = node.child_by_field_name("function")
    {
        let facts = callee_facts(callee, ctx.source, ctx.namespace_aliases);
        out.push(CallShape {
            caller_qualified_name: SyntaxFact::Known(Some(ctx.caller_qualified_name.to_owned())),
            caller_module: SyntaxFact::Known(ctx.module.to_owned()),
            caller_owner: SyntaxFact::Known(ctx.caller_owner.clone()),
            callee_display_name: SyntaxFact::Known(facts.name),
            callee_path_segments: facts
                .path_segments
                .map_or(SyntaxFact::Unknown, SyntaxFact::Known),
            receiver_expr_kind: SyntaxFact::Known(facts.receiver),
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
            let name = callee.utf8_text(source).unwrap_or("").to_owned();
            CalleeFacts {
                name: Some(name.clone()),
                path_segments: Some(vec![name]),
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
            let field_name = field.utf8_text(source).unwrap_or("").to_owned();
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
            Some(vec![node.utf8_text(source).ok()?.to_owned()])
        }
        "selector_expression" => {
            let operand = node.child_by_field_name("operand")?;
            let field = node.child_by_field_name("field")?;
            let mut segments = expression_path(operand, source)?;
            segments.push(field.utf8_text(source).ok()?.to_owned());
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
    let Ok(raw_path) = path_node.utf8_text(source) else {
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
            _ => name
                .utf8_text(source)
                .ok()
                .map(str::to_owned)
                .or(default_alias),
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
