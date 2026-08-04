//! Type-definition extraction for `analyze similarity --target types`.
//!
//! Walks the same item ladder as function extraction — top-level items
//! plus inline `mod` recursion with `#[cfg(test)]` context — but collects
//! `struct` / `enum` / `type` alias definitions into the neutral
//! [`TypeShape`] instead. `impl` and `trait` items carry no type
//! definitions of their own and are skipped.

use lens_domain::{SourceSpan, TypeDefKind, TypeMemberShape, TypeShape, TypeVariantShape};
use syn::spanned::Spanned;

use crate::common::render_tokens;
use crate::parser::{RustParseError, collect_type_paths, generic_summaries};

/// Extract every `struct`, `enum`, and `type` alias in `source`.
pub fn extract_type_defs(source: &str) -> Result<Vec<TypeShape>, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut out = Vec::new();
    walk_items(&file.items, false, &mut out);
    Ok(out)
}

fn walk_items(items: &[syn::Item], in_test_context: bool, out: &mut Vec<TypeShape>) {
    for item in items {
        match item {
            syn::Item::Struct(item_struct) => out.push(struct_shape(item_struct, in_test_context)),
            syn::Item::Enum(item_enum) => out.push(enum_shape(item_enum, in_test_context)),
            syn::Item::Type(item_type) => out.push(alias_shape(item_type, in_test_context)),
            syn::Item::Mod(item_mod) => {
                if let Some((_, nested)) = &item_mod.content {
                    let item_is_test = crate::attrs::has_cfg_test(&item_mod.attrs);
                    walk_items(nested, in_test_context || item_is_test, out);
                }
            }
            _ => {}
        }
    }
}

fn struct_shape(item: &syn::ItemStruct, is_test: bool) -> TypeShape {
    TypeShape {
        display_name: item.ident.to_string(),
        kind: TypeDefKind::Record,
        kind_label: "struct",
        members: field_members(&item.fields),
        variants: Vec::new(),
        generics: generic_summaries(&item.generics),
        doc: crate::attrs::doc_from_attrs(&item.attrs),
        span: item_span(&item.struct_token, item),
        is_test,
    }
}

fn enum_shape(item: &syn::ItemEnum, is_test: bool) -> TypeShape {
    TypeShape {
        display_name: item.ident.to_string(),
        kind: TypeDefKind::Enum,
        kind_label: "enum",
        members: Vec::new(),
        variants: item
            .variants
            .iter()
            .map(|variant| TypeVariantShape {
                name: variant.ident.to_string(),
                members: field_members(&variant.fields),
            })
            .collect(),
        generics: generic_summaries(&item.generics),
        doc: crate::attrs::doc_from_attrs(&item.attrs),
        span: item_span(&item.enum_token, item),
        is_test,
    }
}

fn alias_shape(item: &syn::ItemType, is_test: bool) -> TypeShape {
    TypeShape {
        display_name: item.ident.to_string(),
        kind: TypeDefKind::Alias,
        kind_label: "type_alias",
        members: vec![type_member(None, &item.ty)],
        variants: Vec::new(),
        generics: generic_summaries(&item.generics),
        doc: crate::attrs::doc_from_attrs(&item.attrs),
        span: item_span(&item.type_token, item),
        is_test,
    }
}

fn field_members(fields: &syn::Fields) -> Vec<TypeMemberShape> {
    fields
        .iter()
        .map(|field| type_member(field.ident.as_ref().map(ToString::to_string), &field.ty))
        .collect()
}

fn type_member(name: Option<String>, ty: &syn::Type) -> TypeMemberShape {
    let mut type_paths = Vec::new();
    collect_type_paths(ty, &mut type_paths);
    TypeMemberShape {
        name,
        type_text: Some(render_tokens(ty)),
        type_paths,
    }
}

/// Span of the definition itself. `item.span()` starts at the first
/// attribute, which would count doc-comment lines toward `--min-lines`;
/// starting at the introducing keyword mirrors how function extraction
/// anchors on the signature.
fn item_span(keyword: &impl Spanned, item: &impl Spanned) -> SourceSpan {
    SourceSpan {
        start_line: keyword.span().start().line,
        end_line: item.span().end().line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn extract(source: &str) -> Vec<TypeShape> {
        extract_type_defs(source).expect("source should parse")
    }

    #[test]
    fn extracts_struct_fields_with_types_and_doc() {
        let shapes = extract(
            r#"
/// A user record.
pub struct User<T: Clone> {
    pub id: u64,
    names: Vec<String>,
    extra: T,
}
"#,
        );

        assert_eq!(shapes.len(), 1);
        let shape = &shapes[0];
        assert_eq!(shape.display_name, "User");
        assert_eq!(shape.kind, TypeDefKind::Record);
        assert_eq!(shape.kind_label, "struct");
        assert_eq!(shape.doc.as_deref(), Some("A user record."));
        assert_eq!(shape.generics, ["T : Clone"]);
        assert_eq!(shape.span.start_line, 3);
        assert_eq!(shape.span.end_line, 7);
        assert!(!shape.is_test);
        let names: Vec<_> = shapes[0]
            .members
            .iter()
            .map(|m| m.name.as_deref())
            .collect();
        assert_eq!(names, [Some("id"), Some("names"), Some("extra")]);
        assert_eq!(
            shape.members[1].type_text.as_deref(),
            Some("Vec < String >")
        );
        assert_eq!(shape.members[1].type_paths, ["Vec", "String"]);
    }

    #[rstest]
    #[case::tuple_struct("struct Point(f64, f64);", 2)]
    #[case::unit_struct("struct Marker;", 0)]
    fn extracts_positional_and_unit_structs(#[case] source: &str, #[case] member_count: usize) {
        let shapes = extract(source);

        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].members.len(), member_count);
        assert!(shapes[0].members.iter().all(|m| m.name.is_none()));
    }

    #[test]
    fn extracts_enum_variants_with_payloads() {
        let shapes = extract(
            r#"
enum Event {
    Created { id: u64 },
    Renamed(String),
    Deleted,
}
"#,
        );

        let shape = &shapes[0];
        assert_eq!(shape.kind, TypeDefKind::Enum);
        assert_eq!(shape.kind_label, "enum");
        let variants: Vec<_> = shape.variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(variants, ["Created", "Renamed", "Deleted"]);
        assert_eq!(shape.variants[0].members[0].name.as_deref(), Some("id"));
        assert_eq!(shape.variants[1].members[0].name, None);
        assert!(shape.variants[2].members.is_empty());
    }

    #[test]
    fn extracts_type_alias_target() {
        let shapes = extract("type UserMap = std::collections::HashMap<u64, User>;");

        let shape = &shapes[0];
        assert_eq!(shape.kind, TypeDefKind::Alias);
        assert_eq!(shape.kind_label, "type_alias");
        assert_eq!(shape.members.len(), 1);
        assert!(
            shape.members[0]
                .type_paths
                .contains(&"std::collections::HashMap".to_owned()),
        );
    }

    #[test]
    fn recurses_into_modules_and_marks_cfg_test_context() {
        let shapes = extract(
            r#"
struct Outer { a: u32 }

mod inner {
    pub struct Nested { b: u32 }
}

#[cfg(test)]
mod tests {
    struct Fixture { c: u32 }
}
"#,
        );

        let by_name: Vec<_> = shapes
            .iter()
            .map(|s| (s.display_name.as_str(), s.is_test))
            .collect();
        assert_eq!(
            by_name,
            [("Outer", false), ("Nested", false), ("Fixture", true)],
        );
    }

    #[test]
    fn skips_impl_trait_and_function_items() {
        let shapes = extract(
            r#"
struct Only { a: u32 }
impl Only { fn method(&self) {} }
trait Behavior { fn act(&self); }
fn free() {}
"#,
        );

        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].display_name, "Only");
    }
}
