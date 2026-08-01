//! Type-definition extraction for `analyze similarity --target types`.
//!
//! Python spells data shapes as classes with annotated class-level
//! attributes — dataclasses, `TypedDict`s, pydantic models, plain
//! annotated classes — plus `Enum` subclasses and PEP 695 `type`
//! aliases. Classes without a single annotated class attribute are
//! behaviour containers and stay with function extraction; their nested
//! classes are still walked.

use lens_domain::{
    LineIndex, SourceSpan, TypeDefKind, TypeMemberShape, TypeShape, TypeVariantShape,
};
use ruff_python_ast::{Expr, Stmt, StmtClassDef, StmtTypeAlias};
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;

use crate::attrs::{dotted_path, is_test_class};
use crate::parser::{PythonParseError, annotation_paths};

/// Extract every data-shaped class, `Enum` subclass, and `type` alias
/// in `source`.
pub fn extract_type_defs(source: &str) -> Result<Vec<TypeShape>, PythonParseError> {
    let module = parse_module(source)?.into_syntax();
    let ctx = ExtractContext {
        source,
        line_index: LineIndex::new(source),
    };
    let mut out = Vec::new();
    walk_stmts(&module.body, false, &ctx, &mut out);
    Ok(out)
}

struct ExtractContext<'a> {
    source: &'a str,
    line_index: LineIndex,
}

impl ExtractContext<'_> {
    fn text(&self, node: &impl Ranged) -> String {
        self.source[node.range()].to_owned()
    }

    fn span(&self, start: &impl Ranged, end: &impl Ranged) -> SourceSpan {
        SourceSpan {
            start_line: self.line_index.line(start.range().start().to_u32()),
            // `range.end()` lands just past the last byte; step back onto
            // the line that byte sits on, mirroring function extraction.
            end_line: self
                .line_index
                .line(end.range().end().to_u32().saturating_sub(1)),
        }
    }
}

fn walk_stmts(
    body: &[Stmt],
    in_test_context: bool,
    ctx: &ExtractContext,
    out: &mut Vec<TypeShape>,
) {
    for stmt in body {
        match stmt {
            Stmt::ClassDef(class) => {
                let is_test = in_test_context || is_test_class(class);
                if let Some(shape) = class_shape(class, is_test, ctx) {
                    out.push(shape);
                }
                walk_stmts(&class.body, is_test, ctx, out);
            }
            Stmt::TypeAlias(alias) => out.push(alias_shape(alias, in_test_context, ctx)),
            _ => {}
        }
    }
}

fn class_shape(class: &StmtClassDef, is_test: bool, ctx: &ExtractContext) -> Option<TypeShape> {
    if inherits_enum(class) {
        return Some(enum_class_shape(class, is_test, ctx));
    }
    let members = annotated_members(&class.body, ctx);
    if members.is_empty() {
        return None;
    }
    Some(TypeShape {
        display_name: class.name.to_string(),
        kind: TypeDefKind::Record,
        kind_label: if has_dataclass_decorator(class) {
            "dataclass"
        } else {
            "class"
        },
        members,
        variants: Vec::new(),
        generics: Vec::new(),
        doc: docstring(&class.body),
        span: ctx.span(&class.name, class),
        is_test,
    })
}

fn enum_class_shape(class: &StmtClassDef, is_test: bool, ctx: &ExtractContext) -> TypeShape {
    let variants = class
        .body
        .iter()
        .filter_map(|stmt| {
            let Stmt::Assign(assign) = stmt else {
                return None;
            };
            let [Expr::Name(name)] = assign.targets.as_slice() else {
                return None;
            };
            Some(TypeVariantShape {
                name: name.id.to_string(),
                members: Vec::new(),
            })
        })
        .collect();
    TypeShape {
        display_name: class.name.to_string(),
        kind: TypeDefKind::Enum,
        kind_label: "enum",
        members: Vec::new(),
        variants,
        generics: Vec::new(),
        doc: docstring(&class.body),
        span: ctx.span(&class.name, class),
        is_test,
    }
}

fn alias_shape(alias: &StmtTypeAlias, is_test: bool, ctx: &ExtractContext) -> TypeShape {
    let mut type_paths = Vec::new();
    annotation_paths(&alias.value, &mut type_paths);
    let display_name = match alias.name.as_ref() {
        Expr::Name(name) => name.id.to_string(),
        other => ctx.text(other),
    };
    TypeShape {
        display_name,
        kind: TypeDefKind::Alias,
        kind_label: "type_alias",
        members: vec![TypeMemberShape {
            name: None,
            type_text: Some(ctx.text(alias.value.as_ref())),
            type_paths,
        }],
        variants: Vec::new(),
        generics: Vec::new(),
        doc: None,
        span: ctx.span(alias, alias),
        is_test,
    }
}

/// Class-level `name: annotation [= default]` attributes, in source
/// order. Un-annotated assignments and methods contribute nothing.
fn annotated_members(body: &[Stmt], ctx: &ExtractContext) -> Vec<TypeMemberShape> {
    body.iter()
        .filter_map(|stmt| {
            let Stmt::AnnAssign(ann) = stmt else {
                return None;
            };
            let Expr::Name(name) = ann.target.as_ref() else {
                return None;
            };
            let mut type_paths = Vec::new();
            annotation_paths(&ann.annotation, &mut type_paths);
            Some(TypeMemberShape {
                name: Some(name.id.to_string()),
                type_text: Some(ctx.text(ann.annotation.as_ref())),
                type_paths,
            })
        })
        .collect()
}

fn inherits_enum(class: &StmtClassDef) -> bool {
    class.bases().iter().any(|base| {
        let Some(path) = dotted_path(base) else {
            return false;
        };
        let last = match path.as_slice() {
            [name] => *name,
            [.., last] if path[0] == "enum" => *last,
            _ => return false,
        };
        matches!(last, "Enum" | "IntEnum" | "StrEnum" | "Flag" | "IntFlag")
    })
}

fn has_dataclass_decorator(class: &StmtClassDef) -> bool {
    class.decorator_list.iter().any(|decorator| {
        matches!(
            dotted_path(&decorator.expression).as_deref(),
            Some(["dataclass"]) | Some(["dataclasses", "dataclass"])
        )
    })
}

/// PEP 257 class docstring: a string-literal expression statement as the
/// first statement of the body.
fn docstring(body: &[Stmt]) -> Option<String> {
    let Some(Stmt::Expr(first)) = body.first() else {
        return None;
    };
    let Expr::StringLiteral(literal) = first.value.as_ref() else {
        return None;
    };
    let text = literal.value.to_str().trim().to_owned();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn extract(source: &str) -> Vec<TypeShape> {
        extract_type_defs(source).expect("source should parse")
    }

    #[test]
    fn extracts_dataclass_members_with_annotations_and_doc() {
        let shapes = extract(
            r#"
from dataclasses import dataclass

@dataclass
class User:
    """A user record."""

    id: int
    names: list[str]
    active: bool = True
"#,
        );

        assert_eq!(shapes.len(), 1);
        let shape = &shapes[0];
        assert_eq!(shape.display_name, "User");
        assert_eq!(shape.kind, TypeDefKind::Record);
        assert_eq!(shape.kind_label, "dataclass");
        assert_eq!(shape.doc.as_deref(), Some("A user record."));
        assert_eq!(shape.span.start_line, 5);
        assert_eq!(shape.span.end_line, 10);
        let names: Vec<_> = shape.members.iter().map(|m| m.name.as_deref()).collect();
        assert_eq!(names, [Some("id"), Some("names"), Some("active")]);
        assert_eq!(shape.members[1].type_text.as_deref(), Some("list[str]"));
        assert_eq!(shape.members[1].type_paths, ["list", "str"]);
    }

    #[rstest]
    #[case::typed_dict(
        "from typing import TypedDict\nclass Point(TypedDict):\n    x: int\n    y: int\n",
        "class"
    )]
    #[case::plain_annotated_class("class Config:\n    host: str\n    port: int\n", "class")]
    fn annotated_classes_are_records(#[case] source: &str, #[case] label: &str) {
        let shapes = extract(source);

        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].kind, TypeDefKind::Record);
        assert_eq!(shapes[0].kind_label, label);
        assert_eq!(shapes[0].members.len(), 2);
    }

    #[test]
    fn extracts_enum_subclass_variants() {
        let shapes = extract(
            r#"
import enum

class Level(enum.Enum):
    LOW = 1
    HIGH = 2

class Mode(IntEnum):
    A = enum.auto()
"#,
        );

        assert_eq!(shapes.len(), 2);
        assert_eq!(shapes[0].kind, TypeDefKind::Enum);
        assert_eq!(shapes[0].kind_label, "enum");
        let variants: Vec<_> = shapes[0].variants.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(variants, ["LOW", "HIGH"]);
        assert_eq!(shapes[1].variants.len(), 1);
    }

    /// A base whose tail is `Enum` but whose root is not the `enum`
    /// module must not classify the class: `models.Enum`-style bases
    /// from ORMs are not stdlib enums.
    #[test]
    fn non_enum_rooted_enum_base_is_not_an_enum() {
        let shapes = extract("class Fake(models.Enum):\n    A = 1\n");

        assert!(shapes.is_empty(), "got {shapes:?}");
    }

    #[test]
    fn extracts_pep695_type_alias() {
        let shapes = extract("type UserId = int | str\n");

        let shape = &shapes[0];
        assert_eq!(shape.display_name, "UserId");
        assert_eq!(shape.kind, TypeDefKind::Alias);
        assert_eq!(shape.kind_label, "type_alias");
        assert_eq!(shape.members[0].type_text.as_deref(), Some("int | str"));
        assert_eq!(shape.members[0].type_paths, ["int", "str"]);
    }

    #[test]
    fn behaviour_classes_are_skipped_but_nested_shapes_surface() {
        let shapes = extract(
            r#"
class Service:
    def run(self):
        return 1

    class Config:
        retries: int
"#,
        );

        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].display_name, "Config");
    }

    #[test]
    fn test_classes_and_nested_shapes_inherit_test_flag() {
        let shapes = extract(
            r#"
class TestFixtures:
    class Seed:
        value: int

class Real:
    value: int
"#,
        );

        let flags: Vec<_> = shapes
            .iter()
            .map(|s| (s.display_name.as_str(), s.is_test))
            .collect();
        assert_eq!(flags, [("Seed", true), ("Real", false)]);
    }

    #[test]
    fn unannotated_assignments_are_not_members() {
        let shapes = extract("class Config:\n    host: str\n    PORT = 80\n");

        assert_eq!(shapes[0].members.len(), 1);
        assert_eq!(shapes[0].members[0].name.as_deref(), Some("host"));
    }

    #[test]
    fn parse_errors_surface() {
        assert!(extract_type_defs("class !!!:").is_err());
    }
}
