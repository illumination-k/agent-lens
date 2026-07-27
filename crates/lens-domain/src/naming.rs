//! Language-agnostic name-building helpers.
//!
//! Each language adapter eventually produces fully-qualified function
//! names like `Module::method` or `Class::method`. The mechanic — prefix
//! the owner with `::` separator when present, otherwise use the bare
//! method name — is identical across `lens-rust`, `lens-ts`, `lens-py`,
//! and `lens-golang`, so it lives here. Two spellings of "no owner" are
//! in play, hence both [`qualify`] (`Option<&str>`) and
//! [`qualify_module`] (empty string).

/// Build a fully-qualified function name from an optional owner.
///
/// `qualify(Some("Foo"), "bar")` returns `"Foo::bar"`;
/// `qualify(None, "bar")` returns `"bar"`.
pub fn qualify(owner: Option<&str>, method: &str) -> String {
    match owner {
        Some(owner) => format!("{owner}::{method}"),
        None => method.to_owned(),
    }
}

/// [`qualify`] for the call-index convention where "no module" is spelled
/// as the empty string rather than `None`.
///
/// The module-tree walkers thread a `&str` module path down the AST and
/// use `""` for the crate/file root, so they'd otherwise each repeat the
/// `is_empty` check before formatting.
pub fn qualify_module(module: &str, name: &str) -> String {
    qualify((!module.is_empty()).then_some(module), name)
}

/// Whether `value`'s first character is an ASCII uppercase letter.
///
/// The call-index adapters use this as the "looks like a type, not a
/// function" heuristic when deciding whether a bare call expression names
/// a constructor. ASCII-only on purpose: it is a naming-convention probe,
/// not a Unicode case query, and `Ａ`-style full-width identifiers should
/// not be treated as type names.
pub fn starts_uppercase(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_uppercase())
}

/// Split an identifier into lowercased sub-tokens across `_`,
/// non-alphanumeric boundaries, and camelCase transitions.
///
/// `parse_user2_id` and `loadUserId` both tokenize to
/// `["parse"/"load", "user", ...]`. Used for the `name_tokens` field of
/// [`crate::FunctionSignature`] so signature-aware similarity compares
/// function names the same way regardless of source language.
pub fn identifier_tokens(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut prev_is_lower_or_digit = false;
    for ch in name.chars() {
        if ch == '_' || !ch.is_alphanumeric() {
            push_identifier_token(&mut tokens, &mut current);
            prev_is_lower_or_digit = false;
            continue;
        }
        if ch.is_uppercase() && prev_is_lower_or_digit {
            push_identifier_token(&mut tokens, &mut current);
        }
        current.extend(ch.to_lowercase());
        prev_is_lower_or_digit = ch.is_lowercase() || ch.is_ascii_digit();
    }
    push_identifier_token(&mut tokens, &mut current);
    tokens
}

fn push_identifier_token(tokens: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        tokens.push(std::mem::take(current));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn qualify_with_owner_uses_double_colon() {
        assert_eq!(qualify(Some("Foo"), "bar"), "Foo::bar");
    }

    #[test]
    fn qualify_without_owner_returns_bare_method() {
        assert_eq!(qualify(None, "bar"), "bar");
    }

    #[test]
    fn qualify_is_unicode_safe() {
        assert_eq!(qualify(Some("名前"), "値"), "名前::値");
    }

    #[rstest]
    #[case("foo::bar", "baz", "foo::bar::baz")]
    #[case("", "baz", "baz")]
    #[case("名前", "値", "名前::値")]
    fn qualify_module_treats_empty_module_as_no_owner(
        #[case] module: &str,
        #[case] name: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(qualify_module(module, name), expected);
    }

    #[rstest]
    #[case("Foo", true)]
    #[case("foo", false)]
    #[case("_Foo", false)]
    #[case("", false)]
    // Non-ASCII uppercase is deliberately not a type-name signal.
    #[case("Ａ", false)]
    #[case("Ünicode", false)]
    fn starts_uppercase_is_ascii_only(#[case] value: &str, #[case] expected: bool) {
        assert_eq!(starts_uppercase(value), expected);
    }

    #[test]
    fn identifier_tokens_split_snake_and_camel_case_boundaries() {
        assert_eq!(
            identifier_tokens("parse_user2_id"),
            vec!["parse", "user2", "id"],
        );
        assert_eq!(identifier_tokens("loadUserId"), vec!["load", "user", "id"]);
        assert_eq!(
            identifier_tokens("load-user$id"),
            vec!["load", "user", "id"],
        );
        assert_eq!(
            identifier_tokens("_leading__trailing_"),
            vec!["leading", "trailing"],
        );
    }
}
