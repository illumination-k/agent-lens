//! syn-based complexity extraction for Rust source files.
//!
//! For every free function, inherent / trait method, and trait default
//! method (including those nested inside inline modules) we walk the body
//! and produce a [`FunctionComplexity`]:
//!
//! * **Cyclomatic Complexity** — McCabe; starts at 1 and is incremented
//!   for each branching construct (`if`, `else if`, `while`, `for`,
//!   `loop`, each `match` arm beyond the first, `&&`/`||`, `?`).
//! * **Cognitive Complexity** — Sonar-style; control structures add
//!   `1 + nesting` so deeply-nested code scores higher than the same
//!   number of flat branches. `&&`/`||` adds `1` per occurrence (the
//!   exact "consecutive sequence" rule from Sonar is approximated).
//! * **Max Nesting Depth** — the deepest control-flow nesting reached in
//!   the function body.
//! * **Halstead counts** — operators and operands are derived from the
//!   token stream of the body; Rust keywords and `Punct` tokens are
//!   treated as operators, identifiers as operands, literals as operands.
//!
//! Closures and items defined *inside* a function body are walked with
//! the same visitor instance, so their branches contribute to the
//! enclosing function's score. That matches how a reader actually
//! experiences the code.

use std::collections::HashMap;

use lens_domain::{FunctionComplexity, HalsteadCounts, qualify};

use crate::common::type_path_last_ident;
use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    BinOp, Block, Expr, ExprBinary, ExprForLoop, ExprIf, ExprLoop, ExprMatch, ExprTry, ExprWhile,
    ImplItem, Item, ItemImpl, ItemTrait, TraitItem,
};

/// Failures produced while extracting complexity units.
#[derive(Debug, thiserror::Error)]
pub enum ComplexityError {
    #[error("failed to parse Rust source: {0}")]
    Syn(#[from] syn::Error),
}

/// Extract one [`FunctionComplexity`] per function-shaped item in `source`.
pub fn extract_complexity_units(source: &str) -> Result<Vec<FunctionComplexity>, ComplexityError> {
    let file = syn::parse_file(source)?;
    let mut out = Vec::new();
    for item in &file.items {
        collect_item(item, &mut out);
    }
    Ok(out)
}

fn collect_item(item: &Item, out: &mut Vec<FunctionComplexity>) {
    match item {
        Item::Fn(item_fn) => {
            out.push(analyze_fn(
                item_fn.sig.ident.to_string(),
                &item_fn.sig,
                &item_fn.block,
            ));
        }
        Item::Impl(item_impl) => collect_impl(item_impl, out),
        Item::Trait(item_trait) => collect_trait(item_trait, out),
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

fn collect_impl(item_impl: &ItemImpl, out: &mut Vec<FunctionComplexity>) {
    let owner = type_path_last_ident(&item_impl.self_ty);
    for impl_item in &item_impl.items {
        if let ImplItem::Fn(method) = impl_item {
            let name = qualify(owner.as_deref(), &method.sig.ident.to_string());
            out.push(analyze_fn(name, &method.sig, &method.block));
        }
    }
}

fn collect_trait(item_trait: &ItemTrait, out: &mut Vec<FunctionComplexity>) {
    let owner = item_trait.ident.to_string();
    for trait_item in &item_trait.items {
        if let TraitItem::Fn(method) = trait_item {
            let Some(block) = &method.default else {
                continue;
            };
            let name = qualify(Some(&owner), &method.sig.ident.to_string());
            out.push(analyze_fn(name, &method.sig, block));
        }
    }
}

fn analyze_fn(name: String, sig: &syn::Signature, block: &Block) -> FunctionComplexity {
    let mut visitor = ComplexityVisitor::new();
    visitor.visit_block(block);
    let halstead = halstead_counts(block);
    FunctionComplexity {
        name,
        start_line: sig.span().start().line,
        end_line: block.span().end().line,
        cyclomatic: 1 + visitor.cyclomatic_branches,
        cognitive: visitor.cognitive,
        max_nesting: visitor.max_nesting,
        halstead,
    }
}

struct ComplexityVisitor {
    cyclomatic_branches: u32,
    cognitive: u32,
    nesting: u32,
    max_nesting: u32,
}

impl ComplexityVisitor {
    fn new() -> Self {
        Self {
            cyclomatic_branches: 0,
            cognitive: 0,
            nesting: 0,
            max_nesting: 0,
        }
    }

    fn enter_nest(&mut self) {
        self.nesting += 1;
        if self.nesting > self.max_nesting {
            self.max_nesting = self.nesting;
        }
    }

    fn exit_nest(&mut self) {
        // Paired with enter_nest; saturating to keep the invariant even if
        // a future visitor change introduces an imbalance.
        self.nesting = self.nesting.saturating_sub(1);
    }
}

impl<'ast> Visit<'ast> for ComplexityVisitor {
    fn visit_expr_if(&mut self, e: &'ast ExprIf) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;

        // Condition contributes its own logical operators but no nesting.
        self.visit_expr(&e.cond);

        self.enter_nest();
        self.visit_block(&e.then_branch);
        self.exit_nest();

        if let Some((_, else_expr)) = &e.else_branch {
            // `else if` is rendered by Sonar as the chained if's own +1
            // (no extra penalty for the bare `else`); a plain `else`
            // counts as +1.
            if !matches!(&**else_expr, Expr::If(_)) {
                self.cognitive += 1;
            }
            self.enter_nest();
            self.visit_expr(else_expr);
            self.exit_nest();
        }
    }

    fn visit_expr_while(&mut self, e: &'ast ExprWhile) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.visit_expr(&e.cond);
        self.enter_nest();
        self.visit_block(&e.body);
        self.exit_nest();
    }

    fn visit_expr_for_loop(&mut self, e: &'ast ExprForLoop) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.visit_expr(&e.expr);
        self.enter_nest();
        self.visit_block(&e.body);
        self.exit_nest();
    }

    fn visit_expr_loop(&mut self, e: &'ast ExprLoop) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
        self.enter_nest();
        self.visit_block(&e.body);
        self.exit_nest();
    }

    fn visit_expr_match(&mut self, e: &'ast ExprMatch) {
        // McCabe: every arm beyond the first introduces a new path.
        let arms = u32::try_from(e.arms.len()).unwrap_or(u32::MAX);
        self.cyclomatic_branches += arms.saturating_sub(1);
        // Sonar: the match itself is one structure, regardless of arm count.
        self.cognitive += 1 + self.nesting;

        self.visit_expr(&e.expr);
        self.enter_nest();
        for arm in &e.arms {
            if let Some((_, guard)) = &arm.guard {
                self.visit_expr(guard);
            }
            self.visit_expr(&arm.body);
        }
        self.exit_nest();
    }

    fn visit_expr_binary(&mut self, e: &'ast ExprBinary) {
        if matches!(e.op, BinOp::And(_) | BinOp::Or(_)) {
            self.cyclomatic_branches += 1;
            self.cognitive += 1;
        }
        // Default traversal would recurse into both sides; do it ourselves
        // since we override the method.
        self.visit_expr(&e.left);
        self.visit_expr(&e.right);
    }

    fn visit_expr_try(&mut self, e: &'ast ExprTry) {
        // `?` is an early return and so adds a path; Sonar does not count
        // it as a structural complexity bump, only McCabe does.
        self.cyclomatic_branches += 1;
        visit::visit_expr_try(self, e);
    }
}

#[derive(Default)]
struct HalsteadAccumulator {
    operators: HashMap<String, usize>,
    operands: HashMap<String, usize>,
}

fn halstead_counts(block: &Block) -> HalsteadCounts {
    let mut acc = HalsteadAccumulator::default();
    walk_tokens(block.to_token_stream(), &mut acc);
    HalsteadCounts {
        distinct_operators: acc.operators.len(),
        distinct_operands: acc.operands.len(),
        total_operators: acc.operators.values().sum(),
        total_operands: acc.operands.values().sum(),
    }
}

fn walk_tokens(stream: TokenStream, acc: &mut HalsteadAccumulator) {
    for tt in stream {
        match tt {
            TokenTree::Group(g) => walk_tokens(g.stream(), acc),
            TokenTree::Ident(ident) => {
                let s = ident.to_string();
                if is_rust_keyword(&s) {
                    *acc.operators.entry(s).or_insert(0) += 1;
                } else {
                    *acc.operands.entry(s).or_insert(0) += 1;
                }
            }
            TokenTree::Punct(p) => {
                let s = p.as_char().to_string();
                *acc.operators.entry(s).or_insert(0) += 1;
            }
            TokenTree::Literal(lit) => {
                *acc.operands.entry(lit.to_string()).or_insert(0) += 1;
            }
        }
    }
}

fn is_rust_keyword(s: &str) -> bool {
    matches!(
        s,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extract(src: &str) -> Vec<FunctionComplexity> {
        extract_complexity_units(src).unwrap()
    }

    fn one(src: &str) -> FunctionComplexity {
        let mut units = extract(src);
        assert_eq!(units.len(), 1, "expected exactly one function");
        units.remove(0)
    }

    #[test]
    fn linear_function_has_cc_one() {
        let f = one("fn noop() { let _ = 1 + 2; }");
        assert_eq!(f.cyclomatic, 1);
        assert_eq!(f.cognitive, 0);
        assert_eq!(f.max_nesting, 0);
    }

    #[test]
    fn single_if_adds_one_to_cyclomatic() {
        let f = one(r#"
fn f(x: i32) -> i32 {
    if x > 0 { 1 } else { 0 }
}
"#);
        // 1 (base) + 1 (if) + 1 (`>` is binary but not && / ||, so no bump)
        assert_eq!(f.cyclomatic, 2);
        // Plain `else` adds +1 to cognitive on top of the +1 for the `if`.
        assert_eq!(f.cognitive, 2);
    }

    #[test]
    fn if_without_else_does_not_pay_else_penalty() {
        let f = one(r#"
fn f(x: i32) -> i32 {
    if x > 0 { return 1; }
    0
}
"#);
        assert_eq!(f.cyclomatic, 2);
        // No else branch ⇒ no extra +1 for the bare-else path.
        assert_eq!(f.cognitive, 1);
    }

    #[test]
    fn match_adds_arms_minus_one_to_cyclomatic() {
        let f = one(r#"
fn f(n: i32) -> i32 {
    match n { 0 => 0, 1 => 1, 2 => 2, _ => 3 }
}
"#);
        // base 1 + (4 arms - 1) = 4
        assert_eq!(f.cyclomatic, 4);
    }

    #[test]
    fn logical_operators_each_add_one() {
        let f = one(r#"
fn f(a: bool, b: bool, c: bool) -> bool { a && b || c }
"#);
        // base 1 + 1 (&&) + 1 (||) = 3
        assert_eq!(f.cyclomatic, 3);
    }

    #[test]
    fn try_operator_adds_one_to_cyclomatic() {
        let f = one(r#"
fn f() -> Result<i32, ()> {
    let x: Result<i32, ()> = Ok(1);
    Ok(x?)
}
"#);
        assert_eq!(f.cyclomatic, 2);
    }

    #[test]
    fn nested_loops_track_max_nesting() {
        let f = one(r#"
fn f() {
    for _ in 0..10 {
        for _ in 0..10 {
            if true {}
        }
    }
}
"#);
        assert_eq!(f.max_nesting, 3);
    }

    #[test]
    fn cognitive_grows_with_nesting() {
        let units = extract(
            r#"
fn flat(n: i32) {
    if n > 0 {}
    if n < 0 {}
}
fn nested(n: i32) {
    if n > 0 {
        if n < 5 {}
    }
}
"#,
        );
        let flat = units.iter().find(|f| f.name == "flat").unwrap();
        let nested = units.iter().find(|f| f.name == "nested").unwrap();
        // Flat: 1 + 1 = 2; Nested: (1 + 0) + (1 + 1) = 3
        assert_eq!(flat.cognitive, 2);
        assert_eq!(nested.cognitive, 3);
    }

    #[test]
    fn else_adds_one_to_cognitive_and_else_if_does_not_double_count() {
        let f = one(r#"
fn f(n: i32) -> i32 {
    if n > 0 { 1 } else if n < 0 { -1 } else { 0 }
}
"#);
        // outer if: +1, inner else-if: +1, trailing else: +1
        // (the else-if's inner if is at nesting=1 -> +(1+1)=2; trailing else +1)
        // Outer if (nest 0): 1 + 0 = 1
        // Else-if (visited inside else branch with nest 1): 1 + 1 = 2
        // Trailing bare else from inner if: +1
        // Total: 1 + 2 + 1 = 4
        assert_eq!(f.cognitive, 4);
    }

    #[test]
    fn impl_methods_get_qualified_names() {
        let units = extract(
            r#"
struct Foo;
impl Foo {
    fn bar(&self) {}
}
"#,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "Foo::bar");
    }

    #[test]
    fn trait_default_methods_are_extracted_with_required_skipped() {
        let units = extract(
            r#"
trait T {
    fn required(&self);
    fn with_default(&self) { let _ = 1; }
}
"#,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "T::with_default");
    }

    #[test]
    fn nested_module_functions_are_picked_up() {
        let units = extract(
            r#"
mod inner {
    fn hidden(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }
}
"#,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "hidden");
        assert_eq!(units[0].cyclomatic, 2);
    }

    #[test]
    fn line_range_covers_signature_through_closing_brace() {
        let f = one("fn f() {\n    let x = 1;\n    let y = 2;\n}\n");
        assert_eq!(f.start_line, 1);
        assert_eq!(f.end_line, 4);
        assert_eq!(f.loc(), 4);
    }

    #[test]
    fn halstead_counts_treat_keywords_as_operators_and_idents_as_operands() {
        let f = one("fn f() { let x = 1; }");
        // operators: `let`, `=`, `;` (and the implicit ones from sig — but
        // we walk the body only). Concrete numbers are sensitive to syn's
        // tokenisation, so just assert the structural invariants.
        assert!(f.halstead.distinct_operators >= 3);
        assert!(f.halstead.distinct_operands >= 2); // `x`, `1`
        assert!(f.halstead.total_operators >= 3);
        assert!(f.halstead.total_operands >= 2);
    }

    #[test]
    fn halstead_volume_is_defined_for_a_realistic_function() {
        let f = one(r#"
fn add(a: i32, b: i32) -> i32 {
    let s = a + b;
    s
}
"#);
        let v = f.halstead_volume();
        assert!(v.is_some(), "expected Volume to be defined");
        // MI also defined and within range.
        let mi = f.maintainability_index().unwrap();
        assert!((0.0..=100.0).contains(&mi), "MI out of bounds: {mi}");
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = extract_complexity_units("fn ??? {").unwrap_err();
        assert!(matches!(err, ComplexityError::Syn(_)));
    }

    #[test]
    fn empty_file_yields_no_units() {
        let units = extract("// just a comment\n");
        assert!(units.is_empty());
    }

    #[test]
    fn complexity_error_display_includes_inner_message() {
        let parse_err = syn::parse_str::<syn::Expr>("fn???").unwrap_err();
        let err = ComplexityError::Syn(parse_err);
        let msg = err.to_string();
        assert!(msg.contains("failed to parse Rust source"), "got {msg}");
    }

    #[test]
    fn complexity_error_source_is_the_underlying_syn_error() {
        use std::error::Error as _;
        let parse_err = syn::parse_str::<syn::Expr>("fn???").unwrap_err();
        let err = ComplexityError::Syn(parse_err);
        assert!(err.source().is_some());
    }

    #[test]
    fn while_loop_adds_one_to_cyclomatic_and_cognitive() {
        let f = one(r#"
fn f() {
    let mut i = 0;
    while i < 10 { i += 1; }
}
"#);
        assert_eq!(f.cyclomatic, 2, "1 base + 1 while");
        assert_eq!(f.cognitive, 1, "while at nest 0 contributes 1");
        assert_eq!(f.max_nesting, 1);
    }

    #[test]
    fn while_loop_inside_if_pays_nesting_penalty() {
        let f = one(r#"
fn f(go: bool) {
    if go {
        let mut i = 0;
        while i < 10 { i += 1; }
    }
}
"#);
        // 1 (base) + 1 (if) + 1 (while) = 3
        assert_eq!(f.cyclomatic, 3);
        // if at nest 0 → +1; while at nest 1 → +(1+1) = 2; total 3.
        assert_eq!(f.cognitive, 3);
        assert_eq!(f.max_nesting, 2);
    }

    #[test]
    fn for_loop_adds_one_to_cyclomatic_and_cognitive() {
        let f = one(r#"
fn f() {
    for _ in 0..5 {}
}
"#);
        assert_eq!(f.cyclomatic, 2);
        assert_eq!(f.cognitive, 1);
        assert_eq!(f.max_nesting, 1);
    }

    #[test]
    fn for_loop_inside_if_pays_nesting_penalty() {
        let f = one(r#"
fn f(go: bool) {
    if go {
        for _ in 0..5 {}
    }
}
"#);
        // 1 (if at nest 0) + 2 (for at nest 1) = 3
        assert_eq!(f.cognitive, 3);
        assert_eq!(f.cyclomatic, 3);
    }

    #[test]
    fn loop_adds_one_to_cyclomatic_and_cognitive() {
        let f = one(r#"
fn f() {
    loop { break; }
}
"#);
        assert_eq!(f.cyclomatic, 2);
        assert_eq!(f.cognitive, 1);
        assert_eq!(f.max_nesting, 1);
    }

    #[test]
    fn loop_inside_if_pays_nesting_penalty() {
        let f = one(r#"
fn f(go: bool) {
    if go {
        loop { break; }
    }
}
"#);
        // 1 (if) + 2 (loop at nest 1) = 3
        assert_eq!(f.cognitive, 3);
        assert_eq!(f.cyclomatic, 3);
    }

    #[test]
    fn match_at_top_level_charges_one_for_cognitive_regardless_of_arm_count() {
        let f = one(r#"
fn f(n: i32) -> i32 {
    match n { 0 => 0, 1 => 1, _ => 2 }
}
"#);
        // base 1 + (3 arms - 1) = 3
        assert_eq!(f.cyclomatic, 3);
        // Sonar: a top-level match contributes exactly +1, not one per arm.
        assert_eq!(f.cognitive, 1);
    }

    #[test]
    fn match_inside_if_pays_nesting_penalty_only_once() {
        let f = one(r#"
fn f(n: i32) -> i32 {
    if n >= 0 {
        match n { 0 => 0, 1 => 1, _ => 2 }
    } else {
        -1
    }
}
"#);
        // 1 (if) + 1 (else) + (1 + 1 nest) (match) = 4
        assert_eq!(f.cognitive, 4);
        // base 1 + 1 (if) + 2 (match arms-1) = 4
        assert_eq!(f.cyclomatic, 4);
    }

    #[test]
    fn each_logical_operator_bumps_cognitive_by_one() {
        let f = one("fn f(a: bool, b: bool, c: bool) -> bool { a && b || c }");
        // base 1 + 1 (&&) + 1 (||) = 3 cyclomatic
        assert_eq!(f.cyclomatic, 3);
        // && / || each add +1 to cognitive (no nesting penalty).
        assert_eq!(f.cognitive, 2);
    }
}
