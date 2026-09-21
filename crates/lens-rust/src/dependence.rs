//! The Rust adapter's [`DependenceVocabulary`]: which of the labels
//! [`crate::parser`] emits read a local, bind one, or hold a statement
//! list. Consumed by `lens_domain::build_pdg` for
//! `similarity --method pdg`.
//!
//! The lowering keeps names in labels rather than values (`Path(x)`,
//! `CallPath(f)`), so a read is recognised by label shape: a
//! single-segment, lowercase-initial `Path(...)`. Multi-segment paths
//! are items, uppercase-initial ones are unit variants and constants;
//! neither is a local. Bindings are the pattern positions of `let`,
//! `for`, `if let` / `while let`, match arms, and closure parameters,
//! plus `=` and the compound-assignment operators on a bare path.
//!
//! Macro invocations are opaque leaves in the lowering, so a local read
//! only inside `println!` / `format!` / `vec!` arguments is invisible
//! here, as it is to every other comparison over the tree.

use lens_domain::{DependenceRole, DependenceVocabulary, TreeNode, starts_uppercase};

/// Rust's dependence vocabulary.
#[derive(Debug, Clone, Copy, Default)]
pub struct RustVocabulary;

/// The `syn::BinOp` spellings that assign to their left operand.
const COMPOUND_ASSIGNMENT_OPERATORS: &[&str] =
    &["+=", "-=", "*=", "/=", "%=", "^=", "&=", "|=", "<<=", ">>="];

impl DependenceVocabulary for RustVocabulary {
    fn role<'a>(&self, node: &'a TreeNode) -> DependenceRole<'a> {
        let label = node.label.as_str();
        if label == "Block" {
            return DependenceRole::StatementList;
        }
        if let Some(name) = local_path_name(label) {
            return DependenceRole::Reference(name);
        }
        match label {
            // Pattern first, then initialiser (and `let … else` block).
            "Let" | "For" | "LetExpr" | "MatchArm" => DependenceRole::Binding {
                names: node.children.first().map(pattern_names).unwrap_or_default(),
                targets: vec![0],
            },
            "Assign" => match node
                .children
                .first()
                .and_then(|left| local_path_name(&left.label))
            {
                Some(name) => DependenceRole::Binding {
                    names: vec![name],
                    targets: vec![0],
                },
                // `x.field = …`, `xs[i] = …`: the base is read, not rebound.
                None => DependenceRole::Plain,
            },
            // Closure parameters are scoped to the closure, which is
            // part of the statement it sits in: skipping the patterns
            // keeps them out of the reads, and binding nothing keeps
            // them from shadowing the outer name for later statements.
            "Closure" => DependenceRole::Binding {
                names: Vec::new(),
                targets: node
                    .children
                    .iter()
                    .enumerate()
                    .take_while(|(_, child)| child.label.starts_with("Pat"))
                    .map(|(index, _)| index)
                    .collect(),
            },
            _ => match compound_assignment_target(node) {
                Some(name) => DependenceRole::Binding {
                    names: vec![name],
                    targets: Vec::new(),
                },
                None => DependenceRole::Plain,
            },
        }
    }

    fn is_loop(&self, node: &TreeNode) -> bool {
        matches!(node.label.as_str(), "For" | "While" | "Loop")
    }
}

/// The local a `Path(name)` label reads, if it reads one.
fn local_path_name(label: &str) -> Option<&str> {
    let name = label.strip_prefix("Path(")?.strip_suffix(')')?;
    (!name.contains("::") && !starts_uppercase(name)).then_some(name)
}

/// The names a pattern subtree binds: every `PatIdent` /
/// `PatIdentMut` value under it.
fn pattern_names(pattern: &TreeNode) -> Vec<&str> {
    let mut out = Vec::new();
    collect_pattern_names(pattern, &mut out);
    out
}

fn collect_pattern_names<'a>(node: &'a TreeNode, out: &mut Vec<&'a str>) {
    if matches!(node.label.as_str(), "PatIdent" | "PatIdentMut") && !node.value.is_empty() {
        out.push(node.value.as_str());
    }
    for child in &node.children {
        collect_pattern_names(child, out);
    }
}

/// The local a `Binary(op=)` node assigns to, when its operator is a
/// compound assignment and its left operand a bare local.
fn compound_assignment_target(node: &TreeNode) -> Option<&str> {
    let op = node.label.strip_prefix("Binary(")?.strip_suffix(')')?;
    if !COMPOUND_ASSIGNMENT_OPERATORS.contains(&op) {
        return None;
    }
    local_path_name(&node.children.first()?.label)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::RustParser;
    use lens_domain::{DependenceKind, LanguageParser, Pdg, PdgOptions, build_pdg};
    use rstest::rstest;

    fn pdg_of(src: &str) -> Pdg {
        let mut parser = RustParser;
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
            &RustVocabulary,
            PdgOptions::default(),
        )
    }

    fn edges(pdg: &Pdg, kind: DependenceKind) -> Vec<(usize, usize)> {
        pdg.edges_of(kind).collect()
    }

    #[test]
    fn straight_line_branch_and_loop() {
        let pdg = pdg_of(
            "fn f(x: i32) -> i32 {
                let a = x + 1;
                let b = a * 2;
                if b > 3 {
                    return b;
                }
                let mut c = 0;
                for i in 0..b {
                    c += i;
                }
                c
            }",
        );
        // 1 let a, 2 let b, 3 if, 4 return b, 5 let c, 6 for, 7 c += i, 8 c.
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
        assert_eq!(pdg.nodes[7].kind, "ExprStmt");
        assert_eq!(pdg.nodes[8].kind, "\u{0}ref");
    }

    #[test]
    fn match_arms_and_if_let_bind_their_patterns() {
        let pdg = pdg_of(
            "fn f(opt: Option<i32>) -> i32 {
                if let Some(v) = opt {
                    return v;
                }
                match opt {
                    Some(w) => w,
                    None => 0,
                }
            }",
        );
        // 1 if let, 2 return v, 3 match (arm bodies are expressions).
        assert_eq!(pdg.statement_count(), 3);
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![(0, 1), (0, 3), (1, 2)]
        );
    }

    #[test]
    fn closure_parameters_do_not_shadow_outer_locals_for_later_statements() {
        // The closure's `x` is read as the outer parameter (an
        // over-approximation), and the tail `x` still reads the
        // parameter rather than the closure.
        let pdg = pdg_of(
            "fn f(x: i32, xs: Vec<i32>) -> i32 {
                let total = xs.iter().map(|x| x + 1).sum::<i32>();
                total + x
            }",
        );
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![(0, 1), (0, 2), (1, 2)]
        );
    }

    #[rstest]
    #[case::local("Path(value)", Some("value"))]
    #[case::qualified("Path(std::mem::take)", None)]
    #[case::variant("Path(None)", None)]
    #[case::call_label("CallPath(value)", None)]
    #[case::other("Let", None)]
    fn local_path_name_reads_single_segment_lowercase_paths(
        #[case] label: &str,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(local_path_name(label), expected);
    }

    #[test]
    fn field_assignment_reads_its_base() {
        let pdg = pdg_of(
            "fn f(mut s: S, v: i32) {
                s.field = v;
                s.other += 1;
            }",
        );
        assert_eq!(edges(&pdg, DependenceKind::Data), vec![(0, 1), (0, 2)]);
    }

    #[test]
    fn while_header_reads_the_body_assignment() {
        let pdg = pdg_of(
            "fn f(n: i32) -> i32 {
                let mut i = 0;
                while i < n {
                    i += 1;
                }
                i
            }",
        );
        // 1 let i, 2 while, 3 i += 1, 4 i.
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![(0, 2), (1, 2), (1, 3), (1, 4), (3, 2), (3, 3), (3, 4)]
        );
    }
}
