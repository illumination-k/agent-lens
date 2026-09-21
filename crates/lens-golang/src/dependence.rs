//! The Go adapter's [`DependenceVocabulary`]: which of the tree-sitter
//! node kinds [`crate::parser`] passes through as labels read a local,
//! bind one, or hold a statement list. Consumed by
//! `lens_domain::build_pdg` for `similarity --method pdg`.
//!
//! `identifier` carries its text and is a read wherever it is not a
//! binding target; `field_identifier` (the `f` of `x.f`) is not a local.
//! Every statement run is a `statement_list` inside a `block`, so both
//! are lists; the one statement that appears outside a list is an
//! `else if`. The initialiser and post statement of a `for` clause, and
//! the initialiser of an `if`, are bindings folded into the statement
//! that owns them, the way a Rust `for` binds its own pattern.

use lens_domain::{DependenceRole, DependenceVocabulary, TreeNode};

/// Go's dependence vocabulary.
#[derive(Debug, Clone, Copy, Default)]
pub struct GoVocabulary;

/// Statement kinds that can sit outside a `statement_list`.
const STATEMENT_KINDS: &[&str] = &[
    "if_statement",
    "for_statement",
    "expression_switch_statement",
    "type_switch_statement",
    "select_statement",
    "return_statement",
    "go_statement",
    "defer_statement",
    "expression_statement",
    "send_statement",
    "labeled_statement",
    "var_declaration",
    "const_declaration",
    "type_declaration",
    "break_statement",
    "continue_statement",
    "goto_statement",
    "fallthrough_statement",
    "empty_statement",
];

impl DependenceVocabulary for GoVocabulary {
    fn role<'a>(&self, node: &'a TreeNode) -> DependenceRole<'a> {
        match node.label.as_str() {
            "block" | "statement_list" => DependenceRole::StatementList,
            "identifier" if !node.value.is_empty() => DependenceRole::Reference(&node.value),
            // `left := right`: the left list binds, the right is read.
            "short_var_declaration" => list_binding(node, 0),
            // `left = right` rebinds; `left op= right` also reads. The
            // parser carries the operator as the node value.
            "assignment_statement" if node.value == "=" => list_binding(node, 0),
            "assignment_statement" => DependenceRole::Binding {
                names: node.children.first().map(list_names).unwrap_or_default(),
                targets: Vec::new(),
            },
            // `var a, b T = …`: leading identifiers are the names.
            "var_spec" | "const_spec" => {
                let targets: Vec<usize> = node
                    .children
                    .iter()
                    .enumerate()
                    .take_while(|(_, child)| child.label == "identifier")
                    .map(|(index, _)| index)
                    .collect();
                let names = targets
                    .iter()
                    .filter_map(|&index| node.children.get(index))
                    .map(|child| child.value.as_str())
                    .collect();
                DependenceRole::Binding { names, targets }
            }
            // `for k, v := range xs` / `v, ok := <-ch` in a `select`
            // case: a left list is present only when two children are.
            "range_clause" | "receive_statement" if node.children.len() == 2 => {
                list_binding(node, 0)
            }
            "inc_statement" | "dec_statement" => DependenceRole::Binding {
                names: node.children.first().map(list_names).unwrap_or_default(),
                targets: Vec::new(),
            },
            _ => DependenceRole::Plain,
        }
    }

    fn is_statement(&self, node: &TreeNode) -> bool {
        STATEMENT_KINDS.contains(&node.label.as_str())
    }

    fn is_loop(&self, node: &TreeNode) -> bool {
        node.label == "for_statement"
    }
}

/// A binding whose target is the child at `index` when every entry of
/// that list is a bare identifier. `x.f, y = …` mixes a mutation in, and
/// a mutation reads its base, so the list then stays walked and only
/// its bare identifiers bind.
fn list_binding(node: &TreeNode, index: usize) -> DependenceRole<'_> {
    let Some(list) = node.children.get(index) else {
        return DependenceRole::Plain;
    };
    let names = list_names(list);
    let all_bare = list_entries(list).all(|entry| entry.label == "identifier");
    DependenceRole::Binding {
        names,
        targets: if all_bare { vec![index] } else { Vec::new() },
    }
}

/// The identifiers directly in an expression list (or the single
/// expression that stands for one).
fn list_names(list: &TreeNode) -> Vec<&str> {
    list_entries(list)
        .filter(|entry| entry.label == "identifier" && !entry.value.is_empty())
        .map(|entry| entry.value.as_str())
        .collect()
}

fn list_entries(list: &TreeNode) -> impl Iterator<Item = &TreeNode> {
    if list.label == "expression_list" {
        list.children.iter()
    } else {
        std::slice::from_ref(list).iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::GoParser;
    use lens_domain::{DependenceKind, LanguageParser, Pdg, PdgOptions, build_pdg};

    fn pdg_of(src: &str) -> Pdg {
        let mut parser = GoParser::new();
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
            &GoVocabulary,
            PdgOptions::default(),
        )
    }

    fn edges(pdg: &Pdg, kind: DependenceKind) -> Vec<(usize, usize)> {
        pdg.edges_of(kind).collect()
    }

    #[test]
    fn straight_line_branch_and_loop() {
        let pdg = pdg_of(
            "package p

func f(x int) int {
	a := x + 1
	b := a * 2
	if b > 3 {
		return b
	}
	c := 0
	for i := 0; i < b; i++ {
		c += i
	}
	return c
}
",
        );
        // 1 a :=, 2 b :=, 3 if, 4 return b, 5 c :=, 6 for, 7 c += i, 8 return c.
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
        // The `for` clause binds `i` and reads it in its condition and
        // post statement, so once the body has flowed it depends on
        // itself; the accumulator reads its own previous iteration.
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
                (6, 6),
                (6, 7),
                (7, 7),
                (7, 8),
            ]
        );
        assert_eq!(pdg.nodes[1].kind, "short_var_declaration");
    }

    #[test]
    fn range_loops_else_if_and_var_specs() {
        let pdg = pdg_of(
            "package p

func f(xs []int) int {
	var total int
	for _, v := range xs {
		if v > 0 {
			total = total + v
		} else if v < 0 {
			total = total - v
		}
	}
	return total
}
",
        );
        // 1 var total, 2 for, 3 if, 4 total = total + v, 5 else-if, 6 total = total - v, 7 return.
        assert_eq!(
            edges(&pdg, DependenceKind::Control),
            vec![(0, 1), (0, 2), (0, 7), (2, 3), (3, 4), (3, 5), (5, 6)]
        );
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![
                (0, 2),
                (1, 4),
                (1, 6),
                (1, 7),
                (2, 3),
                (2, 4),
                (2, 5),
                (2, 6),
                (4, 4),
                (4, 6),
                (4, 7),
                (6, 4),
                (6, 6),
                (6, 7),
            ]
        );
    }

    #[test]
    fn a_selector_mutation_reads_its_base() {
        let pdg = pdg_of(
            "package p

func f(s *S, v int) {
	s.count = v
	s.count, v = v, 0
}
",
        );
        // Both statements read `s`; the second binds `v` while reading
        // it, since its left list is not all bare identifiers.
        assert_eq!(edges(&pdg, DependenceKind::Data), vec![(0, 1), (0, 2)]);
    }
}
