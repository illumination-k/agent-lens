//! tree-sitter-based cohesion extraction for Go source files.
//!
//! We emit one cohesion unit per receiver type (`func (r T) ...`) plus a
//! file-level (`<module>`) unit built from the top-level free functions,
//! mirroring the Rust and Python adapters. In the package unit, package
//! `var` / `const` declarations act as shared fields and calls between
//! sibling free functions act as call edges.
//!
//! Field references are matched by name after subtracting the function's
//! parameters and locally-bound names (`:=`, `var` / `const` in the
//! body, `range` variables); binding forms beyond those are not tracked,
//! so a local that shadows a package name through an untracked form can
//! still register as a field reference. This is the same name-matching
//! heuristic the Rust / Python module units use.

use std::collections::{BTreeMap, HashSet};

use lens_domain::{CohesionUnit, CohesionUnitKind, MethodCohesion, qualify};
use tree_sitter::Node;

use crate::attrs::name_looks_like_test_function;
use crate::parser::{GoParseError, function_name_text, method_receiver_type, parse_tree};

/// Display name for the file-level cohesion unit (top-level free
/// functions). Matches the Rust / Python adapters' `<module>` label.
const MODULE_UNIT_NAME: &str = "<module>";

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

    if let Some(unit) = build_package_unit(tree.root_node(), bytes) {
        out.push(unit);
    }

    Ok(out)
}

/// Build the file-level (`<module>`) cohesion unit from top-level free
/// functions, or `None` when the file has no production free function
/// (method-only or test-only files don't get one). Package `var` /
/// `const` names are shared fields; calls between sibling free functions
/// are call edges.
fn build_package_unit(root: Node<'_>, source: &[u8]) -> Option<CohesionUnit> {
    let mut functions: Vec<Node<'_>> = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if child.kind() == "function_declaration"
            && let Some(name) = function_name_text(child, source)
            && !name_looks_like_test_function(name)
            && child.child_by_field_name("body").is_some()
        {
            functions.push(child);
        }
    }
    if functions.is_empty() {
        return None;
    }

    let package_fields = collect_package_fields(root, source);
    let sibling_names: HashSet<String> = functions
        .iter()
        .filter_map(|f| function_name_text(*f, source).map(str::to_owned))
        .collect();

    let methods: Vec<MethodCohesion> = functions
        .iter()
        .filter_map(|f| package_function_cohesion(*f, source, &package_fields, &sibling_names))
        .collect();
    if methods.is_empty() {
        return None;
    }

    let start_line = functions
        .iter()
        .map(|f| f.start_position().row + 1)
        .min()
        .unwrap_or(1);
    let end_line = functions
        .iter()
        .map(|f| f.end_position().row + 1)
        .max()
        .unwrap_or(start_line);

    Some(CohesionUnit::build(
        CohesionUnitKind::Module,
        MODULE_UNIT_NAME.to_owned(),
        start_line,
        end_line,
        methods,
    ))
}

fn package_function_cohesion(
    func: Node<'_>,
    source: &[u8],
    package_fields: &HashSet<String>,
    siblings: &HashSet<String>,
) -> Option<MethodCohesion> {
    let name = function_name_text(func, source)?.to_owned();
    let body = func.child_by_field_name("body")?;
    let locals = collect_locals(func, source);

    let mut visitor = PackageRefVisitor {
        source,
        package_fields,
        siblings,
        locals: &locals,
        fields: Vec::new(),
        calls: Vec::new(),
    };
    visitor.visit(body);

    let mut fields = visitor.fields;
    fields.sort();
    fields.dedup();
    let mut calls = visitor.calls;
    calls.sort();
    calls.dedup();

    Some(MethodCohesion::new(
        name,
        func.start_position().row + 1,
        func.end_position().row + 1,
        fields,
        calls,
    ))
}

/// Names of every package-level `var` / `const` binding — the "fields"
/// of the file-level unit. Grouped declarations (`var ( ... )`) are
/// descended into so each spec's names are collected.
fn collect_package_fields(root: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut fields = HashSet::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        if matches!(child.kind(), "var_declaration" | "const_declaration") {
            collect_spec_names(child, source, &mut fields);
        }
    }
    fields
}

/// Descend a `var_declaration` / `const_declaration` (possibly grouped)
/// and collect the `name:` identifiers of every `var_spec` / `const_spec`.
fn collect_spec_names(node: Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if matches!(child.kind(), "var_spec" | "const_spec") {
            collect_named_field_identifiers(child, source, out);
        } else {
            collect_spec_names(child, source, out);
        }
    }
}

/// Collect the identifiers sitting in a `name:` field of `node` (used for
/// spec names and parameter names, both of which can repeat the field).
fn collect_named_field_identifiers(node: Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return;
    }
    loop {
        if cursor.field_name() == Some("name")
            && cursor.node().kind() == "identifier"
            && let Ok(text) = cursor.node().utf8_text(source)
        {
            out.insert(text.to_owned());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
}

/// Function-local bindings that may shadow a package name: parameters,
/// `:=` short declarations, `var` / `const` in the body, and `range`
/// variables. Subtracting these keeps a local from being mistaken for a
/// package field reference.
fn collect_locals(func: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut locals = HashSet::new();
    if let Some(params) = func.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for decl in params.named_children(&mut cursor) {
            if decl.kind() == "parameter_declaration" {
                collect_named_field_identifiers(decl, source, &mut locals);
            }
        }
    }
    if let Some(body) = func.child_by_field_name("body") {
        collect_local_bindings(body, source, &mut locals);
    }
    locals
}

fn collect_local_bindings(node: Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    match node.kind() {
        "short_var_declaration" | "range_clause" => {
            if let Some(left) = node.child_by_field_name("left") {
                collect_identifier_list(left, source, out);
            }
        }
        "var_spec" | "const_spec" => collect_named_field_identifiers(node, source, out),
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_local_bindings(child, source, out);
    }
}

fn collect_identifier_list(node: Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == "identifier"
            && let Ok(text) = child.utf8_text(source)
        {
            out.insert(text.to_owned());
        }
    }
}

/// Walks a free function body recording (a) references to package fields
/// not shadowed by a local and (b) calls to sibling free functions.
struct PackageRefVisitor<'a> {
    source: &'a [u8],
    package_fields: &'a HashSet<String>,
    siblings: &'a HashSet<String>,
    locals: &'a HashSet<String>,
    fields: Vec<String>,
    calls: Vec<String>,
}

impl PackageRefVisitor<'_> {
    fn visit(&mut self, node: Node<'_>) {
        match node.kind() {
            "call_expression" => self.record_call(node),
            "identifier" => self.record_identifier(node),
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.visit(child);
        }
    }

    fn record_call(&mut self, node: Node<'_>) {
        if let Some(func) = node.child_by_field_name("function")
            && func.kind() == "identifier"
            && let Ok(name) = func.utf8_text(self.source)
            && self.siblings.contains(name)
        {
            self.calls.push(name.to_owned());
        }
    }

    fn record_identifier(&mut self, node: Node<'_>) {
        if let Ok(name) = node.utf8_text(self.source)
            && self.package_fields.contains(name)
            && !self.locals.contains(name)
        {
            self.fields.push(name.to_owned());
        }
    }
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

    fn module_unit(units: &[CohesionUnit]) -> &CohesionUnit {
        units
            .iter()
            .find(|u| matches!(u.kind, CohesionUnitKind::Module))
            .expect("module unit missing")
    }

    #[test]
    fn emits_package_unit_for_free_functions() {
        // Two free functions calling a third, plus a package var read by
        // two of them: the file-level unit must capture both the call
        // edges and the shared-field edge.
        let src = r#"
package p

var counter int

func inc() int {
    counter++
    return helper(counter)
}

func helper(x int) int {
    return x + counter
}

func standalone() int {
    return 1
}
"#;
        let units = extract_cohesion_units(src).unwrap();
        let unit = module_unit(&units);
        assert_eq!(unit.type_name, "<module>");
        assert_eq!(unit.methods.len(), 3);
        // The unit spans the first free function (line 6) to the last
        // (line 17), pinning the 1-based line arithmetic.
        assert_eq!(unit.start_line, 6);
        assert_eq!(unit.end_line, 17);

        let inc = unit.methods.iter().find(|m| m.name == "inc").unwrap();
        assert!(inc.fields.contains(&"counter".to_owned()));
        assert!(inc.calls.contains(&"helper".to_owned()));
        assert_eq!(inc.start_line, 6);
        assert_eq!(inc.end_line, 9);

        let helper = unit.methods.iter().find(|m| m.name == "helper").unwrap();
        assert!(helper.fields.contains(&"counter".to_owned()));

        // `inc`/`helper` are connected (call + shared field); `standalone`
        // touches neither, so it is its own component.
        assert_eq!(unit.components.len(), 2);
    }

    #[test]
    fn package_unit_excludes_body_var_shadow_from_fields() {
        // A `var counter` declared inside the body shadows the package
        // `counter`; reads of that local must not register as a field
        // reference on the package unit.
        let src = r#"
package p

var counter int

func reads() int {
    return counter
}

func shadows() int {
    var counter = 3
    return counter
}
"#;
        let units = extract_cohesion_units(src).unwrap();
        let unit = module_unit(&units);
        let reads = unit.methods.iter().find(|m| m.name == "reads").unwrap();
        let shadows = unit.methods.iter().find(|m| m.name == "shadows").unwrap();
        assert!(reads.fields.contains(&"counter".to_owned()));
        assert!(
            shadows.fields.is_empty(),
            "body `var` shadow leaked: {:?}",
            shadows.fields,
        );
    }

    #[test]
    fn package_unit_excludes_shadowing_locals_from_fields() {
        // A parameter and a `:=` local both named like package state must
        // not register as field references — only genuine package reads
        // should. Here nothing reads the real package `counter`, so the
        // two functions share no field.
        let src = r#"
package p

var counter int

func a(counter int) int {
    return counter + 1
}

func b() int {
    counter := 5
    return counter
}
"#;
        let units = extract_cohesion_units(src).unwrap();
        let unit = module_unit(&units);
        let a = unit.methods.iter().find(|m| m.name == "a").unwrap();
        let b = unit.methods.iter().find(|m| m.name == "b").unwrap();
        assert!(a.fields.is_empty(), "param shadow leaked: {:?}", a.fields);
        assert!(
            b.fields.is_empty(),
            "short-var shadow leaked: {:?}",
            b.fields
        );
    }

    #[test]
    fn package_unit_collects_all_names_from_grouped_var_spec() {
        // A single `var x, y int` spec declares two fields; both must be
        // recognised so functions reading either register the reference.
        let src = r#"
package p

var x, y int

func usesX() int { return x }

func usesY() int { return y }
"#;
        let units = extract_cohesion_units(src).unwrap();
        let unit = module_unit(&units);
        let uses_x = unit.methods.iter().find(|m| m.name == "usesX").unwrap();
        let uses_y = unit.methods.iter().find(|m| m.name == "usesY").unwrap();
        assert!(uses_x.fields.contains(&"x".to_owned()));
        assert!(
            uses_y.fields.contains(&"y".to_owned()),
            "second name in `var x, y int` was dropped: {:?}",
            uses_y.fields,
        );
    }

    #[test]
    fn package_unit_skips_test_functions() {
        let src = r#"
package p

import "testing"

func real() int { return 1 }

func TestReal(t *testing.T) { _ = real() }
"#;
        let units = extract_cohesion_units(src).unwrap();
        let unit = module_unit(&units);
        let names: Vec<&str> = unit.methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["real"]);
    }

    #[test]
    fn no_package_unit_for_method_only_file() {
        let src = "package p\ntype S struct{}\nfunc (s *S) A() int { return s.x }\n";
        let units = extract_cohesion_units(src).unwrap();
        assert!(
            !units
                .iter()
                .any(|u| matches!(u.kind, CohesionUnitKind::Module)),
            "unexpected module unit for a method-only file",
        );
    }
}
