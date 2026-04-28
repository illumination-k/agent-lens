//! Small `syn` helpers shared between the lens-rust extractors.
//!
//! Each extractor (parser, complexity, cohesion, wrapper) used to keep
//! its own copy of `type_path_last_ident` and its own item-recursion
//! ladder; consolidating them here cuts the structural duplication and
//! means a future fix lands in one place.

use syn::{Block, ImplItem, Item, ItemImpl, ItemTrait, Signature, TraitItem, Type};

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
    /// Trait being implemented for `impl Trait for Type` methods, and
    /// the trait name itself for trait default methods. `None` for
    /// inherent impls and free fns.
    pub(crate) trait_name: Option<&'a str>,
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
    pub(crate) skip_cfg_test_blocks: bool,
    /// Skip individual fns / methods carrying a test attribute
    /// (`#[test]`, `#[rstest]`, `#[tokio::test]`, etc.).
    pub(crate) skip_test_fns: bool,
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
        walk_item(item, opts, visit);
    }
}

fn walk_item<F>(item: &Item, opts: WalkOptions, visit: &mut F)
where
    F: FnMut(FnSite<'_>),
{
    match item {
        Item::Fn(item_fn) => {
            if opts.skip_test_fns && is_test_function(&item_fn.attrs) {
                return;
            }
            visit(FnSite {
                owner: None,
                trait_name: None,
                sig: &item_fn.sig,
                block: &item_fn.block,
            });
        }
        Item::Impl(item_impl) => {
            if opts.skip_cfg_test_blocks && has_cfg_test(&item_impl.attrs) {
                return;
            }
            walk_impl(item_impl, opts, visit);
        }
        Item::Trait(item_trait) => {
            if opts.skip_cfg_test_blocks && has_cfg_test(&item_trait.attrs) {
                return;
            }
            walk_trait(item_trait, opts, visit);
        }
        Item::Mod(item_mod) => {
            if opts.skip_cfg_test_blocks && has_cfg_test(&item_mod.attrs) {
                return;
            }
            if let Some((_, items)) = &item_mod.content {
                for nested in items {
                    walk_item(nested, opts, visit);
                }
            }
        }
        _ => {}
    }
}

fn walk_impl<F>(item_impl: &ItemImpl, opts: WalkOptions, visit: &mut F)
where
    F: FnMut(FnSite<'_>),
{
    let owner = type_path_last_ident(&item_impl.self_ty);
    let trait_name = item_impl
        .trait_
        .as_ref()
        .and_then(|(_, path, _)| path.segments.last().map(|s| s.ident.to_string()));
    for impl_item in &item_impl.items {
        if let ImplItem::Fn(method) = impl_item {
            if opts.skip_test_fns && is_test_function(&method.attrs) {
                continue;
            }
            visit(FnSite {
                owner: owner.as_deref(),
                trait_name: trait_name.as_deref(),
                sig: &method.sig,
                block: &method.block,
            });
        }
    }
}

fn walk_trait<F>(item_trait: &ItemTrait, opts: WalkOptions, visit: &mut F)
where
    F: FnMut(FnSite<'_>),
{
    let owner = item_trait.ident.to_string();
    for trait_item in &item_trait.items {
        if let TraitItem::Fn(method) = trait_item {
            let Some(block) = &method.default else {
                continue;
            };
            if opts.skip_test_fns && is_test_function(&method.attrs) {
                continue;
            }
            visit(FnSite {
                owner: Some(&owner),
                trait_name: Some(&owner),
                sig: &method.sig,
                block,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use syn::parse_str;

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
}
