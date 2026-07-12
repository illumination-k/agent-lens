//! syn-based error-handling shape extraction for Rust source files.
//!
//! For every free function, inherent / trait method, and trait default
//! method we walk the body and produce a
//! [`FunctionErrorShape`](lens_domain::FunctionErrorShape):
//!
//! * **Error regions** — `Err(..)` match-arm bodies, `if let Err(..)`
//!   then-branches, and `map_err(..)` closure bodies. Each entry bumps
//!   `error_branch_count`; the lines of the outermost regions feed
//!   `error_loc`.
//! * **Propagate-only handlers** — an `Err` arm / `if let Err` block
//!   whose body does nothing but produce (or `return`) another
//!   `Err(..)` or `bail!(..)`. Wrapping the payload on the way out
//!   still counts as propagate-only; the point is that no recovery
//!   happens.
//! * **Log-and-rethrow handlers** — handler blocks whose statements
//!   are logging (`tracing`/`log` macros, `eprintln!`, …) followed by
//!   a final propagation.
//! * **`wrap_only_error_path`** — true when the function touches
//!   errors only to propagate them: some combination of `?`,
//!   `map_err`, and propagate-only handlers, with no recovery handler
//!   and no logging. `try`-less, `match`-less functions that just
//!   chain `?` are the canonical case.
//!
//! Heuristic boundaries, chosen for cheap syntactic detection: method
//! calls named `map_err` are assumed to be `Result::map_err`;
//! recovery-flavoured combinators (`unwrap_or_else`, `ok`, …) are not
//! classified because Option/Result cannot be told apart without type
//! information; `let ... else` blocks are not treated as error
//! regions. `disjoint_try_count` / `single_stmt_try_count` are always
//! 0 — Rust has no `try` statement.

use lens_domain::{FunctionErrorShape, qualify};

use crate::common::{WalkOptions, walk_fn_items};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Arm, Block, Expr, ExprIf, ExprMatch, ExprMethodCall, ExprTry, Pat, Path, Stmt};

/// Failures produced while extracting error shapes.
#[derive(Debug, thiserror::Error)]
pub enum ErrorShapeError {
    #[error("failed to parse Rust source: {0}")]
    Syn(#[from] syn::Error),
}

/// Extract one [`FunctionErrorShape`] per function-shaped item in `source`.
pub fn extract_error_shapes(source: &str) -> Result<Vec<FunctionErrorShape>, ErrorShapeError> {
    let file = syn::parse_file(source)?;
    let mut out = Vec::new();
    walk_fn_items(&file.items, WalkOptions::default(), &mut |site| {
        let name = qualify(site.owner, &site.sig.ident.to_string());
        out.push(analyze_fn(name, site.sig, site.block));
    });
    Ok(out)
}

fn analyze_fn(name: String, sig: &syn::Signature, block: &Block) -> FunctionErrorShape {
    let mut visitor = ErrorShapeVisitor::default();
    visitor.visit_block(block);
    // Pure propagation somewhere, and *every* handler is propagate-only
    // (log-and-rethrow and recovery handlers both make a decision).
    let propagates = visitor.try_ops > 0 || visitor.map_err_count > 0 || visitor.rethrow_only > 0;
    let wrap_only = propagates && visitor.handler_count == visitor.rethrow_only;
    FunctionErrorShape {
        name,
        start_line: sig.span().start().line,
        end_line: block.span().end().line,
        error_branch_count: visitor.handler_count + visitor.map_err_count,
        error_loc: visitor.error_loc,
        disjoint_try_count: 0,
        single_stmt_try_count: 0,
        rethrow_only_handlers: visitor.rethrow_only,
        log_and_rethrow_handlers: visitor.log_and_rethrow,
        wrap_only_error_path: wrap_only,
    }
}

#[derive(Default)]
struct ErrorShapeVisitor {
    /// `Err` match arms plus `if let Err` blocks.
    handler_count: u32,
    rethrow_only: u32,
    log_and_rethrow: u32,
    map_err_count: u32,
    /// `?` operators.
    try_ops: u32,
    error_loc: usize,
    /// Depth of nested error regions; lines are attributed only at the
    /// outermost entry so nested handlers aren't double-counted.
    region_depth: u32,
}

impl ErrorShapeVisitor {
    fn record_handler(&mut self, kind: HandlerKind) {
        self.handler_count += 1;
        match kind {
            HandlerKind::PropagateOnly => self.rethrow_only += 1,
            HandlerKind::LogAndRethrow => self.log_and_rethrow += 1,
            HandlerKind::Recovery => {}
        }
    }

    /// Attribute `lines` to `error_loc` unless we are already inside an
    /// error region, then run `walk` one region deeper.
    fn enter_region(&mut self, lines: usize, walk: impl FnOnce(&mut Self)) {
        if self.region_depth == 0 {
            self.error_loc += lines;
        }
        self.region_depth += 1;
        walk(self);
        self.region_depth -= 1;
    }
}

impl<'ast> Visit<'ast> for ErrorShapeVisitor {
    fn visit_expr_match(&mut self, e: &'ast ExprMatch) {
        self.visit_expr(&e.expr);
        for arm in &e.arms {
            if is_err_pattern(&arm.pat) {
                self.record_handler(classify_arm(arm));
                self.enter_region(span_lines(&*arm.body), |v| v.visit_expr(&arm.body));
            } else {
                if let Some((_, guard)) = &arm.guard {
                    self.visit_expr(guard);
                }
                self.visit_expr(&arm.body);
            }
        }
    }

    fn visit_expr_if(&mut self, e: &'ast ExprIf) {
        let err_arm = matches!(&*e.cond, Expr::Let(l) if is_err_pattern(&l.pat));
        self.visit_expr(&e.cond);
        if err_arm {
            self.record_handler(classify_block(&e.then_branch));
            self.enter_region(span_lines(&e.then_branch), |v| {
                v.visit_block(&e.then_branch);
            });
        } else {
            self.visit_block(&e.then_branch);
        }
        if let Some((_, else_expr)) = &e.else_branch {
            self.visit_expr(else_expr);
        }
    }

    fn visit_expr_method_call(&mut self, e: &'ast ExprMethodCall) {
        if e.method == "map_err" {
            self.map_err_count += 1;
            self.visit_expr(&e.receiver);
            for arg in &e.args {
                self.enter_region(span_lines(arg), |v| v.visit_expr(arg));
            }
            return;
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_try(&mut self, e: &'ast ExprTry) {
        self.try_ops += 1;
        visit::visit_expr_try(self, e);
    }
}

enum HandlerKind {
    PropagateOnly,
    LogAndRethrow,
    Recovery,
}

fn span_lines(node: &impl Spanned) -> usize {
    let span = node.span();
    span.end().line.saturating_sub(span.start().line) + 1
}

fn path_last_is(path: &Path, ident: &str) -> bool {
    path.segments.last().is_some_and(|seg| seg.ident == ident)
}

fn is_err_pattern(pat: &Pat) -> bool {
    match pat {
        Pat::TupleStruct(p) => path_last_is(&p.path, "Err"),
        Pat::Ident(p) => p
            .subpat
            .as_ref()
            .is_some_and(|(_, sub)| is_err_pattern(sub)),
        Pat::Reference(p) => is_err_pattern(&p.pat),
        Pat::Paren(p) => is_err_pattern(&p.pat),
        _ => false,
    }
}

fn classify_arm(arm: &Arm) -> HandlerKind {
    // A guarded arm inspects the error's content — that is a decision,
    // not plain propagation.
    if arm.guard.is_some() {
        return HandlerKind::Recovery;
    }
    classify_handler_expr(&arm.body)
}

fn classify_handler_expr(expr: &Expr) -> HandlerKind {
    if let Expr::Block(b) = expr {
        return classify_block(&b.block);
    }
    if is_propagation_expr(expr) {
        HandlerKind::PropagateOnly
    } else {
        HandlerKind::Recovery
    }
}

fn classify_block(block: &Block) -> HandlerKind {
    let stmts = &block.stmts;
    let Some((last, rest)) = stmts.split_last() else {
        // An empty handler swallows the error; that's a decision, not
        // propagation.
        return HandlerKind::Recovery;
    };
    let last_propagates = match last {
        Stmt::Expr(expr, _) => is_propagation_expr(expr),
        _ => false,
    };
    if !last_propagates {
        return HandlerKind::Recovery;
    }
    if rest.is_empty() {
        return HandlerKind::PropagateOnly;
    }
    // Only statement-position macros can be logging: `tracing::error!(…);`
    // parses as `Stmt::Macro`, and method-call loggers are treated as
    // recovery because we cannot tell them from arbitrary calls.
    let all_logging = rest.iter().all(|stmt| match stmt {
        Stmt::Macro(m) => macro_path_is_logging(&m.mac.path),
        _ => false,
    });
    if all_logging {
        HandlerKind::LogAndRethrow
    } else {
        HandlerKind::Recovery
    }
}

/// Expressions that hand the error onward: `Err(..)`, `return Err(..)`,
/// or `bail!(..)`. Block-wrapped handler bodies go through
/// [`classify_block`] instead, so no block arm is needed here.
fn is_propagation_expr(expr: &Expr) -> bool {
    match expr {
        Expr::Call(c) => matches!(&*c.func, Expr::Path(p) if path_last_is(&p.path, "Err")),
        Expr::Return(r) => r.expr.as_deref().is_some_and(is_propagation_expr),
        Expr::Macro(m) => path_last_is(&m.mac.path, "bail"),
        _ => false,
    }
}

fn macro_path_is_logging(path: &Path) -> bool {
    path.segments.last().is_some_and(|seg| {
        matches!(
            seg.ident.to_string().as_str(),
            "error" | "warn" | "info" | "debug" | "trace" | "eprintln" | "println"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn extract(src: &str) -> Vec<FunctionErrorShape> {
        extract_error_shapes(src).unwrap()
    }

    fn one(src: &str) -> FunctionErrorShape {
        let mut units = extract(src);
        assert_eq!(units.len(), 1, "expected exactly one function");
        units.remove(0)
    }

    #[rstest]
    #[case::no_error_handling("fn f() { let _ = 1 + 2; }", 0, 0, 0, false)]
    #[case::question_mark_only(
        r#"
fn f() -> Result<i32, ()> {
    let x: Result<i32, ()> = Ok(1);
    Ok(x?)
}
"#,
        0,
        0,
        0,
        true
    )]
    #[case::map_err_then_question(
        r#"
fn f(s: &str) -> Result<i32, String> {
    let n = s.parse::<i32>().map_err(|e| e.to_string())?;
    Ok(n)
}
"#,
        1,
        0,
        0,
        true
    )]
    #[case::propagate_only_match_arm(
        r#"
fn f(r: Result<i32, String>) -> Result<i32, String> {
    match r {
        Ok(v) => Ok(v + 1),
        Err(e) => Err(e),
    }
}
"#,
        1,
        1,
        0,
        true
    )]
    #[case::wrapping_propagation_still_counts(
        r#"
fn f(r: Result<i32, std::io::Error>) -> Result<i32, String> {
    match r {
        Ok(v) => Ok(v),
        Err(e) => return Err(format!("io: {e}")),
    }
}
"#,
        1,
        1,
        0,
        true
    )]
    #[case::recovery_match_arm(
        r#"
fn f(r: Result<i32, String>) -> i32 {
    match r {
        Ok(v) => v,
        Err(_) => 0,
    }
}
"#,
        1,
        0,
        0,
        false
    )]
    #[case::log_and_rethrow(
        r#"
fn f(r: Result<i32, String>) -> Result<i32, String> {
    match r {
        Ok(v) => Ok(v),
        Err(e) => {
            tracing::error!("failed: {e}");
            Err(e)
        }
    }
}
"#,
        1,
        0,
        1,
        false
    )]
    #[case::if_let_err_propagates(
        r#"
fn f(r: Result<(), String>) -> Result<(), String> {
    if let Err(e) = r {
        return Err(e);
    }
    Ok(())
}
"#,
        1,
        1,
        0,
        true
    )]
    #[case::if_let_err_recovers(
        r#"
fn f(r: Result<(), String>) {
    if let Err(e) = r {
        handle(e);
    }
}
fn handle(_e: String) {}
"#,
        1,
        0,
        0,
        false
    )]
    #[case::guarded_err_arm_is_recovery(
        r#"
fn f(r: Result<i32, String>) -> Result<i32, String> {
    match r {
        Ok(v) => Ok(v),
        Err(e) if e.is_empty() => Err(e),
        Err(e) => Err(e),
    }
}
"#,
        2,
        1,
        0,
        false
    )]
    #[case::bail_macro_is_propagation(
        r#"
fn f(r: Result<i32, String>) -> Result<i32, String> {
    match r {
        Ok(v) => Ok(v),
        Err(e) => anyhow::bail!("failed: {e}"),
    }
}
"#,
        1,
        1,
        0,
        true
    )]
    #[case::empty_handler_swallows(
        r#"
fn f(r: Result<(), String>) {
    if let Err(_e) = r {
    }
}
"#,
        1,
        0,
        0,
        false
    )]
    #[case::binding_subpattern_is_recognised(
        r#"
fn f(r: Result<i32, String>) -> i32 {
    match r {
        Ok(v) => v,
        e @ Err(_) => { drop(e); 0 }
    }
}
"#,
        1,
        0,
        0,
        false
    )]
    #[case::parenthesised_err_pattern_is_recognised(
        r#"
fn f(r: Result<i32, String>) -> Result<i32, String> {
    match r {
        Ok(v) => Ok(v),
        (Err(e)) => Err(e),
    }
}
"#,
        1,
        1,
        0,
        true
    )]
    #[case::reference_err_pattern_is_recognised(
        r#"
fn f(r: &Result<i32, String>) -> i32 {
    match r {
        Ok(v) => *v,
        &Err(_) => 0,
    }
}
"#,
        1,
        0,
        0,
        false
    )]
    #[case::non_logging_macro_before_propagation_is_recovery(
        r#"
fn f(r: Result<i32, String>) -> Result<i32, String> {
    match r {
        Ok(v) => Ok(v),
        Err(e) => {
            dbg!(&e);
            Err(e)
        }
    }
}
"#,
        1,
        0,
        0,
        false
    )]
    fn error_shape_metrics_match(
        #[case] src: &str,
        #[case] branch_count: u32,
        #[case] rethrow_only: u32,
        #[case] log_and_rethrow: u32,
        #[case] wrap_only: bool,
    ) {
        let f = extract(src).remove(0);
        assert_eq!(f.error_branch_count, branch_count, "error_branch_count");
        assert_eq!(f.rethrow_only_handlers, rethrow_only, "rethrow_only");
        assert_eq!(
            f.log_and_rethrow_handlers, log_and_rethrow,
            "log_and_rethrow"
        );
        assert_eq!(f.wrap_only_error_path, wrap_only, "wrap_only_error_path");
    }

    #[test]
    fn error_loc_counts_handler_lines_once() {
        let f = one(r#"
fn f(r: Result<i32, String>) -> Result<i32, String> {
    match r {
        Ok(v) => Ok(v),
        Err(e) => {
            let msg = format!("failed: {e}");
            Err(msg)
        }
    }
}
"#);
        // The Err arm body spans 4 lines (`{` through `}`).
        assert_eq!(f.error_loc, 4);
        assert!(f.error_loc_ratio() > 0.0);
    }

    #[test]
    fn nested_error_regions_are_not_double_counted() {
        let f = one(r#"
fn f(r: Result<i32, Result<i32, String>>) -> i32 {
    match r {
        Ok(v) => v,
        Err(inner) => {
            match inner {
                Ok(v) => v,
                Err(_) => 0,
            }
        }
    }
}
"#);
        // Outer handler block spans 6 lines (`{` through `}`); the
        // inner Err arm lies inside it and must not be added again.
        assert_eq!(f.error_loc, 6);
        // Both handlers are still counted as branches.
        assert_eq!(f.error_branch_count, 2);
    }

    #[test]
    fn rust_has_no_try_statements() {
        let f = one(r#"
fn f() -> Result<i32, ()> {
    let x: Result<i32, ()> = Ok(1);
    Ok(x?)
}
"#);
        assert_eq!(f.disjoint_try_count, 0);
        assert_eq!(f.single_stmt_try_count, 0);
    }

    #[test]
    fn map_err_closure_lines_count_as_error_loc() {
        let f = one(r#"
fn f(s: &str) -> Result<i32, String> {
    s.parse::<i32>().map_err(|e| {
        format!("bad int: {e}")
    })
}
"#);
        // The closure spans 3 lines.
        assert_eq!(f.error_loc, 3);
        assert_eq!(f.error_branch_count, 1);
        // `map_err` with no recovery handler is pure propagation even
        // without a `?` in sight.
        assert!(f.wrap_only_error_path);
    }

    #[test]
    fn sibling_error_regions_are_both_counted() {
        // Two handlers at the same depth: exiting the first region must
        // restore depth 0 so the second still contributes lines.
        let f = one(r#"
fn f(a: Result<i32, String>, b: Result<i32, String>) -> i32 {
    let x = match a {
        Ok(v) => v,
        Err(_) => {
            0
        }
    };
    let y = match b {
        Ok(v) => v,
        Err(_) => {
            0
        }
    };
    x + y
}
"#);
        // Each handler block spans 3 lines.
        assert_eq!(f.error_loc, 6);
        assert_eq!(f.error_branch_count, 2);
    }

    #[test]
    fn methods_and_trait_defaults_are_extracted() {
        let units = extract(
            r#"
struct Foo;
impl Foo {
    fn bar(&self) -> Result<(), ()> {
        self.baz()?;
        Ok(())
    }
    fn baz(&self) -> Result<(), ()> { Ok(()) }
}
"#,
        );
        let names: Vec<&str> = units.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Foo::bar", "Foo::baz"]);
        assert!(units[0].wrap_only_error_path);
        assert!(!units[1].wrap_only_error_path);
    }

    #[test]
    fn recovery_elsewhere_defeats_wrap_only() {
        // `?` propagation plus a recovering handler: the function makes
        // a real decision about at least one error, so it is not a pure
        // link in a wrap chain.
        let f = one(r#"
fn f(a: Result<i32, ()>, b: Result<i32, ()>) -> Result<i32, ()> {
    let x = a?;
    let y = match b {
        Ok(v) => v,
        Err(_) => 0,
    };
    Ok(x + y)
}
"#);
        assert!(!f.wrap_only_error_path);
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = extract_error_shapes("fn ??? {").unwrap_err();
        assert!(matches!(err, ErrorShapeError::Syn(_)));
    }

    #[test]
    fn empty_file_yields_no_units() {
        assert!(extract("// just a comment\n").is_empty());
    }

    #[test]
    fn line_range_covers_signature_through_closing_brace() {
        let f = one("fn f() {\n    let _ = 1;\n}\n");
        assert_eq!(f.start_line, 1);
        assert_eq!(f.end_line, 3);
    }
}
