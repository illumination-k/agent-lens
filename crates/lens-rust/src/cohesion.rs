//! syn-based cohesion extraction for Rust source files.
//!
//! For every `impl` block in the file (including those nested inside inline
//! modules) we collect each instance method's referenced `self.<field>`
//! accesses and its `self.<sibling>(...)` / `Self::<sibling>(...)` calls.
//! Static / associated functions (no `self` receiver) are ignored: they have
//! no fields to share, so including them would inflate LCOM4 with isolated
//! components that don't reflect a real cohesion problem.
//!
//! In addition to `impl`-block units we emit a **module-level** unit per
//! scope (the file body, plus each inline `mod foo { ... }` block): top-level
//! free `fn` items act as the methods, top-level `static` / `const` items
//! act as the shared fields, and direct sibling calls / shared field access
//! form edges. Locally-bound names inside a function (parameters, `let`
//! bindings, closure params, `for` / `match` / `if let` / `while let`
//! patterns) shadow module names, so a `let counter = 0;` inside a function
//! is not counted as a reference to the module-level `static COUNTER`.
//! `#[test]`-tagged free functions and `#[cfg(test)]`-gated modules are
//! filtered to keep test scaffolding out of production cohesion reports.
//!
//! Calls are matched verbatim against the names of *sibling* methods on the
//! same impl block (or sibling free functions in the same module). Calls to
//! anything else are dropped on the floor — the cohesion graph only ever
//! sees in-unit edges.

use std::collections::HashSet;

use lens_domain::{CohesionUnit, CohesionUnitKind, MethodCohesion};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{
    Arm, Expr, ExprCall, ExprClosure, ExprField, ExprForLoop, ExprIf, ExprLet, ExprMethodCall,
    ExprPath, ExprWhile, FnArg, ImplItem, ImplItemFn, Item, ItemConst, ItemFn, ItemImpl, ItemMod,
    ItemStatic, Local, Member, Pat,
};

use crate::attrs::{has_cfg_test, is_test_function};
use crate::common::type_path_last_ident;

/// Failures produced while extracting cohesion units.
#[derive(Debug, thiserror::Error)]
pub enum CohesionError {
    #[error("failed to parse Rust source: {0}")]
    Syn(#[from] syn::Error),
}

/// Placeholder name used for the program-level (file-root) module unit.
/// Inline `mod foo { ... }` blocks use `foo` as the unit name instead, so
/// this constant only ever stands in for the file's outermost scope.
const MODULE_UNIT_NAME: &str = "<module>";

/// Extract one [`CohesionUnit`] per `impl` block in `source` plus one
/// module-level unit per scope (the file body, and each inline
/// `mod foo { ... }` block) that has at least one production free
/// function.
///
/// Empty `impl` blocks (no instance methods) are skipped; they have no
/// cohesion to measure and would only add noise to a report. Likewise
/// empty modules and `#[cfg(test)]` modules are skipped.
pub fn extract_cohesion_units(source: &str) -> Result<Vec<CohesionUnit>, CohesionError> {
    let file = syn::parse_file(source)?;
    let mut out = Vec::new();
    collect_scope(&file.items, MODULE_UNIT_NAME, &mut out);
    Ok(out)
}

/// Process one lexical scope: emit `impl` units (recursively descending
/// into inline modules), recurse into nested modules as their own
/// scopes, and finally emit a module-level unit summarising this
/// scope's free functions / fields.
fn collect_scope(items: &[Item], scope_name: &str, out: &mut Vec<CohesionUnit>) {
    for item in items {
        collect_item(item, out);
    }
    if let Some(unit) = build_module_unit(items, scope_name) {
        out.push(unit);
    }
}

fn collect_item(item: &Item, out: &mut Vec<CohesionUnit>) {
    match item {
        Item::Impl(item_impl) => {
            if let Some(unit) = unit_from_impl(item_impl) {
                out.push(unit);
            }
        }
        Item::Mod(item_mod) => {
            // `#[cfg(test)]` modules are test scaffolding; their cohesion
            // is dictated by the test harness, not by a real production
            // responsibility split.
            if has_cfg_test(&item_mod.attrs) {
                return;
            }
            if let Some((_, items)) = &item_mod.content {
                collect_scope(items, &item_mod.ident.to_string(), out);
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

// ---------- Module-level extraction ----------

/// Build the module unit for a single scope, if any. Returns `None`
/// when the scope has no production free function — pure-impl files,
/// pure-`use` files, and `#[test]`-only scopes don't get a unit.
fn build_module_unit(items: &[Item], scope_name: &str) -> Option<CohesionUnit> {
    let functions: Vec<&ItemFn> = items
        .iter()
        .filter_map(|i| match i {
            Item::Fn(f) if !is_test_function(&f.attrs) => Some(f),
            _ => None,
        })
        .collect();
    if functions.is_empty() {
        return None;
    }

    let module_fields = collect_module_fields(items);
    let sibling_names: HashSet<String> =
        functions.iter().map(|f| f.sig.ident.to_string()).collect();

    let cohesions: Vec<MethodCohesion> = functions
        .iter()
        .map(|f| module_function_cohesion(f, &module_fields, &sibling_names))
        .collect();

    let (start_line, end_line) = scope_line_range(items);
    Some(CohesionUnit::build(
        CohesionUnitKind::Module,
        scope_name,
        start_line,
        end_line,
        cohesions,
    ))
}

fn scope_line_range(items: &[Item]) -> (usize, usize) {
    let Some(first) = items.first() else {
        return (1, 1);
    };
    let last = items.last().unwrap_or(first);
    (first.span().start().line, last.span().end().line)
}

/// Names of every top-level binding that participates in cohesion as a
/// "field": `static` and `const` items. `use` imports, type aliases,
/// and nested `fn` / `mod` / `impl` declarations are excluded — they
/// are not shared mutable / read state, they're sibling units of their
/// own (or scoped names).
fn collect_module_fields(items: &[Item]) -> HashSet<String> {
    let mut fields = HashSet::new();
    for item in items {
        match item {
            Item::Static(s) => {
                fields.insert(s.ident.to_string());
            }
            Item::Const(c) => {
                fields.insert(c.ident.to_string());
            }
            _ => {}
        }
    }
    fields
}

fn module_function_cohesion(
    func: &ItemFn,
    module_fields: &HashSet<String>,
    siblings: &HashSet<String>,
) -> MethodCohesion {
    let locals = collect_local_names(func);
    let mut visitor = ModuleRefVisitor {
        module_fields,
        siblings,
        locals: &locals,
        fields: Vec::new(),
        calls: Vec::new(),
    };
    visitor.visit_block(&func.block);

    let mut fields = visitor.fields;
    fields.sort();
    fields.dedup();
    let mut calls = visitor.calls;
    calls.sort();
    calls.dedup();

    MethodCohesion::new(
        func.sig.ident.to_string(),
        func.sig.span().start().line,
        func.block.span().end().line,
        fields,
        calls,
    )
}

/// Function-local bindings that may shadow module-level names:
/// parameters, `let` bindings, closure parameters, `for` / `match` /
/// `if let` / `while let` patterns, and nested `fn` / `const` / `static`
/// items. We *do* descend into closures (they share the enclosing
/// scope and capture from it) but we do *not* descend into nested
/// `fn` / `impl` / `mod` items — those have their own scopes.
fn collect_local_names(func: &ItemFn) -> HashSet<String> {
    let mut locals = HashSet::new();
    for arg in &func.sig.inputs {
        if let FnArg::Typed(pat) = arg {
            collect_pat_names(&pat.pat, &mut locals);
        }
    }
    let mut walker = LocalWalker {
        locals: &mut locals,
    };
    walker.visit_block(&func.block);
    locals
}

/// Recursively pull every plain `Pat::Ident` out of a pattern (`x`,
/// `(a, b)`, `Foo { x, y }`, `Some(x)`, `&x`, etc.). Reference / mut
/// modifiers are unwrapped; literals, ranges and wildcards introduce
/// no bindings.
fn collect_pat_names(pat: &Pat, out: &mut HashSet<String>) {
    match pat {
        Pat::Ident(p) => {
            out.insert(p.ident.to_string());
            if let Some((_, sub)) = &p.subpat {
                collect_pat_names(sub, out);
            }
        }
        Pat::Tuple(t) => {
            for elem in &t.elems {
                collect_pat_names(elem, out);
            }
        }
        Pat::TupleStruct(t) => {
            for elem in &t.elems {
                collect_pat_names(elem, out);
            }
        }
        Pat::Struct(s) => {
            for field in &s.fields {
                collect_pat_names(&field.pat, out);
            }
        }
        Pat::Slice(s) => {
            for elem in &s.elems {
                collect_pat_names(elem, out);
            }
        }
        Pat::Or(o) => {
            for case in &o.cases {
                collect_pat_names(case, out);
            }
        }
        Pat::Reference(r) => collect_pat_names(&r.pat, out),
        Pat::Paren(p) => collect_pat_names(&p.pat, out),
        Pat::Type(t) => collect_pat_names(&t.pat, out),
        _ => {}
    }
}

struct LocalWalker<'a> {
    locals: &'a mut HashSet<String>,
}

impl<'ast> Visit<'ast> for LocalWalker<'_> {
    fn visit_local(&mut self, local: &'ast Local) {
        collect_pat_names(&local.pat, self.locals);
        if let Some(init) = &local.init {
            self.visit_expr(&init.expr);
            if let Some((_, diverge)) = &init.diverge {
                self.visit_expr(diverge);
            }
        }
    }

    fn visit_expr_for_loop(&mut self, e: &'ast ExprForLoop) {
        collect_pat_names(&e.pat, self.locals);
        self.visit_expr(&e.expr);
        self.visit_block(&e.body);
    }

    fn visit_expr_let(&mut self, e: &'ast ExprLet) {
        collect_pat_names(&e.pat, self.locals);
        self.visit_expr(&e.expr);
    }

    fn visit_expr_if(&mut self, e: &'ast ExprIf) {
        // `if let Pat = expr { ... } else { ... }` — the let-pattern
        // binds in the consequent block, which `visit_expr_let`
        // already covers via the default walker. Just delegate.
        syn::visit::visit_expr_if(self, e);
    }

    fn visit_expr_while(&mut self, e: &'ast ExprWhile) {
        syn::visit::visit_expr_while(self, e);
    }

    fn visit_arm(&mut self, arm: &'ast Arm) {
        collect_pat_names(&arm.pat, self.locals);
        if let Some((_, guard)) = &arm.guard {
            self.visit_expr(guard);
        }
        self.visit_expr(&arm.body);
    }

    fn visit_expr_closure(&mut self, c: &'ast ExprClosure) {
        for p in &c.inputs {
            collect_pat_names(p, self.locals);
        }
        self.visit_expr(&c.body);
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        // Nested fn declares its name in the enclosing scope; descend
        // no further — the body is its own scope.
        self.locals.insert(item.sig.ident.to_string());
    }

    fn visit_item_const(&mut self, item: &'ast ItemConst) {
        self.locals.insert(item.ident.to_string());
    }

    fn visit_item_static(&mut self, item: &'ast ItemStatic) {
        self.locals.insert(item.ident.to_string());
    }

    fn visit_item_impl(&mut self, _: &'ast ItemImpl) {
        // Nested impl bodies are their own scopes; don't descend.
    }

    fn visit_item_mod(&mut self, _: &'ast ItemMod) {
        // Likewise for nested modules.
    }
}

/// Visitor that records (a) free identifier references that resolve to
/// a module-level field and (b) calls to sibling free functions, in
/// both cases skipping names shadowed by a function-local binding.
/// Mirrors [`SelfRefVisitor`] but tracks free names instead of `self.x`.
struct ModuleRefVisitor<'a> {
    module_fields: &'a HashSet<String>,
    siblings: &'a HashSet<String>,
    locals: &'a HashSet<String>,
    fields: Vec<String>,
    calls: Vec<String>,
}

impl<'ast> Visit<'ast> for ModuleRefVisitor<'_> {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        // Sibling call: callee is a single-segment unqualified path.
        // Record and skip recursing into the callee itself so the
        // path doesn't *also* fire as a field reference.
        if let Expr::Path(ep) = &*call.func
            && let Some(name) = single_segment_path(ep)
            && !self.locals.contains(&name)
            && self.siblings.contains(&name)
        {
            self.calls.push(name);
        } else {
            self.visit_expr(&call.func);
        }
        for arg in &call.args {
            self.visit_expr(arg);
        }
    }

    fn visit_expr_path(&mut self, ep: &'ast ExprPath) {
        if let Some(name) = single_segment_path(ep)
            && !self.locals.contains(&name)
            && self.module_fields.contains(&name)
        {
            self.fields.push(name);
        }
    }

    // Skip nested items: their scopes are independent.
    fn visit_item_fn(&mut self, _: &'ast ItemFn) {}
    fn visit_item_mod(&mut self, _: &'ast ItemMod) {}
    fn visit_item_impl(&mut self, _: &'ast ItemImpl) {}

    // Don't double-walk macro contents (and we couldn't do anything
    // useful with them anyway — macros are tokens, not expressions).
    fn visit_macro(&mut self, _: &'ast syn::Macro) {}
}

/// Return the single-segment, unqualified, no-leading-colon name from
/// an `ExprPath`, or `None` if the path is qualified, has multiple
/// segments, or has generic arguments.
fn single_segment_path(ep: &ExprPath) -> Option<String> {
    if ep.qself.is_some() || ep.path.leading_colon.is_some() {
        return None;
    }
    if ep.path.segments.len() != 1 {
        return None;
    }
    let seg = &ep.path.segments[0];
    if !seg.arguments.is_none() {
        return None;
    }
    Some(seg.ident.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn unit(src: &str) -> CohesionUnit {
        let mut units = extract_cohesion_units(src).unwrap();
        units.retain(|u| !matches!(u.kind, CohesionUnitKind::Module));
        assert_eq!(units.len(), 1, "expected exactly one impl block");
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
        assert_eq!(u.components.len(), 1);
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
        assert_eq!(u.components.len(), 2);
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
        assert_eq!(u.components.len(), 1);
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
        assert_eq!(u.components.len(), 1);
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
        assert_eq!(u.components.len(), 1);
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
        assert_eq!(u.components.len(), 2);
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
        assert_eq!(u.components.len(), 1);
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
        assert_eq!(u.components.len(), 2);
    }

    // ---------- Module-level cohesion ----------

    #[test]
    fn module_unit_collapses_when_all_functions_share_a_static() {
        let src = r#"
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn bump() {
    COUNTER.fetch_add(1, Ordering::Relaxed);
}

fn get() -> usize {
    COUNTER.load(Ordering::Relaxed)
}
"#;
        let u = module_unit(src);
        assert!(matches!(u.kind, CohesionUnitKind::Module));
        assert_eq!(u.type_name, "<module>");
        assert_eq!(u.methods.len(), 2);
        assert_eq!(u.components.len(), 1);
    }

    #[test]
    fn module_split_responsibilities_show_multiple_components() {
        let src = r#"
const MAX_COUNT: usize = 100;
const MAX_LOG: usize = 1000;

fn check_count(n: usize) -> bool { n < MAX_COUNT }
fn at_count(n: usize) -> bool { n == MAX_COUNT }
fn check_log(n: usize) -> bool { n < MAX_LOG }
fn at_log(n: usize) -> bool { n == MAX_LOG }
"#;
        let u = module_unit(src);
        assert_eq!(u.components.len(), 2);
        assert_eq!(u.components, vec![vec![0, 1], vec![2, 3]]);
    }

    #[test]
    fn module_sibling_call_forms_an_edge() {
        let src = r#"
fn outer() -> i32 { helper() }
fn helper() -> i32 { 1 }
"#;
        let u = module_unit(src);
        assert_eq!(u.components.len(), 1);
        let outer = u.methods.iter().find(|m| m.name == "outer").unwrap();
        assert_eq!(outer.calls, vec!["helper"]);
    }

    // Each case asserts the lcom4 value of the file's module-level
    // unit. The rationale comment above each case explains *why* the
    // expected value is what it is.
    #[rstest]
    // The `let COUNTER = 99;` rebinds the name, so the reference is
    // NOT to the module-level `COUNTER`. Without shadow-tracking the
    // two functions would spuriously share and collapse.
    #[case::module_local_let_shadows_module_field(
        r#"
const COUNTER: i32 = 0;

fn reads_module() -> i32 { COUNTER }
fn shadowed() -> i32 {
    let COUNTER = 99;
    COUNTER
}
"#,
        2
    )]
    // A parameter with the same name as a module field shadows it for
    // that function, so the reference is local.
    #[case::module_param_shadows_module_field(
        r#"
const TAG: i32 = 0;

fn reader() -> i32 { TAG }
fn shadowed(TAG: i32) -> i32 { TAG }
"#,
        2
    )]
    // `usize::MAX` / `usize::MIN` are multi-segment path expressions,
    // not module-level field references. Two functions that both use
    // them must not collapse on that basis.
    #[case::module_external_calls_do_not_create_edges(
        r#"
fn a() { let _ = usize::MAX; }
fn b() { let _ = usize::MIN; }
"#,
        2
    )]
    // A closure parameter named after a module field shadows it inside
    // the closure body. With over-shadowing (closure params added to
    // outer locals), the reference inside the closure is not counted
    // as a module-level field reference.
    #[case::module_closure_param_shadows_field(
        r#"
const X: i32 = 1;

fn reader() -> i32 { X }
fn shadowed() -> i32 {
    let f = |X: i32| X + 1;
    f(2)
}
"#,
        2
    )]
    // A closure that captures a module-level const without shadowing
    // it should still count the const as a reference of the enclosing
    // function — outer's behaviour depends on it.
    #[case::module_closure_captures_field(
        r#"
const X: i32 = 1;

fn alpha() -> i32 {
    let f = |y: i32| X + y;
    f(2)
}
fn beta() -> i32 { X }
"#,
        1
    )]
    fn module_unit_lcom4(#[case] src: &str, #[case] expected: usize) {
        let u = module_unit(src);
        assert_eq!(u.components.len(), expected);
    }

    #[test]
    fn module_test_functions_are_excluded() {
        // `#[test]` functions are scaffolding; their cohesion is
        // dictated by the test harness, not by a real production
        // responsibility split.
        let src = r#"
const COUNTER: i32 = 0;

fn production() -> i32 { COUNTER }

#[test]
fn smoke() {
    assert_eq!(production(), 0);
}
"#;
        let u = module_unit(src);
        let names: Vec<&str> = u.methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["production"]);
    }

    #[test]
    fn module_cfg_test_module_is_skipped() {
        // The `#[cfg(test)]` mod's cohesion would be entirely about
        // tests; drop the whole subtree.
        let src = r#"
fn production() -> i32 { 0 }

#[cfg(test)]
mod tests {
    const FIXTURE: i32 = 1;
    fn helper() -> i32 { FIXTURE }
    fn user() -> i32 { helper() + FIXTURE }
}
"#;
        let units = extract_cohesion_units(src).unwrap();
        // Only the file-root module unit (single function -> still
        // emitted) should appear, no `tests` module unit.
        let module_units: Vec<&CohesionUnit> = units
            .iter()
            .filter(|u| matches!(u.kind, CohesionUnitKind::Module))
            .collect();
        assert_eq!(module_units.len(), 1);
        assert_eq!(module_units[0].type_name, "<module>");
    }

    #[test]
    fn module_unit_is_skipped_when_no_top_level_functions() {
        // Pure-`impl` files have no module unit.
        let src = r#"
struct Counter { n: i32 }
impl Counter {
    fn inc(&mut self) { self.n += 1; }
    fn get(&self) -> i32 { self.n }
}
"#;
        let units = extract_cohesion_units(src).unwrap();
        assert_eq!(units.len(), 1);
        assert!(matches!(units[0].kind, CohesionUnitKind::Inherent));
    }

    #[test]
    fn nested_mod_gets_its_own_module_unit() {
        // An inline `mod foo { ... }` block is its own scope; sibling
        // functions inside it must collapse via the inner module's
        // own unit, named after the module.
        let src = r#"
mod inner {
    static COUNTER: i32 = 0;
    pub fn bump() { let _ = COUNTER; }
    pub fn get() -> i32 { COUNTER }
}
"#;
        let units = extract_cohesion_units(src).unwrap();
        let module_units: Vec<&CohesionUnit> = units
            .iter()
            .filter(|u| matches!(u.kind, CohesionUnitKind::Module))
            .collect();
        assert_eq!(module_units.len(), 1);
        assert_eq!(module_units[0].type_name, "inner");
        assert_eq!(module_units[0].components.len(), 1);
    }

    #[test]
    fn module_nested_fn_is_not_a_separate_unit_or_field_reference() {
        // The nested `inner` is a local binding inside `outer`; it
        // must NOT register as a sibling and must NOT be picked up as
        // its own sibling fn. Without this guard, `outer` would record
        // a reference to `helper` via its nested call, polluting the
        // graph.
        let src = r#"
fn outer() -> i32 {
    fn inner() -> i32 { helper() }
    inner()
}

fn helper() -> i32 { 1 }
"#;
        let u = module_unit(src);
        let outer = u.methods.iter().find(|m| m.name == "outer").unwrap();
        // outer's body contains `fn inner` (skipped) and `inner()` —
        // `inner` is a local, `helper` is referenced only inside
        // `inner`'s body which we don't descend into.
        assert!(
            outer.calls.is_empty(),
            "outer.calls should be empty, got {:?}",
            outer.calls,
        );
    }
}
