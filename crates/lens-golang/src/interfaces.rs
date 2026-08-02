//! Go interface method-set extraction.
//!
//! Collects one [`InterfaceShape`] per `type Name interface { … }`
//! declaration, carrying the directly declared method names and their
//! parameter-slot counts. Consumers match concrete methods against
//! these sets by name and arity — the structural approximation of "may
//! satisfy this interface" that needs no type inference.
//!
//! Embedded interfaces (`type Store interface { io.Reader; local }`)
//! are deliberately not expanded: an embed declared in the analyzed
//! tree contributes its methods through its own declaration, and an
//! out-of-tree embed has no visible method set to expand. Anonymous
//! interface literals in field or parameter positions carry no name to
//! report and are skipped for the same reason.

use lens_domain::{InterfaceMethodShape, InterfaceShape, SyntaxFact, qualify_module};
use tree_sitter::Node;

use crate::node_text::node_str;
use crate::parser::{GoParseError, parameter_slot_names, parse_tree};

/// Extract the named interface declarations of one Go source file,
/// qualified at `module`. Function-local declarations are included: an
/// interface declared inside a function body dispatches like any other.
pub fn extract_interface_shapes_with_module(
    source: &str,
    module: &str,
) -> Result<Vec<InterfaceShape>, GoParseError> {
    let tree = parse_tree(source)?;
    let mut out = Vec::new();
    collect_interfaces(tree.root_node(), source.as_bytes(), module, &mut out);
    Ok(out)
}

fn collect_interfaces(node: Node<'_>, source: &[u8], module: &str, out: &mut Vec<InterfaceShape>) {
    if node.kind() == "type_declaration" {
        let mut cursor = node.walk();
        for spec in node.named_children(&mut cursor) {
            // `type_alias` (`type A = B`) declares no new method set,
            // and its target contributes through its own declaration.
            if spec.kind() == "type_spec" {
                collect_interface_spec(spec, source, module, out);
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_interfaces(child, source, module, out);
    }
}

fn collect_interface_spec(
    spec: Node<'_>,
    source: &[u8],
    module: &str,
    out: &mut Vec<InterfaceShape>,
) {
    let Some(ty) = spec.child_by_field_name("type") else {
        return;
    };
    if ty.kind() != "interface_type" {
        return;
    }
    let Some(name) = spec
        .child_by_field_name("name")
        .and_then(|name| node_str(name, source))
    else {
        return;
    };
    out.push(InterfaceShape {
        display_name: name.to_owned(),
        qualified_name: SyntaxFact::Known(qualify_module(module, name)),
        methods: interface_methods(ty, source),
    });
}

/// The `method_elem` children of an `interface_type`: its directly
/// declared methods. Embedded types appear as `type_elem` children and
/// are skipped (see the module docs).
fn interface_methods(interface_type: Node<'_>, source: &[u8]) -> Vec<InterfaceMethodShape> {
    let mut methods = Vec::new();
    let mut cursor = interface_type.walk();
    for elem in interface_type.named_children(&mut cursor) {
        if elem.kind() != "method_elem" {
            continue;
        }
        let Some(name) = elem
            .child_by_field_name("name")
            .and_then(|name| node_str(name, source))
        else {
            continue;
        };
        let param_count = elem
            .child_by_field_name("parameters")
            .map_or(0, |params| parameter_slot_names(params, source).len());
        methods.push(InterfaceMethodShape {
            name: name.to_owned(),
            param_count,
        });
    }
    methods
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn interfaces(source: &str) -> Vec<InterfaceShape> {
        extract_interface_shapes_with_module(source, "pkg").unwrap()
    }

    fn methods_of(shape: &InterfaceShape) -> Vec<(&str, usize)> {
        shape
            .methods
            .iter()
            .map(|m| (m.name.as_str(), m.param_count))
            .collect()
    }

    #[test]
    fn interfaces_carry_their_qualified_name_and_direct_methods() {
        let src = "package p\n\
                   type Store interface {\n\
                   \tGet(id string) string\n\
                   \tPut(id string, value string) error\n\
                   }\n";
        let shapes = interfaces(src);
        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].display_name, "Store");
        assert_eq!(
            shapes[0].qualified_name.known_value().map(String::as_str),
            Some("pkg::Store"),
        );
        assert_eq!(methods_of(&shapes[0]), [("Get", 1), ("Put", 2)]);
    }

    /// The arity of a method spec counts parameter slots the way Go
    /// compares signatures: grouped names expand, unnamed types count
    /// one each, a variadic slot is one, and a niladic method is zero.
    #[rstest]
    #[case::grouped_names("Do(a, b int)", 2)]
    #[case::unnamed_types("Do(int, string)", 2)]
    #[case::variadic("Do(prefix string, rest ...int)", 2)]
    #[case::niladic("Do()", 0)]
    fn method_arity_counts_parameter_slots(#[case] method: &str, #[case] expected: usize) {
        let src = format!("package p\ntype I interface {{\n\t{method}\n}}\n");
        let shapes = interfaces(&src);
        assert_eq!(methods_of(&shapes[0]), [("Do", expected)]);
    }

    /// Embedded interfaces contribute no methods here: their own
    /// declarations carry them when in scope, so expanding embeds would
    /// only double-count.
    #[test]
    fn embedded_interfaces_are_not_expanded() {
        let src = "package p\n\
                   type Reader interface {\n\
                   \tRead(p []byte) (int, error)\n\
                   }\n\
                   type Store interface {\n\
                   \tReader\n\
                   \tio.Closer\n\
                   \tGet(id string) string\n\
                   }\n";
        let shapes = interfaces(src);
        let by_name: Vec<(&str, Vec<(&str, usize)>)> = shapes
            .iter()
            .map(|s| (s.display_name.as_str(), methods_of(s)))
            .collect();
        assert_eq!(
            by_name,
            [("Reader", vec![("Read", 1)]), ("Store", vec![("Get", 1)]),],
        );
    }

    #[test]
    fn grouped_type_declarations_yield_every_interface() {
        let src = "package p\n\
                   type (\n\
                   \tA interface{ One() }\n\
                   \tB interface{ Two() }\n\
                   \tS struct{}\n\
                   )\n";
        let shapes = interfaces(src);
        let names: Vec<&str> = shapes.iter().map(|s| s.display_name.as_str()).collect();
        assert_eq!(names, ["A", "B"]);
    }

    #[test]
    fn non_interface_types_and_aliases_are_skipped() {
        let src = "package p\n\
                   type S struct{ x int }\n\
                   type N int\n\
                   type A = interface{ Hidden() }\n";
        assert!(
            interfaces(src).is_empty(),
            "aliases declare no new method set",
        );
    }

    /// A function-local interface dispatches like a package-level one,
    /// so the walk descends into function bodies.
    #[test]
    fn function_local_interfaces_are_collected() {
        let src = "package p\n\
                   func f() {\n\
                   \ttype local interface{ Emit(x int) }\n\
                   \t_ = 1\n\
                   }\n";
        let shapes = interfaces(src);
        assert_eq!(shapes.len(), 1);
        assert_eq!(shapes[0].display_name, "local");
        assert_eq!(methods_of(&shapes[0]), [("Emit", 1)]);
    }

    /// An empty interface (`interface{}`) constrains nothing and yields
    /// an empty method set rather than being dropped: the declaration
    /// itself is still a named interface.
    #[test]
    fn empty_interfaces_have_no_methods() {
        let shapes = interfaces("package p\ntype Any interface{}\n");
        assert_eq!(shapes.len(), 1);
        assert!(shapes[0].methods.is_empty());
    }
}
