//! ruff-based error-handling shape extraction for Python source files.
//!
//! For every function-shaped item — top-level `def` / `async def` and
//! every method on a class — we walk the body and produce a
//! [`FunctionErrorShape`](lens_domain::FunctionErrorShape):
//!
//! * **Error regions** — `except` (and `except*`) handler bodies. Each
//!   handler bumps `error_branch_count`; the lines of the outermost
//!   regions feed `error_loc`.
//! * **`disjoint_try_count` / `single_stmt_try_count`** — every `try`
//!   statement in the function, and the subset whose protected body
//!   holds a single statement.
//! * **Propagate-only handlers** — an `except` whose body is exactly
//!   one `raise` (bare `raise`, `raise e`, or `raise New(...) from e`).
//!   Re-wrapping on the way out still counts; the point is that no
//!   recovery happens.
//! * **Log-and-rethrow handlers** — handlers whose statements are
//!   `logger.*` / `logging.*` / `log.*` calls followed by a final
//!   `raise`.
//! * **`wrap_only_error_path`** — true when the function has at least
//!   one handler and every one of them is propagate-only.
//!
//! Functions without any `try` are reported with all-zero counts —
//! exceptions bubbling implicitly is the language default. Stub
//! functions (Protocol methods, `...`-bodies, `@overload`) are
//! filtered the same way the complexity extractor filters them.
//! Nested `def`s contribute to the enclosing function's counts and are
//! not surfaced as separate units, mirroring the complexity extractor.

use lens_domain::{FunctionErrorShape, qualify};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{ExceptHandler, Expr, Stmt, StmtClassDef, StmtFunctionDef, StmtTry};
use ruff_python_parser::{ParseError, parse_module};

use crate::attrs::{inherits_protocol, is_stub_function};
use crate::line_index::LineIndex;

/// Failures produced while extracting error shapes.
#[derive(Debug, thiserror::Error)]
pub enum ErrorShapeError {
    #[error("failed to parse Python source: {0}")]
    Parse(#[from] ParseError),
}

/// Extract one [`FunctionErrorShape`] per function-shaped item in
/// `source`. Methods are reported as `Class::method`; free functions
/// keep their bare name.
pub fn extract_error_shapes(source: &str) -> Result<Vec<FunctionErrorShape>, ErrorShapeError> {
    let module = parse_module(source)?.into_syntax();
    let lines = LineIndex::new(source);
    let mut out = Vec::new();
    for stmt in &module.body {
        collect_stmt(stmt, None, &lines, &mut out);
    }
    Ok(out)
}

fn collect_stmt(
    stmt: &Stmt,
    owner: Option<&str>,
    lines: &LineIndex,
    out: &mut Vec<FunctionErrorShape>,
) {
    match stmt {
        Stmt::FunctionDef(func) => {
            if is_stub_function(func) {
                return;
            }
            let name = qualify(owner, func.name.as_str());
            out.push(analyze(&name, func, lines));
        }
        Stmt::ClassDef(class) => collect_class(class, lines, out),
        _ => {}
    }
}

fn collect_class(class: &StmtClassDef, lines: &LineIndex, out: &mut Vec<FunctionErrorShape>) {
    if inherits_protocol(class) {
        return;
    }
    let class_name = class.name.as_str();
    for inner in &class.body {
        collect_stmt(inner, Some(class_name), lines, out);
    }
}

fn analyze(name: &str, func: &StmtFunctionDef, lines: &LineIndex) -> FunctionErrorShape {
    let mut visitor = ErrorShapeVisitor {
        lines,
        handler_count: 0,
        rethrow_only: 0,
        log_and_rethrow: 0,
        try_count: 0,
        single_stmt_try: 0,
        error_loc: 0,
        region_depth: 0,
    };
    for stmt in &func.body {
        visitor.visit_stmt(stmt);
    }
    let wrap_only = visitor.handler_count > 0 && visitor.handler_count == visitor.rethrow_only;
    let start_line = lines.line_of(func.range.start().to_usize());
    let end_offset = func.range.end().to_usize().saturating_sub(1);
    FunctionErrorShape {
        name: name.to_owned(),
        start_line,
        end_line: lines.line_of(end_offset),
        error_branch_count: visitor.handler_count,
        error_loc: visitor.error_loc,
        disjoint_try_count: visitor.try_count,
        single_stmt_try_count: visitor.single_stmt_try,
        rethrow_only_handlers: visitor.rethrow_only,
        log_and_rethrow_handlers: visitor.log_and_rethrow,
        wrap_only_error_path: wrap_only,
    }
}

struct ErrorShapeVisitor<'s> {
    lines: &'s LineIndex,
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

impl<'a> Visitor<'a> for ErrorShapeVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        if let Stmt::Try(s) = stmt {
            self.visit_try(s);
        } else {
            walk_stmt(self, stmt);
        }
    }
}

impl ErrorShapeVisitor<'_> {
    fn visit_try(&mut self, stmt: &StmtTry) {
        self.try_count += 1;
        if stmt.body.len() == 1 {
            self.single_stmt_try += 1;
        }
        for s in &stmt.body {
            self.visit_stmt(s);
        }
        for handler in &stmt.handlers {
            let ExceptHandler::ExceptHandler(h) = handler;
            self.handler_count += 1;
            match classify_handler(&h.body) {
                HandlerKind::PropagateOnly => self.rethrow_only += 1,
                HandlerKind::LogAndRethrow => self.log_and_rethrow += 1,
                HandlerKind::Recovery => {}
            }
            let start = self.lines.line_of(h.range.start().to_usize());
            let end_offset = h.range.end().to_usize().saturating_sub(1);
            let end = self.lines.line_of(end_offset);
            if self.region_depth == 0 {
                self.error_loc += end.saturating_sub(start) + 1;
            }
            self.region_depth += 1;
            for s in &h.body {
                self.visit_stmt(s);
            }
            self.region_depth -= 1;
        }
        for s in &stmt.orelse {
            self.visit_stmt(s);
        }
        for s in &stmt.finalbody {
            self.visit_stmt(s);
        }
    }
}

enum HandlerKind {
    PropagateOnly,
    LogAndRethrow,
    Recovery,
}

fn classify_handler(stmts: &[Stmt]) -> HandlerKind {
    let Some((last, rest)) = stmts.split_last() else {
        return HandlerKind::Recovery;
    };
    if !matches!(last, Stmt::Raise(_)) {
        // A lone `pass` or any recovery logic: the handler makes a
        // decision instead of propagating.
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

fn is_logging_statement(stmt: &Stmt) -> bool {
    let Stmt::Expr(es) = stmt else {
        return false;
    };
    let Expr::Call(call) = &*es.value else {
        return false;
    };
    root_name(&call.func).is_some_and(|name| matches!(name, "logger" | "logging" | "log"))
}

/// Root name of a (possibly chained) attribute access: `logger` for
/// `logger.error(...)` and `logger.child.warning(...)`, `self` for
/// `self.logger.error(...)` — only chains rooted at a bare
/// `logger`/`logging`/`log` name are treated as logging.
fn root_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Attribute(attr) => root_name(&attr.value),
        Expr::Name(name) => Some(name.id.as_str()),
        _ => None,
    }
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
    #[case::no_error_handling("def f():\n    return 1 + 2\n", 0, 0, 0, 0, false)]
    #[case::except_with_recovery(
        "
def f():
    try:
        return risky()
    except ValueError:
        return 0
",
        1,
        1,
        0,
        0,
        false
    )]
    #[case::bare_reraise(
        "
def f():
    try:
        return risky()
    except ValueError:
        raise
",
        1,
        1,
        1,
        0,
        true
    )]
    #[case::wrap_and_reraise_counts_as_propagate(
        "
def f():
    try:
        return risky()
    except ValueError as e:
        raise AppError('risky failed') from e
",
        1,
        1,
        1,
        0,
        true
    )]
    #[case::log_and_reraise(
        "
def f():
    try:
        return risky()
    except ValueError as e:
        logger.error('risky failed: %s', e)
        raise
",
        1,
        1,
        0,
        1,
        false
    )]
    #[case::fragmented_single_stmt_tries(
        "
def f():
    try:
        step_one()
    except ValueError:
        raise
    try:
        step_two()
    except ValueError:
        raise
",
        2,
        2,
        2,
        0,
        true
    )]
    #[case::swallowing_handler(
        "
def f():
    try:
        risky()
    except ValueError:
        pass
",
        1,
        1,
        0,
        0,
        false
    )]
    #[case::try_finally_without_except(
        "
def f():
    try:
        risky()
    finally:
        cleanup()
",
        0,
        1,
        0,
        0,
        false
    )]
    #[case::multiple_handlers_on_one_try(
        "
def f():
    try:
        return risky()
    except ValueError:
        raise
    except KeyError:
        return 0
",
        2,
        1,
        1,
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
        // asymmetric on purpose so inverting the length check cannot
        // produce the same total.
        let f = one("
def f():
    try:
        step_one()
    except ValueError:
        raise
    try:
        step_two()
    except ValueError:
        raise
    try:
        step_three()
        step_four()
    except ValueError:
        raise
");
        assert_eq!(f.disjoint_try_count, 3);
        assert_eq!(f.single_stmt_try_count, 2);
    }

    #[test]
    fn sibling_handler_regions_are_both_counted() {
        // Exiting the first handler region must restore depth 0 so the
        // second handler still contributes lines.
        let f = one("
def f():
    try:
        step_one()
    except ValueError:
        recover_one()
        recover_more()
    try:
        step_two()
    except ValueError:
        recover_two()
        recover_more()
");
        // Each handler spans `except` line + 2 body lines = 3 lines.
        assert_eq!(f.error_loc, 6);
        assert_eq!(f.error_branch_count, 2);
    }

    #[test]
    fn error_loc_counts_handler_lines() {
        let f = one("
def f():
    try:
        return risky()
    except ValueError as e:
        logger.error('failed: %s', e)
        return 0
");
        // Handler spans `except` line through its last body line: 3 lines.
        assert_eq!(f.error_loc, 3);
        assert!(f.error_loc_ratio() > 0.0);
    }

    #[test]
    fn nested_try_inside_handler_is_not_double_counted() {
        let f = one("
def f():
    try:
        return risky()
    except ValueError:
        try:
            return fallback()
        except KeyError:
            return 0
");
        // Outer handler spans 5 lines; the inner handler lies inside it.
        assert_eq!(f.error_loc, 5);
        assert_eq!(f.error_branch_count, 2);
        assert_eq!(f.disjoint_try_count, 2);
    }

    #[test]
    fn non_logging_call_before_raise_is_recovery() {
        let f = one("
def f():
    try:
        return risky()
    except ValueError:
        metrics.increment('risky_failures')
        raise
");
        assert_eq!(f.log_and_rethrow_handlers, 0);
        assert_eq!(f.rethrow_only_handlers, 0);
        assert!(!f.wrap_only_error_path);
    }

    #[test]
    fn methods_are_qualified_and_stubs_filtered() {
        let units = extract(
            "
class Repo:
    def load(self):
        try:
            return read()
        except OSError:
            raise
    def stub(self): ...
",
        );
        let names: Vec<&str> = units.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Repo::load"]);
        assert!(units[0].wrap_only_error_path);
    }

    #[test]
    fn nested_def_contributes_to_enclosing_function() {
        let units = extract(
            "
def outer():
    def inner():
        try:
            risky()
        except ValueError:
            raise
    return inner
",
        );
        let names: Vec<&str> = units.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["outer"]);
        assert_eq!(units[0].disjoint_try_count, 1);
        assert_eq!(units[0].error_branch_count, 1);
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = extract_error_shapes("def !!!(:").unwrap_err();
        assert!(matches!(err, ErrorShapeError::Parse(_)));
    }

    #[test]
    fn empty_file_yields_no_units() {
        assert!(extract("# nothing here\n").is_empty());
    }
}
