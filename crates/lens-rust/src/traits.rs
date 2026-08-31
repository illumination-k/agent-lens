//! Trait declaration and `impl Trait for Type` extraction, the Rust
//! half of the single-impl analyzer's implementor census.
//!
//! Declarations carry every directly declared method (body-less
//! signatures included — the function walk in [`crate::common`] skips
//! those on purpose, so this cannot reuse it), plus span and visibility.
//! Impl blocks carry only the two trailing path identifiers a census
//! matches on: which trait, which type. Matching by trailing identifier
//! is the consumer's contract — an `impl fmt::Display for Foo` yields
//! `Display`, which only counts if a same-named trait is declared in
//! the analyzed tree.

use lens_domain::{
    InterfaceMethodShape, SourceSpan, SyntaxFact, TraitDeclShape, TraitImplShape, qualify_module,
};
use syn::spanned::Spanned;

use crate::attrs::has_cfg_test;
use crate::common::{impl_trait_last_ident, type_path_last_ident};
use crate::parser::{RustParseError, visibility_shape};

/// Extract the trait declarations and trait `impl` blocks of one Rust
/// source file, with declarations qualified at `module`. Inline modules
/// are descended into, tracking `#[cfg(test)]` context; the module path
/// of a nested declaration still uses `module` as its base, matching
/// [`crate::extract_function_shapes_with_modules`].
pub fn extract_trait_shapes_with_module(
    source: &str,
    module: &str,
) -> Result<(Vec<TraitDeclShape>, Vec<TraitImplShape>), RustParseError> {
    let file = syn::parse_file(source)?;
    let mut decls = Vec::new();
    let mut impls = Vec::new();
    for item in &file.items {
        walk_item(item, module, false, &mut decls, &mut impls);
    }
    Ok((decls, impls))
}

fn walk_item(
    item: &syn::Item,
    module: &str,
    in_test_context: bool,
    decls: &mut Vec<TraitDeclShape>,
    impls: &mut Vec<TraitImplShape>,
) {
    match item {
        syn::Item::Trait(item_trait) => {
            let is_test = in_test_context || has_cfg_test(&item_trait.attrs);
            decls.push(trait_decl(item_trait, module, is_test));
        }
        syn::Item::Impl(item_impl) => {
            let Some(trait_name) = impl_trait_last_ident(item_impl) else {
                return;
            };
            let is_test = in_test_context || has_cfg_test(&item_impl.attrs);
            impls.push(TraitImplShape {
                trait_name,
                self_type: type_path_last_ident(&item_impl.self_ty),
                span: span_of(item_impl),
                is_test,
            });
        }
        syn::Item::Mod(item_mod) => {
            let Some((_, items)) = &item_mod.content else {
                return;
            };
            let nested = in_test_context || has_cfg_test(&item_mod.attrs);
            let nested_module = qualify_module(module, &item_mod.ident.to_string());
            for nested_item in items {
                walk_item(nested_item, &nested_module, nested, decls, impls);
            }
        }
        _ => {}
    }
}

fn trait_decl(item_trait: &syn::ItemTrait, module: &str, is_test: bool) -> TraitDeclShape {
    let name = item_trait.ident.to_string();
    let methods = item_trait
        .items
        .iter()
        .filter_map(|trait_item| {
            let syn::TraitItem::Fn(method) = trait_item else {
                return None;
            };
            Some(InterfaceMethodShape {
                name: method.sig.ident.to_string(),
                param_count: method
                    .sig
                    .inputs
                    .iter()
                    .filter(|arg| !matches!(arg, syn::FnArg::Receiver(_)))
                    .count(),
            })
        })
        .collect();
    TraitDeclShape {
        qualified_name: qualify_module(module, &name),
        display_name: name,
        methods,
        span: span_of(item_trait),
        visibility: SyntaxFact::Known(visibility_shape(&item_trait.vis)),
        is_test,
    }
}

fn span_of<T: Spanned>(item: &T) -> SourceSpan {
    let span = item.span();
    SourceSpan {
        start_line: span.start().line,
        end_line: span.end().line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lens_domain::VisibilityShape;

    fn extract(source: &str) -> (Vec<TraitDeclShape>, Vec<TraitImplShape>) {
        extract_trait_shapes_with_module(source, "crate").unwrap()
    }

    #[test]
    fn extracts_a_trait_with_bodyless_and_default_methods() {
        let (decls, impls) = extract(
            "pub trait Store {\n\
                 fn get(&self, key: &str) -> Option<String>;\n\
                 fn len(&self) -> usize { 0 }\n\
             }\n",
        );
        assert!(impls.is_empty());
        assert_eq!(decls.len(), 1);
        let decl = &decls[0];
        assert_eq!(decl.display_name, "Store");
        assert_eq!(decl.qualified_name, "crate::Store");
        assert_eq!(decl.visibility, SyntaxFact::Known(VisibilityShape::Public));
        assert_eq!(decl.span.start_line, 1);
        assert_eq!(decl.span.end_line, 4);
        assert!(!decl.is_test);
        let methods: Vec<(&str, usize)> = decl
            .methods
            .iter()
            .map(|m| (m.name.as_str(), m.param_count))
            .collect();
        assert_eq!(methods, [("get", 1), ("len", 0)]);
    }

    #[test]
    fn extracts_trait_impls_with_trailing_idents() {
        let (decls, impls) = extract(
            "struct Memory;\n\
             impl std::fmt::Display for Memory {\n\
                 fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }\n\
             }\n\
             impl Memory { fn helper(&self) {} }\n",
        );
        assert!(decls.is_empty());
        // The inherent impl is not a trait impl and is not extracted.
        assert_eq!(impls.len(), 1);
        assert_eq!(impls[0].trait_name, "Display");
        assert_eq!(impls[0].self_type.as_deref(), Some("Memory"));
        assert_eq!(impls[0].span.start_line, 2);
    }

    #[test]
    fn cfg_test_context_marks_nested_declarations() {
        let (decls, impls) = extract(
            "trait Live { fn go(&self); }\n\
             #[cfg(test)]\n\
             mod tests {\n\
                 use super::Live;\n\
                 trait Harness { fn run(&self); }\n\
                 struct Fake;\n\
                 impl Live for Fake { fn go(&self) {} }\n\
             }\n",
        );
        assert_eq!(decls.len(), 2);
        assert!(!decls[0].is_test, "Live is production");
        assert!(decls[1].is_test, "Harness is test scoped");
        assert_eq!(decls[1].qualified_name, "crate::tests::Harness");
        assert_eq!(impls.len(), 1);
        assert!(impls[0].is_test, "Fake's impl is test scoped");
    }

    #[test]
    fn a_non_path_self_type_yields_no_type_name() {
        let (_, impls) = extract(
            "trait Marker {}\n\
             impl Marker for (u8, u8) {}\n",
        );
        assert_eq!(impls.len(), 1);
        assert_eq!(impls[0].self_type, None);
    }

    #[test]
    fn restricted_visibility_is_preserved() {
        let (decls, _) = extract("pub(crate) trait Inner { fn f(&self); }\n");
        assert!(matches!(
            decls[0].visibility,
            SyntaxFact::Known(VisibilityShape::Restricted(_)),
        ));
    }
}
