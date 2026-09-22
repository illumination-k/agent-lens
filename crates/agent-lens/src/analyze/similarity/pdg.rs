//! Dependence-graph similarity scoring.
//!
//! The third body-scoring method: each function body is lowered to a
//! program dependence graph — one node per statement, control and data
//! dependence edges between them — and a pair is scored by the
//! Weisfeiler-Lehman kernel over the two graphs. Reordering independent
//! statements, renaming locals, and threading a value through a
//! differently named temporary leave the graph alone, so this is the
//! method for the clone a tree-edit distance scores low on. The graph
//! construction and the kernel live in [`lens_domain::pdg`]; this
//! module only picks the vocabulary each unit's language owns and
//! precomputes the features once per unit.

use lens_domain::{DependenceVocabulary, PdgFeatures, PdgOptions, build_pdg};

use super::OwnedUnit;
use crate::analyze::SourceLang;

/// Kernel features of one unit's dependence graph, precomputed once so
/// pairwise scoring never rebuilds the graph.
#[derive(Debug)]
pub(super) struct PdgProfile {
    features: PdgFeatures,
}

impl PdgProfile {
    /// Lower `unit`'s body through its language's vocabulary. The
    /// signature's parameter names seed the entry node, so a read of a
    /// parameter is a dependence and a read of a free name is not.
    pub(super) fn from_unit(unit: &OwnedUnit, compare_values: bool) -> Self {
        let parameters: Vec<&str> = unit
            .signature()
            .map(|signature| signature.parameter_names().collect())
            .unwrap_or_default();
        let pdg = build_pdg(
            unit.body_tree(),
            &parameters,
            vocabulary_for(unit.lang),
            PdgOptions { compare_values },
        );
        Self {
            features: pdg.features(),
        }
    }

    #[cfg(test)]
    pub(super) fn statement_count(&self) -> usize {
        self.features.statement_count()
    }
}

/// Graph-kernel similarity of two profiles, in `[0.0, 1.0]`; see
/// [`lens_domain::pdg_similarity`].
pub(super) fn pdg_similarity(a: &PdgProfile, b: &PdgProfile) -> f64 {
    lens_domain::pdg_similarity(&a.features, &b.features)
}

/// The vocabulary that reads the labels `lang`'s parser emits.
fn vocabulary_for(lang: SourceLang) -> &'static dyn DependenceVocabulary {
    match lang {
        SourceLang::Rust => &lens_rust::RustVocabulary,
        SourceLang::TypeScript(_) => &lens_ts::TsVocabulary,
        SourceLang::Python => &lens_py::PythonVocabulary,
        SourceLang::Go => &lens_golang::GoVocabulary,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lens_domain::{FunctionDef, FunctionShape, FunctionSignature, ReceiverShape, TreeNode};
    use rstest::rstest;
    use std::path::PathBuf;

    fn unit(lang: SourceLang, tree: TreeNode, parameter_names: &[&str]) -> OwnedUnit {
        OwnedUnit {
            file: PathBuf::from("lib"),
            rel_path: "lib".to_owned(),
            is_test: false,
            kind: None,
            implements: None,
            lang,
            shape: FunctionShape::from(FunctionDef {
                name: "f".to_owned(),
                start_line: 1,
                end_line: 5,
                is_test: false,
                signature: Some(FunctionSignature {
                    name_tokens: vec!["f".to_owned()],
                    parameter_count: parameter_names.len(),
                    parameter_names: parameter_names.iter().map(|n| (*n).to_owned()).collect(),
                    parameter_type_paths: Vec::new(),
                    return_type_paths: Vec::new(),
                    generics: Vec::new(),
                    receiver: ReceiverShape::None,
                }),
                doc: None,
                implements: None,
                tree,
            }),
        }
    }

    /// Each language's vocabulary is reached through its variant: the
    /// same three-statement chain, spelled in each lowering's labels,
    /// yields the same graph shape.
    #[rstest]
    #[case::rust(
        SourceLang::Rust,
        TreeNode::with_children(
            "Block",
            "",
            vec![
                TreeNode::with_children(
                    "Let",
                    "",
                    vec![TreeNode::new("PatIdent", "a"), TreeNode::leaf("Path(x)")],
                ),
                TreeNode::with_children(
                    "Let",
                    "",
                    vec![TreeNode::new("PatIdent", "b"), TreeNode::leaf("Path(a)")],
                ),
                TreeNode::leaf("Path(b)"),
            ],
        )
    )]
    #[case::python(
        SourceLang::Python,
        TreeNode::with_children(
            "Block",
            "",
            vec![
                TreeNode::with_children(
                    "Assign",
                    "",
                    vec![TreeNode::new("Name", "x"), TreeNode::new("Name", "a")],
                ),
                TreeNode::with_children(
                    "Assign",
                    "",
                    vec![TreeNode::new("Name", "a"), TreeNode::new("Name", "b")],
                ),
                TreeNode::with_children("Return", "", vec![TreeNode::new("Name", "b")]),
            ],
        )
    )]
    #[case::typescript(
        SourceLang::TypeScript(lens_ts::Dialect::Ts),
        TreeNode::with_children(
            "FunctionBody",
            "",
            vec![
                TreeNode::with_children(
                    "VarDecl",
                    "const",
                    vec![TreeNode::with_children(
                        "Declarator",
                        "a",
                        vec![TreeNode::new("Ident", "x")],
                    )],
                ),
                TreeNode::with_children(
                    "VarDecl",
                    "const",
                    vec![TreeNode::with_children(
                        "Declarator",
                        "b",
                        vec![TreeNode::new("Ident", "a")],
                    )],
                ),
                TreeNode::with_children("Return", "", vec![TreeNode::new("Ident", "b")]),
            ],
        )
    )]
    #[case::go(
        SourceLang::Go,
        TreeNode::with_children(
            "Block",
            "",
            vec![TreeNode::with_children(
                "statement_list",
                "",
                vec![
                    TreeNode::with_children(
                        "short_var_declaration",
                        "",
                        vec![
                            TreeNode::with_children(
                                "expression_list",
                                "",
                                vec![TreeNode::new("identifier", "a")],
                            ),
                            TreeNode::with_children(
                                "expression_list",
                                "",
                                vec![TreeNode::new("identifier", "x")],
                            ),
                        ],
                    ),
                    TreeNode::with_children(
                        "short_var_declaration",
                        "",
                        vec![
                            TreeNode::with_children(
                                "expression_list",
                                "",
                                vec![TreeNode::new("identifier", "b")],
                            ),
                            TreeNode::with_children(
                                "expression_list",
                                "",
                                vec![TreeNode::new("identifier", "a")],
                            ),
                        ],
                    ),
                    TreeNode::with_children(
                        "return_statement",
                        "",
                        vec![TreeNode::with_children(
                            "expression_list",
                            "",
                            vec![TreeNode::new("identifier", "b")],
                        )],
                    ),
                ],
            )],
        )
    )]
    fn every_language_lowers_a_chain_to_three_statements(
        #[case] lang: SourceLang,
        #[case] tree: TreeNode,
    ) {
        let profile = PdgProfile::from_unit(&unit(lang, tree.clone(), &["x"]), false);
        assert_eq!(profile.statement_count(), 3);
        let again = PdgProfile::from_unit(&unit(lang, tree, &["x"]), false);
        assert!((pdg_similarity(&profile, &again) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn a_parameter_read_is_wiring_a_free_name_is_not() {
        let tree = TreeNode::with_children(
            "Block",
            "",
            vec![TreeNode::with_children(
                "Let",
                "",
                vec![TreeNode::new("PatIdent", "a"), TreeNode::leaf("Path(x)")],
            )],
        );
        let with_param =
            PdgProfile::from_unit(&unit(SourceLang::Rust, tree.clone(), &["x"]), false);
        let without = PdgProfile::from_unit(&unit(SourceLang::Rust, tree, &[]), false);
        assert!(pdg_similarity(&with_param, &without) < 1.0);
    }
}
