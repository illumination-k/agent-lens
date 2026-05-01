//! Item walk: traverse `syn::Item` trees and emit module-aware
//! [`RustFunctionDef`] entries for free functions, `impl` methods, and
//! `trait` default methods.

use lens_domain::{FunctionDef, OwnerKind, VisibilityShape, qualify as qualify_name};
use quote::ToTokens;
use syn::spanned::Spanned;

use super::{RustFunctionDef, function_tree, signature_info};

pub(super) fn extract_module_functions(
    items: &[syn::Item],
    module: &str,
    in_test_context: bool,
    out: &mut Vec<RustFunctionDef>,
) {
    for item in items {
        extract_item_functions(item, module, in_test_context, out);
    }
}

fn extract_item_functions(
    item: &syn::Item,
    module: &str,
    in_test_context: bool,
    out: &mut Vec<RustFunctionDef>,
) {
    match item {
        syn::Item::Fn(item_fn) => {
            let name = item_fn.sig.ident.to_string();
            let function = FunctionDef {
                name: name.clone(),
                start_line: item_fn.sig.span().start().line,
                end_line: item_fn.block.span().end().line,
                is_test: in_test_context || crate::attrs::is_test_function(&item_fn.attrs),
                signature: Some(signature_info(&item_fn.sig)),
                tree: function_tree(&item_fn.sig, &item_fn.block),
            };
            out.push(RustFunctionDef {
                function,
                qualified_name: qualify_module(module, &name),
                module: module.to_owned(),
                impl_owner: None,
                impl_owner_kind: None,
                visibility: visibility_shape(&item_fn.vis),
                name,
            });
        }
        syn::Item::Impl(item_impl) => {
            let item_is_test = crate::attrs::has_cfg_test(&item_impl.attrs);
            extract_impl_functions(item_impl, module, in_test_context || item_is_test, out);
        }
        syn::Item::Trait(item_trait) => {
            let item_is_test = crate::attrs::has_cfg_test(&item_trait.attrs);
            extract_trait_functions(item_trait, module, in_test_context || item_is_test, out);
        }
        syn::Item::Mod(item_mod) => {
            if let Some((_, nested_items)) = &item_mod.content {
                let nested_module = qualify_module(module, &item_mod.ident.to_string());
                let item_is_test = crate::attrs::has_cfg_test(&item_mod.attrs);
                extract_module_functions(
                    nested_items,
                    &nested_module,
                    in_test_context || item_is_test,
                    out,
                );
            }
        }
        _ => {}
    }
}

fn extract_impl_functions(
    item_impl: &syn::ItemImpl,
    module: &str,
    in_test_context: bool,
    out: &mut Vec<RustFunctionDef>,
) {
    let owner = crate::common::type_path_last_ident(&item_impl.self_ty);
    for impl_item in &item_impl.items {
        let syn::ImplItem::Fn(method) = impl_item else {
            continue;
        };
        let name = method.sig.ident.to_string();
        let function_name = qualify_name(owner.as_deref(), &name);
        let qualified_name = owner.as_ref().map_or_else(
            || qualify_module(module, &name),
            |owner| qualify_module(module, &format!("{owner}::{name}")),
        );
        let function = FunctionDef {
            name: function_name,
            start_line: method.sig.span().start().line,
            end_line: method.block.span().end().line,
            is_test: in_test_context || crate::attrs::is_test_function(&method.attrs),
            signature: Some(signature_info(&method.sig)),
            tree: function_tree(&method.sig, &method.block),
        };
        out.push(RustFunctionDef {
            function,
            qualified_name,
            module: module.to_owned(),
            impl_owner: owner.clone(),
            impl_owner_kind: owner.as_ref().map(|_| OwnerKind::Impl),
            visibility: visibility_shape(&method.vis),
            name,
        });
    }
}

fn extract_trait_functions(
    item_trait: &syn::ItemTrait,
    module: &str,
    in_test_context: bool,
    out: &mut Vec<RustFunctionDef>,
) {
    let owner = item_trait.ident.to_string();
    for trait_item in &item_trait.items {
        let syn::TraitItem::Fn(method) = trait_item else {
            continue;
        };
        let Some(block) = &method.default else {
            continue;
        };
        let name = method.sig.ident.to_string();
        let function = FunctionDef {
            name: qualify_name(Some(&owner), &name),
            start_line: method.sig.span().start().line,
            end_line: block.span().end().line,
            is_test: in_test_context || crate::attrs::is_test_function(&method.attrs),
            signature: Some(signature_info(&method.sig)),
            tree: function_tree(&method.sig, block),
        };
        out.push(RustFunctionDef {
            function,
            qualified_name: qualify_module(module, &format!("{owner}::{name}")),
            module: module.to_owned(),
            impl_owner: Some(owner.clone()),
            impl_owner_kind: Some(OwnerKind::Trait),
            visibility: visibility_shape(&item_trait.vis),
            name,
        });
    }
}

fn visibility_shape(vis: &syn::Visibility) -> VisibilityShape {
    match vis {
        syn::Visibility::Public(_) => VisibilityShape::Public,
        syn::Visibility::Restricted(restricted) => {
            VisibilityShape::Restricted(restricted.to_token_stream().to_string())
        }
        syn::Visibility::Inherited => VisibilityShape::Private,
    }
}

fn qualify_module(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{module}::{name}")
    }
}
