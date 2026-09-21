//! The TypeScript adapter's [`DependenceVocabulary`]: which of the
//! labels [`crate::tree`] emits read a local, bind one, or hold a
//! statement list. Consumed by `lens_domain::build_pdg` for
//! `similarity --method pdg`.
//!
//! Identifiers carry their name as the node value, so a read is any
//! `Ident`; a `Declarator` binds the name it carries. The lowering
//! keeps only the right-hand side of an assignment and drops the
//! binding of `for…in` / `for…of` heads, so a plain `x = …` and a loop
//! variable are reads-only here: modern `const`-first code is fully
//! covered, imperative reassignment is under-approximated.
//!
//! `VarDecl` is deliberately not a loose statement: a declaration can
//! only stand alone in a `for` initialiser or a `case` clause, and in
//! both it folds into the statement that owns it, the way a Rust `for`
//! binds its own pattern.

use lens_domain::{DependenceRole, DependenceVocabulary, TreeNode};

/// TypeScript's dependence vocabulary.
#[derive(Debug, Clone, Copy, Default)]
pub struct TsVocabulary;

/// Statement labels that can sit outside a block: an `else if`, the
/// body of a braceless `if` / loop, a `case` clause member.
const STATEMENT_LABELS: &[&str] = &[
    "If",
    "While",
    "DoWhile",
    "For",
    "ForIn",
    "ForOf",
    "Switch",
    "Return",
    "Throw",
    "Try",
    "ExprStmt",
    "Break",
    "Continue",
    "Empty",
    "Labeled",
    "FunctionDecl",
    "ClassDecl",
    "Stmt",
];

impl DependenceVocabulary for TsVocabulary {
    fn role<'a>(&self, node: &'a TreeNode) -> DependenceRole<'a> {
        match node.label.as_str() {
            "Block" | "FunctionBody" | "Catch" | "Finally" => DependenceRole::StatementList,
            "Ident" => DependenceRole::Reference(&node.value),
            // The initialiser is the declarator's only child; the bound
            // name is its value.
            "Declarator" if !node.value.is_empty() => DependenceRole::Binding {
                names: vec![&node.value],
                targets: Vec::new(),
            },
            _ => DependenceRole::Plain,
        }
    }

    fn is_statement(&self, node: &TreeNode) -> bool {
        STATEMENT_LABELS.contains(&node.label.as_str())
    }

    fn is_loop(&self, node: &TreeNode) -> bool {
        matches!(
            node.label.as_str(),
            "While" | "DoWhile" | "For" | "ForIn" | "ForOf"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{Dialect, TypeScriptParser};
    use lens_domain::{DependenceKind, LanguageParser, Pdg, PdgOptions, build_pdg};

    fn pdg_of(src: &str) -> Pdg {
        let mut parser = TypeScriptParser::with_dialect(Dialect::Ts);
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
            &TsVocabulary,
            PdgOptions::default(),
        )
    }

    fn edges(pdg: &Pdg, kind: DependenceKind) -> Vec<(usize, usize)> {
        pdg.edges_of(kind).collect()
    }

    #[test]
    fn declarations_branches_and_loops() {
        let pdg = pdg_of(
            "function f(x: number): number {
                const a = x + 1;
                let b = a * 2;
                if (b > 3) {
                    return b;
                }
                for (const i of items) {
                    b = b + i;
                }
                return b;
            }",
        );
        // 1 const a, 2 let b, 3 if, 4 return b, 5 for-of, 6 b = b + i, 7 return b.
        assert_eq!(pdg.statement_count(), 7);
        assert_eq!(
            edges(&pdg, DependenceKind::Control),
            vec![(0, 1), (0, 2), (0, 3), (0, 5), (0, 7), (3, 4), (5, 6)]
        );
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![(0, 1), (1, 2), (2, 3), (2, 4), (2, 6), (2, 7)]
        );
        assert_eq!(pdg.nodes[1].kind, "VarDecl");
    }

    #[test]
    fn braceless_and_else_if_bodies_are_nested_statements() {
        let pdg = pdg_of(
            "function f(x: number): number {
                if (x > 1) return x;
                else if (x < 0) return -x;
                return 0;
            }",
        );
        // 1 if, 2 return x, 3 else-if, 4 return -x, 5 return 0.
        assert_eq!(
            edges(&pdg, DependenceKind::Control),
            vec![(0, 1), (0, 5), (1, 2), (1, 3), (3, 4)]
        );
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![(0, 1), (0, 2), (0, 3), (0, 4)]
        );
    }

    #[test]
    fn a_declaration_inside_a_block_is_its_own_statement() {
        let pdg = pdg_of(
            "function f(c: boolean): void {
                if (c) {
                    const x = next();
                    use(x);
                }
            }",
        );
        assert_eq!(pdg.statement_count(), 3);
        assert_eq!(
            edges(&pdg, DependenceKind::Control),
            vec![(0, 1), (1, 2), (1, 3)]
        );
        assert_eq!(edges(&pdg, DependenceKind::Data), vec![(0, 1), (2, 3)]);
    }

    #[test]
    fn an_if_body_flows_once() {
        // Only a loop body reaches its own start: the later declaration
        // in the branch must not feed the earlier read of its name.
        let pdg = pdg_of(
            "function f(c: boolean): number {
                if (c) {
                    const a = f(d);
                    const d = g(a);
                    return d;
                }
                return 0;
            }",
        );
        // 1 if, 2 const a, 3 const d, 4 return d, 5 return 0.
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![(0, 1), (2, 3), (3, 4)]
        );
    }

    #[test]
    fn a_for_initialiser_feeds_the_header_and_body() {
        let pdg = pdg_of(
            "function f(n: number): number {
                let total = 0;
                for (let i = 0; i < n; i++) {
                    total = total + i;
                }
                return total;
            }",
        );
        // 1 let total, 2 for (binding `i` in its initialiser), 3 total = total + i, 4 return.
        assert_eq!(
            edges(&pdg, DependenceKind::Control),
            vec![(0, 1), (0, 2), (0, 4), (2, 3)]
        );
        // The header reads `i` from its own initialiser once the loop
        // has flowed; the body reads both locals; `return` reads the
        // declaration, since plain reassignment is invisible.
        assert_eq!(
            edges(&pdg, DependenceKind::Data),
            vec![(0, 2), (1, 3), (1, 4), (2, 2), (2, 3)]
        );
    }
}
