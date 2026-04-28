//! tree-sitter-based complexity extraction for Go source files.
//!
//! For every top-level function and method we walk the body and produce
//! a [`FunctionComplexity`]:
//!
//! * **Cyclomatic Complexity** — McCabe; starts at 1 and is incremented
//!   for each branching construct (`if`, `for`, each switch/select case
//!   beyond the first, and `&&` / `||`).
//! * **Cognitive Complexity** — Sonar-style; control structures add
//!   `1 + nesting`, so deeply nested code scores higher than the same
//!   number of flat branches.
//! * **Max Nesting Depth** — the deepest control-flow nesting reached in
//!   the function body.
//! * **Halstead counts** — identifiers and literals are operands;
//!   keywords, operators, and punctuation are operators.
//!
//! Function literals inside a function body contribute to the enclosing
//! function's score, matching how the similarity parser treats Go
//! closures as part of their parent function.

use std::collections::HashMap;

use lens_domain::{FunctionComplexity, HalsteadCounts, qualify};
use tree_sitter::Node;

use crate::parser::{GoParseError, function_name_text, method_receiver_type, parse_tree};

/// Extract one [`FunctionComplexity`] per function-shaped item in
/// `source`. Methods are reported as `Receiver::method`; free functions
/// keep their bare name.
pub fn extract_complexity_units(source: &str) -> Result<Vec<FunctionComplexity>, GoParseError> {
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
) -> Option<FunctionComplexity> {
    let body = node.child_by_field_name("body")?;
    let name = qualify(owner, function_name_text(node, source)?);
    let mut visitor = ComplexityVisitor::new(source);
    visitor.visit_node(body);
    Some(FunctionComplexity {
        name,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        cyclomatic: 1 + visitor.cyclomatic_branches,
        cognitive: visitor.cognitive,
        max_nesting: visitor.max_nesting,
        halstead: visitor.halstead_counts(),
    })
}

#[derive(Default)]
struct HalsteadAcc {
    operators: HashMap<String, usize>,
    operands: HashMap<String, usize>,
}

impl HalsteadAcc {
    fn op(&mut self, s: &str) {
        bump_count(&mut self.operators, s);
    }

    fn operand(&mut self, s: &str) {
        bump_count(&mut self.operands, s);
    }
}

fn bump_count(map: &mut HashMap<String, usize>, s: &str) {
    *map.entry(s.to_owned()).or_insert(0) += 1;
}

struct ComplexityVisitor<'a> {
    source: &'a [u8],
    cyclomatic_branches: u32,
    cognitive: u32,
    nesting: u32,
    max_nesting: u32,
    halstead: HalsteadAcc,
}

impl<'a> ComplexityVisitor<'a> {
    fn new(source: &'a [u8]) -> Self {
        Self {
            source,
            cyclomatic_branches: 0,
            cognitive: 0,
            nesting: 0,
            max_nesting: 0,
            halstead: HalsteadAcc::default(),
        }
    }

    fn halstead_counts(&self) -> HalsteadCounts {
        HalsteadCounts {
            distinct_operators: self.halstead.operators.len(),
            distinct_operands: self.halstead.operands.len(),
            total_operators: self.halstead.operators.values().sum(),
            total_operands: self.halstead.operands.values().sum(),
        }
    }

    fn enter_nest(&mut self) {
        self.nesting += 1;
        self.max_nesting = self.max_nesting.max(self.nesting);
    }

    fn exit_nest(&mut self) {
        self.nesting = self.nesting.saturating_sub(1);
    }

    fn add_branch(&mut self) {
        self.cyclomatic_branches += 1;
        self.cognitive += 1 + self.nesting;
    }

    fn visit_node(&mut self, node: Node<'_>) {
        if !node.is_named() {
            self.record_operator_token(node);
            return;
        }

        match node.kind() {
            "if_statement" => self.visit_if(node),
            "for_statement" => self.visit_control_with_block(node, "for"),
            "expression_switch_statement" | "type_switch_statement" => {
                self.visit_case_control(node, "switch");
            }
            "select_statement" => self.visit_case_control(node, "select"),
            "binary_expression" => self.visit_binary_expression(node),
            _ => {
                self.record_named_node(node);
                self.visit_children(node);
            }
        }
    }

    fn visit_if(&mut self, node: Node<'_>) {
        self.add_branch();
        self.halstead.op("if");
        let mut saw_else = false;
        for_each_child(node, |child| {
            if child.kind() == "else" {
                saw_else = true;
                self.visit_node(child);
                return;
            }

            if child.kind() == "block" {
                if saw_else {
                    self.cognitive += 1;
                    saw_else = false;
                }
                self.enter_nest();
                self.visit_node(child);
                self.exit_nest();
            } else {
                saw_else = false;
                self.visit_node(child);
            }
        });
    }

    fn visit_control_with_block(&mut self, node: Node<'_>, op: &str) {
        self.add_branch();
        self.halstead.op(op);
        self.visit_control_children(node, true);
    }

    fn visit_case_control(&mut self, node: Node<'_>, op: &str) {
        let arms = u32::try_from(count_decision_case_nodes(node)).unwrap_or(u32::MAX);
        self.cyclomatic_branches += arms.saturating_sub(1);
        self.cognitive += 1 + self.nesting;
        self.halstead.op(op);
        self.visit_control_children(node, false);
    }

    fn visit_binary_expression(&mut self, node: Node<'_>) {
        if logical_operator_text(node, self.source).is_some() {
            self.cyclomatic_branches += 1;
            self.cognitive += 1;
        }
        self.record_named_node(node);
        self.visit_children(node);
    }

    fn visit_control_children(&mut self, node: Node<'_>, nest_blocks: bool) {
        for_each_child(node, |child| {
            if should_nest_control_child(child, nest_blocks) {
                self.enter_nest();
                self.visit_node(child);
                self.exit_nest();
            } else {
                self.visit_node(child);
            }
        });
    }

    fn visit_children(&mut self, node: Node<'_>) {
        for_each_child(node, |child| self.visit_node(child));
    }

    fn record_named_node(&mut self, node: Node<'_>) {
        match node.kind() {
            "identifier" | "field_identifier" | "package_identifier" | "type_identifier"
            | "label_name" => self.record_operand_text(node),
            "int_literal"
            | "float_literal"
            | "imaginary_literal"
            | "rune_literal"
            | "raw_string_literal"
            | "interpreted_string_literal" => self.record_operand_text(node),
            "true" | "false" | "nil" => self.halstead.operand(node.kind()),
            "return_statement" => self.halstead.op("return"),
            "go_statement" => self.halstead.op("go"),
            "defer_statement" => self.halstead.op("defer"),
            "fallthrough_statement" => self.halstead.op("fallthrough"),
            "break_statement" => self.halstead.op("break"),
            "continue_statement" => self.halstead.op("continue"),
            "assignment_statement" | "short_var_declaration" => self.halstead.op("="),
            "var_declaration" => self.halstead.op("var"),
            "const_declaration" => self.halstead.op("const"),
            "type_declaration" => self.halstead.op("type"),
            "call_expression" => self.halstead.op("call"),
            _ => {}
        }
    }

    fn record_operator_token(&mut self, node: Node<'_>) {
        let kind = node.kind();
        if is_operator_token(kind) || is_keyword_token(kind) {
            self.halstead.op(kind);
        }
    }

    fn record_operand_text(&mut self, node: Node<'_>) {
        if let Ok(text) = node.utf8_text(self.source) {
            self.halstead.operand(text);
        }
    }
}

fn for_each_child(node: Node<'_>, mut f: impl FnMut(Node<'_>)) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        f(child);
    }
}

fn count_decision_case_nodes(node: Node<'_>) -> usize {
    let mut count = 0;
    for_each_child(node, |child| {
        if is_non_default_case_node(child) {
            count += 1;
        }
    });
    count
}

fn should_nest_control_child(node: Node<'_>, nest_blocks: bool) -> bool {
    match node.kind() {
        "expression_case" | "type_case" | "communication_case" | "default_case" => true,
        "block" => nest_blocks,
        _ => false,
    }
}

fn is_non_default_case_node(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "expression_case" | "type_case" | "communication_case"
    )
}

fn logical_operator_text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    let mut cursor = node.walk();
    node.children(&mut cursor)
        .filter(|child| !child.is_named())
        .filter_map(|child| child.utf8_text(source).ok())
        .find(|text| matches!(*text, "&&" | "||"))
}

fn is_operator_token(kind: &str) -> bool {
    matches!(
        kind,
        "+" | "-"
            | "*"
            | "/"
            | "%"
            | "&"
            | "|"
            | "^"
            | "<<"
            | ">>"
            | "&^"
            | "+="
            | "-="
            | "*="
            | "/="
            | "%="
            | "&="
            | "|="
            | "^="
            | "<<="
            | ">>="
            | "&^="
            | "&&"
            | "||"
            | "<-"
            | "++"
            | "--"
            | "=="
            | "<"
            | ">"
            | "="
            | "!"
            | "!="
            | "<="
            | ">="
            | ":="
            | "..."
            | "."
            | ","
            | ";"
            | ":"
    )
}

fn is_keyword_token(kind: &str) -> bool {
    matches!(
        kind,
        "break"
            | "default"
            | "func"
            | "interface"
            | "select"
            | "case"
            | "defer"
            | "go"
            | "map"
            | "struct"
            | "chan"
            | "else"
            | "goto"
            | "package"
            | "switch"
            | "const"
            | "fallthrough"
            | "if"
            | "range"
            | "type"
            | "continue"
            | "for"
            | "import"
            | "return"
            | "var"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lens_domain::HalsteadCounts;
    use rstest::rstest;

    fn extract(src: &str) -> Vec<FunctionComplexity> {
        extract_complexity_units(src).unwrap()
    }

    fn one(src: &str) -> FunctionComplexity {
        let mut units = extract(src);
        assert_eq!(
            units.len(),
            1,
            "expected exactly one function, got {}",
            units.len()
        );
        units.remove(0)
    }

    #[rstest]
    #[case::linear_function("package p\nfunc noop() { x := 1 + 2; _ = x }\n", 1, 0, 0)]
    #[case::single_if(
        "package p\nfunc f(x int) int {\n    if x > 0 { return 1 } else { return 0 }\n}\n",
        2,
        2,
        1
    )]
    #[case::logical_operators(
        "package p\nfunc f(a, b, c bool) bool { return a && b || c }\n",
        3,
        2,
        0
    )]
    #[case::for_loop("package p\nfunc f() { for i := 0; i < 10; i++ { _ = i } }\n", 2, 1, 1)]
    #[case::nested_control_flow(
        r#"
package p
func f(xs []int) int {
    total := 0
    for _, x := range xs {
        if x > 0 {
            total += x
        }
    }
    return total
}
"#,
        3,
        3,
        2
    )]
    #[case::switch_cases(
        r#"
package p
func f(n int) int {
    switch n {
    case 0:
        return 0
    case 1:
        return 1
    default:
        return 2
    }
}
"#,
        2,
        1,
        1
    )]
    #[case::select_cases(
        r#"
package p
func f(ch chan int, out chan<- int) {
    select {
    case x := <-ch:
        out <- x
    case out <- 1:
    default:
    }
}
"#,
        2,
        1,
        1
    )]
    #[case::switch_inside_if_adds_nesting_penalty(
        r#"
package p
func f(n int) int {
    if n > 0 {
        switch n {
        case 1:
            return 1
        default:
            return 0
        }
    }
    return 0
}
"#,
        2,
        3,
        2
    )]
    fn complexity_metrics_match(
        #[case] src: &str,
        #[case] cyclomatic: u32,
        #[case] cognitive: u32,
        #[case] max_nesting: u32,
    ) {
        let f = one(src);
        assert_eq!(f.cyclomatic, cyclomatic);
        assert_eq!(f.cognitive, cognitive);
        assert_eq!(f.max_nesting, max_nesting);
    }

    #[test]
    fn methods_are_qualified_by_receiver() {
        let units = extract("package p\nfunc (s *Service) Compute(x int) int { return x }\n");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].name, "Service::Compute");
    }

    #[test]
    fn extracts_top_level_function_name_and_lines() {
        let units = extract(
            "package p\n\nfunc First() int {\n    return 1\n}\n\nfunc Second() int {\n    return 2\n}\n",
        );
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].name, "First");
        assert_eq!(units[0].start_line, 3);
        assert_eq!(units[0].end_line, 5);
        assert_eq!(units[1].name, "Second");
    }

    #[test]
    fn halstead_counts_are_populated() {
        let f = one("package p\nfunc f(x int) int { y := x + 1; return y }\n");
        assert!(f.halstead.total_operators > 0);
        assert!(f.halstead.total_operands > 0);
        assert!(f.maintainability_index().is_some());
    }

    #[test]
    fn halstead_counts_include_go_named_node_categories() {
        let f = one(r#"
package p

import "fmt"

func f(ch chan int, svc Service, any interface{}) int {
    type local int
    const c = 1.5
    var total local = 0
    total = local(c)
    fmt.Println(total, true, false, nil, 1i, 'x', `raw`, "str")
    go svc.Start()
    defer svc.Stop()
    switch total {
    case 0:
        fallthrough
    default:
    }
loop:
    for total < 10 {
        total += 1
        break loop
        continue
    }
    return int(total)
}
"#);

        assert_eq!(
            f.halstead,
            HalsteadCounts {
                distinct_operators: 20,
                distinct_operands: 21,
                total_operators: 49,
                total_operands: 34,
            }
        );
    }

    #[rstest]
    #[case::plus("+", true)]
    #[case::logical_and("&&", true)]
    #[case::left_paren("(", false)]
    #[case::identifier("identifier", false)]
    fn operator_token_classification_is_precise(#[case] kind: &str, #[case] expected: bool) {
        assert_eq!(is_operator_token(kind), expected);
    }

    #[rstest]
    #[case::func("func", true)]
    #[case::select("select", true)]
    #[case::left_paren("(", false)]
    #[case::identifier("identifier", false)]
    fn keyword_token_classification_is_precise(#[case] kind: &str, #[case] expected: bool) {
        assert_eq!(is_keyword_token(kind), expected);
    }

    #[test]
    fn control_child_nesting_is_limited_to_blocks_and_cases() {
        let tree = parse_tree(
            r#"
package p
func f() {
    for i := 0; i < 10; i++ {
        switch i {
        case 1:
        default:
        }
    }
}
"#,
        )
        .unwrap();
        let for_stmt = first_descendant(tree.root_node(), "for_statement").unwrap();
        let for_clause = first_child_of_kind(for_stmt, "for_clause").unwrap();
        let block = first_child_of_kind(for_stmt, "block").unwrap();
        let case = first_descendant(for_stmt, "expression_case").unwrap();
        let default_case = first_descendant(for_stmt, "default_case").unwrap();

        assert!(!should_nest_control_child(for_clause, true));
        assert!(should_nest_control_child(block, true));
        assert!(!should_nest_control_child(block, false));
        assert!(should_nest_control_child(case, false));
        assert!(should_nest_control_child(default_case, false));
    }

    fn first_descendant<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
        if node.kind() == kind {
            return Some(node);
        }

        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find_map(|child| first_descendant(child, kind))
    }

    fn first_child_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find(|child| child.kind() == kind)
    }

    #[test]
    fn parse_errors_are_reported() {
        let err = extract_complexity_units("package p\nfunc !!! {").unwrap_err();
        assert!(format!("{err}").contains("parse"));
    }
}
