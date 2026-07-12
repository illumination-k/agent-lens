//! oxc-based error-handling shape extraction for TypeScript /
//! JavaScript source files.
//!
//! For every function-shaped item (see [`crate::walk`]) we walk the
//! body and produce a
//! [`FunctionErrorShape`](lens_domain::FunctionErrorShape):
//!
//! * **Error regions** — `catch` clause bodies. Each `catch` bumps
//!   `error_branch_count`; the lines of the outermost regions feed
//!   `error_loc`.
//! * **`disjoint_try_count` / `single_stmt_try_count`** — every `try`
//!   statement in the function, and the subset whose protected block
//!   holds a single statement (the "wrap each call individually"
//!   shape).
//! * **Propagate-only handlers** — a `catch` whose body is exactly one
//!   `throw` statement. Throwing a wrapped error still counts; the
//!   point is that no recovery happens.
//! * **Log-and-rethrow handlers** — a `catch` whose statements are
//!   `console.*` / `log.*` / `logger.*` calls followed by a final
//!   `throw`.
//! * **`wrap_only_error_path`** — true when the function has at least
//!   one `catch` and every one of them is propagate-only.
//!
//! Functions without any `try` are reported with all-zero counts;
//! exceptions bubbling implicitly is the language default, not a
//! shape worth flagging. Nested closures follow the complexity
//! convention: their handlers contribute to the enclosing function's
//! counts *and* the closure is emitted as its own `<parent>::closure#N`
//! unit.

use lens_domain::FunctionErrorShape;
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::Visit;
use oxc_parser::Parser;

use crate::line_index::LineIndex;
use crate::parser::{Dialect, TsParseError};
use crate::walk::{FunctionItem, FunctionVisitor, walk_program};

/// Failures produced while extracting error shapes.
#[derive(Debug, thiserror::Error)]
pub enum ErrorShapeError {
    #[error(transparent)]
    Parse(#[from] TsParseError),
}

/// Extract one [`FunctionErrorShape`] per function-shaped item in `source`.
pub fn extract_error_shapes(
    source: &str,
    dialect: Dialect,
) -> Result<Vec<FunctionErrorShape>, ErrorShapeError> {
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, source, dialect.source_type()).parse();
    if !ret.errors.is_empty() {
        return Err(ErrorShapeError::Parse(TsParseError::from_diagnostics(
            ret.errors.iter().map(|e| e.message.as_ref().to_owned()),
        )));
    }
    let line_index = LineIndex::new(source);
    let mut collector = ErrorShapeCollector {
        line_index: &line_index,
        out: Vec::new(),
    };
    walk_program(&ret.program, &line_index, &mut collector);
    Ok(collector.out)
}

struct ErrorShapeCollector<'s> {
    line_index: &'s LineIndex,
    out: Vec<FunctionErrorShape>,
}

impl FunctionVisitor for ErrorShapeCollector<'_> {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        let mut visitor = ErrorShapeVisitor {
            line_index: self.line_index,
            handler_count: 0,
            rethrow_only: 0,
            log_and_rethrow: 0,
            try_count: 0,
            single_stmt_try: 0,
            error_loc: 0,
            region_depth: 0,
        };
        visitor.visit_function_body(item.body);
        let wrap_only = visitor.handler_count > 0 && visitor.handler_count == visitor.rethrow_only;
        self.out.push(FunctionErrorShape {
            name: item.name,
            start_line: item.start_line,
            end_line: item.end_line,
            error_branch_count: visitor.handler_count,
            error_loc: visitor.error_loc,
            disjoint_try_count: visitor.try_count,
            single_stmt_try_count: visitor.single_stmt_try,
            rethrow_only_handlers: visitor.rethrow_only,
            log_and_rethrow_handlers: visitor.log_and_rethrow,
            wrap_only_error_path: wrap_only,
        });
    }
}

struct ErrorShapeVisitor<'s> {
    line_index: &'s LineIndex,
    handler_count: u32,
    rethrow_only: u32,
    log_and_rethrow: u32,
    try_count: u32,
    single_stmt_try: u32,
    error_loc: usize,
    /// Depth of nested error regions; lines are attributed only at the
    /// outermost entry so nested handlers aren't double-counted.
    region_depth: u32,
}

impl ErrorShapeVisitor<'_> {
    fn span_lines(&self, span: oxc_span::Span) -> usize {
        let start = self.line_index.line(span.start);
        let end = self
            .line_index
            .line(span.end.saturating_sub(1).max(span.start));
        end.saturating_sub(start) + 1
    }
}

impl<'a> Visit<'a> for ErrorShapeVisitor<'_> {
    fn visit_try_statement(&mut self, it: &TryStatement<'a>) {
        self.try_count += 1;
        if it.block.body.len() == 1 {
            self.single_stmt_try += 1;
        }
        self.visit_block_statement(&it.block);
        if let Some(handler) = &it.handler {
            self.handler_count += 1;
            match classify_handler(&handler.body.body) {
                HandlerKind::PropagateOnly => self.rethrow_only += 1,
                HandlerKind::LogAndRethrow => self.log_and_rethrow += 1,
                HandlerKind::Recovery => {}
            }
            let lines = self.span_lines(handler.body.span);
            if self.region_depth == 0 {
                self.error_loc += lines;
            }
            self.region_depth += 1;
            self.visit_block_statement(&handler.body);
            self.region_depth -= 1;
        }
        if let Some(finalizer) = &it.finalizer {
            self.visit_block_statement(finalizer);
        }
    }
}

enum HandlerKind {
    PropagateOnly,
    LogAndRethrow,
    Recovery,
}

fn classify_handler(stmts: &[Statement<'_>]) -> HandlerKind {
    let Some((last, rest)) = stmts.split_last() else {
        // An empty catch swallows the error; that's a decision, not
        // propagation.
        return HandlerKind::Recovery;
    };
    if !matches!(last, Statement::ThrowStatement(_)) {
        return HandlerKind::Recovery;
    }
    if rest.is_empty() {
        return HandlerKind::PropagateOnly;
    }
    if rest.iter().all(is_logging_statement) {
        HandlerKind::LogAndRethrow
    } else {
        HandlerKind::Recovery
    }
}

fn is_logging_statement(stmt: &Statement<'_>) -> bool {
    let Statement::ExpressionStatement(es) = stmt else {
        return false;
    };
    let Expression::CallExpression(call) = &es.expression else {
        return false;
    };
    let Some(member) = call.callee.as_member_expression() else {
        return false;
    };
    root_identifier(member.object())
        .is_some_and(|name| matches!(name, "console" | "log" | "logger"))
}

/// Root identifier of a (possibly chained) member access: `console`
/// for `console.error`, `logger` for `logger.child.warn`, but `app`
/// for `app.logger.warn` — chains rooted at anything other than a bare
/// `console`/`log`/`logger` identifier are not treated as logging.
fn root_identifier<'a>(mut expr: &'a Expression<'a>) -> Option<&'a str> {
    loop {
        if let Some(member) = expr.as_member_expression() {
            expr = member.object();
            continue;
        }
        return match expr {
            Expression::Identifier(ident) => Some(ident.name.as_str()),
            _ => None,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn extract(src: &str) -> Vec<FunctionErrorShape> {
        extract_error_shapes(src, Dialect::Ts).unwrap()
    }

    fn one(src: &str) -> FunctionErrorShape {
        let mut units = extract(src);
        assert_eq!(units.len(), 1, "expected exactly one function");
        units.remove(0)
    }

    #[rstest]
    #[case::no_error_handling("function f() { return 1 + 2; }", 0, 0, 0, 0, false)]
    #[case::catch_with_recovery(
        r#"
function f(): number {
    try {
        return risky();
    } catch (e) {
        return 0;
    }
}
"#,
        1,
        1,
        0,
        0,
        false
    )]
    #[case::rethrow_only(
        r#"
function f(): number {
    try {
        return risky();
    } catch (e) {
        throw e;
    }
}
"#,
        1,
        1,
        1,
        0,
        true
    )]
    #[case::wrap_and_rethrow_counts_as_propagate(
        r#"
function f(): number {
    try {
        return risky();
    } catch (e) {
        throw new WrappedError("risky failed", { cause: e });
    }
}
"#,
        1,
        1,
        1,
        0,
        true
    )]
    #[case::log_and_rethrow(
        r#"
function f(): number {
    try {
        return risky();
    } catch (e) {
        console.error("risky failed", e);
        throw e;
    }
}
"#,
        1,
        1,
        0,
        1,
        false
    )]
    #[case::fragmented_single_stmt_tries(
        r#"
function f(): void {
    try {
        stepOne();
    } catch (e) {
        throw e;
    }
    try {
        stepTwo();
    } catch (e) {
        throw e;
    }
}
"#,
        2,
        2,
        2,
        0,
        true
    )]
    #[case::empty_catch_swallows(
        r#"
function f(): void {
    try {
        risky();
    } catch (e) {
    }
}
"#,
        1,
        1,
        0,
        0,
        false
    )]
    #[case::try_finally_without_catch(
        r#"
function f(): void {
    try {
        risky();
    } finally {
        cleanup();
    }
}
"#,
        0,
        1,
        0,
        0,
        false
    )]
    fn error_shape_metrics_match(
        #[case] src: &str,
        #[case] branch_count: u32,
        #[case] try_count: u32,
        #[case] rethrow_only: u32,
        #[case] log_and_rethrow: u32,
        #[case] wrap_only: bool,
    ) {
        let f = extract(src).remove(0);
        assert_eq!(f.error_branch_count, branch_count, "error_branch_count");
        assert_eq!(f.disjoint_try_count, try_count, "disjoint_try_count");
        assert_eq!(f.rethrow_only_handlers, rethrow_only, "rethrow_only");
        assert_eq!(
            f.log_and_rethrow_handlers, log_and_rethrow,
            "log_and_rethrow"
        );
        assert_eq!(f.wrap_only_error_path, wrap_only, "wrap_only_error_path");
    }

    #[test]
    fn single_stmt_try_is_distinguished_from_multi_stmt() {
        // Two single-statement tries against one multi-statement try:
        // the counts are asymmetric on purpose, so inverting the
        // length check cannot produce the same total.
        let f = one(r#"
function f(): void {
    try {
        stepOne();
    } catch (e) {
        throw e;
    }
    try {
        stepTwo();
    } catch (e) {
        throw e;
    }
    try {
        stepThree();
        stepFour();
    } catch (e) {
        throw e;
    }
}
"#);
        assert_eq!(f.disjoint_try_count, 3);
        assert_eq!(f.single_stmt_try_count, 2);
    }

    #[test]
    fn sibling_catch_regions_are_both_counted() {
        // Exiting the first catch region must restore depth 0 so the
        // second catch still contributes lines.
        let f = one(r#"
function f(): void {
    try {
        stepOne();
    } catch (e) {
        recoverOne();
    }
    try {
        stepTwo();
    } catch (e) {
        recoverTwo();
    }
}
"#);
        // Each catch body spans 3 lines (`{` through `}`).
        assert_eq!(f.error_loc, 6);
        assert_eq!(f.error_branch_count, 2);
    }

    #[test]
    fn error_loc_counts_catch_body_lines() {
        let f = one(r#"
function f(): number {
    try {
        return risky();
    } catch (e) {
        console.error(e);
        return 0;
    }
}
"#);
        // Catch body `{` through `}` spans 4 lines.
        assert_eq!(f.error_loc, 4);
        assert!(f.error_loc_ratio() > 0.0);
    }

    #[test]
    fn nested_try_inside_catch_is_not_double_counted() {
        let f = one(r#"
function f(): number {
    try {
        return risky();
    } catch (e) {
        try {
            return fallback();
        } catch (inner) {
            return 0;
        }
    }
}
"#);
        // Outer catch body spans 7 lines; the inner catch lies inside
        // it and must not be added again.
        assert_eq!(f.error_loc, 7);
        assert_eq!(f.error_branch_count, 2);
        assert_eq!(f.disjoint_try_count, 2);
    }

    #[test]
    fn logger_member_chains_are_recognised_as_logging() {
        let f = one(r#"
function f(): number {
    try {
        return risky();
    } catch (e) {
        logger.child.warn("risky failed");
        throw e;
    }
}
"#);
        assert_eq!(f.log_and_rethrow_handlers, 1);
    }

    #[test]
    fn non_logging_call_before_throw_is_recovery() {
        let f = one(r#"
function f(): number {
    try {
        return risky();
    } catch (e) {
        metrics.increment("risky_failures");
        throw e;
    }
}
"#);
        assert_eq!(f.log_and_rethrow_handlers, 0);
        assert_eq!(f.rethrow_only_handlers, 0);
        assert!(!f.wrap_only_error_path);
    }

    #[test]
    fn class_methods_and_arrows_are_extracted() {
        let units = extract(
            r#"
class Repo {
    load(): number {
        try {
            return read();
        } catch (e) {
            throw e;
        }
    }
}
const fetchIt = () => {
    try {
        return get();
    } catch (e) {
        return null;
    }
};
"#,
        );
        let names: Vec<&str> = units.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Repo::load", "fetchIt"]);
        assert!(units[0].wrap_only_error_path);
        assert!(!units[1].wrap_only_error_path);
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = extract_error_shapes("function ??? {", Dialect::Ts).unwrap_err();
        assert!(matches!(err, ErrorShapeError::Parse(_)));
    }

    #[test]
    fn empty_file_yields_no_units() {
        assert!(extract("// just a comment\n").is_empty());
    }
}
