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
}
