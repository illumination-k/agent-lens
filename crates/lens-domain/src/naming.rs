//! Language-agnostic name-building helpers.
//!
//! Each language adapter eventually produces fully-qualified function
//! names like `Module::method` or `Class::method`. The mechanic — prefix
//! the owner with `::` separator when present, otherwise use the bare
//! method name — is identical across `lens-rust`, `lens-ts`, and
//! `lens-py`, so it lives here.

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
