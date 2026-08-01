//! Type-definition extraction for `analyze similarity --target types`.
//!
//! Collects `interface`, `type` alias, and `enum` declarations —
//! including exported and `namespace`-nested ones — into the neutral
//! [`TypeShape`]. A `type X = { … }` object literal becomes a
//! [`TypeDefKind::Record`] like an interface; any other alias target is
//! an [`TypeDefKind::Alias`]. Interface method signatures and `class`
//! bodies are function-shaped surface and stay with function
//! extraction.

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::GetSpan;

use lens_domain::{
    LineIndex, SourceSpan, TypeDefKind, TypeMemberShape, TypeShape, TypeVariantShape,
};

use crate::attrs::name_looks_like_test_class;
use crate::parser::{Dialect, TsParseError, jsdoc_by_attach_offset, ts_type_paths};
use crate::walk::method_key_name;

/// Extract every `interface` / `type` alias / `enum` in `source`.
pub fn extract_type_defs(source: &str, dialect: Dialect) -> Result<Vec<TypeShape>, TsParseError> {
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, source, dialect.source_type()).parse();
    if !ret.diagnostics.is_empty() {
        return Err(TsParseError::from_diagnostics(
            ret.diagnostics
                .iter()
                .map(|e| e.message.as_ref().to_owned()),
        ));
    }
    let ctx = ExtractContext {
        source,
        line_index: LineIndex::new(source),
        jsdoc_by_attach: jsdoc_by_attach_offset(source, &ret.program.comments),
    };
    let mut out = Vec::new();
    for stmt in &ret.program.body {
        walk_stmt(stmt, &ctx, &mut out);
    }
    Ok(out)
}

struct ExtractContext<'a> {
    source: &'a str,
    line_index: LineIndex,
    jsdoc_by_attach: std::collections::HashMap<u32, String>,
}

impl ExtractContext<'_> {
    fn span(&self, span: oxc_span::Span) -> SourceSpan {
        SourceSpan {
            start_line: self.line_index.line(span.start),
            end_line: self.line_index.line(span.end),
        }
    }

    fn doc(&self, attach: u32) -> Option<String> {
        self.jsdoc_by_attach.get(&attach).cloned()
    }

    fn text(&self, span: oxc_span::Span) -> String {
        span.source_text(self.source).to_owned()
    }
}

fn walk_stmt(stmt: &Statement, ctx: &ExtractContext, out: &mut Vec<TypeShape>) {
    match stmt {
        Statement::TSInterfaceDeclaration(decl) => {
            out.push(interface_shape(decl, decl.span.start, ctx));
        }
        Statement::TSTypeAliasDeclaration(decl) => {
            out.push(alias_shape(decl, decl.span.start, ctx));
        }
        Statement::TSEnumDeclaration(decl) => out.push(enum_shape(decl, decl.span.start, ctx)),
        Statement::ExportNamedDeclaration(export) => {
            if let Some(decl) = &export.declaration {
                walk_exported_decl(decl, export.span.start, ctx, out);
            }
        }
        Statement::ExportDefaultDeclaration(export) => {
            if let ExportDefaultDeclarationKind::TSInterfaceDeclaration(decl) = &export.declaration
            {
                out.push(interface_shape(decl, export.span.start, ctx));
            }
        }
        Statement::TSModuleDeclaration(module) => {
            if let Some(body) = &module.body {
                walk_module_body(body, ctx, out);
            }
        }
        _ => {}
    }
}

fn walk_exported_decl(
    decl: &Declaration,
    attach: u32,
    ctx: &ExtractContext,
    out: &mut Vec<TypeShape>,
) {
    match decl {
        Declaration::TSInterfaceDeclaration(decl) => out.push(interface_shape(decl, attach, ctx)),
        Declaration::TSTypeAliasDeclaration(decl) => out.push(alias_shape(decl, attach, ctx)),
        Declaration::TSEnumDeclaration(decl) => out.push(enum_shape(decl, attach, ctx)),
        Declaration::TSModuleDeclaration(module) => {
            if let Some(body) = &module.body {
                walk_module_body(body, ctx, out);
            }
        }
        _ => {}
    }
}

fn walk_module_body(
    body: &TSModuleDeclarationBody,
    ctx: &ExtractContext,
    out: &mut Vec<TypeShape>,
) {
    match body {
        TSModuleDeclarationBody::TSModuleBlock(block) => {
            for stmt in &block.body {
                walk_stmt(stmt, ctx, out);
            }
        }
        TSModuleDeclarationBody::TSModuleDeclaration(nested) => {
            if let Some(body) = &nested.body {
                walk_module_body(body, ctx, out);
            }
        }
    }
}

fn interface_shape(decl: &TSInterfaceDeclaration, attach: u32, ctx: &ExtractContext) -> TypeShape {
    let name = decl.id.name.to_string();
    TypeShape {
        is_test: name_looks_like_test_class(&name),
        display_name: name,
        kind: TypeDefKind::Record,
        kind_label: "interface",
        members: signature_members(&decl.body.body, ctx),
        variants: Vec::new(),
        generics: generic_params(decl.type_parameters.as_deref(), ctx),
        doc: ctx.doc(attach),
        span: ctx.span(decl.span),
    }
}

fn alias_shape(decl: &TSTypeAliasDeclaration, attach: u32, ctx: &ExtractContext) -> TypeShape {
    let name = decl.id.name.to_string();
    let (kind, members) = match &decl.type_annotation {
        TSType::TSTypeLiteral(literal) => (
            TypeDefKind::Record,
            signature_members(&literal.members, ctx),
        ),
        target => (TypeDefKind::Alias, vec![type_member(None, target, ctx)]),
    };
    TypeShape {
        is_test: name_looks_like_test_class(&name),
        display_name: name,
        kind,
        kind_label: "type_alias",
        members,
        variants: Vec::new(),
        generics: generic_params(decl.type_parameters.as_deref(), ctx),
        doc: ctx.doc(attach),
        span: ctx.span(decl.span),
    }
}

fn enum_shape(decl: &TSEnumDeclaration, attach: u32, ctx: &ExtractContext) -> TypeShape {
    let name = decl.id.name.to_string();
    TypeShape {
        is_test: name_looks_like_test_class(&name),
        display_name: name,
        kind: TypeDefKind::Enum,
        kind_label: "enum",
        members: Vec::new(),
        variants: decl
            .body
            .members
            .iter()
            .filter_map(|member| enum_member_name(&member.id))
            .map(|name| TypeVariantShape {
                name,
                members: Vec::new(),
            })
            .collect(),
        generics: Vec::new(),
        doc: ctx.doc(attach),
        span: ctx.span(decl.span),
    }
}

/// Property signatures become members; method / call / construct /
/// index signatures are function-shaped or nameless and contribute
/// nothing in the data-shape model.
fn signature_members(signatures: &[TSSignature], ctx: &ExtractContext) -> Vec<TypeMemberShape> {
    signatures
        .iter()
        .filter_map(|signature| {
            let TSSignature::TSPropertySignature(property) = signature else {
                return None;
            };
            let name = method_key_name(&property.key);
            let member = match &property.type_annotation {
                Some(annotation) => type_member(name, &annotation.type_annotation, ctx),
                None => TypeMemberShape {
                    name,
                    type_text: None,
                    type_paths: Vec::new(),
                },
            };
            Some(member)
        })
        .collect()
}

fn type_member(name: Option<String>, ty: &TSType, ctx: &ExtractContext) -> TypeMemberShape {
    let mut type_paths = Vec::new();
    ts_type_paths(ty, &mut type_paths);
    TypeMemberShape {
        name,
        type_text: Some(ctx.text(ty.span())),
        type_paths,
    }
}

fn generic_params(
    params: Option<&TSTypeParameterDeclaration>,
    ctx: &ExtractContext,
) -> Vec<String> {
    params
        .map(|declaration| {
            declaration
                .params
                .iter()
                .map(|param| ctx.text(param.span))
                .collect()
        })
        .unwrap_or_default()
}

fn enum_member_name(name: &TSEnumMemberName) -> Option<String> {
    match name {
        TSEnumMemberName::Identifier(id) => Some(id.name.to_string()),
        TSEnumMemberName::String(literal) | TSEnumMemberName::ComputedString(literal) => {
            Some(literal.value.to_string())
        }
        TSEnumMemberName::ComputedTemplateString(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn extract(source: &str) -> Vec<TypeShape> {
        extract_type_defs(source, Dialect::Ts).expect("source should parse")
    }

    #[test]
    fn extracts_interface_properties_with_types_and_doc() {
        let shapes = extract(
            r#"
/** A user record. */
export interface User<T> {
    id: number;
    names: Array<string>;
    extra: T;
    load(): void;
}
"#,
        );

        assert_eq!(shapes.len(), 1);
        let shape = &shapes[0];
        assert_eq!(shape.display_name, "User");
        assert_eq!(shape.kind, TypeDefKind::Record);
        assert_eq!(shape.kind_label, "interface");
        assert_eq!(shape.doc.as_deref(), Some("A user record."));
        assert_eq!(shape.generics, ["T"]);
        assert_eq!(shape.span.start_line, 3);
        assert_eq!(shape.span.end_line, 8);
        // The method signature is not a data member.
        let names: Vec<_> = shape.members.iter().map(|m| m.name.as_deref()).collect();
        assert_eq!(names, [Some("id"), Some("names"), Some("extra")]);
        assert_eq!(shape.members[1].type_text.as_deref(), Some("Array<string>"));
        assert_eq!(shape.members[1].type_paths, ["Array", "string"]);
    }

    #[rstest]
    #[case::object_literal_alias("type Point = { x: number; y: number };", TypeDefKind::Record, 2)]
    #[case::union_alias("type Id = string | number;", TypeDefKind::Alias, 1)]
    fn alias_kind_follows_target_shape(
        #[case] source: &str,
        #[case] kind: TypeDefKind,
        #[case] member_count: usize,
    ) {
        let shapes = extract(source);

        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].kind, kind);
        assert_eq!(shapes[0].kind_label, "type_alias");
        assert_eq!(shapes[0].members.len(), member_count);
    }

    #[test]
    fn union_alias_records_target_text_and_paths() {
        let shapes = extract("type Id = string | UserId;");

        let member = &shapes[0].members[0];
        assert_eq!(member.name, None);
        assert_eq!(member.type_text.as_deref(), Some("string | UserId"));
        assert_eq!(member.type_paths, ["string", "UserId"]);
    }

    #[test]
    fn extracts_enum_members_as_variants() {
        let shapes = extract(
            r#"
export enum Level {
    Low,
    High = "high",
}
"#,
        );

        let shape = &shapes[0];
        assert_eq!(shape.kind, TypeDefKind::Enum);
        let variants: Vec<_> = shape.variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(variants, ["Low", "High"]);
        assert!(shape.variants.iter().all(|v| v.members.is_empty()));
    }

    #[test]
    fn recurses_into_namespaces_and_handles_default_export() {
        let shapes = extract(
            r#"
namespace api {
    export interface Request { url: string }
}
export default interface Response { status: number }
"#,
        );

        let names: Vec<_> = shapes.iter().map(|s| s.display_name.as_str()).collect();
        assert_eq!(names, ["Request", "Response"]);
    }

    #[test]
    fn marks_test_prefixed_types_and_skips_classes_and_functions() {
        let shapes = extract(
            r#"
interface TestFixture { seed: number }
class Widget { id: number = 0 }
function make(): void {}
"#,
        );

        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].display_name, "TestFixture");
        assert!(shapes[0].is_test);
    }

    #[test]
    fn parse_errors_surface() {
        assert!(extract_type_defs("interface {", Dialect::Ts).is_err());
    }
}
