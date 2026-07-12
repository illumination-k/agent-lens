//! tree-sitter-based error-handling shape extraction for Go source
//! files.
//!
//! For every top-level function and method we walk the body and
//! produce a [`FunctionErrorShape`](lens_domain::FunctionErrorShape):
//!
//! * **Error regions** — the consequence block of an error-check `if`:
//!   a condition comparing an error-named identifier (`err`, `saveErr`,
//!   …) against `nil` with `!=`, possibly inside a `&&` / `||` chain.
//!   Each such `if` bumps `error_branch_count`; the lines of the
//!   outermost regions feed `error_loc`.
//! * **Propagate-only handlers** — an error-check block whose single
//!   statement is a `return` that carries the checked error onward,
//!   bare (`return err`) or wrapped (`return fmt.Errorf("…: %w", err)`).
//! * **Log-and-rethrow handlers** — blocks whose statements are
//!   logging calls (`log.*`, `logger.*`, `slog.*`, `zap.*`,
//!   `logrus.*`) followed by a final propagating `return`.
//! * **`wrap_only_error_path`** — true when the function has at least
//!   one error check and every one of them is propagate-only.
//!
//! `disjoint_try_count` / `single_stmt_try_count` are always 0 — Go
//! has no `try`. The `err`-name heuristic is syntactic: without type
//! information, an identifier whose name is `err` or ends in
//! `err`/`Err` compared against `nil` is taken to be an error check.

use lens_domain::{FunctionErrorShape, qualify};
use tree_sitter::Node;

use crate::parser::{GoParseError, function_name_text, method_receiver_type, parse_tree};

/// Extract one [`FunctionErrorShape`] per function-shaped item in
/// `source`. Methods are reported as `Receiver::method`; free
/// functions keep their bare name.
pub fn extract_error_shapes(source: &str) -> Result<Vec<FunctionErrorShape>, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut cursor = tree.root_node().walk();
    for child in tree.root_node().named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(unit) = analyze_function(child, bytes, None) {
                    out.push(unit);
                }
            }
            "method_declaration" => {
                let owner = method_receiver_type(child, bytes);
                if let Some(unit) = analyze_function(child, bytes, owner.as_deref()) {
                    out.push(unit);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn analyze_function(
    node: Node<'_>,
    source: &[u8],
    owner: Option<&str>,
) -> Option<FunctionErrorShape> {
    let body = node.child_by_field_name("body")?;
    let name = qualify(owner, function_name_text(node, source)?);
    let mut visitor = ErrorShapeVisitor {
        source,
        handler_count: 0,
        rethrow_only: 0,
        log_and_rethrow: 0,
        error_loc: 0,
        region_depth: 0,
    };
    visitor.visit_node(body);
    let wrap_only = visitor.handler_count > 0 && visitor.handler_count == visitor.rethrow_only;
    Some(FunctionErrorShape {
        name,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        error_branch_count: visitor.handler_count,
        error_loc: visitor.error_loc,
        disjoint_try_count: 0,
        single_stmt_try_count: 0,
        rethrow_only_handlers: visitor.rethrow_only,
        log_and_rethrow_handlers: visitor.log_and_rethrow,
        wrap_only_error_path: wrap_only,
    })
}

struct ErrorShapeVisitor<'a> {
    source: &'a [u8],
    handler_count: u32,
    rethrow_only: u32,
    log_and_rethrow: u32,
    error_loc: usize,
    /// Depth of nested error regions; lines are attributed only at the
    /// outermost entry so nested handlers aren't double-counted.
    region_depth: u32,
}

impl ErrorShapeVisitor<'_> {
    fn visit_node(&mut self, node: Node<'_>) {
        if node.kind() == "if_statement" {
            self.visit_if(node);
            return;
        }
        self.visit_children(node);
    }

    fn visit_children(&mut self, node: Node<'_>) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit_node(child);
        }
    }

    fn visit_if(&mut self, node: Node<'_>) {
        let err_name = node
            .child_by_field_name("condition")
            .and_then(|cond| error_check_name(cond, self.source));
        if let Some(init) = node.child_by_field_name("initializer") {
            self.visit_node(init);
        }
        let consequence = node.child_by_field_name("consequence");
        match (err_name, consequence) {
            (Some(err_name), Some(block)) => {
                self.handler_count += 1;
                match classify_handler(block, &err_name, self.source) {
                    HandlerKind::PropagateOnly => self.rethrow_only += 1,
                    HandlerKind::LogAndRethrow => self.log_and_rethrow += 1,
                    HandlerKind::Recovery => {}
                }
                let lines = block.end_position().row - block.start_position().row + 1;
                if self.region_depth == 0 {
                    self.error_loc += lines;
                }
                self.region_depth += 1;
                self.visit_children(block);
                self.region_depth -= 1;
            }
            (None, Some(block)) => self.visit_node(block),
            _ => {}
        }
        if let Some(alternative) = node.child_by_field_name("alternative") {
            self.visit_node(alternative);
        }
    }
}

enum HandlerKind {
    PropagateOnly,
    LogAndRethrow,
    Recovery,
}

/// If `cond` is (or contains, via `&&` / `||`) an `<err-ident> != nil`
/// comparison, return the error identifier's name.
fn error_check_name(cond: Node<'_>, source: &[u8]) -> Option<String> {
    match cond.kind() {
        "binary_expression" => {
            let op = cond.child_by_field_name("operator")?;
            let op_text = op.utf8_text(source).ok()?;
            let left = cond.child_by_field_name("left")?;
            let right = cond.child_by_field_name("right")?;
            match op_text {
                "!=" => nil_comparison_err_name(left, right, source),
                "&&" | "||" => {
                    error_check_name(left, source).or_else(|| error_check_name(right, source))
                }
                _ => None,
            }
        }
        "parenthesized_expression" => error_check_name(cond.named_child(0)?, source),
        _ => None,
    }
}

fn nil_comparison_err_name(left: Node<'_>, right: Node<'_>, source: &[u8]) -> Option<String> {
    let (nil_side, other) = if left.kind() == "nil" {
        (left, right)
    } else if right.kind() == "nil" {
        (right, left)
    } else {
        return None;
    };
    debug_assert_eq!(nil_side.kind(), "nil");
    if other.kind() != "identifier" {
        return None;
    }
    let name = other.utf8_text(source).ok()?;
    is_error_name(name).then(|| name.to_owned())
}

/// Syntactic stand-in for "has type `error`": `err` itself or any
/// identifier starting or ending with `err` (`saveErr`, `parseErr`,
/// `err2`, `errLoad`, …).
fn is_error_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("err") || lower.ends_with("err")
}

/// Named statements of a block, unwrapping the grammar's
/// `statement_list` container node.
fn block_statements(block: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = block.walk();
    let children: Vec<Node<'_>> = block.named_children(&mut cursor).collect();
    if let [single] = children.as_slice()
        && single.kind() == "statement_list"
    {
        let mut inner = single.walk();
        return single.named_children(&mut inner).collect();
    }
    children
}

fn classify_handler(block: Node<'_>, err_name: &str, source: &[u8]) -> HandlerKind {
    let stmts = block_statements(block);
    let Some((last, rest)) = stmts.split_last() else {
        return HandlerKind::Recovery;
    };
    if !is_propagating_return(*last, err_name, source) {
        return HandlerKind::Recovery;
    }
    if rest.is_empty() {
        return HandlerKind::PropagateOnly;
    }
    if rest.iter().all(|stmt| is_logging_statement(*stmt, source)) {
        HandlerKind::LogAndRethrow
    } else {
        HandlerKind::Recovery
    }
}

/// A `return` that carries the checked error onward — the bare
/// identifier appears somewhere in the returned expressions, directly
/// (`return err`) or as a call argument (`return fmt.Errorf("…: %w",
/// err)`). A `return` without it (`return 0, nil`) substitutes a
/// fallback value, which is recovery.
fn is_propagating_return(stmt: Node<'_>, err_name: &str, source: &[u8]) -> bool {
    stmt.kind() == "return_statement" && mentions_identifier(stmt, err_name, source)
}

fn mentions_identifier(node: Node<'_>, name: &str, source: &[u8]) -> bool {
    if node.kind() == "identifier" && node.utf8_text(source) == Ok(name) {
        return true;
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .any(|child| mentions_identifier(child, name, source))
}

fn is_logging_statement(stmt: Node<'_>, source: &[u8]) -> bool {
    // The grammar wraps a bare call in `expression_statement`. A
    // non-call inner node has no `function` field and falls out below.
    if stmt.kind() != "expression_statement" {
        return false;
    }
    let Some(function) = stmt
        .named_child(0)
        .and_then(|inner| inner.child_by_field_name("function"))
    else {
        return false;
    };
    root_identifier_text(function, source)
        .is_some_and(|name| matches!(name, "log" | "logger" | "slog" | "zap" | "logrus"))
}

/// Root identifier of a (possibly chained) selector: `log` for
/// `log.Printf`, `logger` for `logger.Sugar().Warn` — chains rooted at
/// anything else are not treated as logging.
fn root_identifier_text<'s>(mut node: Node<'_>, source: &'s [u8]) -> Option<&'s str> {
    loop {
        match node.kind() {
            "selector_expression" => node = node.child_by_field_name("operand")?,
            "call_expression" => node = node.child_by_field_name("function")?,
            "identifier" => return node.utf8_text(source).ok(),
            _ => return None,
        }
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
    #[case::no_error_handling("package p\nfunc f() int { return 1 + 2 }\n", 0, 0, 0, false)]
    #[case::bare_propagation(
        r#"
package p
func f() error {
    if err := step(); err != nil {
        return err
    }
    return nil
}
"#,
        1,
        1,
        0,
        true
    )]
    #[case::wrapped_propagation(
        r#"
package p
func f() error {
    if err := step(); err != nil {
        return fmt.Errorf("step failed: %w", err)
    }
    return nil
}
"#,
        1,
        1,
        0,
        true
    )]
    #[case::fallback_value_is_recovery(
        r#"
package p
func f() int {
    v, err := load()
    if err != nil {
        return 0
    }
    return v
}
"#,
        1,
        0,
        0,
        false
    )]
    #[case::log_and_return(
        r#"
package p
func f() error {
    if err := step(); err != nil {
        log.Printf("step failed: %v", err)
        return err
    }
    return nil
}
"#,
        1,
        0,
        1,
        false
    )]
    #[case::named_error_variables(
        r#"
package p
func f() error {
    if saveErr := save(); saveErr != nil {
        return saveErr
    }
    return nil
}
"#,
        1,
        1,
        0,
        true
    )]
    #[case::condition_inside_logical_chain(
        r#"
package p
func f() error {
    err := step()
    if err != nil && !errors.Is(err, ErrNotFound) {
        return err
    }
    return nil
}
"#,
        1,
        1,
        0,
        true
    )]
    #[case::non_error_if_is_not_counted(
        r#"
package p
func f(n int) int {
    if n != 0 {
        return n
    }
    return -1
}
"#,
        0,
        0,
        0,
        false
    )]
    #[case::eq_nil_is_happy_path(
        r#"
package p
func f() error {
    err := step()
    if err == nil {
        return nil
    }
    return err
}
"#,
        0,
        0,
        0,
        false
    )]
    #[case::panic_in_handler_is_recovery(
        r#"
package p
func f() {
    if err := step(); err != nil {
        panic(err)
    }
}
"#,
        1,
        0,
        0,
        false
    )]
    #[case::error_check_inside_plain_if_is_still_found(
        r#"
package p
func f(retry bool) error {
    if retry {
        if err := step(); err != nil {
            return err
        }
    }
    return nil
}
"#,
        1,
        1,
        0,
        true
    )]
    #[case::parenthesised_condition_is_recognised(
        r#"
package p
func f() error {
    if err := step(); (err != nil) {
        return err
    }
    return nil
}
"#,
        1,
        1,
        0,
        true
    )]
    #[case::non_error_named_nil_check_is_not_counted(
        r#"
package p
func f(ptr *int) *int {
    if ptr != nil {
        return ptr
    }
    return nil
}
"#,
        0,
        0,
        0,
        false
    )]
    #[case::returning_a_different_identifier_is_recovery(
        r#"
package p
func f(fallback error) error {
    if err := step(); err != nil {
        return fallback
    }
    return nil
}
"#,
        1,
        0,
        0,
        false
    )]
    #[case::chained_logger_call_is_logging(
        r#"
package p
func f() error {
    if err := step(); err != nil {
        logger.Sugar().Warnf("step failed: %v", err)
        return err
    }
    return nil
}
"#,
        1,
        0,
        1,
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
    fn error_loc_counts_handler_block_lines() {
        let f = one(r#"
package p
func f() error {
    if err := step(); err != nil {
        log.Printf("step failed: %v", err)
        return err
    }
    return nil
}
"#);
        // The consequence block spans `{` through `}`: 4 lines.
        assert_eq!(f.error_loc, 4);
        assert!(f.error_loc_ratio() > 0.0);
    }

    #[test]
    fn line_range_covers_signature_through_closing_brace() {
        let units = extract(
            "package p\n\nfunc f() error {\n    if err := step(); err != nil {\n        return err\n    }\n    return nil\n}\n",
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].start_line, 3);
        assert_eq!(units[0].end_line, 8);
    }

    #[test]
    fn sibling_error_checks_are_both_counted() {
        // Exiting the first error region must restore depth 0 so the
        // second check still contributes lines.
        let f = one(r#"
package p
func f() error {
    if err := stepOne(); err != nil {
        return err
    }
    if err := stepTwo(); err != nil {
        return err
    }
    return nil
}
"#);
        // Each consequence block spans 3 lines.
        assert_eq!(f.error_loc, 6);
        assert_eq!(f.error_branch_count, 2);
    }

    #[test]
    fn nested_error_checks_are_not_double_counted() {
        let f = one(r#"
package p
func f() error {
    if err := step(); err != nil {
        if retryErr := retry(); retryErr != nil {
            return retryErr
        }
        return err
    }
    return nil
}
"#);
        // Outer handler block spans 6 lines; the inner one lies inside.
        assert_eq!(f.error_loc, 6);
        assert_eq!(f.error_branch_count, 2);
    }

    #[test]
    fn go_has_no_try_statements() {
        let f = one(r#"
package p
func f() error {
    if err := step(); err != nil {
        return err
    }
    return nil
}
"#);
        assert_eq!(f.disjoint_try_count, 0);
        assert_eq!(f.single_stmt_try_count, 0);
    }

    #[test]
    fn methods_are_qualified_by_receiver() {
        let units = extract(
            r#"
package p
func (s *Service) Load() error {
    if err := s.open(); err != nil {
        return err
    }
    return nil
}
"#,
        );
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "Service::Load");
        assert!(units[0].wrap_only_error_path);
    }

    #[test]
    fn non_logging_call_before_return_is_recovery() {
        let f = one(r#"
package p
func f() error {
    if err := step(); err != nil {
        metrics.Increment("failures")
        return err
    }
    return nil
}
"#);
        assert_eq!(f.log_and_rethrow_handlers, 0);
        assert_eq!(f.rethrow_only_handlers, 0);
        assert!(!f.wrap_only_error_path);
    }

    #[test]
    fn else_branch_of_error_check_is_visited_as_happy_path() {
        let f = one(r#"
package p
func f() error {
    if err := step(); err != nil {
        return err
    } else if err2 := other(); err2 != nil {
        return err2
    }
    return nil
}
"#);
        assert_eq!(f.error_branch_count, 2);
        assert_eq!(f.rethrow_only_handlers, 2);
        assert!(f.wrap_only_error_path);
    }

    #[test]
    fn parse_errors_are_reported() {
        let err = extract_error_shapes("package p\nfunc !!! {").unwrap_err();
        assert!(format!("{err}").contains("parse"));
    }

    #[test]
    fn empty_file_yields_no_units() {
        assert!(extract("package p\n").is_empty());
    }
}
