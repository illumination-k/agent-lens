//! Attribute predicates shared by analysers that need to tell test
//! scaffolding apart from production code.
//!
//! The helpers here are intentionally conservative: anything more
//! elaborate than the canonical `#[cfg(test)]` / `#[test]` shapes falls
//! through and is treated as production code. Borderline gating like
//! `#[cfg(any(test, feature = "..."))]` is rare and hand-tagging the
//! exclusion as an analyser flag would be a better fit than guessing
//! here.

use syn::{Attribute, Meta};

/// True iff one of `attrs` is `#[cfg(test)]` — the canonical guard
/// around unit-test modules. Anything more elaborate
/// (`cfg(any(test, feature = "..."))` etc.) falls through and is
/// treated as production code.
pub(crate) fn has_cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let Meta::List(list) = &attr.meta else {
            return false;
        };
        if !list.path.is_ident("cfg") {
            return false;
        }
        list.tokens.to_string().trim() == "test"
    })
}

/// True iff one of `attrs` marks the function as a test entry point.
/// Recognised shapes:
///
/// * `#[test]` — built-in unit-test attribute,
/// * `#[rstest]` / `#[rstest::rstest]` — parameterised tests,
/// * `#[tokio::test]` / `#[async_std::test]` — async test runners,
/// * any path ending in `::test` (e.g. `#[smol::test]`),
/// * `#[cfg(test)]` directly on the fn (uncommon but valid).
///
/// Used by `--exclude-tests` so the similarity analyser can drop
/// table-driven test bodies from the noise floor.
pub(crate) fn is_test_function(attrs: &[Attribute]) -> bool {
    attrs.iter().any(is_test_attribute) || has_cfg_test(attrs)
}

fn is_test_attribute(attr: &Attribute) -> bool {
    let path = attr.path();
    let last = path.segments.last().map(|s| s.ident.to_string());
    matches!(last.as_deref(), Some("test" | "rstest"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use syn::parse_quote;

    #[rstest]
    #[case::cfg_test(parse_quote! { #[cfg(test)] fn x() {} }, true)]
    #[case::cfg_not_test(parse_quote! { #[cfg(feature = "x")] fn x() {} }, false)]
    #[case::cfg_any_test(parse_quote! { #[cfg(any(test, feature = "x"))] fn x() {} }, false)]
    #[case::no_attrs(parse_quote! { fn x() {} }, false)]
    fn has_cfg_test_recognises_canonical_shape(
        #[case] item_fn: syn::ItemFn,
        #[case] expected: bool,
    ) {
        assert_eq!(has_cfg_test(&item_fn.attrs), expected);
    }

    #[rstest]
    #[case::test(parse_quote! { #[test] fn x() {} })]
    #[case::rstest(parse_quote! { #[rstest] fn x() {} })]
    #[case::tokio_test(parse_quote! { #[tokio::test] fn x() {} })]
    #[case::rstest_qualified(parse_quote! { #[rstest::rstest] fn x() {} })]
    #[case::cfg_test(parse_quote! { #[cfg(test)] fn x() {} })]
    fn is_test_function_recognises_canonical_shapes(#[case] item_fn: syn::ItemFn) {
        assert!(is_test_function(&item_fn.attrs));
    }

    #[rstest]
    #[case::no_attrs(parse_quote! { fn x() {} })]
    #[case::derive(parse_quote! { #[inline] fn x() {} })]
    #[case::cfg_unrelated(parse_quote! { #[cfg(unix)] fn x() {} })]
    fn is_test_function_rejects_non_test_shapes(#[case] item_fn: syn::ItemFn) {
        assert!(!is_test_function(&item_fn.attrs));
    }
}
