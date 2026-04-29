//! oxc-based cohesion extraction for TypeScript / JavaScript classes
//! and modules.
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
//! In addition to class units we emit a **module-level** unit per file
//! and per `namespace` / `module` block: top-level `function`
//! declarations and `const` / `let` / `var` bindings to arrow / function
//! expressions act as the methods, top-level `let` / `const` / `var`
//! bindings to non-function values act as the fields, and free
//! identifier references / sibling references form edges. Locally-bound
//! names inside a function (parameters, body-level `var`/`let`/`const`,
//! nested function / class declarations, destructuring targets, catch
//! params) shadow module-level names, so a `const counter = 0` inside
//! a function is not counted as a reference to the module-level one.
//!
//! Sibling references are matched verbatim against the names of *sibling*
//! methods on the same class (or sibling functions in the same module),
//! and a reference counts whether or not it sits in callee position.
//! That is, `helper()`, `xs.map(helper)`, `<Helper />` and
//! `{ Helper, other }` all add the same edge from the enclosing function
//! to `helper` / `Helper`. References to anything else are dropped — the
//! cohesion graph only ever sees in-unit edges, mirroring `lens-rust`.
//! Capturing non-call references matters for idiomatic TS / React
//! modules where wrappers reach for sibling components through JSX
//! children or callback props rather than direct calls (see issue #66).

use std::collections::HashSet;

use lens_domain::{CohesionUnit, CohesionUnitKind, MethodCohesion};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::Visit;
use oxc_parser::Parser;
use oxc_span::GetSpan;
use oxc_syntax::scope::ScopeFlags;

use crate::line_index::LineIndex;
use crate::parser::{Dialect, TsParseError};

/// Failures produced while extracting cohesion units.
#[derive(Debug, thiserror::Error)]
pub enum CohesionError {
    #[error(transparent)]
    Parse(#[from] TsParseError),
}

/// Placeholder name used for the program-level (file-root) module unit.
/// Namespace bodies use the namespace name instead, so this constant
/// only ever stands in for the file's outermost scope.
const MODULE_UNIT_NAME: &str = "<module>";

/// Extract one [`CohesionUnit`] per class in `source` (with at least one
/// instance method) plus one module-level unit per scope (file root and
/// each `namespace` / `module` block) that has at least one top-level
/// function.
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
    collect_scope(&ret.program.body, MODULE_UNIT_NAME, &line_index, &mut out);
    Ok(out)
}

/// Process one lexical scope: emit class units (recursively), recurse
/// into nested namespaces as their own scopes, and finally emit a module
/// unit that summarises the top-level functions / fields *of this scope*.
fn collect_scope(
    stmts: &[Statement],
    scope_name: &str,
    line_index: &LineIndex,
    out: &mut Vec<CohesionUnit>,
) {
    for stmt in stmts {
        collect_stmt(stmt, line_index, out);
    }
    if let Some(unit) = build_module_unit(stmts, scope_name, line_index) {
        out.push(unit);
    }
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
            collect_module_declaration(m, line_index, out);
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
            collect_module_declaration(m, line_index, out);
        }
        _ => {}
    }
}

/// Recurse into a `namespace Foo { ... }` declaration. Each nested
/// namespace becomes its own scope, named after the namespace
/// identifier — so `namespace A { namespace B { ... } }` produces
/// module units named `A` and `B`. Falls back to the file-root
/// placeholder when the body uses an unnamed shape.
fn collect_module_declaration(
    decl: &TSModuleDeclaration,
    line_index: &LineIndex,
    out: &mut Vec<CohesionUnit>,
) {
    let Some(body) = &decl.body else { return };
    let name = module_decl_name(&decl.id);
    collect_module_body(body, &name, line_index, out);
}

fn collect_module_body(
    body: &TSModuleDeclarationBody,
    scope_name: &str,
    line_index: &LineIndex,
    out: &mut Vec<CohesionUnit>,
) {
    match body {
        TSModuleDeclarationBody::TSModuleBlock(block) => {
            collect_scope(&block.body, scope_name, line_index, out);
        }
        TSModuleDeclarationBody::TSModuleDeclaration(nested) => {
            collect_module_declaration(nested, line_index, out);
        }
    }
}

fn module_decl_name(id: &TSModuleDeclarationName) -> String {
    match id {
        TSModuleDeclarationName::Identifier(i) => i.name.to_string(),
        TSModuleDeclarationName::StringLiteral(s) => s.value.to_string(),
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

// ---------- Module-level extraction ----------

/// One top-level function discovered in a scope (file root or
/// namespace body). Both `function f(...)` declarations and
/// `const f = () => ...` style bindings are normalised into this shape
/// so the cohesion walker only has one thing to think about.
struct ModuleFunction<'a> {
    name: String,
    start_line: usize,
    end_line: usize,
    params: &'a FormalParameters<'a>,
    body: &'a FunctionBody<'a>,
}

/// Build the module unit for a single scope, if any. Returns `None`
/// when the scope has no top-level function — pure-class files,
/// pure-type / pure-import files and empty namespaces are skipped.
fn build_module_unit(
    stmts: &[Statement],
    scope_name: &str,
    line_index: &LineIndex,
) -> Option<CohesionUnit> {
    let functions = collect_module_functions(stmts, line_index);
    if functions.is_empty() {
        return None;
    }
    let module_fields = collect_module_fields(stmts);
    let sibling_names: HashSet<String> = functions.iter().map(|f| f.name.clone()).collect();

    let cohesions: Vec<MethodCohesion> = functions
        .iter()
        .map(|f| module_function_cohesion(f, &module_fields, &sibling_names))
        .collect();

    let (start_line, end_line) = scope_line_range(stmts, line_index);
    Some(CohesionUnit::build(
        CohesionUnitKind::Module,
        scope_name,
        start_line,
        end_line,
        cohesions,
    ))
}

fn scope_line_range(stmts: &[Statement], line_index: &LineIndex) -> (usize, usize) {
    let Some(first) = stmts.first() else {
        return (1, 1);
    };
    let last = stmts.last().unwrap_or(first);
    (
        line_index.line(first.span().start),
        line_index.line(last.span().end),
    )
}

/// Walk one scope's statements and return every top-level function:
/// `function f(...)` declarations, `const f = () => ...` bindings, and
/// the same shapes hidden behind an `export` modifier.
fn collect_module_functions<'a>(
    stmts: &'a [Statement<'a>],
    line_index: &LineIndex,
) -> Vec<ModuleFunction<'a>> {
    let mut out = Vec::new();
    for stmt in stmts {
        push_module_functions_from_stmt(stmt, line_index, &mut out);
    }
    out
}

fn push_module_functions_from_stmt<'a>(
    stmt: &'a Statement<'a>,
    line_index: &LineIndex,
    out: &mut Vec<ModuleFunction<'a>>,
) {
    match stmt {
        Statement::FunctionDeclaration(f) => push_function_decl(f, line_index, out),
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                push_var_function(d, line_index, out);
            }
        }
        Statement::ExportNamedDeclaration(e) => {
            if let Some(decl) = &e.declaration {
                push_module_functions_from_decl(decl, line_index, out);
            }
        }
        Statement::ExportDefaultDeclaration(e) => {
            if let ExportDefaultDeclarationKind::FunctionDeclaration(f) = &e.declaration {
                push_function_decl(f, line_index, out);
            }
        }
        _ => {}
    }
}

fn push_module_functions_from_decl<'a>(
    decl: &'a Declaration<'a>,
    line_index: &LineIndex,
    out: &mut Vec<ModuleFunction<'a>>,
) {
    match decl {
        Declaration::FunctionDeclaration(f) => push_function_decl(f, line_index, out),
        Declaration::VariableDeclaration(v) => {
            for d in &v.declarations {
                push_var_function(d, line_index, out);
            }
        }
        _ => {}
    }
}

fn push_function_decl<'a>(
    func: &'a Function<'a>,
    line_index: &LineIndex,
    out: &mut Vec<ModuleFunction<'a>>,
) {
    let Some(body) = &func.body else { return };
    let Some(id) = func.id.as_ref() else { return };
    out.push(ModuleFunction {
        name: id.name.to_string(),
        start_line: line_index.line(func.span.start),
        end_line: line_index.line(body.span.end),
        params: &func.params,
        body,
    });
}

fn push_var_function<'a>(
    decl: &'a VariableDeclarator<'a>,
    line_index: &LineIndex,
    out: &mut Vec<ModuleFunction<'a>>,
) {
    let Some(init) = &decl.init else { return };
    let Some(id) = decl.id.get_binding_identifier() else {
        return;
    };
    let name = id.name.to_string();
    match init {
        Expression::ArrowFunctionExpression(arrow) => {
            out.push(ModuleFunction {
                name,
                start_line: line_index.line(decl.span.start),
                end_line: line_index.line(arrow.body.span.end),
                params: &arrow.params,
                body: &arrow.body,
            });
        }
        Expression::FunctionExpression(f) => {
            if let Some(body) = &f.body {
                out.push(ModuleFunction {
                    name,
                    start_line: line_index.line(decl.span.start),
                    end_line: line_index.line(body.span.end),
                    params: &f.params,
                    body,
                });
            }
        }
        _ => {}
    }
}

/// Names of every top-level binding that participates in cohesion as a
/// "field": `let` / `const` / `var` declarations whose initialiser is
/// *not* a function. Imports, class declarations and function
/// declarations are excluded — they are not shared mutable / read state,
/// they're sibling units of their own.
fn collect_module_fields(stmts: &[Statement]) -> HashSet<String> {
    let mut fields = HashSet::new();
    for stmt in stmts {
        push_module_fields_from_stmt(stmt, &mut fields);
    }
    fields
}

fn push_module_fields_from_stmt(stmt: &Statement, out: &mut HashSet<String>) {
    match stmt {
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                push_var_field(d, out);
            }
        }
        Statement::ExportNamedDeclaration(e) => {
            if let Some(decl) = &e.declaration {
                push_module_fields_from_decl(decl, out);
            }
        }
        _ => {}
    }
}

fn push_module_fields_from_decl(decl: &Declaration, out: &mut HashSet<String>) {
    if let Declaration::VariableDeclaration(v) = decl {
        for d in &v.declarations {
            push_var_field(d, out);
        }
    }
}

fn push_var_field(decl: &VariableDeclarator, out: &mut HashSet<String>) {
    if matches!(
        decl.init,
        Some(Expression::ArrowFunctionExpression(_)) | Some(Expression::FunctionExpression(_))
    ) {
        return;
    }
    collect_binding_pattern_names(&decl.id, out);
}

/// Pull every plain identifier out of a destructuring pattern. Object,
/// array and rest patterns are unwound; computed / literal property
/// keys are skipped because they don't introduce new bindings.
fn collect_binding_pattern_names(pat: &BindingPattern, out: &mut HashSet<String>) {
    match pat {
        BindingPattern::BindingIdentifier(id) => {
            out.insert(id.name.to_string());
        }
        BindingPattern::ObjectPattern(o) => {
            for prop in &o.properties {
                collect_binding_pattern_names(&prop.value, out);
            }
            if let Some(rest) = &o.rest {
                collect_binding_pattern_names(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(a) => {
            for elem in a.elements.iter().flatten() {
                collect_binding_pattern_names(elem, out);
            }
            if let Some(rest) = &a.rest {
                collect_binding_pattern_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(a) => {
            collect_binding_pattern_names(&a.left, out);
        }
    }
}

fn module_function_cohesion(
    func: &ModuleFunction,
    module_fields: &HashSet<String>,
    siblings: &HashSet<String>,
) -> MethodCohesion {
    let locals = collect_local_names(func.params, func.body);
    let mut visitor = ModuleRefVisitor {
        module_fields,
        siblings,
        locals: &locals,
        fields: Vec::new(),
        calls: Vec::new(),
        in_callee: false,
    };
    visitor.visit_function_body(func.body);
    let mut fields = visitor.fields;
    fields.sort();
    fields.dedup();
    let mut calls = visitor.calls;
    calls.sort();
    calls.dedup();
    MethodCohesion::new(&func.name, func.start_line, func.end_line, fields, calls)
}

/// Function-local bindings: parameters plus any `var` / `let` / `const`,
/// nested function / class declaration, catch parameter, or
/// destructuring target inside the body. Used to decide whether a free
/// identifier reference resolves to the enclosing module scope or to a
/// local binding that would shadow it. We do *not* descend into nested
/// function bodies — those are independent scopes.
fn collect_local_names(params: &FormalParameters, body: &FunctionBody) -> HashSet<String> {
    let mut locals = HashSet::new();
    for item in &params.items {
        collect_binding_pattern_names(&item.pattern, &mut locals);
    }
    if let Some(rest) = &params.rest {
        collect_binding_pattern_names(&rest.rest.argument, &mut locals);
    }
    let mut walker = LocalNameWalker {
        locals: &mut locals,
    };
    for stmt in &body.statements {
        walker.walk_stmt(stmt);
    }
    locals
}

struct LocalNameWalker<'a> {
    locals: &'a mut HashSet<String>,
}

impl LocalNameWalker<'_> {
    fn walk_stmt(&mut self, stmt: &Statement) {
        match stmt {
            Statement::VariableDeclaration(v) => self.collect_var_declaration(v),
            Statement::FunctionDeclaration(f) => self.record_optional_binding(&f.id),
            Statement::ClassDeclaration(c) => self.record_optional_binding(&c.id),
            Statement::BlockStatement(b) => self.walk_statement_list(&b.body),
            Statement::IfStatement(i) => self.walk_if(i),
            Statement::WhileStatement(w) => self.walk_stmt(&w.body),
            Statement::DoWhileStatement(d) => self.walk_stmt(&d.body),
            Statement::ForStatement(f) => self.walk_for(f),
            Statement::ForInStatement(f) => self.walk_for_in(f),
            Statement::ForOfStatement(f) => self.walk_for_of(f),
            Statement::SwitchStatement(s) => self.walk_switch(s),
            Statement::TryStatement(t) => self.walk_try(t),
            Statement::WithStatement(w) => self.walk_stmt(&w.body),
            Statement::LabeledStatement(l) => self.walk_stmt(&l.body),
            _ => {}
        }
    }

    fn walk_if(&mut self, stmt: &IfStatement) {
        self.walk_stmt(&stmt.consequent);
        if let Some(alt) = &stmt.alternate {
            self.walk_stmt(alt);
        }
    }

    fn walk_for(&mut self, stmt: &ForStatement) {
        if let Some(ForStatementInit::VariableDeclaration(decl)) = &stmt.init {
            self.collect_var_declaration(decl);
        }
        self.walk_stmt(&stmt.body);
    }

    fn walk_for_in(&mut self, stmt: &ForInStatement) {
        self.collect_for_left(&stmt.left);
        self.walk_stmt(&stmt.body);
    }

    fn walk_for_of(&mut self, stmt: &ForOfStatement) {
        self.collect_for_left(&stmt.left);
        self.walk_stmt(&stmt.body);
    }

    fn walk_switch(&mut self, stmt: &SwitchStatement) {
        for case in &stmt.cases {
            self.walk_statement_list(&case.consequent);
        }
    }

    fn walk_try(&mut self, stmt: &TryStatement) {
        self.walk_statement_list(&stmt.block.body);
        if let Some(handler) = &stmt.handler {
            if let Some(param) = &handler.param {
                collect_binding_pattern_names(&param.pattern, self.locals);
            }
            self.walk_statement_list(&handler.body.body);
        }
        if let Some(finalizer) = &stmt.finalizer {
            self.walk_statement_list(&finalizer.body);
        }
    }

    fn collect_for_left(&mut self, left: &ForStatementLeft) {
        if let ForStatementLeft::VariableDeclaration(decl) = left {
            self.collect_var_declaration(decl);
        }
    }

    fn collect_var_declaration(&mut self, decl: &VariableDeclaration) {
        for declarator in &decl.declarations {
            collect_binding_pattern_names(&declarator.id, self.locals);
        }
    }

    fn walk_statement_list(&mut self, stmts: &[Statement]) {
        for stmt in stmts {
            self.walk_stmt(stmt);
        }
    }

    fn record_optional_binding(&mut self, id: &Option<BindingIdentifier>) {
        if let Some(id) = id {
            self.locals.insert(id.name.to_string());
        }
    }
}

/// Visitor that records (a) free identifier references that resolve to
/// a module-level field and (b) calls to sibling top-level functions,
/// in both cases skipping names shadowed by a function-local binding.
/// Mirrors [`ThisRefVisitor`] but tracks free names instead of `this.x`.
struct ModuleRefVisitor<'a> {
    module_fields: &'a HashSet<String>,
    siblings: &'a HashSet<String>,
    locals: &'a HashSet<String>,
    fields: Vec<String>,
    calls: Vec<String>,
    in_callee: bool,
}

impl<'a, 'ast> Visit<'ast> for ModuleRefVisitor<'a> {
    fn visit_call_expression(&mut self, it: &CallExpression<'ast>) {
        let prev = self.in_callee;
        self.in_callee = true;
        self.visit_expression(&it.callee);
        self.in_callee = prev;
        for arg in &it.arguments {
            self.visit_argument(arg);
        }
    }

    fn visit_identifier_reference(&mut self, it: &IdentifierReference<'ast>) {
        let name = it.name.as_str();
        if self.locals.contains(name) {
            return;
        }
        // Any reference to a sibling — whether in callee position or
        // not — counts as an edge: that covers JSX children
        // (`<Helper />`), callback props (`xs.map(helper)`) and
        // shorthand object exports (`{ Helper }`), all of which are
        // idiomatic ways for one TS / React function to reach for
        // another in the same module.
        if self.siblings.contains(name) {
            self.calls.push(name.to_owned());
        } else if !self.in_callee && self.module_fields.contains(name) {
            self.fields.push(name.to_owned());
        }
    }

    fn visit_static_member_expression(&mut self, it: &StaticMemberExpression<'ast>) {
        // The property side (`foo.bar`) is not an identifier reference;
        // we only want the object. Walk the object with `in_callee`
        // reset so `foo.bar()` doesn't accidentally promote `foo` to a
        // sibling call.
        let prev = self.in_callee;
        self.in_callee = false;
        self.visit_expression(&it.object);
        self.in_callee = prev;
    }

    fn visit_computed_member_expression(&mut self, it: &ComputedMemberExpression<'ast>) {
        let prev = self.in_callee;
        self.in_callee = false;
        self.visit_expression(&it.object);
        self.visit_expression(&it.expression);
        self.in_callee = prev;
    }

    fn visit_private_field_expression(&mut self, it: &PrivateFieldExpression<'ast>) {
        let prev = self.in_callee;
        self.in_callee = false;
        self.visit_expression(&it.object);
        self.in_callee = prev;
    }

    fn visit_function(&mut self, _it: &Function<'ast>, _flags: ScopeFlags) {
        // Don't descend into nested function bodies — they have their
        // own scope, and the cohesion graph is "what does *this*
        // function reach".
    }

    fn visit_arrow_function_expression(&mut self, _it: &ArrowFunctionExpression<'ast>) {
        // Same rationale as visit_function.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn unit(src: &str) -> CohesionUnit {
        let mut units = extract_cohesion_units(src, Dialect::Ts).unwrap();
        units.retain(|u| !matches!(u.kind, CohesionUnitKind::Module));
        assert_eq!(units.len(), 1, "expected exactly one class");
        units.remove(0)
    }

    fn module_unit(src: &str) -> CohesionUnit {
        module_unit_for(src, Dialect::Ts)
    }

    fn module_unit_for(src: &str, dialect: Dialect) -> CohesionUnit {
        extract_cohesion_units(src, dialect)
            .unwrap()
            .into_iter()
            .find(|u| matches!(u.kind, CohesionUnitKind::Module))
            .expect("expected a module-level cohesion unit")
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
        assert_eq!(u.components.len(), 1);
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
        assert_eq!(u.components.len(), 2);
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
        assert_eq!(u.components.len(), 1);
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
        assert_eq!(u.components.len(), 1);
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
        assert_eq!(u.components.len(), 2);
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
        assert_eq!(u.components.len(), 2);
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
        assert_eq!(u.components.len(), 1);
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
        assert_eq!(u.components.len(), 1);
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
        assert_eq!(u.components.len(), 1);
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

    #[rstest]
    #[case::only_static_methods(
        r#"
class Foo {
    static a(): void {}
    static b(): void {}
}
"#
    )]
    #[case::anonymous_class(
        r#"
const F = class { get(): number { return 1; } };
"#
    )]
    #[case::pure_type_or_import_file(
        r#"
import { foo } from "bar";
type T = number;
interface I { x: number; }
"#
    )]
    fn extracts_no_units(#[case] src: &str) {
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
        assert_eq!(u.components.len(), 2);
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

    // ---------- Module-level cohesion ----------

    #[test]
    fn module_unit_collapses_when_all_functions_share_a_field() {
        let src = r#"
let counter = 0;

function bump(): void { counter += 1; }
function get(): number { return counter; }
"#;
        let u = module_unit(src);
        assert!(matches!(u.kind, CohesionUnitKind::Module));
        assert_eq!(u.type_name, "<module>");
        assert_eq!(u.methods.len(), 2);
        assert_eq!(u.components.len(), 1);
    }

    #[test]
    fn module_split_responsibilities_show_multiple_components() {
        let src = r#"
let counter = 0;
let log: string[] = [];

function bump(): void { counter += 1; }
function current(): number { return counter; }
function record(s: string): void { log.push(s); }
function dump(): string[] { return log; }
"#;
        let u = module_unit(src);
        assert_eq!(u.components.len(), 2);
        assert_eq!(u.components, vec![vec![0, 1], vec![2, 3]]);
    }

    #[test]
    fn module_arrow_const_acts_as_a_method() {
        // `const f = () => ...` is a function, not a field. Two such
        // arrows that share a module-level `let` should collapse to one
        // component just like `function` declarations.
        let src = r#"
let counter = 0;

const bump = (): void => { counter += 1; };
const get = (): number => counter;
"#;
        let u = module_unit(src);
        assert_eq!(u.methods.len(), 2);
        assert_eq!(u.components.len(), 1);
    }

    #[test]
    fn module_sibling_call_forms_an_edge() {
        let src = r#"
function outer(): number { return helper(); }
function helper(): number { return 1; }
"#;
        let u = module_unit(src);
        assert_eq!(u.components.len(), 1);
        let outer = u.methods.iter().find(|m| m.name == "outer").unwrap();
        assert_eq!(outer.calls, vec!["helper"]);
    }

    #[test]
    fn module_local_declaration_shadows_module_field() {
        // The function's local `counter` rebinds the name, so the
        // reference is NOT to the module-level `counter`. Without
        // shadow-tracking the two functions would spuriously share
        // and collapse to one component.
        let src = r#"
let counter = 0;

function readsModule(): number { return counter; }
function shadowed(): number {
    const counter = 99;
    return counter;
}
"#;
        let u = module_unit(src);
        assert_eq!(u.components.len(), 2);
    }

    #[test]
    fn module_param_shadows_module_field() {
        // A parameter with the same name as a module field shadows it
        // for that function, so the reference is local.
        let src = r#"
let log: string[] = [];

function reader(): string[] { return log; }
function shadowed(log: string[]): string[] { return log; }
"#;
        let u = module_unit(src);
        assert_eq!(u.components.len(), 2);
    }

    #[test]
    fn module_external_calls_do_not_create_edges() {
        // `console.log` and `Math.max` are not siblings. Two functions
        // that both call them must not collapse on that basis.
        let src = r#"
function a(): void { console.log(1); }
function b(): void { console.log(2); }
"#;
        let u = module_unit(src);
        assert_eq!(u.components.len(), 2);
    }

    #[test]
    fn module_unit_is_skipped_for_pure_class_files() {
        // No top-level function → no module unit, just the class.
        let src = r#"
class Counter {
    n: number = 0;
    inc(): void { this.n += 1; }
    get(): number { return this.n; }
}
"#;
        let units = extract_cohesion_units(src, Dialect::Ts).unwrap();
        assert_eq!(units.len(), 1);
        assert!(matches!(units[0].kind, CohesionUnitKind::Inherent));
    }

    #[test]
    fn namespace_gets_its_own_module_unit() {
        // A `namespace Foo { ... }` body is its own scope; sibling
        // functions inside it must collapse via the namespace's
        // own module unit, named after the namespace.
        let src = r#"
namespace inner {
    let counter = 0;
    export function bump(): void { counter += 1; }
    export function get(): number { return counter; }
}
"#;
        let units = extract_cohesion_units(src, Dialect::Ts).unwrap();
        let module_units: Vec<&CohesionUnit> = units
            .iter()
            .filter(|u| matches!(u.kind, CohesionUnitKind::Module))
            .collect();
        assert_eq!(module_units.len(), 1);
        assert_eq!(module_units[0].type_name, "inner");
        assert_eq!(module_units[0].components.len(), 1);
    }

    #[test]
    fn module_exported_function_participates() {
        // `export function f(...)` should be picked up as a sibling,
        // same as a plain `function f(...)`.
        let src = r#"
let counter = 0;

export function bump(): void { counter += 1; }
export function get(): number { return counter; }
"#;
        let u = module_unit(src);
        assert_eq!(u.methods.len(), 2);
        assert_eq!(u.components.len(), 1);
    }

    #[test]
    fn module_destructured_local_shadows_module_field() {
        let src = r#"
let tag = 0;

function reader(): number { return tag; }
function destructured(o: { tag: number }): number {
    const { tag } = o;
    return tag;
}
"#;
        let u = module_unit(src);
        assert_eq!(u.components.len(), 2);
    }

    /// Regression for issue #66: a wrapper component that renders a
    /// sibling helper as a JSX child must form an edge to that helper.
    /// Before the fix the analyzer only counted call-position
    /// references, so `<Icon />` was invisible and `Wrapper`/`Icon`
    /// looked like two disconnected components.
    #[test]
    fn module_jsx_child_reference_forms_an_edge() {
        let src = r#"
function Wrapper(): JSX.Element { return <div><Icon /></div>; }
function Icon(): JSX.Element { return <svg />; }
"#;
        let u = module_unit_for(src, Dialect::Tsx);
        assert_eq!(u.methods.len(), 2);
        assert_eq!(u.components.len(), 1);
        let wrapper = u.methods.iter().find(|m| m.name == "Wrapper").unwrap();
        assert_eq!(wrapper.calls, vec!["Icon"]);
    }

    /// JSX member-expression tags (`<Foo.Bar />`) walk the head as an
    /// `IdentifierReference`, so a wrapper that renders `<Sibling.Item />`
    /// should connect to `Sibling` even though there is no plain
    /// `<Sibling />` anywhere.
    #[test]
    fn module_jsx_member_expression_head_forms_an_edge() {
        let src = r#"
function Wrapper(): JSX.Element { return <Helper.Item />; }
function Helper(): JSX.Element { return <span />; }
"#;
        let u = module_unit_for(src, Dialect::Tsx);
        assert_eq!(u.components.len(), 1);
        let wrapper = u.methods.iter().find(|m| m.name == "Wrapper").unwrap();
        assert_eq!(wrapper.calls, vec!["Helper"]);
    }

    /// Passing a sibling as a callback (`xs.map(helper)`) is an
    /// identifier reference in argument position, not a callee. Issue #66
    /// asks for that to count as an edge alongside direct calls.
    #[test]
    fn module_callback_reference_forms_an_edge() {
        let src = r#"
function outer(xs: number[]): number[] { return xs.map(helper); }
function helper(x: number): number { return x + 1; }
"#;
        let u = module_unit(src);
        assert_eq!(u.components.len(), 1);
        let outer = u.methods.iter().find(|m| m.name == "outer").unwrap();
        assert_eq!(outer.calls, vec!["helper"]);
    }

    /// Object-literal shorthand (`{ helper }`) emits an
    /// `IdentifierReference` for the value. A re-exporter that bundles
    /// siblings into a single record should still link to them.
    #[test]
    fn module_object_shorthand_reference_forms_an_edge() {
        let src = r#"
function bundle() { return { helper }; }
function helper(x: number): number { return x + 1; }
"#;
        let u = module_unit(src);
        assert_eq!(u.components.len(), 1);
        let bundle = u.methods.iter().find(|m| m.name == "bundle").unwrap();
        assert_eq!(bundle.calls, vec!["helper"]);
    }

    /// The shadow-tracking guard must still win over the new
    /// non-call-reference path: a function whose local rebinding
    /// happens to share a name with a sibling should not pretend to
    /// reference the sibling.
    #[test]
    fn module_local_shadow_blocks_sibling_reference_edge() {
        let src = r#"
function outer(): number {
    const helper = 1;
    return helper;
}
function helper(): number { return 0; }
function caller(): number { return helper(); }
"#;
        let u = module_unit(src);
        // `outer` only sees its local `helper`, so it must remain its
        // own component; `caller` and `helper` connect via the call.
        assert_eq!(u.components.len(), 2);
        let outer = u.methods.iter().find(|m| m.name == "outer").unwrap();
        assert!(outer.calls.is_empty());
    }

    /// JSX tags whose name is lowercase (`<div />`, `<span />`) lower to
    /// `JSXIdentifier`, not `IdentifierReference`, so they must not be
    /// confused with a sibling reference even if a sibling happens to
    /// share the name.
    #[test]
    fn module_lowercase_jsx_tag_is_not_a_sibling_reference() {
        let src = r#"
function Wrapper(): JSX.Element { return <div />; }
function div(): number { return 0; }
"#;
        let u = module_unit_for(src, Dialect::Tsx);
        // `Wrapper` and `div` should stay disconnected — `<div />`
        // here is the HTML element, not a reference to the sibling.
        assert_eq!(u.components.len(), 2);
        let wrapper = u.methods.iter().find(|m| m.name == "Wrapper").unwrap();
        assert!(wrapper.calls.is_empty());
    }
}
