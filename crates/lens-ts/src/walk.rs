//! Single-pass walker over a TypeScript / JavaScript program that
//! visits every "function-shaped" item — top-level `function`
//! declarations, exported variants, class methods, arrow / function
//! expressions bound to a `const`/`let`/`var`, and items declared inside
//! `namespace` / `module` blocks.
//!
//! Functions nested inside another function's body — callbacks, event
//! handlers, closures such as `el.onclick = () => …` — are emitted as
//! well, each as a synthetic `<parent>::closure#N` unit (1-based, source
//! order; see [`scan_nested_functions`]). The walk is depth-first: a
//! function is emitted before any function found inside its body.
//!
//! A statement-level call carries functions too, and in a TS test suite
//! it carries all of them: `describe("…", () => …)` at module scope is an
//! expression statement, not a declaration. Those callbacks are emitted
//! as module-scope units named after the call
//! (`describe#1("groupFor")::it#2("uses the crate name")`, see
//! [`crate::harness`]) so a vitest / jest file contributes its bodies to
//! every analyzer instead of reading as an empty module. The same descent
//! picks up IIFEs and `app.listen(() => …)`.
//!
//! The shape of the AST traversal used to be duplicated between
//! [`crate::parser`] (which collects [`lens_domain::FunctionDef`]) and
//! [`crate::complexity`] (which collects [`lens_domain::FunctionComplexity`]).
//! Both files matched the same `Statement` / `Declaration` /
//! `TSModuleDeclarationBody` / `Class` shapes, only differing in what
//! they pushed at the leaves. The walker normalises every leaf into
//! [`FunctionItem`] so consumers only implement the conversion to their
//! own target type.
//!
//! The trait is crate-private; consumers live in `lens-ts` itself. New
//! analysers should add a `FunctionVisitor` impl rather than rewriting
//! the AST shape.

use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_syntax::scope::ScopeFlags;

use lens_domain::LineIndex;

use crate::harness::{CLOSURE_CALLEE, call_title, harness_callee, synthetic_segment};

/// One extracted function unit. The walker has already resolved the
/// 1-based inclusive line range and the qualified name (e.g.
/// `ClassName::method`), so visitors only convert this into their own
/// target type.
pub(crate) struct FunctionItem<'a> {
    pub name: String,
    pub start_line: usize,
    pub end_line: usize,
    pub body: &'a FunctionBody<'a>,
    pub params: &'a FormalParameters<'a>,
    /// True for class constructors; analyzers like `wrapper` use this to
    /// skip mandatory boilerplate (`super(...)`) that structurally looks
    /// like a thin forwarding call.
    pub is_constructor: bool,
    /// Byte offset of the outermost token a leading JSDoc comment would
    /// attach to — the `export` / `const` / decorator start for wrapped
    /// declarations, the item itself otherwise. `None` for nested
    /// closures, which never carry their own doc block.
    pub doc_attach_start: Option<u32>,
}

/// Receiver for function-shaped items found by [`walk_program`].
pub(crate) trait FunctionVisitor {
    fn on_function(&mut self, item: FunctionItem<'_>);
}

/// Walk every top-level statement in `program` and emit one
/// [`FunctionItem`] per function-shaped item.
pub(crate) fn walk_program<V: FunctionVisitor>(
    program: &Program,
    line_index: &LineIndex,
    visitor: &mut V,
) {
    // Module-scope callbacks are numbered across the whole file, not per
    // statement, so two top-level `describe(…)` blocks cannot both mint
    // `describe#1`. The same counter threads through `namespace` blocks
    // for the same reason.
    let mut units = ModuleUnitCounter::default();
    for stmt in &program.body {
        walk_stmt(stmt, None, line_index, visitor, &mut units);
    }
}

/// Running count of module-scope units minted for one file — see
/// [`walk_program`].
#[derive(Default)]
struct ModuleUnitCounter(usize);

fn walk_stmt<V: FunctionVisitor>(
    stmt: &Statement,
    owner: Option<&str>,
    line_index: &LineIndex,
    visitor: &mut V,
    units: &mut ModuleUnitCounter,
) {
    match stmt {
        Statement::FunctionDeclaration(f) => {
            visit_function(f, owner, f.span.start, line_index, visitor);
        }
        Statement::ClassDeclaration(c) => walk_class(c, line_index, visitor),
        Statement::VariableDeclaration(v) => {
            for d in &v.declarations {
                visit_variable_declarator(d, v.span.start, line_index, visitor);
            }
        }
        Statement::ExportNamedDeclaration(e) => {
            if let Some(decl) = &e.declaration {
                walk_decl(decl, owner, e.span.start, line_index, visitor, units);
            }
        }
        Statement::ExportDefaultDeclaration(e) => match &e.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                visit_function(f, owner, e.span.start, line_index, visitor);
            }
            ExportDefaultDeclarationKind::ClassDeclaration(c) => walk_class(c, line_index, visitor),
            _ => {}
        },
        Statement::TSModuleDeclaration(m) => {
            if let Some(body) = &m.body {
                walk_module_body(body, line_index, visitor, units);
            }
        }
        Statement::ExpressionStatement(e) => {
            scan_expression_functions(&e.expression, line_index, visitor, units);
        }
        _ => {}
    }
}

fn walk_decl<V: FunctionVisitor>(
    decl: &Declaration,
    owner: Option<&str>,
    attach_start: u32,
    line_index: &LineIndex,
    visitor: &mut V,
    units: &mut ModuleUnitCounter,
) {
    match decl {
        Declaration::FunctionDeclaration(f) => {
            visit_function(f, owner, attach_start, line_index, visitor);
        }
        Declaration::ClassDeclaration(c) => walk_class(c, line_index, visitor),
        Declaration::VariableDeclaration(v) => {
            for d in &v.declarations {
                visit_variable_declarator(d, attach_start, line_index, visitor);
            }
        }
        Declaration::TSModuleDeclaration(m) => {
            if let Some(body) = &m.body {
                walk_module_body(body, line_index, visitor, units);
            }
        }
        _ => {}
    }
}

fn walk_module_body<V: FunctionVisitor>(
    body: &TSModuleDeclarationBody,
    line_index: &LineIndex,
    visitor: &mut V,
    units: &mut ModuleUnitCounter,
) {
    match body {
        TSModuleDeclarationBody::TSModuleBlock(block) => {
            for stmt in &block.body {
                walk_stmt(stmt, None, line_index, visitor, units);
            }
        }
        TSModuleDeclarationBody::TSModuleDeclaration(nested) => {
            if let Some(body) = &nested.body {
                walk_module_body(body, line_index, visitor, units);
            }
        }
    }
}

fn walk_class<V: FunctionVisitor>(class: &Class, line_index: &LineIndex, visitor: &mut V) {
    let class_name = class
        .id
        .as_ref()
        .map(|i| i.name.as_str())
        .unwrap_or("anonymous");
    for elem in &class.body.body {
        if let ClassElement::MethodDefinition(m) = elem
            && let Some(body) = &m.value.body
            && let Some(name) = method_key_name(&m.key)
        {
            let qualified = format!("{class_name}::{name}");
            visitor.on_function(FunctionItem {
                name: qualified.clone(),
                start_line: line_index.line(m.span.start),
                end_line: line_index.line(m.span.end),
                body,
                params: &m.value.params,
                is_constructor: matches!(m.kind, MethodDefinitionKind::Constructor),
                doc_attach_start: Some(m.span.start),
            });
            scan_nested_functions(&qualified, body, line_index, visitor);
        }
    }
}

fn visit_function<V: FunctionVisitor>(
    func: &Function,
    owner: Option<&str>,
    attach_start: u32,
    line_index: &LineIndex,
    visitor: &mut V,
) {
    let Some(body) = &func.body else { return };
    let raw_name = func
        .id
        .as_ref()
        .map(|i| i.name.as_str())
        .unwrap_or("anonymous");
    let name = match owner {
        Some(o) => format!("{o}::{raw_name}"),
        None => raw_name.to_owned(),
    };
    visitor.on_function(FunctionItem {
        name: name.clone(),
        start_line: line_index.line(func.span.start),
        end_line: line_index.line(body.span.end),
        body,
        params: &func.params,
        is_constructor: false,
        doc_attach_start: Some(attach_start),
    });
    scan_nested_functions(&name, body, line_index, visitor);
}

fn visit_variable_declarator<V: FunctionVisitor>(
    decl: &VariableDeclarator,
    attach_start: u32,
    line_index: &LineIndex,
    visitor: &mut V,
) {
    let Some(init) = &decl.init else { return };
    let Some(id) = decl.id.get_binding_identifier() else {
        return;
    };
    let name = id.name.to_string();
    match init {
        Expression::ArrowFunctionExpression(arrow) => {
            visitor.on_function(FunctionItem {
                name: name.clone(),
                start_line: line_index.line(decl.span.start),
                end_line: line_index.line(arrow.body.span.end),
                body: &arrow.body,
                params: &arrow.params,
                is_constructor: false,
                doc_attach_start: Some(attach_start),
            });
            scan_nested_functions(&name, &arrow.body, line_index, visitor);
        }
        Expression::FunctionExpression(f) => {
            if let Some(body) = &f.body {
                visitor.on_function(FunctionItem {
                    name: name.clone(),
                    start_line: line_index.line(decl.span.start),
                    end_line: line_index.line(body.span.end),
                    body,
                    params: &f.params,
                    is_constructor: false,
                    doc_attach_start: Some(attach_start),
                });
                scan_nested_functions(&name, body, line_index, visitor);
            }
        }
        _ => {}
    }
}

/// Emit every function nested inside `body` as its own [`FunctionItem`].
///
/// Arrow functions, function expressions and function declarations found
/// anywhere in `body` — call arguments (`addEventListener("click", …)`),
/// assignment right-hand sides (`el.onclick = …`), JSX attribute values
/// (`onClick={…}`), initialisers, object methods, and so on — each become
/// a unit named `<parent>::closure#<N>`. Indices are 1-based and reset per
/// parent, so a closure inside `setup`'s first closure is
/// `setup::closure#1::closure#1`.
fn scan_nested_functions<V: FunctionVisitor>(
    parent: &str,
    body: &FunctionBody,
    line_index: &LineIndex,
    visitor: &mut V,
) {
    let mut scanner = NestedScanner {
        visitor,
        parent,
        line_index,
        counter: 0,
    };
    scanner.visit_function_body(body);
}

/// Emit every function carried by a module-scope expression, using the
/// file-wide [`ModuleUnitCounter`] so names stay unique across statements.
///
/// The units have no enclosing function to be named after, so their names
/// are bare segments: `describe#1("groupFor")`, `closure#4`.
fn scan_expression_functions<V: FunctionVisitor>(
    expr: &Expression,
    line_index: &LineIndex,
    visitor: &mut V,
    units: &mut ModuleUnitCounter,
) {
    let mut scanner = NestedScanner {
        visitor,
        parent: "",
        line_index,
        counter: units.0,
    };
    scanner.visit_expression(expr);
    units.0 = scanner.counter;
}

/// [`Visit`] adapter that finds the functions directly nested in a body
/// or a module-scope expression.
///
/// Every node type apart from the two function shapes and harness calls
/// uses the default walk, so the scan reaches functions buried deep
/// inside expressions and JSX. The function arms emit the nested unit and
/// then re-enter [`scan_nested_functions`] for its body rather than
/// letting the default walk descend — that keeps `#N` indices scoped to
/// one parent.
struct NestedScanner<'s, V> {
    visitor: &'s mut V,
    parent: &'s str,
    line_index: &'s LineIndex,
    counter: usize,
}

impl<V: FunctionVisitor> NestedScanner<'_, V> {
    fn emit(
        &mut self,
        callee: &str,
        title: Option<&str>,
        params: &FormalParameters,
        body: &FunctionBody,
        start: u32,
    ) {
        self.counter += 1;
        let segment = synthetic_segment(callee, self.counter, title);
        let name = match self.parent {
            "" => segment,
            parent => format!("{parent}::{segment}"),
        };
        self.visitor.on_function(FunctionItem {
            name: name.clone(),
            start_line: self.line_index.line(start),
            end_line: self.line_index.line(body.span.end),
            body,
            params,
            is_constructor: false,
            doc_attach_start: None,
        });
        scan_nested_functions(&name, body, self.line_index, self.visitor);
    }
}

impl<'a, V: FunctionVisitor> Visit<'a> for NestedScanner<'_, V> {
    fn visit_function(&mut self, func: &Function<'a>, _flags: ScopeFlags) {
        if let Some(body) = &func.body {
            self.emit(CLOSURE_CALLEE, None, &func.params, body, func.span.start);
        }
    }

    fn visit_arrow_function_expression(&mut self, arrow: &ArrowFunctionExpression<'a>) {
        self.emit(
            CLOSURE_CALLEE,
            None,
            &arrow.params,
            &arrow.body,
            arrow.span.start,
        );
    }

    /// A callback registered with a test harness is named after the call
    /// that registers it rather than by position, so `it#2("adds")` says
    /// which case a finding is about. Everything else — the callee
    /// expression, non-function arguments — takes the default walk, so a
    /// closure inside `it.each([…])`'s table is still found.
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        let Some(callee) = harness_callee(&call.callee) else {
            walk::walk_call_expression(self, call);
            return;
        };
        let title = call_title(call);
        self.visit_expression(&call.callee);
        for arg in &call.arguments {
            match arg.as_expression().and_then(callback_shape) {
                Some((params, body, start)) => {
                    self.emit(callee, title.as_deref(), params, body, start);
                }
                None => self.visit_argument(arg),
            }
        }
    }
}

/// The parameters, body and span start of an argument that is written as
/// a function; `None` for anything else, including a function passed by
/// reference (`it("adds", checkAdds)`) which is a call, not a body.
fn callback_shape<'b, 'a>(
    expr: &'b Expression<'a>,
) -> Option<(&'b FormalParameters<'a>, &'b FunctionBody<'a>, u32)> {
    match expr {
        Expression::ArrowFunctionExpression(arrow) => {
            Some((&arrow.params, &arrow.body, arrow.span.start))
        }
        Expression::FunctionExpression(func) => func
            .body
            .as_ref()
            .map(|body| (&*func.params, &**body, func.span.start)),
        _ => None,
    }
}

/// Resolve a `PropertyKey` into the user-visible method name. Shared
/// between [`walk_class`], `cohesion`, and `wrapper` so the three
/// agree on how to spell `["computed-string"]`, `#private`, etc.
pub(crate) fn method_key_name(key: &PropertyKey) -> Option<String> {
    match key {
        PropertyKey::StaticIdentifier(id) => Some(id.name.to_string()),
        PropertyKey::PrivateIdentifier(id) => Some(format!("#{}", id.name)),
        PropertyKey::StringLiteral(s) => Some(s.value.to_string()),
        _ => None,
    }
}
