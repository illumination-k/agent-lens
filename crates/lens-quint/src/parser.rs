//! [`lens_domain::LanguageParser`] for Quint, over the `quint parse` IR.
//!
//! Each top-level definition becomes a [`FunctionDef`]. Its tree is the
//! definition's value with the parameter lambda peeled off: built-in
//! operators (`ite`, `assign`, `actionAny`, …) are labelled by opcode so
//! APTED can tell control structures apart, while calls to user
//! definitions share one `Call` label and carry the callee as the value.

use lens_domain::{
    FunctionDef, FunctionSignature, LanguageParseError, LanguageParser, ReceiverShape, TreeNode,
    identifier_tokens,
};

use crate::cli::{ParsedFile, parse_source};
use crate::defs::{def_sites, doc_comment, is_builtin};
use crate::ir::Expr;

/// Quint parser backed by the `quint` CLI. Stateless; parses are cached
/// process-wide by [`parse_source`].
#[derive(Debug, Default, Clone, Copy)]
pub struct QuintParser;

impl QuintParser {
    pub fn new() -> Self {
        Self
    }
}

impl LanguageParser for QuintParser {
    fn language(&self) -> &'static str {
        "quint"
    }

    fn parse(&mut self, source: &str) -> Result<TreeNode, LanguageParseError> {
        let file = parse_source(source).map_err(|e| LanguageParseError::new("quint", e))?;
        let children = def_sites(&file)
            .iter()
            .map(|site| {
                TreeNode::with_children("Def", site.name.clone(), vec![expr_tree(site.body)])
            })
            .collect();
        Ok(TreeNode::with_children("Block", "", children))
    }

    fn extract_functions(&mut self, source: &str) -> Result<Vec<FunctionDef>, LanguageParseError> {
        let file = parse_source(source).map_err(|e| LanguageParseError::new("quint", e))?;
        Ok(functions_of(&file, source))
    }
}

pub(crate) fn functions_of(file: &ParsedFile, source: &str) -> Vec<FunctionDef> {
    def_sites(file)
        .into_iter()
        .map(|site| FunctionDef {
            start_line: site.start_line,
            end_line: site.end_line,
            is_test: site.is_test(),
            signature: Some(FunctionSignature {
                name_tokens: identifier_tokens(&site.def.name),
                parameter_count: site.params.len(),
                parameter_names: site.params.iter().map(|p| (*p).to_owned()).collect(),
                parameter_type_paths: Vec::new(),
                return_type_paths: Vec::new(),
                generics: Vec::new(),
                receiver: ReceiverShape::None,
            }),
            doc: doc_comment(source, site.start_line),
            implements: None,
            tree: TreeNode::with_children("Block", "", vec![expr_tree(site.body)]),
            name: site.name,
        })
        .collect()
}

/// Lower one IR expression into the shared comparison tree.
pub(crate) fn expr_tree(expr: &Expr) -> TreeNode {
    match expr {
        Expr::Name { name, .. } => TreeNode::new("Name", name.clone()),
        Expr::Bool { value, .. } => TreeNode::new("Bool", value.to_string()),
        Expr::Int { value, .. } => TreeNode::new("Int", value.to_string()),
        Expr::Str { value, .. } => TreeNode::new("Str", value.clone()),
        Expr::App { opcode, args, .. } => {
            let children = args.iter().map(expr_tree).collect();
            if is_builtin(opcode) {
                TreeNode::with_children(opcode.clone(), "", children)
            } else {
                TreeNode::with_children("Call", opcode.clone(), children)
            }
        }
        Expr::Lambda { params, expr, .. } => {
            let mut children: Vec<TreeNode> = params
                .iter()
                .map(|p| TreeNode::new("Param", p.name.clone()))
                .collect();
            children.push(expr_tree(expr));
            TreeNode::with_children("Lambda", "", children)
        }
        Expr::Let { opdef, expr, .. } => TreeNode::with_children(
            opdef.qualifier.clone(),
            opdef.name.clone(),
            vec![expr_tree(&opdef.expr), expr_tree(expr)],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{counter, helpers};

    #[test]
    fn functions_carry_signature_doc_and_test_flag() {
        let fixture = counter();
        let fns = functions_of(&fixture.file, fixture.source);
        let double = fns.iter().find(|f| f.name == "double").unwrap();
        assert_eq!(double.doc.as_deref(), Some("Doubles its input."));
        let sig = double.signature.as_ref().unwrap();
        assert_eq!(sig.parameter_names, vec!["n".to_owned()]);
        assert!(fns.iter().find(|f| f.name == "bumpTest").unwrap().is_test);
    }

    #[test]
    fn multi_module_files_qualify_names() {
        let fixture = helpers();
        let names: Vec<String> = functions_of(&fixture.file, fixture.source)
            .into_iter()
            .map(|f| f.name)
            .collect();
        assert_eq!(
            names,
            vec!["helpers::inc".to_owned(), "extra::twice".to_owned()]
        );
    }

    #[test]
    fn builtins_label_by_opcode_and_calls_by_callee() {
        let fixture = counter();
        let fns = functions_of(&fixture.file, fixture.source);
        let forward = fns.iter().find(|f| f.name == "forward").unwrap();
        let call = &forward.tree.children[0];
        assert_eq!(
            (call.label.as_str(), call.value.as_str()),
            ("Call", "double")
        );
        let classify = fns.iter().find(|f| f.name == "classify").unwrap();
        assert_eq!(classify.tree.children[0].label, "ite");
    }
}
