//! syn-based complexity extraction for Rust source files.
//!
//! For every free function, inherent / trait method, and trait default
//! method (including those nested inside inline modules) we walk the body
//! and produce a [`FunctionComplexity`]:
//!
//! * **Cyclomatic Complexity** — McCabe; starts at 1 and is incremented
//!   for each branching construct (`if`, `else if`, `while`, `for`,
//!   `loop`, each `match` arm beyond the first, `&&`/`||`, `?`).
//! * **Cognitive Complexity** — SonarSource's rules: control structures
//!   add `1 + nesting`; `else if`, `else`, labelled `break` / `continue`,
//!   and each run of like `&&` / `||` operators add a flat `1`; direct
//!   recursion adds `1` once. Closures and nested `fn`s raise the nesting
//!   of their bodies.
//! * **Max Nesting Depth** — the deepest control-flow nesting reached in
//!   the function body.
//! * **Halstead counts** — operators and operands are derived from the
//!   token stream of the body; Rust keywords and `Punct` tokens are
//!   treated as operators, identifiers as operands, literals as operands.
//!
//! Closures and items defined *inside* a function body are walked with
//! the same visitor instance, one nesting level deeper, so their branches
//! contribute to the enclosing function's score. That matches how a
//! reader actually experiences the code, and Sonar's own rule.

use lens_domain::{ComplexityCounters, FunctionComplexity, HalsteadAcc, HalsteadCounts, qualify};

use crate::common::{WalkOptions, split_guard, walk_fn_items};
use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    BinOp, Block, Expr, ExprBinary, ExprBreak, ExprCall, ExprClosure, ExprContinue, ExprForLoop,
    ExprIf, ExprLoop, ExprMatch, ExprMethodCall, ExprTry, ExprWhile, ItemFn,
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
    walk_fn_items(&file.items, WalkOptions::default(), &mut |site| {
        let name = qualify(site.owner, &site.sig.ident.to_string());
        out.push(analyze_fn(name, site.sig, site.block, site.is_test));
    });
    Ok(out)
}

fn analyze_fn(
    name: String,
    sig: &syn::Signature,
    block: &Block,
    is_test: bool,
) -> FunctionComplexity {
    let mut visitor = ComplexityVisitor::new(sig.ident.to_string());
    visitor.visit_block(block);
    let halstead = halstead_counts(block);
    FunctionComplexity {
        name,
        start_line: sig.span().start().line,
        end_line: block.span().end().line,
        cyclomatic: visitor.counters.cyclomatic(),
        cognitive: visitor.counters.cognitive(),
        max_nesting: visitor.counters.max_nesting(),
        halstead,
        is_test,
    }
}

/// Rust-specific half of the complexity walk: which `syn` node counts as
/// a branch. The scoring itself lives in [`ComplexityCounters`].
struct ComplexityVisitor {
    counters: ComplexityCounters,
    /// The function's own name, for spotting direct recursion
    /// (`name(..)`, `Self::name(..)`, `self.name(..)`).
    name: String,
}

impl ComplexityVisitor {
    fn new(name: String) -> Self {
        Self {
            counters: ComplexityCounters::default(),
            name,
        }
    }

    /// Walk one `if`'s condition, body, and `else` chain. An `else if`
    /// is a flat `+1` read at the same depth as the `if` it continues; a
    /// plain `else` is `+1` with no McCabe path.
    fn visit_if_chain(&mut self, e: &ExprIf) {
        self.visit_expr(&e.cond);
        self.visit_nested_block(&e.then_branch);
        match e.else_branch.as_ref().map(|(_, expr)| &**expr) {
            Some(Expr::If(next)) => {
                self.counters.add_flat_branch();
                self.visit_if_chain(next);
            }
            Some(other) => {
                self.counters.add_cognitive(1);
                self.counters.enter_nest();
                self.visit_expr(other);
                self.counters.exit_nest();
            }
            None => {}
        }
    }

    fn visit_nested_block(&mut self, block: &Block) {
        self.counters.enter_nest();
        self.visit_block(block);
        self.counters.exit_nest();
    }

    fn is_own_name(&self, ident: &syn::Ident) -> bool {
        ident == self.name.as_str()
    }

    /// Score one loop: `+1` McCabe branch, `+(1 + nesting)` cognitive,
    /// then walk an optional header (the `while` condition or `for`
    /// iterator expression) and the body inside `enter_nest`.
    ///
    /// Shared by `visit_expr_while`, `visit_expr_for_loop`, and
    /// `visit_expr_loop` — those three used to spell this out
    /// individually with TSED 1.0 between each pair.
    fn visit_loop<'ast>(&mut self, header: Option<&'ast Expr>, body: &'ast Block) {
        self.counters.add_branch();
        if let Some(header) = header {
            self.visit_expr(header);
        }
        self.counters.enter_nest();
        self.visit_block(body);
        self.counters.exit_nest();
    }
}

impl<'ast> Visit<'ast> for ComplexityVisitor {
    fn visit_expr_if(&mut self, e: &'ast ExprIf) {
        self.counters.add_branch();
        self.visit_if_chain(e);
    }

    fn visit_expr_while(&mut self, e: &'ast ExprWhile) {
        self.visit_loop(Some(&e.cond), &e.body);
    }

    fn visit_expr_for_loop(&mut self, e: &'ast ExprForLoop) {
        self.visit_loop(Some(&e.expr), &e.body);
    }

    fn visit_expr_loop(&mut self, e: &'ast ExprLoop) {
        self.visit_loop(None, &e.body);
    }

    fn visit_expr_match(&mut self, e: &'ast ExprMatch) {
        // McCabe: every arm beyond the first introduces a new path.
        let arms = u32::try_from(e.arms.len()).unwrap_or(u32::MAX);
        // Sonar: the match itself is one structure, regardless of arm count.
        self.counters.add_branches(arms.saturating_sub(1));

        self.visit_expr(&e.expr);
        self.counters.enter_nest();
        for arm in &e.arms {
            if let (_, Some(guard)) = split_guard(&arm.pat) {
                self.visit_expr(guard);
            }
            self.visit_expr(&arm.body);
        }
        self.counters.exit_nest();
    }

    fn visit_expr_binary(&mut self, e: &'ast ExprBinary) {
        if logical_op(e.op).is_none() {
            visit::visit_expr_binary(self, e);
            return;
        }
        // A whole `&&` / `||` chain is scored at its root: one cognitive
        // point per run of like operators. Operands are walked afterwards
        // and start chains of their own.
        let mut ops = Vec::new();
        let mut operands = Vec::new();
        flatten_logical(e, &mut ops, &mut operands);
        self.counters.add_logical_chain(&ops);
        for operand in operands {
            self.visit_expr(operand);
        }
    }

    fn visit_expr_closure(&mut self, e: &'ast ExprClosure) {
        // A closure is read inside its parent; Sonar nests its body.
        self.counters.enter_nest();
        visit::visit_expr_closure(self, e);
        self.counters.exit_nest();
    }

    fn visit_item_fn(&mut self, item: &'ast ItemFn) {
        self.counters.enter_nest();
        visit::visit_item_fn(self, item);
        self.counters.exit_nest();
    }

    fn visit_expr_break(&mut self, e: &'ast ExprBreak) {
        // A labelled jump is a goto in disguise: `+1`, no nesting.
        if e.label.is_some() {
            self.counters.add_cognitive(1);
        }
        visit::visit_expr_break(self, e);
    }

    fn visit_expr_continue(&mut self, e: &'ast ExprContinue) {
        if e.label.is_some() {
            self.counters.add_cognitive(1);
        }
    }

    fn visit_expr_call(&mut self, e: &'ast ExprCall) {
        if let Expr::Path(path) = &*e.func {
            let segments = &path.path.segments;
            let own = match segments.len() {
                1 => true,
                2 => segments[0].ident == "Self",
                _ => false,
            };
            if own && segments.last().is_some_and(|s| self.is_own_name(&s.ident)) {
                self.counters.add_recursion();
            }
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast ExprMethodCall) {
        let on_self = matches!(&*e.receiver, Expr::Path(p) if p.path.is_ident("self"));
        if on_self && self.is_own_name(&e.method) {
            self.counters.add_recursion();
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_try(&mut self, e: &'ast ExprTry) {
        // `?` is an early return and so adds a path; Sonar does not count
        // it as a structural complexity bump, only McCabe does.
        self.counters.add_cyclomatic(1);
        visit::visit_expr_try(self, e);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogicalOp {
    And,
    Or,
}

fn logical_op(op: BinOp) -> Option<LogicalOp> {
    match op {
        BinOp::And(_) => Some(LogicalOp::And),
        BinOp::Or(_) => Some(LogicalOp::Or),
        _ => None,
    }
}

/// Collect a logical chain's operators in source order, looking through
/// parentheses, and the operands that end it.
fn flatten_logical<'ast>(
    e: &'ast ExprBinary,
    ops: &mut Vec<LogicalOp>,
    operands: &mut Vec<&'ast Expr>,
) {
    let mut side = |expr: &'ast Expr, ops: &mut Vec<LogicalOp>| {
        let mut inner = expr;
        while let Expr::Paren(p) = inner {
            inner = &p.expr;
        }
        let logical = match inner {
            Expr::Binary(b) => logical_op(b.op).map(|_| b),
            _ => None,
        };
        match logical {
            Some(b) => flatten_logical(b, ops, operands),
            None => operands.push(expr),
        }
    };
    side(&e.left, ops);
    if let Some(op) = logical_op(e.op) {
        ops.push(op);
    }
    side(&e.right, ops);
}

fn halstead_counts(block: &Block) -> HalsteadCounts {
    let mut acc = HalsteadAcc::default();
    walk_tokens(block.to_token_stream(), &mut acc);
    acc.counts()
}

fn walk_tokens(stream: TokenStream, acc: &mut HalsteadAcc) {
    for tt in stream {
        match tt {
            TokenTree::Group(g) => walk_tokens(g.stream(), acc),
            TokenTree::Ident(ident) => {
                let s = ident.to_string();
                if is_rust_keyword(&s) {
                    acc.op(&s);
                } else {
                    acc.operand(&s);
                }
            }
            TokenTree::Punct(p) => acc.op(&p.as_char().to_string()),
            TokenTree::Literal(lit) => acc.operand(&lit.to_string()),
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
    use rstest::rstest;

    fn extract(src: &str) -> Vec<FunctionComplexity> {
        extract_complexity_units(src).unwrap()
    }

    fn test_flags(units: &[FunctionComplexity]) -> Vec<(&str, bool)> {
        units.iter().map(|f| (f.name.as_str(), f.is_test)).collect()
    }

    #[test]
    fn test_functions_and_cfg_test_modules_are_flagged() {
        let src = r#"
fn prod() {}
#[test]
fn top_level_test() {}
#[cfg(test)]
mod tests {
    fn helper() {}
}
"#;
        assert_eq!(
            test_flags(&extract(src)),
            [("prod", false), ("top_level_test", true), ("helper", true)]
        );
    }

    fn one(src: &str) -> FunctionComplexity {
        let mut units = extract(src);
        assert_eq!(units.len(), 1, "expected exactly one function");
        units.remove(0)
    }

    #[rstest]
    #[case::linear_function("fn noop() { let _ = 1 + 2; }", Some(1), Some(0), Some(0))]
    #[case::single_if(
        r#"
fn f(x: i32) -> i32 {
    if x > 0 { 1 } else { 0 }
}
"#,
        Some(2),
        Some(2),
        None
    )]
    #[case::if_without_else(
        r#"
fn f(x: i32) -> i32 {
    if x > 0 { return 1; }
    0
}
"#,
        Some(2),
        Some(1),
        None
    )]
    #[case::match_arms(
        r#"
fn f(n: i32) -> i32 {
    match n { 0 => 0, 1 => 1, 2 => 2, _ => 3 }
}
"#,
        Some(4),
        None,
        None
    )]
    #[case::logical_operators(
        r#"
fn f(a: bool, b: bool, c: bool) -> bool { a && b || c }
"#,
        Some(3),
        Some(2),
        None
    )]
    // syn 3 moved match guards into the pattern as `Pat::Guard`, so the
    // guard expression is only reached if the arm walk unwraps it. Without
    // that, the `&&` below goes uncounted and cognitive drops to 1.
    #[case::match_guard_expression_is_walked(
        r#"
fn f(n: i32, flag: bool) -> i32 {
    match n { x if x > 0 && flag => x, _ => 0 }
}
"#,
        Some(3),
        Some(2),
        None
    )]
    #[case::try_operator(
        r#"
fn f() -> Result<i32, ()> {
    let x: Result<i32, ()> = Ok(1);
    Ok(x?)
}
"#,
        Some(2),
        None,
        None
    )]
    #[case::nested_loops(
        r#"
fn f() {
    for _ in 0..10 {
        for _ in 0..10 {
            if true {}
        }
    }
}
"#,
        None,
        None,
        Some(3)
    )]
    #[case::else_if_chain(
        r#"
fn f(n: i32) -> i32 {
    if n > 0 { 1 } else if n < 0 { -1 } else { 0 }
}
"#,
        None,
        Some(3),
        None
    )]
    #[case::while_loop(
        r#"
fn f() {
    let mut i = 0;
    while i < 10 { i += 1; }
}
"#,
        Some(2),
        Some(1),
        Some(1)
    )]
    #[case::while_inside_if(
        r#"
fn f(go: bool) {
    if go {
        let mut i = 0;
        while i < 10 { i += 1; }
    }
}
"#,
        Some(3),
        Some(3),
        Some(2)
    )]
    #[case::for_loop(
        r#"
fn f() {
    for _ in 0..5 {}
}
"#,
        Some(2),
        Some(1),
        Some(1)
    )]
    #[case::for_inside_if(
        r#"
fn f(go: bool) {
    if go {
        for _ in 0..5 {}
    }
}
"#,
        Some(3),
        Some(3),
        None
    )]
    #[case::loop_expression(
        r#"
fn f() {
    loop { break; }
}
"#,
        Some(2),
        Some(1),
        Some(1)
    )]
    #[case::loop_inside_if(
        r#"
fn f(go: bool) {
    if go {
        loop { break; }
    }
}
"#,
        Some(3),
        Some(3),
        None
    )]
    #[case::match_at_top_level(
        r#"
fn f(n: i32) -> i32 {
    match n { 0 => 0, 1 => 1, _ => 2 }
}
"#,
        Some(3),
        Some(1),
        None
    )]
    #[case::match_inside_if(
        r#"
fn f(n: i32) -> i32 {
    if n >= 0 {
        match n { 0 => 0, 1 => 1, _ => 2 }
    } else {
        -1
    }
}
"#,
        Some(4),
        Some(4),
        None
    )]
    fn complexity_metrics_match(
        #[case] src: &str,
        #[case] cyclomatic: Option<u32>,
        #[case] cognitive: Option<u32>,
        #[case] max_nesting: Option<u32>,
    ) {
        let f = one(src);
        if let Some(expected) = cyclomatic {
            assert_eq!(f.cyclomatic, expected);
        }
        if let Some(expected) = cognitive {
            assert_eq!(f.cognitive, expected);
        }
        if let Some(expected) = max_nesting {
            assert_eq!(f.max_nesting, expected);
        }
    }

    /// SonarSource's Cognitive Complexity rules, case by case. The first
    /// two are the white paper's own worked examples, ported to Rust.
    #[rstest]
    #[case::sum_of_primes(
        r"
fn sum_of_primes(max: u32) -> u32 {
    let mut total = 0;
    'out: for i in 1..=max {        // +1
        for j in 2..i {             // +2
            if i % j == 0 {         // +3
                continue 'out;      // +1
            }
        }
        total += i;
    }
    total
}",
        7
    )]
    #[case::match_counts_once(
        r#"
fn get_words(n: u32) -> &'static str {
    match n {                       // +1
        1 => "one",
        2 => "a couple",
        3 => "a few",
        _ => "lots",
    }
}"#,
        1
    )]
    #[case::else_if_is_flat_and_does_not_nest_deeper(
        r"
fn f(xs: &[bool], b: bool) {
    for a in xs {                   // +1
        if *a {                     // +2
        } else if b {               // +1
            if b {}                 // +3
        } else {                    // +1
        }
    }
}",
        8
    )]
    #[case::like_operators_form_one_run(
        r"
fn f(a: bool, b: bool, c: bool) -> bool {
    a && b && c                     // +1
}",
        1
    )]
    #[case::each_operator_switch_is_a_new_run(
        r"
fn f(a: bool, b: bool, c: bool, d: bool) -> bool {
    a && b || c && d                // +3
}",
        3
    )]
    #[case::parentheses_do_not_break_a_run(
        r"
fn f(a: bool, b: bool, c: bool) -> bool {
    a && (b && c)                   // +1
}",
        1
    )]
    #[case::negation_starts_a_new_run(
        r"
fn f(a: bool, b: bool, c: bool) -> bool {
    a && !(b && c)                  // +1 +1
}",
        2
    )]
    #[case::direct_recursion_counts_once(
        r"
fn fact(n: u64) -> u64 {
    if n == 0 { 1 } else { n * fact(n - 1) + fact(0) }   // +1 +1, recursion +1
}",
        3
    )]
    #[case::method_recursion_through_self(
        r"
struct S;
impl S {
    fn walk(&self, n: u32) {
        if n > 0 {                  // +1
            self.walk(n - 1);       // recursion +1
            Self::walk(self, n - 1);
        }
    }
}",
        2
    )]
    #[case::self_path_recursion(
        r"
struct S;
impl S {
    fn walk(n: u32) {
        if n > 0 {                  // +1
            Self::walk(n - 1);      // recursion +1
        }
    }
}",
        2
    )]
    #[case::calls_that_only_look_like_recursion(
        r"
struct S;
impl S {
    fn walk(&self, other: &S) {
        Other::walk(other);
        a::b::walk(other);
        other.walk(other);
        self.run(other);
        walker();
    }
}",
        0
    )]
    #[case::unlabelled_jumps_are_free(
        r"
fn f(xs: &[i32]) {
    loop {                          // +1
        for x in xs {               // +2
            if *x > 0 { continue; } // +3
            break;
        }
        break;
    }
}",
        6
    )]
    #[case::closure_body_is_nested(
        r"
fn f(xs: &[i32]) -> usize {
    xs.iter().filter(|x| if **x > 0 { true } else { false }).count()   // +2 +1
}",
        3
    )]
    #[case::nested_fn_body_is_nested(
        r"
fn f(n: i32) -> i32 {
    fn inner(n: i32) -> i32 {
        if n > 0 { 1 } else { 0 }   // +2 +1
    }
    inner(n)
}",
        3
    )]
    #[case::labelled_break_counts_flat(
        r"
fn f(xs: &[i32]) {
    'scan: loop {                   // +1
        for x in xs {               // +2
            if *x > 0 {             // +3
                break 'scan;        // +1
            }
        }
        break;
    }
}",
        7
    )]
    #[case::question_mark_is_free(
        r"
fn f(s: &str) -> Result<i32, std::num::ParseIntError> {
    let n = s.parse::<i32>()?;
    Ok(n)
}",
        0
    )]
    fn cognitive_follows_the_sonar_rules(#[case] src: &str, #[case] cognitive: u32) {
        let units = extract(src);
        let f = units.last().expect("one unit");
        assert_eq!(f.cognitive, cognitive, "{}", f.name);
    }

    #[test]
    fn logical_chain_keeps_one_mccabe_path_per_operator() {
        let f = one("fn f(a: bool, b: bool, c: bool, d: bool) -> bool { a && b || c && d }");
        assert_eq!(f.cyclomatic, 4);
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

    #[rstest]
    #[case::impl_method(
        r#"
struct Foo;
impl Foo {
    fn bar(&self) {}
}
"#,
        "Foo::bar",
        None
    )]
    #[case::trait_default_method(
        r#"
trait T {
    fn required(&self);
    fn with_default(&self) { let _ = 1; }
}
"#,
        "T::with_default",
        None
    )]
    #[case::nested_module_function(
        r#"
mod inner {
    fn hidden(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }
}
"#,
        "hidden",
        Some(2)
    )]
    fn extracted_function_matches(
        #[case] src: &str,
        #[case] expected_name: &str,
        #[case] expected_cyclomatic: Option<u32>,
    ) {
        let units = extract(src);
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, expected_name);
        if let Some(expected) = expected_cyclomatic {
            assert_eq!(units[0].cyclomatic, expected);
        }
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
        let v = f.halstead.volume();
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
}
