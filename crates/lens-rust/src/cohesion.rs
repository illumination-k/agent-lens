//! syn-based cohesion extraction for Rust source files.
//!
//! For every `impl` block in the file (including those nested inside inline
//! modules) we collect each instance method's referenced `self.<field>`
//! accesses and its `self.<sibling>(...)` / `Self::<sibling>(...)` calls.
//! Static / associated functions (no `self` receiver) are ignored: they have
//! no fields to share, so including them would inflate LCOM4 with isolated
//! components that don't reflect a real cohesion problem.
//!
//! Calls are matched verbatim against the names of *sibling* methods on the
//! same impl block. Calls to anything else are dropped on the floor — the
//! cohesion graph only ever sees in-unit edges.

use lens_domain::{CohesionUnit, CohesionUnitKind, MethodCohesion};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{
    Expr, ExprCall, ExprField, ExprMethodCall, ExprPath, FnArg, ImplItem, ImplItemFn, Item,
    ItemImpl, Member,
};

/// Failures produced while extracting cohesion units.
#[derive(Debug)]
pub enum CohesionError {
    Syn(syn::Error),
}

impl std::fmt::Display for CohesionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Syn(e) => write!(f, "failed to parse Rust source: {e}"),
        }
    }
}

impl std::error::Error for CohesionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Syn(e) => Some(e),
        }
    }
}

impl From<syn::Error> for CohesionError {
    fn from(value: syn::Error) -> Self {
        Self::Syn(value)
    }
}

/// Extract one [`CohesionUnit`] per `impl` block in `source`.
///
/// Empty `impl` blocks (no instance methods) are skipped; they have no
/// cohesion to measure and would only add noise to a report.
pub fn extract_cohesion_units(source: &str) -> Result<Vec<CohesionUnit>, CohesionError> {
    let file = syn::parse_file(source)?;
    let mut out = Vec::new();
    for item in &file.items {
        collect_item(item, &mut out);
    }
    Ok(out)
}

fn collect_item(item: &Item, out: &mut Vec<CohesionUnit>) {
    match item {
        Item::Impl(item_impl) => {
            if let Some(unit) = unit_from_impl(item_impl) {
                out.push(unit);
            }
        }
        Item::Mod(item_mod) => {
            if let Some((_, items)) = &item_mod.content {
                for nested in items {
                    collect_item(nested, out);
                }
            }
        }
        _ => {}
    }
}

fn unit_from_impl(item_impl: &ItemImpl) -> Option<CohesionUnit> {
    let type_name = type_path_last_ident(&item_impl.self_ty)?;
    let kind = match &item_impl.trait_ {
        Some((_, path, _)) => CohesionUnitKind::Trait {
            trait_name: path
                .segments
                .last()
                .map(|s| s.ident.to_string())
                .unwrap_or_default(),
        },
        None => CohesionUnitKind::Inherent,
    };

    let instance_methods: Vec<&ImplItemFn> = item_impl
        .items
        .iter()
        .filter_map(|i| match i {
            ImplItem::Fn(f) if has_self_receiver(f) => Some(f),
            _ => None,
        })
        .collect();
    if instance_methods.is_empty() {
        return None;
    }

    let sibling_names: std::collections::HashSet<String> = instance_methods
        .iter()
        .map(|m| m.sig.ident.to_string())
        .collect();

    let methods: Vec<MethodCohesion> = instance_methods
        .iter()
        .map(|m| method_cohesion(m, &sibling_names))
        .collect();

    let start_line = item_impl.span().start().line;
    let end_line = item_impl.span().end().line;

    Some(CohesionUnit::build(
        kind, type_name, start_line, end_line, methods,
    ))
}

fn method_cohesion(
    method: &ImplItemFn,
    siblings: &std::collections::HashSet<String>,
) -> MethodCohesion {
    let mut visitor = SelfRefVisitor::default();
    visitor.visit_block(&method.block);

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

    MethodCohesion::new(
        method.sig.ident.to_string(),
        method.sig.span().start().line,
        method.block.span().end().line,
        fields,
        calls,
    )
}

fn has_self_receiver(method: &ImplItemFn) -> bool {
    matches!(method.sig.inputs.first(), Some(FnArg::Receiver(_)))
}

fn type_path_last_ident(ty: &syn::Type) -> Option<String> {
    if let syn::Type::Path(type_path) = ty {
        type_path
            .path
            .segments
            .last()
            .map(|seg| seg.ident.to_string())
    } else {
        None
    }
}

#[derive(Default)]
struct SelfRefVisitor {
    fields: Vec<String>,
    calls: Vec<String>,
}

impl<'ast> Visit<'ast> for SelfRefVisitor {
    fn visit_expr_field(&mut self, ef: &'ast ExprField) {
        if is_self_expr(&ef.base) {
            // Tuple-struct fields (`self.0`) participate in cohesion just
            // like named fields, so include them under their numeric name.
            let name = match &ef.member {
                Member::Named(id) => id.to_string(),
                Member::Unnamed(idx) => idx.index.to_string(),
            };
            self.fields.push(name);
        }
        syn::visit::visit_expr_field(self, ef);
    }

    fn visit_expr_method_call(&mut self, mc: &'ast ExprMethodCall) {
        if is_self_expr(&mc.receiver) {
            self.calls.push(mc.method.to_string());
        }
        syn::visit::visit_expr_method_call(self, mc);
    }

    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if let Expr::Path(ExprPath {
            qself: None, path, ..
        }) = &*call.func
            && path.leading_colon.is_none()
            && path.segments.len() == 2
            && path.segments[0].ident == "Self"
        {
            self.calls.push(path.segments[1].ident.to_string());
        }
        syn::visit::visit_expr_call(self, call);
    }
}

fn is_self_expr(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Path(ExprPath { qself: None, path, .. })
            if path.leading_colon.is_none()
                && path.segments.len() == 1
                && path.segments[0].ident == "self"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(src: &str) -> CohesionUnit {
        let mut units = extract_cohesion_units(src).unwrap();
        assert_eq!(units.len(), 1, "expected exactly one impl block");
        units.remove(0)
    }

    #[test]
    fn cohesive_impl_collapses_to_one_component() {
        let src = r#"
struct Counter { n: i32 }
impl Counter {
    fn inc(&mut self) { self.n += 1; }
    fn get(&self) -> i32 { self.n }
}
"#;
        let u = unit(src);
        assert_eq!(u.type_name, "Counter");
        assert_eq!(u.lcom4(), 1);
        assert_eq!(u.methods.len(), 2);
    }

    #[test]
    fn split_responsibilities_show_multiple_components() {
        let src = r#"
struct Thing {
    counter: i32,
    log: String,
}
impl Thing {
    fn bump(&mut self) { self.counter += 1; }
    fn current(&self) -> i32 { self.counter }
    fn record(&mut self, s: &str) { self.log.push_str(s); }
    fn dump(&self) -> &str { &self.log }
}
"#;
        let u = unit(src);
        assert_eq!(u.lcom4(), 2);
        // Components are sorted by smallest index, so the counter pair
        // (indices 0, 1) lands first.
        assert_eq!(u.components, vec![vec![0, 1], vec![2, 3]]);
    }

    #[test]
    fn calls_via_self_method_form_an_edge() {
        let src = r#"
struct Foo;
impl Foo {
    fn outer(&self) { self.helper(); }
    fn helper(&self) {}
}
"#;
        let u = unit(src);
        assert_eq!(u.lcom4(), 1);
        let outer = &u.methods[0];
        assert_eq!(outer.calls, vec!["helper"]);
    }

    #[test]
    fn calls_via_self_type_form_an_edge() {
        let src = r#"
struct Foo;
impl Foo {
    fn outer(&self) { Self::helper(self); }
    fn helper(&self) {}
}
"#;
        let u = unit(src);
        assert_eq!(u.lcom4(), 1);
    }

    #[test]
    fn associated_functions_are_excluded() {
        let src = r#"
struct Foo { n: i32 }
impl Foo {
    fn new() -> Self { Self { n: 0 } }
    fn get(&self) -> i32 { self.n }
    fn set(&mut self, n: i32) { self.n = n; }
}
"#;
        let u = unit(src);
        // `new` has no self receiver and is dropped; only the two instance
        // methods participate, sharing the field `n`.
        assert_eq!(u.methods.len(), 2);
        assert_eq!(u.lcom4(), 1);
    }

    #[test]
    fn external_calls_do_not_create_edges() {
        let src = r#"
struct Foo;
impl Foo {
    fn a(&self) { other_function(); }
    fn b(&self) { another(); }
}
fn other_function() {}
fn another() {}
"#;
        let u = unit(src);
        // No shared fields, no in-unit calls — the two methods stay
        // isolated.
        assert_eq!(u.lcom4(), 2);
    }

    #[test]
    fn trait_impl_kind_is_recorded() {
        let src = r#"
struct Foo;
trait Greet { fn hi(&self); }
impl Greet for Foo {
    fn hi(&self) {}
}
"#;
        let units = extract_cohesion_units(src).unwrap();
        let kinds: Vec<&CohesionUnitKind> = units.iter().map(|u| &u.kind).collect();
        assert!(matches!(
            kinds.as_slice(),
            [CohesionUnitKind::Trait { trait_name }] if trait_name == "Greet"
        ));
    }

    #[test]
    fn nested_module_impls_are_picked_up() {
        let src = r#"
mod inner {
    pub struct Foo { n: i32 }
    impl Foo {
        pub fn get(&self) -> i32 { self.n }
        pub fn set(&mut self, n: i32) { self.n = n; }
    }
}
"#;
        let units = extract_cohesion_units(src).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].type_name, "Foo");
    }

    #[test]
    fn impls_with_no_instance_methods_are_skipped() {
        let src = r#"
struct Foo;
impl Foo {
    fn new() -> Self { Foo }
    const C: i32 = 1;
}
"#;
        let units = extract_cohesion_units(src).unwrap();
        assert!(units.is_empty());
    }

    #[test]
    fn tuple_struct_fields_are_counted_for_lcom4() {
        let src = r#"
struct ModulePath(String);
impl ModulePath {
    fn as_str(&self) -> &str { &self.0 }
    fn len(&self) -> usize { self.0.len() }
    fn first_byte(&self) -> Option<u8> { self.0.bytes().next() }
}
"#;
        let u = unit(src);
        // All three methods read `self.0`, so they share a single field and
        // collapse to one component instead of being mistakenly reported as
        // three disjoint responsibilities (the bug: `Member::Unnamed` was
        // ignored, so tuple-struct impls had `LCOM4 = method_count`).
        assert_eq!(u.lcom4(), 1);
        assert_eq!(u.methods.len(), 3);
        for m in &u.methods {
            assert_eq!(
                m.fields,
                vec!["0"],
                "method {} should reference self.0",
                m.name
            );
        }
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = extract_cohesion_units("fn ??? {").unwrap_err();
        assert!(matches!(err, CohesionError::Syn(_)));
    }

    #[test]
    fn cohesion_error_display_includes_inner_message() {
        let parse_err = syn::parse_str::<syn::Expr>("fn???").unwrap_err();
        let err = CohesionError::Syn(parse_err);
        let msg = err.to_string();
        assert!(msg.contains("failed to parse Rust source"), "got {msg}");
    }

    #[test]
    fn cohesion_error_source_is_the_underlying_syn_error() {
        use std::error::Error as _;
        let parse_err = syn::parse_str::<syn::Expr>("fn???").unwrap_err();
        let err = CohesionError::Syn(parse_err);
        assert!(err.source().is_some());
    }

    #[test]
    fn field_access_on_non_self_receiver_is_not_counted_as_self_field() {
        // `is_self_expr` distinguishes `self.x` from `other.x`. A method
        // that only touches another struct's fields must produce an empty
        // field set; otherwise it would spuriously share state with any
        // sibling that uses a same-named field.
        let src = r#"
struct Other { tag: i32 }
struct Foo { tag: i32 }
impl Foo {
    fn external(&self, o: &Other) -> i32 { o.tag }
    fn local(&self) -> i32 { self.tag }
}
"#;
        let u = unit(src);
        let external = u.methods.iter().find(|m| m.name == "external").unwrap();
        let local = u.methods.iter().find(|m| m.name == "local").unwrap();
        assert!(
            external.fields.is_empty(),
            "external.fields should be empty, got {:?}",
            external.fields,
        );
        assert_eq!(local.fields, vec!["tag"]);
        // The two methods do not share any self field, so they form
        // separate components.
        assert_eq!(u.lcom4(), 2);
    }
}
