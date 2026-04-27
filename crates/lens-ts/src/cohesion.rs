//! oxc-based cohesion extraction for TypeScript / JavaScript classes.
//!
//! For every `class` body we collect each instance method's referenced
//! `this.<field>` accesses (including private `this.#field`) and its
//! `this.<sibling>(...)` calls. Static methods and the constructor are
//! ignored:
//!
//! * static methods cannot reach `this` and have no field cohesion;
//! * a constructor's job is to initialise fields, so it would over-link
//!   every method through every field it assigns.
//!
//! Calls are matched verbatim against the names of *sibling* methods on
//! the same class. Calls to anything else are dropped — the cohesion
//! graph only ever sees in-unit edges, mirroring `lens-rust`.

use std::collections::HashSet;

use lens_domain::{CohesionUnit, CohesionUnitKind, MethodCohesion};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::Visit;
use oxc_parser::Parser;

use crate::line_index::LineIndex;
use crate::parser::{Dialect, TsParseError};

/// Failures produced while extracting cohesion units.
#[derive(Debug, thiserror::Error)]
pub enum CohesionError {
    #[error(transparent)]
    Parse(#[from] TsParseError),
}

/// Extract one [`CohesionUnit`] per class in `source` that has at least
/// one instance method.
pub fn extract_cohesion_units(
    source: &str,
    dialect: Dialect,
) -> Result<Vec<CohesionUnit>, CohesionError> {
    let alloc = Allocator::default();
    let ret = Parser::new(&alloc, source, dialect.source_type()).parse();
    if !ret.errors.is_empty() {
        return Err(CohesionError::Parse(TsParseError::from_diagnostics(
            ret.errors.iter().map(|e| e.message.as_ref().to_owned()),
        )));
    }
    let line_index = LineIndex::new(source);
    let mut out = Vec::new();
    for stmt in &ret.program.body {
        collect_stmt(stmt, &line_index, &mut out);
    }
    Ok(out)
}

fn collect_stmt(stmt: &Statement, line_index: &LineIndex, out: &mut Vec<CohesionUnit>) {
    match stmt {
        Statement::ClassDeclaration(c) => {
            if let Some(unit) = unit_from_class(c, line_index) {
                out.push(unit);
            }
        }
        Statement::ExportNamedDeclaration(e) => {
            if let Some(decl) = &e.declaration {
                collect_decl(decl, line_index, out);
            }
        }
        Statement::ExportDefaultDeclaration(e) => {
            if let ExportDefaultDeclarationKind::ClassDeclaration(c) = &e.declaration
                && let Some(unit) = unit_from_class(c, line_index)
            {
                out.push(unit);
            }
        }
        Statement::TSModuleDeclaration(m) => {
            if let Some(body) = &m.body {
                collect_module_body(body, line_index, out);
            }
        }
        _ => {}
    }
}

fn collect_decl(decl: &Declaration, line_index: &LineIndex, out: &mut Vec<CohesionUnit>) {
    match decl {
        Declaration::ClassDeclaration(c) => {
            if let Some(unit) = unit_from_class(c, line_index) {
                out.push(unit);
            }
        }
        Declaration::TSModuleDeclaration(m) => {
            if let Some(body) = &m.body {
                collect_module_body(body, line_index, out);
            }
        }
        _ => {}
    }
}

fn collect_module_body(
    body: &TSModuleDeclarationBody,
    line_index: &LineIndex,
    out: &mut Vec<CohesionUnit>,
) {
    match body {
        TSModuleDeclarationBody::TSModuleBlock(block) => {
            for stmt in &block.body {
                collect_stmt(stmt, line_index, out);
            }
        }
        TSModuleDeclarationBody::TSModuleDeclaration(nested) => {
            if let Some(body) = &nested.body {
                collect_module_body(body, line_index, out);
            }
        }
    }
}

fn unit_from_class(class: &Class, line_index: &LineIndex) -> Option<CohesionUnit> {
    let class_name = class.id.as_ref().map(|i| i.name.as_str())?;

    let methods: Vec<&MethodDefinition> = class
        .body
        .body
        .iter()
        .filter_map(|elem| match elem {
            ClassElement::MethodDefinition(m) if is_instance_method(m) => Some(m.as_ref()),
            _ => None,
        })
        .collect();
    if methods.is_empty() {
        return None;
    }

    let sibling_names: HashSet<String> = methods.iter().filter_map(|m| method_name(m)).collect();

    let cohesions: Vec<MethodCohesion> = methods
        .iter()
        .filter_map(|m| method_cohesion(m, &sibling_names, line_index))
        .collect();
    if cohesions.is_empty() {
        return None;
    }

    Some(CohesionUnit::build(
        CohesionUnitKind::Inherent,
        class_name,
        line_index.line(class.span.start),
        line_index.line(class.span.end),
        cohesions,
    ))
}

fn is_instance_method(method: &MethodDefinition) -> bool {
    if method.r#static {
        return false;
    }
    // Constructors initialise every field they touch; including them
    // would collapse every method into one component regardless of real
    // cohesion. Mirrors the way lens-rust skips associated functions.
    !matches!(method.kind, MethodDefinitionKind::Constructor)
}

fn method_cohesion(
    method: &MethodDefinition,
    siblings: &HashSet<String>,
    line_index: &LineIndex,
) -> Option<MethodCohesion> {
    let body = method.value.body.as_ref()?;
    let name = method_name(method)?;
    let mut visitor = ThisRefVisitor::default();
    visitor.visit_function_body(body);
    let mut fields = visitor.fields;
    fields.sort();
    fields.dedup();
    let mut calls: Vec<String> = visitor
        .calls
        .into_iter()
        .filter(|c| siblings.contains(c))
        .collect();
    calls.sort();
    calls.dedup();
    Some(MethodCohesion::new(
        name,
        line_index.line(method.span.start),
        line_index.line(method.span.end),
        fields,
        calls,
    ))
}

fn method_name(method: &MethodDefinition) -> Option<String> {
    crate::walk::method_key_name(&method.key)
}

#[derive(Default)]
struct ThisRefVisitor {
    fields: Vec<String>,
    calls: Vec<String>,
    /// We need to know when a member expression is the callee of a
    /// `CallExpression` so we count `this.bar()` as a call rather than
    /// as a field access. The flag is set just for the descent into
    /// `CallExpression::callee`.
    in_callee: bool,
}

impl<'a> Visit<'a> for ThisRefVisitor {
    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        let prev = self.in_callee;
        self.in_callee = true;
        self.visit_expression(&it.callee);
        self.in_callee = prev;
        for arg in &it.arguments {
            self.visit_argument(arg);
        }
    }

    fn visit_static_member_expression(&mut self, it: &StaticMemberExpression<'a>) {
        if matches!(&it.object, Expression::ThisExpression(_)) {
            let name = it.property.name.to_string();
            if self.in_callee {
                self.calls.push(name);
            } else {
                self.fields.push(name);
            }
            // The object is `this`; nothing to descend into. Skipping
            // the recursive walk avoids accidentally counting `this`
            // as anything else.
            return;
        }
        let prev = self.in_callee;
        self.in_callee = false;
        self.visit_expression(&it.object);
        self.in_callee = prev;
    }

    fn visit_private_field_expression(&mut self, it: &PrivateFieldExpression<'a>) {
        if matches!(&it.object, Expression::ThisExpression(_)) {
            let name = format!("#{}", it.field.name);
            if self.in_callee {
                self.calls.push(name);
            } else {
                self.fields.push(name);
            }
            return;
        }
        let prev = self.in_callee;
        self.in_callee = false;
        self.visit_expression(&it.object);
        self.in_callee = prev;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(src: &str) -> CohesionUnit {
        let mut units = extract_cohesion_units(src, Dialect::Ts).unwrap();
        assert_eq!(units.len(), 1, "expected exactly one class");
        units.remove(0)
    }

    #[test]
    fn cohesive_class_collapses_to_one_component() {
        let src = r#"
class Counter {
    n: number = 0;
    inc(): void { this.n += 1; }
    get(): number { return this.n; }
}
"#;
        let u = unit(src);
        assert_eq!(u.type_name, "Counter");
        assert_eq!(u.lcom4(), 1);
        assert_eq!(u.methods.len(), 2);
    }

    #[test]
    fn split_responsibilities_show_multiple_components() {
        let src = r#"
class Thing {
    counter: number = 0;
    log: string = "";
    bump(): void { this.counter += 1; }
    current(): number { return this.counter; }
    record(s: string): void { this.log += s; }
    dump(): string { return this.log; }
}
"#;
        let u = unit(src);
        assert_eq!(u.lcom4(), 2);
        assert_eq!(u.components, vec![vec![0, 1], vec![2, 3]]);
    }

    #[test]
    fn calls_via_this_method_form_an_edge() {
        let src = r#"
class Foo {
    outer(): void { this.helper(); }
    helper(): void {}
}
"#;
        let u = unit(src);
        assert_eq!(u.lcom4(), 1);
        let outer = u.methods.iter().find(|m| m.name == "outer").unwrap();
        assert_eq!(outer.calls, vec!["helper"]);
    }

    #[test]
    fn static_methods_are_excluded() {
        let src = r#"
class Foo {
    n: number = 0;
    static make(): Foo { return new Foo(); }
    get(): number { return this.n; }
    set(n: number): void { this.n = n; }
}
"#;
        let u = unit(src);
        assert_eq!(u.methods.len(), 2);
        assert_eq!(u.lcom4(), 1);
        for m in &u.methods {
            assert_ne!(m.name, "make", "static methods should be filtered");
        }
    }

    #[test]
    fn constructor_is_excluded() {
        // Constructors initialise every field they touch; including the
        // constructor would link every method through every field it
        // assigned, defeating the purpose of LCOM4.
        let src = r#"
class Foo {
    a: number;
    b: number;
    constructor() { this.a = 0; this.b = 0; }
    getA(): number { return this.a; }
    getB(): number { return this.b; }
}
"#;
        let u = unit(src);
        assert_eq!(u.methods.len(), 2);
        assert_eq!(u.lcom4(), 2);
    }

    #[test]
    fn external_calls_do_not_create_edges() {
        let src = r#"
class Foo {
    a(): void { other(); }
    b(): void { another(); }
}
function other(): void {}
function another(): void {}
"#;
        let u = unit(src);
        assert_eq!(u.lcom4(), 2);
    }

    #[test]
    fn private_field_access_participates_in_cohesion() {
        let src = r#"
class Foo {
    #n = 0;
    get(): number { return this.#n; }
    bump(): void { this.#n += 1; }
}
"#;
        let u = unit(src);
        assert_eq!(u.lcom4(), 1);
        for m in &u.methods {
            assert_eq!(m.fields, vec!["#n"]);
        }
    }

    #[test]
    fn private_method_call_forms_an_edge() {
        let src = r#"
class Foo {
    outer(): void { this.#helper(); }
    #helper(): void {}
}
"#;
        let u = unit(src);
        assert_eq!(u.lcom4(), 1);
        let outer = u.methods.iter().find(|m| m.name == "outer").unwrap();
        assert_eq!(outer.calls, vec!["#helper"]);
    }

    #[test]
    fn getters_and_setters_participate() {
        let src = r#"
class Foo {
    _n = 0;
    get n(): number { return this._n; }
    set n(v: number) { this._n = v; }
}
"#;
        let u = unit(src);
        assert_eq!(u.methods.len(), 2);
        assert_eq!(u.lcom4(), 1);
    }

    #[test]
    fn classes_in_namespaces_are_picked_up() {
        let src = r#"
namespace inner {
    export class Foo {
        n = 0;
        get(): number { return this.n; }
        set(v: number) { this.n = v; }
    }
}
"#;
        let units = extract_cohesion_units(src, Dialect::Ts).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].type_name, "Foo");
    }

    #[test]
    fn exported_class_is_picked_up() {
        let src = r#"
export class Foo {
    n = 0;
    get(): number { return this.n; }
    set(v: number) { this.n = v; }
}
"#;
        let units = extract_cohesion_units(src, Dialect::Ts).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].type_name, "Foo");
    }

    #[test]
    fn export_default_class_is_picked_up() {
        let src = r#"
export default class Foo {
    n = 0;
    get(): number { return this.n; }
    set(v: number) { this.n = v; }
}
"#;
        let units = extract_cohesion_units(src, Dialect::Ts).unwrap();
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].type_name, "Foo");
    }

    #[test]
    fn classes_with_only_static_methods_are_skipped() {
        let src = r#"
class Foo {
    static a(): void {}
    static b(): void {}
}
"#;
        let units = extract_cohesion_units(src, Dialect::Ts).unwrap();
        assert!(units.is_empty());
    }

    #[test]
    fn field_access_on_other_object_is_not_self_field() {
        let src = r#"
class Foo {
    tag = 0;
    external(o: { tag: number }): number { return o.tag; }
    local(): number { return this.tag; }
}
"#;
        let u = unit(src);
        let external = u.methods.iter().find(|m| m.name == "external").unwrap();
        let local = u.methods.iter().find(|m| m.name == "local").unwrap();
        assert!(external.fields.is_empty());
        assert_eq!(local.fields, vec!["tag"]);
        assert_eq!(u.lcom4(), 2);
    }

    #[test]
    fn anonymous_class_is_skipped() {
        // We cannot name the unit, so we drop it rather than emit a
        // generic placeholder.
        let src = r#"
const F = class { get(): number { return 1; } };
"#;
        let units = extract_cohesion_units(src, Dialect::Ts).unwrap();
        assert!(units.is_empty());
    }

    #[test]
    fn invalid_source_surfaces_parse_error() {
        let err = extract_cohesion_units("class ??? {", Dialect::Ts).unwrap_err();
        assert!(matches!(err, CohesionError::Parse(_)));
    }

    #[test]
    fn cohesion_error_source_is_present() {
        use std::error::Error as _;
        let err = extract_cohesion_units("class ??? {", Dialect::Ts).unwrap_err();
        assert!(err.source().is_some());
    }

    #[test]
    fn line_range_covers_class_through_closing_brace() {
        let src = "class Foo {\n    n = 0;\n    get(): number { return this.n; }\n}\n";
        let u = unit(src);
        assert_eq!(u.start_line, 1);
        assert_eq!(u.end_line, 4);
    }
}
