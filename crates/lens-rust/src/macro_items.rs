//! Methods an `impl` block gains through a macro invocation.
//!
//! `syn` does not expand macros, so a method written by
//! `with_setter! { fn with_top, top: Option<usize> }` or by
//! `delegate_filter_builders!(filter);` inside an `impl` is invisible to
//! [`crate::extract_function_shapes_with_module`]. A call to it then has
//! no node to land on, and the call graph's name fallback hands the edge
//! to whichever same-named method *is* visible — on another type, often
//! in another module, which is how a phantom module cycle appears.
//!
//! Expanding `macro_rules!` is out of reach, but two shapes name the
//! generated methods without it:
//!
//! * the invocation spells the name itself: `fn with_top` appears in its
//!   tokens ([`ImplMacroCall::named_fns`]);
//! * the macro's transcriber spells it: `pub fn with_diff_only(...)` is
//!   written out in the `macro_rules!` body ([`MacroTemplate`]).
//!
//! The template is usually in another file than the invocation, so this
//! module only extracts both halves; the caller joins them across the
//! workspace with [`ImplMacroCall::generated_function_shapes`].
//!
//! Both are lower bounds. A name built from a fragment (`fn $name`) is
//! only seen through the invocation, and a macro that writes a whole
//! `impl` block at item level is not an `impl`-item invocation at all.

use lens_domain::{
    BodyShape, FunctionShape, OwnerKind, OwnerShape, SourceSpan, SyntaxFact, TreeNode,
    qualify_module,
};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use syn::spanned::Spanned;

use crate::parser::RustParseError;

/// A `macro_rules!` definition and the method names its transcribers
/// write out literally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MacroTemplate {
    pub name: String,
    pub generated_fns: Vec<String>,
}

/// A macro invoked in `impl`-item position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplMacroCall {
    /// Last path segment of the invoked macro (`delegate_filter_builders`).
    pub macro_name: String,
    pub owner: String,
    pub owner_kind: OwnerKind,
    pub module: String,
    pub span: SourceSpan,
    pub is_test: bool,
    /// `fn <name>` pairs written in the invocation's own tokens.
    pub named_fns: Vec<String>,
}

/// Everything one file contributes to macro-generated method discovery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MacroFacts {
    pub templates: Vec<MacroTemplate>,
    pub impl_calls: Vec<ImplMacroCall>,
}

impl ImplMacroCall {
    /// One function shape per method this invocation generates: the
    /// names it spells itself, plus `template_fns` — the names the
    /// matching [`MacroTemplate`]s write out — deduplicated.
    ///
    /// Only the name and the owner are known. Visibility, signature and
    /// attributes come from the macro body, so they stay
    /// [`SyntaxFact::Unknown`] (reachability reads that as "may be an
    /// entry point", never as "private"); the body is empty, so the
    /// node has no outgoing edges; the span is the invocation's.
    pub fn generated_function_shapes(&self, template_fns: &[String]) -> Vec<FunctionShape> {
        let mut names: Vec<&String> = self.named_fns.iter().chain(template_fns).collect();
        names.sort();
        names.dedup();
        names
            .into_iter()
            .map(|name| FunctionShape {
                display_name: name.clone(),
                qualified_name: SyntaxFact::Known(qualify_module(
                    &self.module,
                    &format!("{}::{name}", self.owner),
                )),
                module_path: SyntaxFact::Known(self.module.clone()),
                owner: SyntaxFact::Known(Some(OwnerShape {
                    display_name: self.owner.clone(),
                    kind: self.owner_kind,
                })),
                visibility: SyntaxFact::Unknown,
                signature: SyntaxFact::Unknown,
                doc: None,
                attributes: SyntaxFact::Unknown,
                body: BodyShape {
                    tree: TreeNode::leaf("MacroGenerated"),
                },
                span: self.span,
                is_test: self.is_test,
            })
            .collect()
    }
}

/// Collect the `macro_rules!` templates and `impl`-item macro
/// invocations of one Rust file. `base_module` qualifies the owners the
/// same way [`crate::extract_function_shapes_with_module`] does.
pub fn extract_macro_facts_with_module(
    source: &str,
    base_module: &str,
) -> Result<MacroFacts, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut facts = MacroFacts::default();
    collect_items(&file.items, base_module, false, &mut facts);
    Ok(facts)
}

fn collect_items(items: &[syn::Item], module: &str, in_test: bool, facts: &mut MacroFacts) {
    for item in items {
        match item {
            syn::Item::Macro(item_macro) => {
                if let Some(template) = macro_template(item_macro) {
                    facts.templates.push(template);
                }
            }
            syn::Item::Impl(item_impl) => {
                let in_test = in_test || crate::attrs::has_cfg_test(&item_impl.attrs);
                collect_impl_calls(item_impl, module, in_test, facts);
            }
            syn::Item::Mod(item_mod) => {
                if let Some((_, nested)) = &item_mod.content {
                    let nested_module = qualify_module(module, &item_mod.ident.to_string());
                    let in_test = in_test || crate::attrs::has_cfg_test(&item_mod.attrs);
                    collect_items(nested, &nested_module, in_test, facts);
                }
            }
            _ => {}
        }
    }
}

fn macro_template(item_macro: &syn::ItemMacro) -> Option<MacroTemplate> {
    if !item_macro.mac.path.is_ident("macro_rules") {
        return None;
    }
    let name = item_macro.ident.as_ref()?.to_string();
    let mut generated_fns = Vec::new();
    for transcriber in transcribers(item_macro.mac.tokens.clone()) {
        literal_fn_names(transcriber, &mut generated_fns);
    }
    generated_fns.sort();
    generated_fns.dedup();
    Some(MacroTemplate {
        name,
        generated_fns,
    })
}

/// The right-hand side of every `(matcher) => { transcriber }` rule.
fn transcribers(rules: TokenStream) -> Vec<TokenStream> {
    let tokens: Vec<TokenTree> = rules.into_iter().collect();
    tokens
        .windows(3)
        .filter_map(|window| match window {
            [
                TokenTree::Punct(eq),
                TokenTree::Punct(gt),
                TokenTree::Group(body),
            ] if eq.as_char() == '=' && gt.as_char() == '>' => Some(body.stream()),
            _ => None,
        })
        .collect()
}

fn collect_impl_calls(
    item_impl: &syn::ItemImpl,
    module: &str,
    in_test: bool,
    facts: &mut MacroFacts,
) {
    let Some(owner) = crate::common::impl_self_type_name(&item_impl.self_ty) else {
        return;
    };
    let owner_kind = if item_impl.trait_.is_some() {
        OwnerKind::TraitImpl
    } else {
        OwnerKind::Impl
    };
    for impl_item in &item_impl.items {
        let syn::ImplItem::Macro(item) = impl_item else {
            continue;
        };
        let Some(macro_name) = item.mac.path.segments.last() else {
            continue;
        };
        let mut named_fns = Vec::new();
        literal_fn_names(item.mac.tokens.clone(), &mut named_fns);
        let span = item.span();
        facts.impl_calls.push(ImplMacroCall {
            macro_name: macro_name.ident.to_string(),
            owner: owner.clone(),
            owner_kind,
            module: module.to_owned(),
            span: SourceSpan {
                start_line: span.start().line,
                end_line: span.end().line,
            },
            is_test: in_test || crate::attrs::has_cfg_test(&item.attrs),
            named_fns,
        });
    }
}

/// Push every `fn <ident>` written at item level of `tokens`.
///
/// Brace groups are function bodies or nested items and are not entered,
/// so a helper `fn` inside a generated method is not mistaken for a
/// method. `$( … )*` repetitions are entered: they repeat items. A
/// `fn $name` is skipped — the fragment is filled in by the invocation,
/// which spells the name itself.
fn literal_fn_names(tokens: TokenStream, out: &mut Vec<String>) {
    let tokens: Vec<TokenTree> = tokens.into_iter().collect();
    for (i, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Ident(ident) if ident == "fn" => {
                if let Some(TokenTree::Ident(name)) = tokens.get(i + 1) {
                    out.push(name.to_string());
                }
            }
            TokenTree::Group(group)
                if group.delimiter() != Delimiter::Brace
                    && matches!(
                        i.checked_sub(1).and_then(|prev| tokens.get(prev)),
                        Some(TokenTree::Punct(dollar)) if dollar.as_char() == '$'
                    ) =>
            {
                literal_fn_names(group.stream(), out);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn facts(src: &str) -> MacroFacts {
        extract_macro_facts_with_module(src, "crate").unwrap()
    }

    #[test]
    fn template_collects_literal_fn_names_from_every_rule() {
        let src = r#"
macro_rules! builders {
    ($field:ident) => {
        pub fn with_a(mut self) -> Self { fn helper() {} self }
        pub fn with_b(mut self) -> Self { self }
    };
    () => {
        $( pub fn with_c(self) -> Self { self } )*
    };
}
"#;
        assert_eq!(
            facts(src).templates,
            vec![MacroTemplate {
                name: "builders".to_owned(),
                generated_fns: vec!["with_a".into(), "with_b".into(), "with_c".into()],
            }]
        );
    }

    /// Only `$( … )` repetitions are entered. Another group — the
    /// arguments of a nested macro call here — is not item level of
    /// this transcriber, so a `fn` inside it names nothing this macro
    /// is known to write.
    #[test]
    fn template_skips_fn_inside_a_non_repetition_group() {
        let src = r#"
macro_rules! outer {
    () => {
        pub fn kept(self) -> Self { self }
        inner!(fn hidden);
    };
}
"#;
        assert_eq!(
            facts(src).templates[0].generated_fns,
            vec!["kept".to_owned()]
        );
    }

    #[test]
    fn template_skips_fragment_names() {
        let src = r#"
macro_rules! with_setter {
    (fn $name:ident, $field:ident: $ty:ty) => {
        pub fn $name(mut self, $field: $ty) -> Self { self.$field = $field; self }
    };
}
"#;
        assert_eq!(facts(src).templates[0].generated_fns, Vec::<String>::new());
    }

    #[test]
    fn impl_call_records_owner_module_and_spelled_names() {
        let src = r#"
mod inner {
    impl Analyzer {
        with_setter! {
            /// Docs ride along as attribute tokens.
            fn with_top, top: usize
        }
    }
}
"#;
        let calls = facts(src).impl_calls;
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(call.macro_name, "with_setter");
        assert_eq!(call.owner, "Analyzer");
        assert_eq!(call.owner_kind, OwnerKind::Impl);
        assert_eq!(call.module, "crate::inner");
        assert_eq!(call.named_fns, vec!["with_top".to_owned()]);
        assert!(!call.is_test);
    }

    #[rstest]
    #[case::cfg_test_impl("#[cfg(test)] impl A { m!(x); }")]
    #[case::cfg_test_mod("#[cfg(test)] mod t { impl A { m!(x); } }")]
    #[case::cfg_test_item("impl A { #[cfg(test)] m!(x); }")]
    fn impl_call_in_test_context_is_test(#[case] src: &str) {
        assert!(facts(src).impl_calls[0].is_test);
    }

    #[test]
    fn trait_impl_call_is_owned_by_a_trait_impl() {
        let calls = facts("impl Display for A { m!(); }").impl_calls;
        assert_eq!(calls[0].owner_kind, OwnerKind::TraitImpl);
    }

    #[test]
    fn generated_shapes_join_spelled_and_template_names() {
        let call = &facts("impl A { m! { fn with_x, x: u8 } }").impl_calls[0];
        let shapes = call.generated_function_shapes(&["with_y".into(), "with_x".into()]);
        let names: Vec<_> = shapes
            .iter()
            .map(|s| s.qualified_name.known_value().cloned().unwrap())
            .collect();
        assert_eq!(names, vec!["crate::A::with_x", "crate::A::with_y"]);
        let shape = &shapes[0];
        assert_eq!(shape.display_name, "with_x");
        assert_eq!(shape.visibility, SyntaxFact::Unknown);
        assert_eq!(shape.signature, SyntaxFact::Unknown);
        assert_eq!(shape.attributes, SyntaxFact::Unknown);
        assert_eq!(shape.span.start_line, 1);
    }
}
