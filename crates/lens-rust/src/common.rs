//! Small `syn` helpers shared between the lens-rust extractors.
//!
//! Each extractor (parser, complexity, cohesion, wrapper) used to keep
//! its own copy of `type_path_last_ident` and its own item-recursion
//! ladder; consolidating them here cuts the structural duplication and
//! means a future fix lands in one place.

use syn::{
    Attribute, Block, ImplItem, Item, ItemFn, ItemImpl, ItemMod, ItemTrait, Signature, TraitItem,
    Type,
};

use crate::attrs::{has_cfg_test, is_test_function};

/// Return the trailing identifier of a `Type::Path` (e.g. `Foo` for
/// `mod::Foo<T>`). Returns `None` for non-path types like
/// `Type::Reference`, function-pointer types, tuples, etc.
pub(crate) fn type_path_last_ident(ty: &Type) -> Option<String> {
    if let Type::Path(type_path) = ty {
        type_path
            .path
            .segments
            .last()
            .map(|seg| seg.ident.to_string())
    } else {
        None
    }
}

/// One function-shaped site discovered while walking a Rust file:
/// a free `fn`, an inherent `impl` method, or a trait default method.
pub(crate) struct FnSite<'a> {
    /// `Self` type for `impl Foo` methods, or the trait name for trait
    /// default methods. `None` for free fns at module scope.
    pub(crate) owner: Option<&'a str>,
    /// True only for methods inside `impl Trait for Type` blocks. Trait
    /// default methods are not trait impl methods.
    pub(crate) is_trait_impl: bool,
    pub(crate) is_test: bool,
    pub(crate) sig: &'a Signature,
    pub(crate) block: &'a Block,
}

/// Filtering knobs for [`walk_fn_items`].
///
/// Defaults walk everything — every analyser was previously hand-rolling
/// its own version of these flags, so keeping the default permissive
/// matches the historical behaviour of [`extract_complexity_units`] and
/// the default mode of [`RustParser::extract_functions`].
#[derive(Default, Clone, Copy)]
pub(crate) struct WalkOptions {
    /// Skip `#[cfg(test)]`-gated `mod`/`impl`/`trait` blocks entirely.
    /// Some analyzers, such as wrappers, historically ignore test modules
    /// but do not classify individual `#[test]` functions.
    pub(crate) skip_cfg_test_blocks: bool,
}

/// Walk every function-shaped node reachable from `items`, yielding one
/// [`FnSite`] per free fn, inherent / trait `impl` method, and trait
/// default method (recursively descending into inline modules).
///
/// `visit` is invoked once per site. The closure receives borrowed
/// references that live as long as the items being walked, so it is
/// free to push references into a longer-lived buffer or build owned
/// reports on the fly.
pub(crate) fn walk_fn_items<F>(items: &[Item], opts: WalkOptions, visit: &mut F)
where
    F: FnMut(FnSite<'_>),
{
    for item in items {
        walk_item(item, opts, false, visit);
    }
}

fn walk_item<F>(item: &Item, opts: WalkOptions, in_test_context: bool, visit: &mut F)
where
    F: FnMut(FnSite<'_>),
{
    match item {
        Item::Fn(item_fn) => walk_fn(item_fn, in_test_context, visit),
        Item::Impl(item_impl) => {
            if let Some(nested_ctx) = enter_cfg_test_block(&item_impl.attrs, opts, in_test_context)
            {
                walk_impl(item_impl, nested_ctx, visit);
            }
        }
        Item::Trait(item_trait) => {
            if let Some(nested_ctx) = enter_cfg_test_block(&item_trait.attrs, opts, in_test_context)
            {
                walk_trait(item_trait, nested_ctx, visit);
            }
        }
        Item::Mod(item_mod) => {
            if let Some(nested_ctx) = enter_cfg_test_block(&item_mod.attrs, opts, in_test_context) {
                walk_mod(item_mod, opts, nested_ctx, visit);
            }
        }
        _ => {}
    }
}

/// Decide whether to descend into a `#[cfg(test)]`-gated `mod`/`impl`/`trait`
/// block, and what test-context flag to propagate inside.
///
/// Returns `None` when the block should be skipped per `opts`. Otherwise
/// returns the test-context flag for nested items: `true` once we are inside
/// any `#[cfg(test)]` ancestor.
fn enter_cfg_test_block(
    attrs: &[Attribute],
    opts: WalkOptions,
    in_test_context: bool,
) -> Option<bool> {
    let item_is_test = has_cfg_test(attrs);
    if opts.skip_cfg_test_blocks && item_is_test {
        return None;
    }
    Some(in_test_context || item_is_test)
}

fn walk_fn<F>(item_fn: &ItemFn, in_test_context: bool, visit: &mut F)
where
    F: FnMut(FnSite<'_>),
{
    let is_test = in_test_context || is_test_function(&item_fn.attrs);
    visit(FnSite {
        owner: None,
        is_trait_impl: false,
        is_test,
        sig: &item_fn.sig,
        block: &item_fn.block,
    });
}

fn walk_mod<F>(item_mod: &ItemMod, opts: WalkOptions, in_test_context: bool, visit: &mut F)
where
    F: FnMut(FnSite<'_>),
{
    let Some((_, items)) = &item_mod.content else {
        return;
    };
    for nested in items {
        walk_item(nested, opts, in_test_context, visit);
    }
}

fn walk_impl<F>(item_impl: &ItemImpl, in_test_context: bool, visit: &mut F)
where
    F: FnMut(FnSite<'_>),
{
    let owner = type_path_last_ident(&item_impl.self_ty);
    let is_trait_impl = item_impl.trait_.is_some();
    for impl_item in &item_impl.items {
        if let ImplItem::Fn(method) = impl_item {
            let is_test = in_test_context || is_test_function(&method.attrs);
            visit(FnSite {
                owner: owner.as_deref(),
                is_trait_impl,
                is_test,
                sig: &method.sig,
                block: &method.block,
            });
        }
    }
}

fn walk_trait<F>(item_trait: &ItemTrait, in_test_context: bool, visit: &mut F)
where
    F: FnMut(FnSite<'_>),
{
    let owner = item_trait.ident.to_string();
    for trait_item in &item_trait.items {
        if let TraitItem::Fn(method) = trait_item {
            let Some(block) = &method.default else {
                continue;
            };
            let is_test = in_test_context || is_test_function(&method.attrs);
            visit(FnSite {
                owner: Some(&owner),
                is_trait_impl: false,
                is_test,
                sig: &method.sig,
                block,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::{parse_file, parse_str};

    fn walked_names(src: &str, opts: WalkOptions) -> Vec<String> {
        let file = parse_file(src).unwrap();
        let mut names = Vec::new();
        walk_fn_items(&file.items, opts, &mut |site| {
            if let Some(owner) = site.owner {
                names.push(format!("{owner}::{}", site.sig.ident));
            } else {
                names.push(site.sig.ident.to_string());
            }
        });
        names
    }

    #[test]
    fn extracts_trailing_ident_from_qualified_path() {
        let ty: Type = parse_str("crate::Foo<T>").unwrap();
        assert_eq!(type_path_last_ident(&ty), Some("Foo".to_owned()));
    }

    #[test]
    fn returns_none_for_reference_type() {
        let ty: Type = parse_str("&Foo").unwrap();
        assert_eq!(type_path_last_ident(&ty), None);
    }

    #[test]
    fn returns_none_for_tuple_type() {
        let ty: Type = parse_str("(Foo, Bar)").unwrap();
        assert_eq!(type_path_last_ident(&ty), None);
    }

    #[test]
    fn cfg_test_context_propagates_through_nested_items() {
        let src = r#"
#[cfg(test)]
mod tests {
    fn module_helper() {}

    mod inner {
        fn nested_helper() {}
    }

    struct Bag;
    impl Bag {
        fn fixture(&self) {}
    }

    trait Harness {
        fn default_helper(&self) {}
    }
}
"#;
        let opts = WalkOptions {
            skip_cfg_test_blocks: false,
        };

        let file = parse_file(src).unwrap();
        let mut seen_test_flags = Vec::new();
        walk_fn_items(&file.items, opts, &mut |site| {
            seen_test_flags.push(site.is_test);
        });
        assert_eq!(
            walked_names(src, opts),
            [
                "module_helper",
                "nested_helper",
                "Bag::fixture",
                "Harness::default_helper"
            ]
        );
        assert!(seen_test_flags.iter().all(|is_test| *is_test));
    }
}
