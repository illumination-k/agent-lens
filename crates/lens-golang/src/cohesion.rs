//! tree-sitter-based cohesion extraction for Go source files.
//!
//! We emit one cohesion unit per receiver type (`func (r T) ...`) and
//! ignore free functions for now.

use std::collections::{BTreeMap, HashSet};

use lens_domain::{CohesionUnit, CohesionUnitKind, MethodCohesion, qualify};
use tree_sitter::Node;

use crate::parser::{GoParseError, function_name_text, method_receiver_type, parse_tree};

/// Extract one [`CohesionUnit`] per receiver type in `source`.
pub fn extract_cohesion_units(source: &str) -> Result<Vec<CohesionUnit>, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let mut by_owner: BTreeMap<String, Vec<MethodRow>> = BTreeMap::new();

    let mut cursor = tree.root_node().walk();
    for child in tree.root_node().named_children(&mut cursor) {
        if child.kind() != "method_declaration" {
            continue;
        }
        let Some(owner) = method_receiver_type(child, bytes) else {
            continue;
        };
        if let Some(row) = method_row(child, bytes) {
            by_owner.entry(owner).or_default().push(row);
        }
    }

    let mut out = Vec::new();
    for (owner, rows) in by_owner {
        let sibling_names: HashSet<String> = rows.iter().map(|r| r.short_name.clone()).collect();
        let methods: Vec<MethodCohesion> = rows
            .iter()
            .map(|row| {
                let mut calls: Vec<String> = row
                    .calls
                    .iter()
                    .filter(|c| sibling_names.contains(*c))
                    .cloned()
                    .collect();
                calls.sort();
                calls.dedup();

                let mut fields = row.fields.clone();
                fields.sort();
                fields.dedup();

                MethodCohesion::new(
                    qualify(Some(owner.as_str()), row.short_name.as_str()),
                    row.start_line,
                    row.end_line,
                    fields,
                    calls,
                )
            })
            .collect();

        if methods.is_empty() {
            continue;
        }
        let start_line = rows.iter().map(|r| r.start_line).min().unwrap_or(1);
        let end_line = rows.iter().map(|r| r.end_line).max().unwrap_or(start_line);

        out.push(CohesionUnit::build(
            CohesionUnitKind::Inherent,
            owner,
            start_line,
            end_line,
            methods,
        ));
    }

    Ok(out)
}

#[derive(Debug, Clone)]
struct MethodRow {
    short_name: String,
    start_line: usize,
    end_line: usize,
    fields: Vec<String>,
    calls: Vec<String>,
}

fn method_row(node: Node<'_>, source: &[u8]) -> Option<MethodRow> {
    let body = node.child_by_field_name("body")?;
    let short_name = function_name_text(node, source)?.to_owned();
    let receiver = receiver_name(node, source)?;
    let mut visitor = ReceiverRefVisitor::new(receiver.as_str(), source);
    visitor.visit(body);

    Some(MethodRow {
        short_name,
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        fields: visitor.fields,
        calls: visitor.calls,
    })
}

fn receiver_name(node: Node<'_>, source: &[u8]) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let mut cursor = receiver.walk();
    for child in receiver.named_children(&mut cursor) {
        if child.kind() != "parameter_declaration" {
            continue;
        }
        if let Some(name) = child.child_by_field_name("name") {
            return Some(node_text(name, source));
        }

        let mut inner = child.walk();
        for part in child.named_children(&mut inner) {
            if part.kind() == "identifier" {
                return Some(node_text(part, source));
            }
        }
    }
    None
}

struct ReceiverRefVisitor<'a> {
    receiver_name: &'a str,
    source: &'a [u8],
    fields: Vec<String>,
    calls: Vec<String>,
}

impl<'a> ReceiverRefVisitor<'a> {
    fn new(receiver_name: &'a str, source: &'a [u8]) -> Self {
        Self {
            receiver_name,
            source,
            fields: Vec::new(),
            calls: Vec::new(),
        }
    }

    fn visit(&mut self, node: Node<'_>) {
        if node.kind() == "call_expression" {
            self.record_call(node);
        }
        if node.kind() == "selector_expression" {
            self.record_field(node);
        }

        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit(child);
        }
    }

    fn record_call(&mut self, node: Node<'_>) {
        let Some(func) = node.child_by_field_name("function") else {
            return;
        };

        if func.kind() == "selector_expression"
            && let Some((recv, member)) = selector_parts(func, self.source)
            && recv == self.receiver_name
        {
            self.calls.push(member.to_owned());
        }
    }

    fn record_field(&mut self, node: Node<'_>) {
        let Some((recv, member)) = selector_parts(node, self.source) else {
            return;
        };
        if recv == self.receiver_name {
            self.fields.push(member.to_owned());
        }
    }
}

fn selector_parts<'a>(node: Node<'_>, source: &'a [u8]) -> Option<(&'a str, &'a str)> {
    let operand = node.child_by_field_name("operand")?;
    let field = node.child_by_field_name("field")?;
    let recv = operand.utf8_text(source).ok()?;
    let member = field.utf8_text(source).ok()?;
    Some((recv, member))
}

fn node_text(node: Node<'_>, source: &[u8]) -> String {
    node.utf8_text(source).unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_methods_by_receiver_type() {
        let src = r#"
package p

type S struct{}

func (s *S) A() int {
    return s.x + s.B()
}

func (s *S) B() int {
    return s.x
}
"#;
        let units = extract_cohesion_units(src).unwrap();
        assert_eq!(units.len(), 1);
        let unit = &units[0];
        assert_eq!(unit.type_name, "S");
        assert_eq!(unit.methods.len(), 2);
        assert_eq!(unit.start_line, 6);
        assert_eq!(unit.end_line, 12);

        let a = unit
            .methods
            .iter()
            .find(|m| m.name.ends_with("::A"))
            .expect("A method");
        assert!(a.fields.contains(&"x".to_owned()));
        assert!(a.calls.contains(&"B".to_owned()));
        assert_eq!(a.start_line, 6);
        assert_eq!(a.end_line, 8);
    }

    #[test]
    fn drops_methods_without_receiver_name() {
        let src = "package p\ntype S struct{}\nfunc (S) A() int { return 1 }\n";
        let units = extract_cohesion_units(src).unwrap();
        assert!(units.is_empty());
    }
}
