//! Scope and binding facts for one Python module, built on
//! [`ruff_python_semantic`].
//!
//! [`SemanticModel`] is the bookkeeping half of ruff's linter: it owns the
//! scope tree, the binding table, and the lookup rules (class bodies
//! invisible to their methods, comprehension scopes), but the traversal
//! that feeds it lives in the linter's checker. [`Binder`] is the minimal
//! version of that traversal the call-graph extractor needs: it binds
//! definitions, imports, parameters, and stored names, and records the
//! scope every loaded name sits in.
//!
//! Names are looked up only once the whole module is bound. That is
//! Python's own rule: an assignment anywhere in a function makes the name
//! local to all of it, and a body looks module names up when it runs, so
//! a call to a function defined further down the file still resolves.
//! Lookup is scope-accurate but not flow-sensitive: a name bound more than
//! once in a scope resolves to its last binding there.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use lens_domain::qualify_module;
use ruff_python_ast::name::QualifiedName;
use ruff_python_ast::visitor::{Visitor, walk_except_handler, walk_expr, walk_pattern, walk_stmt};
use ruff_python_ast::{
    Comprehension, ExceptHandler, Expr, ExprContext, ExprName, Identifier, Operator, Parameters,
    Pattern, Stmt, StmtAnnAssign, StmtAssign, StmtClassDef, StmtFunctionDef, StmtImport,
    StmtImportFrom,
};
use ruff_python_semantic::{
    BindingFlags, BindingId, BindingKind, FromImport, GeneratorKind, Import, Module, ModuleKind,
    ModuleSource, ScopeId, ScopeKind, SemanticModel,
};
use ruff_python_stdlib::builtins::{python_builtins, python_magic_globals};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::module_path::{dotted_to_module_path, resolve_from_base, top_segment};

/// The newest Python minor version whose builtins are bound. A builtin
/// that an older interpreter lacks costs nothing: a module that defines a
/// function of that name shadows the builtin binding.
const PYTHON_MINOR: u8 = 14;

/// What a loaded name refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Referent<'b> {
    /// A Python builtin (`print`, `len`, `dict`) the module does not
    /// shadow.
    Builtin,
    /// An import binding; the `::`-separated module path it names
    /// (`pkg::sub::name` for `from pkg.sub import name`).
    Import(&'b str),
    /// A `def` or `class` at module scope.
    ModuleDefinition,
    /// Any other module-scope binding: an assignment, a loop variable.
    ModuleValue,
    /// Any name bound in a function-level scope: a parameter, a local, a
    /// nested `def` or `class`. Calling one calls whatever it holds, which
    /// no workspace definition of that name can be.
    Local,
}

/// The resolved loads of one module, queried after binding completes.
pub(crate) struct ModuleBindings<'a> {
    semantic: SemanticModel<'a>,
    /// Name-load start offset → the scope the load sits in.
    loads: HashMap<TextSize, ScopeId>,
    import_targets: HashMap<BindingId, String>,
    declared_classes: HashMap<BindingId, &'a Expr>,
    aliases: HashMap<BindingId, &'a Expr>,
}

impl<'a> ModuleBindings<'a> {
    /// What `name` — a loaded name in the bound module — refers to;
    /// `None` when it resolves to nothing the file binds and is no
    /// builtin (a star import's member, a typo).
    pub(crate) fn referent(&self, name: &ExprName) -> Option<Referent<'_>> {
        let scope = *self.loads.get(&name.range.start())?;
        let id = self
            .semantic
            .lookup_symbol_in_scope(name.id.as_str(), scope, false)?;
        let binding = self.semantic.binding(id);
        Some(match &binding.kind {
            BindingKind::Builtin => Referent::Builtin,
            BindingKind::Import(_)
            | BindingKind::FromImport(_)
            | BindingKind::SubmoduleImport(_) => Referent::Import(self.import_targets.get(&id)?),
            BindingKind::FunctionDefinition(_) | BindingKind::ClassDefinition(_)
                if binding.scope == ScopeId::global() =>
            {
                Referent::ModuleDefinition
            }
            _ if binding.scope == ScopeId::global() => Referent::ModuleValue,
            _ => Referent::Local,
        })
    }

    /// The class the value in `name` — a loaded name in the bound module
    /// — is declared or constructed as: `Session` for a parameter
    /// `s: Session`, a local `s: Optional[Session]`, or `s = Session()`.
    /// Only a name bound once in its scope has one; a rebinding could
    /// hold anything. The returned expression is a plain path whose own
    /// head [`Self::referent`] can look up.
    pub(crate) fn declared_class(&self, name: &ExprName) -> Option<&'a Expr> {
        self.declared_classes
            .get(&self.sole_binding(name)?)
            .copied()
    }

    /// The plain path `name` — a loaded name in the bound module — was
    /// assigned from, when that is its only binding in its scope:
    /// `mi.extract` for `extract = mi.extract`.
    pub(crate) fn alias_of(&self, name: &ExprName) -> Option<&'a Expr> {
        self.aliases.get(&self.sole_binding(name)?).copied()
    }

    /// The binding `name` loads, when it is the only binding of that name
    /// in its scope.
    fn sole_binding(&self, name: &ExprName) -> Option<BindingId> {
        let scope = *self.loads.get(&name.range.start())?;
        let id = self
            .semantic
            .lookup_symbol_in_scope(name.id.as_str(), scope, false)?;
        let binding = self.semantic.binding(id);
        self.semantic.scopes[binding.scope]
            .shadowed_binding(id)
            .is_none()
            .then_some(id)
    }
}

/// Bind every scope of `body`, the module at lexical path `module`
/// (`pkg::sub::main`), which relative imports are resolved against.
pub(crate) fn bind_module<'a>(body: &'a [Stmt], module: &str) -> ModuleBindings<'a> {
    let semantic = SemanticModel::new(
        &[],
        Path::new(""),
        Module {
            kind: ModuleKind::Module,
            source: ModuleSource::File(Path::new("")),
            python_ast: body,
            name: None,
        },
    );
    let mut binder = Binder {
        semantic,
        module,
        declared_outer: HashMap::new(),
        loads: HashMap::new(),
        import_targets: HashMap::new(),
        declared_classes: HashMap::new(),
        aliases: HashMap::new(),
    };
    binder.bind_builtins();
    for stmt in body {
        binder.visit_stmt(stmt);
    }
    ModuleBindings {
        semantic: binder.semantic,
        loads: binder.loads,
        import_targets: binder.import_targets,
        declared_classes: binder.declared_classes,
        aliases: binder.aliases,
    }
}

struct Binder<'a, 'm> {
    semantic: SemanticModel<'a>,
    module: &'m str,
    /// Names a scope declares `global` or `nonlocal`: a store to one binds
    /// in an outer scope, so it must not create a local.
    declared_outer: HashMap<ScopeId, HashSet<&'a str>>,
    loads: HashMap<TextSize, ScopeId>,
    import_targets: HashMap<BindingId, String>,
    declared_classes: HashMap<BindingId, &'a Expr>,
    aliases: HashMap<BindingId, &'a Expr>,
}

impl<'a> Binder<'a, '_> {
    fn bind_builtins(&mut self) {
        for builtin in
            python_builtins(PYTHON_MINOR, false).chain(python_magic_globals(PYTHON_MINOR))
        {
            let id = self.semantic.push_builtin();
            self.semantic.global_scope_mut().add(builtin, id);
        }
    }

    fn bind(
        &mut self,
        name: &'a str,
        range: TextRange,
        kind: BindingKind<'a>,
    ) -> Option<BindingId> {
        if self
            .declared_outer
            .get(&self.semantic.scope_id)
            .is_some_and(|names| names.contains(name))
        {
            return None;
        }
        // PEP 572: a walrus inside a comprehension binds in the scope that
        // contains the outermost comprehension.
        let scope_id = if kind.is_named_expr_assignment() {
            self.semantic
                .scopes
                .ancestor_ids(self.semantic.scope_id)
                .find(|id| !self.semantic.scopes[*id].kind.is_generator())
                .unwrap_or(self.semantic.scope_id)
        } else {
            self.semantic.scope_id
        };
        let id = self
            .semantic
            .push_binding(range, kind, BindingFlags::empty());
        self.semantic.scopes[scope_id].add(name, id);
        Some(id)
    }

    fn bind_import(
        &mut self,
        name: &'a str,
        range: TextRange,
        kind: BindingKind<'a>,
        target: String,
    ) {
        if let Some(id) = self.bind(name, range, kind) {
            self.import_targets.insert(id, target);
        }
    }

    fn declare_class(&mut self, id: Option<BindingId>, class: Option<&'a Expr>) {
        if let (Some(id), Some(class)) = (id, class) {
            self.declared_classes.insert(id, class);
        }
    }

    fn declare_outer(&mut self, names: &'a [Identifier]) {
        self.declared_outer
            .entry(self.semantic.scope_id)
            .or_default()
            .extend(names.iter().map(Identifier::as_str));
    }

    fn bind_parameters(&mut self, parameters: &'a Parameters) {
        for param in parameters.iter() {
            let name = param.name();
            let id = self.bind(name.as_str(), name.range, BindingKind::Argument);
            self.declare_class(id, param.annotation().and_then(annotated_class));
        }
    }

    /// Defaults and annotations run in the scope the `def` or `lambda`
    /// sits in, not in its body's.
    fn visit_parameter_exprs(&mut self, parameters: &'a Parameters) {
        for param in parameters.iter() {
            if let Some(annotation) = param.annotation() {
                self.visit_expr(annotation);
            }
            if let Some(default) = param.default() {
                self.visit_expr(default);
            }
        }
    }

    fn visit_function_def(&mut self, func: &'a StmtFunctionDef) {
        for decorator in &func.decorator_list {
            self.visit_decorator(decorator);
        }
        self.visit_parameter_exprs(&func.parameters);
        if let Some(returns) = &func.returns {
            self.visit_expr(returns);
        }
        let kind = BindingKind::FunctionDefinition(self.semantic.scope_id);
        self.bind(func.name.as_str(), func.name.range, kind);
        self.semantic.push_scope(ScopeKind::Function(func));
        self.bind_parameters(&func.parameters);
        for stmt in &func.body {
            self.visit_stmt(stmt);
        }
        self.semantic.pop_scope();
    }

    fn visit_import(&mut self, import: &'a StmtImport) {
        for alias in &import.names {
            let imported = alias.name.as_str();
            // `referent` reads every import kind alike, so `import a.b`
            // needs no `SubmoduleImport` of its own.
            let kind = BindingKind::Import(Import {
                qualified_name: Box::new(QualifiedName::user_defined(imported)),
            });
            let Some(asname) = &alias.asname else {
                // `import a.b` binds `a`, the top-level package.
                let top = top_segment(imported);
                self.bind_import(top, alias.range, kind, top.to_owned());
                continue;
            };
            let target = dotted_to_module_path(imported);
            self.bind_import(asname.as_str(), alias.range, kind, target);
        }
    }

    fn visit_import_from(&mut self, from: &'a StmtImportFrom) {
        let base = resolve_from_base(
            self.module,
            from.level,
            from.module.as_ref().map(|m| m.as_str()),
        );
        for alias in from.names.iter().filter(|alias| &alias.name != "*") {
            let imported = alias.name.as_str();
            let local = alias.asname.as_ref().unwrap_or(&alias.name).as_str();
            match base.as_deref() {
                Some(base) => {
                    let kind = BindingKind::FromImport(FromImport {
                        qualified_name: Box::new(QualifiedName::user_defined(imported)),
                    });
                    let target = qualify_module(base, imported);
                    self.bind_import(local, alias.range, kind, target);
                }
                // A relative import that outruns the module's depth names
                // nothing, but still shadows outer names.
                None => {
                    self.bind(local, alias.range, BindingKind::Assignment);
                }
            }
        }
    }

    fn visit_class_def(&mut self, class: &'a StmtClassDef) {
        for decorator in &class.decorator_list {
            self.visit_decorator(decorator);
        }
        if let Some(arguments) = &class.arguments {
            self.visit_arguments(arguments);
        }
        self.semantic.push_scope(ScopeKind::Class(class));
        for stmt in &class.body {
            self.visit_stmt(stmt);
        }
        self.semantic.pop_scope();
        let kind = BindingKind::ClassDefinition(self.semantic.scope_id);
        self.bind(class.name.as_str(), class.name.range, kind);
    }

    /// The value runs before the targets bind, so `f = f(x)` loads the
    /// outer `f`.
    fn visit_assign(&mut self, assign: &'a StmtAssign) {
        self.visit_expr(&assign.value);
        if let [Expr::Name(name)] = assign.targets.as_slice() {
            let id = self.bind(name.id.as_str(), name.range, BindingKind::Assignment);
            self.declare_class(id, constructed_class(&assign.value));
            if let Some(id) = id
                && matches!(&*assign.value, Expr::Name(_) | Expr::Attribute(_))
            {
                self.aliases.insert(id, &assign.value);
            }
            return;
        }
        for target in &assign.targets {
            self.visit_expr(target);
        }
    }

    /// `x: T = v` binds `x`, and so does a bare `x: T`: inside a function
    /// it makes `x` a local (PEP 526) whether or not it is assigned.
    fn visit_ann_assign(&mut self, assign: &'a StmtAnnAssign) {
        self.visit_expr(&assign.annotation);
        if let Some(value) = &assign.value {
            self.visit_expr(value);
        }
        let Expr::Name(name) = assign.target.as_ref() else {
            self.visit_expr(&assign.target);
            return;
        };
        let id = self.bind(name.id.as_str(), name.range, BindingKind::Assignment);
        self.declare_class(id, annotated_class(&assign.annotation));
    }

    /// Bind a comprehension's scope: the first iterable runs in the
    /// enclosing scope, everything else in the comprehension's own.
    fn visit_comprehension_scope(
        &mut self,
        generators: &'a [Comprehension],
        kind: GeneratorKind,
        elements: &[Option<&'a Expr>],
    ) {
        let Some((first, rest)) = generators.split_first() else {
            return;
        };
        self.visit_expr(&first.iter);
        self.semantic.push_scope(ScopeKind::Generator {
            kind,
            is_async: generators.iter().any(|g| g.is_async),
        });
        self.visit_expr(&first.target);
        for condition in &first.ifs {
            self.visit_expr(condition);
        }
        for generator in rest {
            self.visit_comprehension(generator);
        }
        for element in elements.iter().flatten() {
            self.visit_expr(element);
        }
        self.semantic.pop_scope();
    }
}

impl<'a> Visitor<'a> for Binder<'a, '_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(func) => self.visit_function_def(func),
            Stmt::ClassDef(class) => self.visit_class_def(class),
            Stmt::Import(import) => self.visit_import(import),
            Stmt::ImportFrom(from) => self.visit_import_from(from),
            Stmt::Global(ast) => self.declare_outer(&ast.names),
            Stmt::Nonlocal(ast) => self.declare_outer(&ast.names),
            Stmt::Assign(assign) => self.visit_assign(assign),
            Stmt::AnnAssign(assign) => self.visit_ann_assign(assign),
            _ => walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Name(name) => match name.ctx {
                ExprContext::Load => {
                    self.loads
                        .insert(name.range.start(), self.semantic.scope_id);
                }
                ExprContext::Store => {
                    self.bind(name.id.as_str(), name.range, BindingKind::Assignment);
                }
                ExprContext::Del | ExprContext::Invalid => {}
            },
            Expr::Named(named) => {
                self.visit_expr(&named.value);
                match named.target.as_ref() {
                    Expr::Name(name) => {
                        self.bind(
                            name.id.as_str(),
                            name.range,
                            BindingKind::NamedExprAssignment,
                        );
                    }
                    target => self.visit_expr(target),
                }
            }
            Expr::Lambda(lambda) => {
                if let Some(parameters) = &lambda.parameters {
                    self.visit_parameter_exprs(parameters);
                }
                self.semantic.push_scope(ScopeKind::Lambda(lambda));
                if let Some(parameters) = &lambda.parameters {
                    self.bind_parameters(parameters);
                }
                self.visit_expr(&lambda.body);
                self.semantic.pop_scope();
            }
            Expr::ListComp(comp) => self.visit_comprehension_scope(
                &comp.generators,
                GeneratorKind::ListComprehension,
                &[Some(&comp.elt)],
            ),
            Expr::SetComp(comp) => self.visit_comprehension_scope(
                &comp.generators,
                GeneratorKind::SetComprehension,
                &[Some(&comp.elt)],
            ),
            Expr::Generator(comp) => self.visit_comprehension_scope(
                &comp.generators,
                GeneratorKind::Generator,
                &[Some(&comp.elt)],
            ),
            Expr::DictComp(comp) => self.visit_comprehension_scope(
                &comp.generators,
                GeneratorKind::DictComprehension,
                &[comp.key.as_deref(), Some(&comp.value)],
            ),
            _ => walk_expr(self, expr),
        }
    }

    fn visit_except_handler(&mut self, handler: &'a ExceptHandler) {
        let ExceptHandler::ExceptHandler(inner) = handler;
        if let Some(name) = &inner.name {
            self.bind(name.as_str(), name.range, BindingKind::BoundException);
        }
        walk_except_handler(self, handler);
    }

    fn visit_pattern(&mut self, pattern: &'a Pattern) {
        let name = match pattern {
            Pattern::MatchAs(ast) => ast.name.as_ref(),
            Pattern::MatchStar(ast) => ast.name.as_ref(),
            Pattern::MatchMapping(ast) => ast.rest.as_ref(),
            _ => None,
        };
        if let Some(name) = name {
            self.bind(name.as_str(), name.range(), BindingKind::Assignment);
        }
        walk_pattern(self, pattern);
    }
}

/// The class an annotation names, as a plain path: `Session`,
/// `mod.Session`, and the same inside `Optional[...]`, `... | None`,
/// `Final[...]`, `ClassVar[...]` or `Annotated[..., meta]`. A generic
/// (`List[int]`) names its origin, whose methods are what a call on the
/// value reaches. `None` where no single class is named: a union, a
/// `type[...]` (whose methods are the class's own, called on the class),
/// a string annotation.
fn annotated_class(annotation: &Expr) -> Option<&Expr> {
    match annotation {
        Expr::Name(_) | Expr::Attribute(_) => Some(annotation),
        Expr::Subscript(subscript) => match last_name(&subscript.value)? {
            "Optional" | "Final" | "ClassVar" => annotated_class(&subscript.slice),
            "Annotated" => match subscript.slice.as_ref() {
                Expr::Tuple(tuple) => annotated_class(tuple.elts.first()?),
                _ => None,
            },
            "Union" | "type" | "Type" => None,
            _ => annotated_class(&subscript.value),
        },
        Expr::BinOp(binop) if binop.op == Operator::BitOr => {
            match (binop.left.as_ref(), binop.right.as_ref()) {
                (class, Expr::NoneLiteral(_)) | (Expr::NoneLiteral(_), class) => {
                    annotated_class(class)
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// The class `value` constructs, when it is a call whose callee's last
/// segment is capitalized, Python's class naming convention: `Session()`,
/// `threading.Event()`.
fn constructed_class(value: &Expr) -> Option<&Expr> {
    let Expr::Call(call) = value else {
        return None;
    };
    last_name(&call.func)?
        .starts_with(char::is_uppercase)
        .then_some(&*call.func)
}

fn last_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(attr) => Some(attr.attr.as_str()),
        _ => None,
    }
}
