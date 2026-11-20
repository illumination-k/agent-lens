//! The function-shaped unit of a Quint file: one top-level operator
//! definition (`def`, `pure def`, `action`, `val`, `temporal`, `run`).
//!
//! Names stay bare while a file declares a single module, the common
//! layout. A file with several modules qualifies every definition as
//! `Module::name` so two modules' `init` actions stay distinguishable.

use lens_domain::qualify;

use crate::cli::ParsedFile;
use crate::ir::{Decl, Expr, OpDef};

/// The operator names Quint resolves without a declaration, taken from
/// the `builtinNames` list of the `quint` 0.33 name resolver. An `app`
/// whose opcode is on this list is a language construct; any other
/// opcode calls a user definition.
pub const BUILTIN_OPCODES: &[&str] = &[
    "not",
    "and",
    "or",
    "iff",
    "implies",
    "exists",
    "forall",
    "in",
    "union",
    "contains",
    "fold",
    "intersect",
    "exclude",
    "subseteq",
    "map",
    "applyTo",
    "filter",
    "powerset",
    "flatten",
    "allLists",
    "allListsUpTo",
    "getOnlyElement",
    "chooseSome",
    "apalache::generate",
    "oneOf",
    "isFinite",
    "size",
    "get",
    "put",
    "keys",
    "mapBy",
    "setToMap",
    "setOfMaps",
    "set",
    "setBy",
    "fields",
    "with",
    "tuples",
    "append",
    "concat",
    "head",
    "tail",
    "length",
    "nth",
    "indices",
    "replaceAt",
    "slice",
    "select",
    "foldl",
    "foldr",
    "to",
    "always",
    "eventually",
    "next",
    "then",
    "expect",
    "reps",
    "fail",
    "assert",
    "orKeep",
    "mustChange",
    "enabled",
    "weakFair",
    "strongFair",
    "leadsTo",
    "Bool",
    "Int",
    "Nat",
    "Set",
    "Map",
    "List",
    "Tup",
    "Rec",
    "range",
    "igt",
    "ilt",
    "igte",
    "ilte",
    "iadd",
    "isub",
    "iuminus",
    "imul",
    "idiv",
    "imod",
    "ipow",
    "actionAll",
    "actionAny",
    "field",
    "fieldNames",
    "item",
    "assign",
    "of",
    "eq",
    "neq",
    "ite",
    "cross",
    "difference",
    "matchVariant",
    "variant",
    "q::debug",
    "q::lastTrace",
    "q::test",
    "q::testOnce",
];

pub fn is_builtin(opcode: &str) -> bool {
    BUILTIN_OPCODES.contains(&opcode)
}

/// One top-level definition with what every extractor reads off it.
pub struct DefSite<'a> {
    pub def: &'a OpDef,
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
    /// Parameter names, when the definition is an operator.
    pub params: Vec<&'a str>,
    /// The definition's value with the parameter lambda peeled off.
    pub body: &'a Expr,
}

impl DefSite<'_> {
    /// `run` definitions are Quint's tests, executed by `quint test`.
    pub fn is_test(&self) -> bool {
        self.def.qualifier == "run"
    }
}

/// Every top-level definition of `file`, in source order.
pub fn def_sites(file: &ParsedFile) -> Vec<DefSite<'_>> {
    let qualified = file.modules.len() > 1;
    let mut out = Vec::new();
    for module in &file.modules {
        for decl in &module.declarations {
            let Decl::Def(def) = decl else { continue };
            let (params, body) = match &def.expr {
                Expr::Lambda { params, expr, .. } => {
                    (params.iter().map(|p| p.name.as_str()).collect(), &**expr)
                }
                other => (Vec::new(), other),
            };
            if is_variant_constructor(body) {
                continue;
            }
            // The definition's own id spans only its name; its value's id
            // spans the whole `qualifier name(...) = ...` text.
            let spans = [file.span(def.id), file.span(def.expr.id())];
            let start_line = spans.iter().flatten().map(|s| s.0).min().unwrap_or(1);
            let end_line = spans
                .iter()
                .flatten()
                .map(|s| s.1)
                .max()
                .unwrap_or(start_line);
            out.push(DefSite {
                def,
                name: qualify(qualified.then_some(module.name.as_str()), &def.name),
                start_line,
                end_line,
                params,
                body,
            });
        }
    }
    out.sort_by_key(|site| site.start_line);
    out
}

/// Quint lowers each `type T = A(int) | B` arm into a definition whose
/// value is a bare `variant(...)`; those are generated constructors, not
/// code anyone wrote.
fn is_variant_constructor(body: &Expr) -> bool {
    matches!(body, Expr::App { opcode, .. } if opcode == "variant")
}

/// The `///` comment block directly above `start_line` (1-based), with
/// the markers stripped.
pub fn doc_comment(source: &str, start_line: usize) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut doc = Vec::new();
    let mut idx = start_line.checked_sub(1)?;
    while idx > 0 {
        idx -= 1;
        let Some(text) = lines.get(idx).map(|l| l.trim_start()) else {
            break;
        };
        let Some(rest) = text.strip_prefix("///") else {
            break;
        };
        doc.push(rest.strip_prefix(' ').unwrap_or(rest));
    }
    if doc.is_empty() {
        return None;
    }
    doc.reverse();
    Some(doc.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::none("module m {\n  val x = 1\n}\n", 2, None)]
    #[case::single("module m {\n  /// Doc.\n  val x = 1\n}\n", 3, Some("Doc."))]
    #[case::multi(
        "module m {\n  /// One.\n  /// Two.\n  val x = 1\n}\n",
        4,
        Some("One.\nTwo.")
    )]
    #[case::at_file_start("/// Top.\nval x = 1\n", 2, Some("Top."))]
    #[case::plain_comment_is_not_doc("module m {\n  // no\n  val x = 1\n}\n", 3, None)]
    fn doc_comment_reads_the_block_above(
        #[case] source: &str,
        #[case] line: usize,
        #[case] expected: Option<&str>,
    ) {
        assert_eq!(doc_comment(source, line).as_deref(), expected);
    }

    #[test]
    fn builtin_list_knows_core_operators() {
        assert!(is_builtin("ite"));
        assert!(is_builtin("actionAny"));
        assert!(!is_builtin("double"));
    }
}
