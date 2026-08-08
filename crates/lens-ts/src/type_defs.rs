//! Type-definition extraction for `analyze similarity --target types`.
//!
//! Collects `interface`, `type` alias, and `enum` declarations —
//! including exported and `namespace`-nested ones — into the neutral
//! [`TypeShape`]. A `type X = { … }` object literal becomes a
//! [`TypeDefKind::Record`] like an interface; any other alias target is
//! an [`TypeDefKind::Alias`]. An interface member is a member whether it
//! is spelled as a property, a method, a call / construct signature, or
//! an index signature — see [`signature_members`]. `class` bodies are
//! function-shaped surface and stay with function extraction.

use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_parser::Parser;
use oxc_span::GetSpan;

use lens_domain::{
    LineIndex, SourceSpan, TypeDefKind, TypeMemberShape, TypeShape, TypeVariantShape,
};

use crate::attrs::name_looks_like_test_class;
use crate::parser::{
    Dialect, TsParseError, formal_parameter_type_paths, jsdoc_by_attach_offset, ts_type_paths,
};
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

/// Every signature in the body becomes a member.
///
/// A property signature contributes its declared type. The other four
/// forms are function-shaped or nameless, but dropping them would leave a
/// method-only interface — the normal spelling of a repository or service
/// contract — with no members at all, and an empty shape matches every
/// other empty shape vacuously. So each is rendered into the *same*
/// currency a property holding that value would carry: a method signature
/// becomes its arrow type, so `load(): void` and `load: () => void` (the
/// same declaration in TypeScript) produce the same member.
///
/// Getters and setters are rendered as the accessor's own arrow type
/// rather than as the property type they stand for; the accessor spelling
/// is rare enough in interfaces that the extra branch would not pay for
/// itself.
fn signature_members(signatures: &[TSSignature], ctx: &ExtractContext) -> Vec<TypeMemberShape> {
    signatures
        .iter()
        .map(|signature| match signature {
            TSSignature::TSPropertySignature(property) => {
                let name = method_key_name(&property.key);
                match &property.type_annotation {
                    Some(annotation) => type_member(name, &annotation.type_annotation, ctx),
                    None => TypeMemberShape {
                        name,
                        type_text: None,
                        type_paths: Vec::new(),
                    },
                }
            }
            TSSignature::TSMethodSignature(method) => function_member(
                method_key_name(&method.key),
                "",
                method.type_parameters.as_deref(),
                &method.params,
                method.return_type.as_deref(),
                ctx,
            ),
            TSSignature::TSCallSignatureDeclaration(call) => function_member(
                None,
                "",
                call.type_parameters.as_deref(),
                &call.params,
                call.return_type.as_deref(),
                ctx,
            ),
            TSSignature::TSConstructSignatureDeclaration(construct) => function_member(
                None,
                "new ",
                construct.type_parameters.as_deref(),
                &construct.params,
                construct.return_type.as_deref(),
                ctx,
            ),
            TSSignature::TSIndexSignature(index) => index_member(index, ctx),
        })
        .collect()
}

/// Render a callable signature as the arrow type a property holding it
/// would declare: `<T>(a: string) => T`. An omitted return annotation is
/// spelled `any`, which is what TypeScript infers for it.
fn function_member(
    name: Option<String>,
    prefix: &str,
    type_parameters: Option<&TSTypeParameterDeclaration>,
    params: &FormalParameters,
    return_type: Option<&TSTypeAnnotation>,
    ctx: &ExtractContext,
) -> TypeMemberShape {
    let mut type_paths = Vec::new();
    formal_parameter_type_paths(params, &mut type_paths);
    let rendered: Vec<String> = params
        .items
        .iter()
        .map(|param| ctx.text(param.span))
        .chain(params.rest.iter().map(|rest| ctx.text(rest.span)))
        .collect();
    let generics = type_parameters.map(|params| ctx.text(params.span));
    let returns = match return_type {
        Some(annotation) => {
            ts_type_paths(&annotation.type_annotation, &mut type_paths);
            ctx.text(annotation.type_annotation.span())
        }
        None => "any".to_owned(),
    };
    TypeMemberShape {
        name,
        type_text: Some(format!(
            "{prefix}{}({}) => {returns}",
            generics.unwrap_or_default(),
            rendered.join(", "),
        )),
        type_paths,
    }
}

/// Render an index signature as `[string]: unknown`. The key *name*
/// (`key`, `k`, `_`) is a local binding with no bearing on the shape, so
/// only its type survives.
fn index_member(index: &TSIndexSignature, ctx: &ExtractContext) -> TypeMemberShape {
    let mut type_paths = Vec::new();
    let keys: Vec<String> = index
        .parameters
        .iter()
        .map(|parameter| {
            ts_type_paths(&parameter.type_annotation.type_annotation, &mut type_paths);
            ctx.text(parameter.type_annotation.type_annotation.span())
        })
        .collect();
    ts_type_paths(&index.type_annotation.type_annotation, &mut type_paths);
    let value = ctx.text(index.type_annotation.type_annotation.span());
    TypeMemberShape {
        name: None,
        type_text: Some(format!("[{}]: {value}", keys.join(", "))),
        type_paths,
    }
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
        let names: Vec<_> = shape.members.iter().map(|m| m.name.as_deref()).collect();
        assert_eq!(
            names,
            [Some("id"), Some("names"), Some("extra"), Some("load")]
        );
        assert_eq!(shape.members[1].type_text.as_deref(), Some("Array<string>"));
        assert_eq!(shape.members[1].type_paths, ["Array", "string"]);
    }

    /// The regression behind issue #425: a repository / service contract
    /// is spelled entirely with method signatures. Dropping those left an
    /// empty shape, and every empty shape matched every other one, so a
    /// 5-method repository, a 1-method health check, and an index-signature
    /// bag all landed in a single 95-99% cluster.
    #[test]
    fn method_only_interface_keeps_its_method_set() {
        let shapes = extract(
            r#"
export interface IArticleRepository {
    retrieveArticleBySlug(slug: string): Promise<Article>;
    listArticles(): Promise<Article[]>;
}
"#,
        );

        let members = &shapes[0].members;
        let names: Vec<_> = members.iter().map(|m| m.name.as_deref()).collect();
        assert_eq!(names, [Some("retrieveArticleBySlug"), Some("listArticles")]);
        assert_eq!(
            members[0].type_text.as_deref(),
            Some("(slug: string) => Promise<Article>"),
        );
        assert_eq!(members[0].type_paths, ["string", "Promise", "Article"]);
        assert_eq!(
            members[1].type_text.as_deref(),
            Some("() => Promise<Article[]>")
        );
    }

    /// `load(): void` and `load: () => void` declare the same thing in
    /// TypeScript. They must reduce to the same member, or a mirror
    /// interface that swapped spellings reads as drift.
    #[test]
    fn method_and_property_spellings_of_one_signature_agree() {
        let method = extract("interface A { load(a: string, ...rest: number[]): void }");
        let property = extract("interface B { load: (a: string, ...rest: number[]) => void }");

        let normalize = |shape: &TypeShape| {
            shape.members[0]
                .type_text
                .as_deref()
                .map(lens_domain::normalize_type_text)
        };
        assert_eq!(normalize(&method[0]), normalize(&property[0]));
        assert_eq!(
            method[0].members[0].type_paths,
            property[0].members[0].type_paths
        );
    }

    #[rstest]
    #[case::no_return_annotation("interface I { run(); }", "() => any", &[])]
    #[case::generic_method("interface I { map<T>(v: T): T }", "<T>(v: T) => T", &["T", "T"])]
    #[case::call_signature("interface I { (a: string): number }", "(a: string) => number", &["string", "number"])]
    #[case::construct_signature(
        "interface I { new (a: string): Widget }",
        "new (a: string) => Widget",
        &["string", "Widget"]
    )]
    #[case::index_signature("interface I { [key: string]: unknown }", "[string]: unknown", &["string"])]
    fn callable_and_index_signatures_render_as_member_types(
        #[case] source: &str,
        #[case] expected_text: &str,
        #[case] expected_paths: &[&str],
    ) {
        let shapes = extract(source);

        let member = &shapes[0].members[0];
        assert_eq!(member.type_text.as_deref(), Some(expected_text));
        assert_eq!(member.type_paths, expected_paths);
    }

    /// The index key binding is a local name with no bearing on the
    /// shape, so two bags spelled with different key names agree.
    #[test]
    fn index_signature_ignores_the_key_binding_name() {
        let a = extract("interface A { [key: string]: unknown }");
        let b = extract("interface B { [k: string]: unknown }");

        assert_eq!(a[0].members, b[0].members);
    }

    /// An empty interface is still empty — nothing was dropped, there was
    /// nothing there. The similarity corpus, not the extractor, decides
    /// what to do with a shapeless definition.
    #[test]
    fn empty_interface_stays_shapeless() {
        let shapes = extract("interface Marker {}");

        assert!(shapes[0].is_shapeless());
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

    /// Every declaration form must be reachable both bare and behind
    /// `export`: the two walkers have separate match arms, and dropping
    /// any one of them silently loses a whole category.
    #[rstest]
    #[case::bare_enum("enum Level { Low, High }\n", "Level")]
    #[case::exported_alias("export type Id = string | number;\n", "Id")]
    #[case::exported_namespace(
        "export namespace api {\n    export interface Request { url: string }\n}\n",
        "Request"
    )]
    fn bare_and_exported_declaration_forms_are_extracted(
        #[case] source: &str,
        #[case] expected_name: &str,
    ) {
        let shapes = extract(source);

        assert_eq!(shapes.len(), 1, "got {shapes:?}");
        assert_eq!(shapes[0].display_name, expected_name);
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
