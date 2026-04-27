//! Small `syn` helpers shared between the lens-rust extractors.
//!
//! Each extractor (parser, complexity, cohesion, wrapper) used to keep
//! its own copy of `type_path_last_ident`; consolidating them here cuts
//! the structural duplication and means a future fix lands in one place.

use syn::Type;

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
