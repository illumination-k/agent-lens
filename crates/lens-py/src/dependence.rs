//! The Python adapter's [`DependenceVocabulary`]: which of the labels
//! [`crate::parser`] emits read a local, bind one, or hold a statement
//! list. Consumed by `lens_domain::build_pdg` for
//! `similarity --method pdg`.
//!
//! ruff's AST has no block node: a suite's statements are direct
//! children of the compound statement that owns them, after its
//! expression children. So there is no list role here; every statement
//! label answers `is_statement`, and the domain groups the consecutive
//! siblings into one list. `Name` carries the identifier and is a read
//! wherever it is not a binding target; binding targets are the name
//! patterns of assignment, `for`, `with … as`, the walrus, and
//! annotated assignment. ruff's walker visits a value before the
//! targets it is assigned to, so targets sit after the value here.
//! `Elif` / `Else`, `ExceptHandler`, and `MatchCase` wrap their clause,
//! so each arm's suite is its own list.

use lens_domain::{DependenceRole, DependenceVocabulary, TreeNode};

/// Python's dependence vocabulary.
#[derive(Debug, Clone, Copy, Default)]
pub struct PythonVocabulary;

/// Every label `stmt_label` in the parser can emit.
const STATEMENT_LABELS: &[&str] = &[
    "FunctionDef",
    "ClassDef",
    "Return",
    "Delete",
    "Assign",
    "AugAssign",
    "AnnAssign",
    "TypeAlias",
    "For",
    "While",
    "If",
    "With",
    "Match",
    "Raise",
    "Try",
    "Assert",
    "Import",
    "ImportFrom",
    "Global",
    "Nonlocal",
    "Expr",
    "Pass",
    "Break",
    "Continue",
    "IpyEscapeCommand",
];

impl DependenceVocabulary for PythonVocabulary {
    fn role<'a>(&self, node: &'a TreeNode) -> DependenceRole<'a> {
        match node.label.as_str() {
            "Name" => DependenceRole::Reference(&node.value),
            // Value first, then every target: `a = b = value`.
            "Assign" => binding_over(node, |index, child| index >= 1 && is_name_pattern(child)),
            // `x += 1` reads what it writes, so the target stays walked.
            "AugAssign" => DependenceRole::Binding {
                names: node.children.last().map(pattern_names).unwrap_or_default(),
                targets: Vec::new(),
            },
            // Value, annotation, target; `x: int` alone has no value
            // and binds nothing.
            "AnnAssign" if node.children.len() == 3 => {
                binding_over(node, |index, child| index == 2 && is_name_pattern(child))
            }
            // Iterator, target, then the suite; value, target.
            "For" | "Named" => {
                binding_over(node, |index, child| index == 1 && is_name_pattern(child))
            }
            // Context expressions and `as` targets alternate with no
            // marker between them; a bare name in either position is
            // taken as a target, which over-binds `with lock:`.
            "With" => binding_over(node, |_, child| is_name_pattern(child)),
            _ => DependenceRole::Plain,
        }
    }

    fn is_statement(&self, node: &TreeNode) -> bool {
        STATEMENT_LABELS.contains(&node.label.as_str())
    }

    fn is_loop(&self, node: &TreeNode) -> bool {
        matches!(node.label.as_str(), "For" | "While")
    }
}

/// A binding whose targets are the children `is_target` selects.
fn binding_over<'a>(
    node: &'a TreeNode,
    is_target: impl Fn(usize, &TreeNode) -> bool,
) -> DependenceRole<'a> {
    let targets: Vec<usize> = node
        .children
        .iter()
        .enumerate()
        .filter(|(index, child)| is_target(*index, child))
        .map(|(index, _)| index)
        .collect();
    let names = targets
        .iter()
        .filter_map(|&index| node.children.get(index))
        .flat_map(pattern_names)
        .collect();
    DependenceRole::Binding { names, targets }
}

/// A name, or a tuple / list / starred nest of names: the target shapes
/// that bind rather than mutate. `obj.attr = …` and `xs[i] = …` are
/// mutations whose base is read, so they stay walked.
fn is_name_pattern(node: &TreeNode) -> bool {
    match node.label.as_str() {
        "Name" => true,
        "Tuple" | "List" | "Starred" => node.children.iter().all(is_name_pattern),
        _ => false,
    }
}

fn pattern_names(pattern: &TreeNode) -> Vec<&str> {
    let mut out = Vec::new();
    collect_names(pattern, &mut out);
    out
}

fn collect_names<'a>(node: &'a TreeNode, out: &mut Vec<&'a str>) {
    if node.label == "Name" && !node.value.is_empty() {
        out.push(node.value.as_str());
    }
    for child in &node.children {
        collect_names(child, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::PythonParser;
    use lens_domain::{DependenceKind, LanguageParser, Pdg, PdgOptions, build_pdg};
    use rstest::rstest;

    fn pdg_of(src: &str) -> Pdg {
        let mut parser = PythonParser::new();
        let functions = parser.extract_functions(src).unwrap();
        let function = &functions[0];
        let params: Vec<&str> = function
            .signature
            .as_ref()
            .map(|sig| sig.parameter_names.iter().map(String::as_str).collect())
            .unwrap_or_default();
        build_pdg(
            function.body_tree(),
            &params,
            &PythonVocabulary,
            PdgOptions::default(),
        )
    }

    fn edges(pdg: &Pdg, kind: DependenceKind) -> Vec<(usize, usize)> {
        pdg.edges_of(kind).collect()
    }

    #[test]
    fn straight_line_branch_and_loop() {
        let pdg = pdg_of(
            "def f(x):
    a = x + 1
    b = a * 2
    if b > 3:
        return b
    c = 0
    for i in range(b):
        c += i
    return c
",
        );
        // 1 a =, 2 b =, 3 if, 4 return b, 5 c =, 6 for, 7 c += i, 8 return c.
        assert_eq!(pdg.statement_count(), 8);
        assert_eq!(
            edges(&pdg, DependenceKind::Control),
            vec![
                (0, 1),
                (0, 2),
                (0, 3),
                (0, 5),
                (0, 6),
                (0, 8),
                (3, 4),
                (6, 7)
            ]
        );
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![
                (0, 1),
                (1, 2),
                (2, 3),
                (2, 4),
                (2, 6),
                (5, 7),
                (5, 8),
                (6, 7),
                (7, 7),
                (7, 8),
            ]
        );
        assert_eq!(pdg.nodes[1].kind, "Assign");
    }

    #[test]
    fn if_else_arms_both_reach_the_read_after_them() {
        let pdg = pdg_of(
            "def f(c):
    if c:
        x = 1
    else:
        x = 2
    return x
",
        );
        // 1 if, 2 x = 1, 3 x = 2, 4 return x.
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![(0, 1), (2, 4), (3, 4)]
        );
    }

    #[test]
    fn tuple_targets_and_attribute_mutation() {
        let pdg = pdg_of(
            "def f(self, pair):
    a, b = pair
    self.total = a + b
    return self.total
",
        );
        // `self` is a parameter of a free function, so both attribute
        // statements read it from the entry; the tuple binds `a` and
        // `b` for the sum, and the return reads nothing local.
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![(0, 1), (0, 2), (0, 3), (1, 2)]
        );
    }

    #[test]
    fn try_arms_and_match_cases_are_separate_lists() {
        let pdg = pdg_of(
            "def f(c):
    try:
        x = load(c)
    except Error:
        x = None
    match c:
        case 1:
            y = x
        case _:
            y = 0
    return y
",
        );
        // 1 try, 2 x = load, 3 x = None, 4 match, 5 y = x, 6 y = 0, 7 return.
        assert_eq!(
            edges(&pdg, DependenceKind::Control),
            vec![(0, 1), (0, 4), (0, 7), (1, 2), (1, 3), (4, 5), (4, 6)]
        );
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![(0, 2), (0, 4), (2, 5), (3, 5), (5, 7), (6, 7)]
        );
    }

    #[test]
    fn annotated_and_for_targets_bind_only_the_target() {
        let pdg = pdg_of(
            "def f(x, xs):
    y: int = x
    w = x
    for i in xs:
        y += i
    return w + y + len(xs)
",
        );
        // 1 y: int = x, 2 w = x, 3 for, 4 y += i, 5 return.
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![
                (0, 1),
                (0, 2),
                (0, 3),
                (0, 5),
                (1, 4),
                (1, 5),
                (2, 5),
                (3, 4),
                (4, 4),
                (4, 5),
            ]
        );
    }

    #[test]
    fn an_if_body_flows_once() {
        let pdg = pdg_of(
            "def f(c):
    b = 0
    if c:
        a = f(b)
        b = g(a)
    return b
",
        );
        // 1 b = 0, 2 if, 3 a = f(b), 4 b = g(a), 5 return.
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![(0, 2), (1, 3), (1, 5), (3, 4), (4, 5)]
        );
    }

    #[test]
    fn with_binds_its_as_target() {
        let pdg = pdg_of(
            "def f(path):
    with open(path) as fh:
        data = fh.read()
    return data
",
        );
        // 1 with, 2 data =, 3 return.
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![(0, 1), (1, 2), (2, 3)]
        );
    }

    #[rstest]
    #[case::name(TreeNode::new("Name", "x"), true)]
    #[case::tuple(
        TreeNode::with_children("Tuple", "", vec![TreeNode::new("Name", "a"), TreeNode::new("Name", "b")]),
        true
    )]
    #[case::attribute(
        TreeNode::with_children("Attribute", "f", vec![TreeNode::new("Name", "obj")]),
        false
    )]
    #[case::tuple_with_subscript(
        TreeNode::with_children("Tuple", "", vec![TreeNode::new("Name", "a"), TreeNode::leaf("Subscript")]),
        false
    )]
    fn name_pattern_shapes(#[case] node: TreeNode, #[case] expected: bool) {
        assert_eq!(is_name_pattern(&node), expected);
    }
}
