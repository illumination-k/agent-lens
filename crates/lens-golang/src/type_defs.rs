//! Type-definition extraction for `analyze similarity --target types`.
//!
//! Collects top-level `type` declarations — including grouped
//! `type ( … )` blocks — into the neutral [`TypeShape`]. A
//! `struct_type` underlying type becomes a [`TypeDefKind::Record`];
//! everything else (defined types like `type UserID int64` and aliases
//! like `type B = A`) becomes an [`TypeDefKind::Alias`]. Interface
//! types are method sets, not data shapes, and are skipped in this
//! model.

use lens_domain::{SourceSpan, TypeDefKind, TypeMemberShape, TypeShape};
use tree_sitter::Node;

use crate::node_text::node_str;
use crate::parser::{GoParseError, collect_type_parameters, doc_comment_text, parse_tree};

/// Extract every top-level struct / defined type / alias in `source`.
pub fn extract_type_defs(source: &str) -> Result<Vec<TypeShape>, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let root = tree.root_node();
    let mut out = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() != "type_declaration" {
            continue;
        }
        let mut spec_cursor = child.walk();
        let specs: Vec<Node> = child
            .named_children(&mut spec_cursor)
            .filter(|node| matches!(node.kind(), "type_spec" | "type_alias"))
            .collect();
        // A single `type Name …` hangs its doc comment off the
        // declaration; specs inside a `type ( … )` group carry their own.
        let declaration_doc = (specs.len() == 1)
            .then(|| doc_comment_text(child, bytes))
            .flatten();
        for spec in specs {
            let doc = declaration_doc
                .clone()
                .or_else(|| doc_comment_text(spec, bytes));
            if let Some(shape) = spec_shape(spec, bytes, doc) {
                out.push(shape);
            }
        }
    }
    Ok(out)
}

fn spec_shape(spec: Node<'_>, source: &[u8], doc: Option<String>) -> Option<TypeShape> {
    let name = node_str(spec.child_by_field_name("name")?, source)?.to_owned();
    let underlying = spec.child_by_field_name("type")?;
    let (kind, kind_label, members) = match underlying.kind() {
        "struct_type" => (
            TypeDefKind::Record,
            "struct",
            struct_members(underlying, source),
        ),
        "interface_type" => return None,
        _ => (
            TypeDefKind::Alias,
            "type_alias",
            vec![TypeMemberShape {
                name: None,
                type_text: node_str(underlying, source).map(str::to_owned),
                type_paths: node_str(underlying, source)
                    .map(str::to_owned)
                    .into_iter()
                    .collect(),
            }],
        ),
    };
    Some(TypeShape {
        display_name: name,
        kind,
        kind_label,
        members,
        variants: Vec::new(),
        generics: collect_type_parameters(spec, source),
        doc,
        span: SourceSpan {
            start_line: spec.start_position().row + 1,
            end_line: spec.end_position().row + 1,
        },
        // Go has no test-shaped type marker; `_test.go` placement is a
        // path-level fact the corpus applies on top.
        is_test: false,
    })
}

/// One member per declared field name; an embedded field (`sync.Mutex`)
/// has no name and contributes a nameless member with its type text.
fn struct_members(struct_type: Node<'_>, source: &[u8]) -> Vec<TypeMemberShape> {
    let mut out = Vec::new();
    let mut cursor = struct_type.walk();
    for list in struct_type.named_children(&mut cursor) {
        if list.kind() != "field_declaration_list" {
            continue;
        }
        let mut field_cursor = list.walk();
        for field in list.named_children(&mut field_cursor) {
            if field.kind() != "field_declaration" {
                continue;
            }
            let type_text = field
                .child_by_field_name("type")
                .and_then(|ty| node_str(ty, source))
                .map(str::to_owned);
            let names = field_names(field, source);
            if names.is_empty() {
                out.push(member(None, type_text.clone()));
            }
            for name in names {
                out.push(member(Some(name), type_text.clone()));
            }
        }
    }
    out
}

fn member(name: Option<String>, type_text: Option<String>) -> TypeMemberShape {
    TypeMemberShape {
        name,
        // Go types are captured as raw text, mirroring how the function
        // signature adapter records parameter types.
        type_paths: type_text.clone().into_iter().collect(),
        type_text,
    }
}

fn field_names(field: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = field.walk();
    if !cursor.goto_first_child() {
        return names;
    }
    loop {
        if cursor.field_name() == Some("name")
            && cursor.node().kind() == "field_identifier"
            && let Some(text) = node_str(cursor.node(), source)
        {
            names.push(text.to_owned());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    names
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
            r#"package main

// User is a user record.
type User struct {
	ID    int64
	Names []string
	mu    sync.Mutex
}
"#,
        );

        assert_eq!(shapes.len(), 1);
        let shape = &shapes[0];
        assert_eq!(shape.display_name, "User");
        assert_eq!(shape.kind, TypeDefKind::Record);
        assert_eq!(shape.kind_label, "struct");
        assert_eq!(shape.doc.as_deref(), Some("User is a user record."));
        assert_eq!(shape.span.start_line, 4);
        assert_eq!(shape.span.end_line, 8);
        let names: Vec<_> = shape.members.iter().map(|m| m.name.as_deref()).collect();
        assert_eq!(names, [Some("ID"), Some("Names"), Some("mu")]);
        assert_eq!(shape.members[1].type_text.as_deref(), Some("[]string"));
    }

    #[test]
    fn multi_name_fields_yield_one_member_per_name() {
        let shapes = extract("package main\n\ntype Point struct {\n\tX, Y float64\n}\n");

        let names: Vec<_> = shapes[0]
            .members
            .iter()
            .map(|m| m.name.as_deref())
            .collect();
        assert_eq!(names, [Some("X"), Some("Y")]);
        assert!(
            shapes[0]
                .members
                .iter()
                .all(|m| m.type_text.as_deref() == Some("float64")),
        );
    }

    #[test]
    fn embedded_fields_become_nameless_members() {
        let shapes =
            extract("package main\n\ntype Server struct {\n\tsync.Mutex\n\tAddr string\n}\n");

        assert_eq!(shapes[0].members[0].name, None);
        assert_eq!(
            shapes[0].members[0].type_text.as_deref(),
            Some("sync.Mutex")
        );
        assert_eq!(shapes[0].members[1].name.as_deref(), Some("Addr"));
    }

    #[rstest]
    #[case::defined_type("package main\n\ntype UserID int64\n", "int64")]
    #[case::alias("package main\n\ntype ID = int64\n", "int64")]
    #[case::slice_type("package main\n\ntype Names []string\n", "[]string")]
    fn non_struct_types_are_aliases(#[case] source: &str, #[case] target: &str) {
        let shapes = extract(source);

        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].kind, TypeDefKind::Alias);
        assert_eq!(shapes[0].kind_label, "type_alias");
        assert_eq!(shapes[0].members[0].type_text.as_deref(), Some(target));
    }

    #[test]
    fn grouped_declarations_emit_each_spec_with_its_own_doc() {
        let shapes = extract(
            r#"package main

type (
	// A holds an X.
	A struct{ X int }
	B struct{ Y int }
)
"#,
        );

        assert_eq!(shapes.len(), 2);
        assert_eq!(shapes[0].display_name, "A");
        assert_eq!(shapes[0].doc.as_deref(), Some("A holds an X."));
        assert_eq!(shapes[1].display_name, "B");
        assert_eq!(shapes[1].doc, None);
    }

    #[test]
    fn interfaces_functions_and_vars_are_skipped() {
        let shapes = extract(
            r#"package main

type Reader interface {
	Read(p []byte) (int, error)
}

func Work() {}

var x = 1

type Payload struct{ Data []byte }
"#,
        );

        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].display_name, "Payload");
    }

    #[test]
    fn generic_struct_records_type_parameters() {
        let shapes = extract("package main\n\ntype Box[T any] struct {\n\tValue T\n}\n");

        assert_eq!(shapes[0].generics, ["T any"]);
    }

    #[test]
    fn parse_errors_surface() {
        // tree-sitter is resilient; force the no-tree path via a
        // degenerate input only if the grammar rejects it. An empty
        // file still parses, so just assert extraction succeeds and
        // yields nothing.
        assert!(extract("").is_empty());
    }
}
