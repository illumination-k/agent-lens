//! Python function and call-shape extraction for the function-graph
//! analyzer.
//!
//! Free functions and class methods become [`FunctionShape`]s qualified
//! at the lexical module path the caller supplies. Each call expression
//! inside a function body becomes a [`CallShape`] tagged with the imports
//! visible at its file. The callee's leading name is bound through the
//! module's scopes ([`crate::semantic`]), so a call through an import, to
//! a module-level `def`, or to a builtin carries a
//! [`CallShape::callee_binding`], and one through a local carries the
//! local's shape. No type inference is attempted: `self.method(...)` and
//! `obj.method(...)` stay receiver calls.
//!
//! The treatment of imports mirrors `lens-ts`: aliases imported as a whole
//! module (`import os`, `import os as o`) participate as namespace aliases
//! so `os.path()` reads as a path call, while `from pkg import name`
//! aliases are treated as value imports.

use std::collections::HashSet;

use lens_domain::{
    ArgumentShape, BodyShape, CallShape, CalleeBinding, FunctionShape, ImportShape,
    LexicalResolutionStatus, LineIndex, OwnerKind, OwnerShape, ReceiverExprKind, SignatureShape,
    SourceSpan, SyntaxFact, qualify_module, starts_uppercase,
};
use ruff_python_ast::visitor::{Visitor, walk_expr};
use ruff_python_ast::{
    Alias, Expr, ExprAttribute, ExprCall, ExprName, Stmt, StmtClassDef, StmtFunctionDef,
    StmtImport, UnaryOp,
};
use ruff_python_parser::parse_module;
use ruff_text_size::Ranged;

use crate::module_path::{dotted_to_module_path, resolve_from_base, top_segment};
use crate::parser::{PythonParseError, function_body_tree};
use crate::semantic::{ModuleBindings, Referent, bind_module};
use crate::walk::{FnSite, walk_module_fns};

/// Extract neutral function-shape facts for Python.
///
/// `module` is the lexical module path the file lives at, in `::`-separated
/// form (e.g. `pkg::sub::main`). It is used to qualify each function name
/// so cross-file resolution in the function-graph analyzer can match them.
pub fn extract_function_shapes_with_module(
    source: &str,
    module: &str,
) -> Result<Vec<FunctionShape>, PythonParseError> {
    let parsed = parse_module(source)?.into_syntax();
    let lines = LineIndex::new(source);
    let mut out = Vec::new();
    walk_module_fns(&parsed.body, &mut |site| {
        out.push(function_shape(&site, module, &lines));
    });
    Ok(out)
}

/// Extract neutral call-shape facts for Python.
///
/// Calls outside any `def` (top-level statements, class-body expressions)
/// are skipped: the function-graph analyzer only attributes calls to
/// callers it can name.
pub fn extract_call_shapes_with_module(
    source: &str,
    module: &str,
) -> Result<Vec<CallShape>, PythonParseError> {
    let parsed = parse_module(source)?.into_syntax();
    let lines = LineIndex::new(source);
    let imports = collect_imports(&parsed.body, module);
    let namespace_aliases: HashSet<String> = imports
        .iter()
        .filter_map(|import| {
            let alias = import.local_alias.known_value().and_then(Option::as_ref)?;
            let exported = import
                .exported_symbol
                .known_value()
                .and_then(Option::as_ref);
            (exported.is_none()).then(|| alias.clone())
        })
        .collect();
    let file = FileFacts {
        source,
        module,
        line_index: &lines,
        imports: &imports,
        namespace_aliases: &namespace_aliases,
        bindings: &bind_module(&parsed.body, module),
    };
    let mut out = Vec::new();
    walk_module_fns(&parsed.body, &mut |site| {
        collect_calls_in_function(&site, &file, &mut out);
    });
    Ok(out)
}

/// The facts every call site in one file shares.
struct FileFacts<'a, 'b> {
    source: &'a str,
    module: &'a str,
    line_index: &'a LineIndex,
    imports: &'a [ImportShape],
    namespace_aliases: &'a HashSet<String>,
    bindings: &'a ModuleBindings<'b>,
}

fn function_shape(site: &FnSite<'_>, module: &str, lines: &LineIndex) -> FunctionShape {
    let func = site.func;
    let display_name = func.name.as_str().to_owned();
    let qualified = match site.owner {
        Some(class) => qualify_module(module, &format!("{class}::{display_name}")),
        None => qualify_module(module, &display_name),
    };
    let owner_shape = site.owner.map(|class_name| OwnerShape {
        display_name: class_name.to_owned(),
        kind: OwnerKind::Class,
    });
    FunctionShape {
        display_name,
        qualified_name: SyntaxFact::Known(qualified),
        module_path: SyntaxFact::Known(module.to_owned()),
        owner: SyntaxFact::Known(owner_shape),
        // Export status is not extracted yet, so stay honest
        // with `Unknown` instead of hardcoding `Unexported`.
        visibility: SyntaxFact::Unknown,
        signature: SyntaxFact::Known(SignatureShape::from(crate::parser::signature_info(
            func,
            site.owner.is_some(),
        ))),
        doc: crate::parser::docstring_text(func),
        // Decorators are not extracted yet; `Unknown` keeps a
        // framework-registered function from reading as unannotated.
        attributes: SyntaxFact::Unknown,
        body: BodyShape {
            tree: function_body_tree(func),
        },
        span: function_span(func, lines),
        is_test: site.is_test,
    }
}

fn collect_calls_in_function(
    site: &FnSite<'_>,
    file: &FileFacts<'_, '_>,
    out: &mut Vec<CallShape>,
) {
    let display_name = site.func.name.as_str();
    let caller_qualified = match site.owner {
        Some(class) => qualify_module(file.module, &format!("{class}::{display_name}")),
        None => qualify_module(file.module, display_name),
    };
    let mut visitor = FunctionBodyCallVisitor {
        file,
        caller_qualified_name: caller_qualified,
        caller_owner: site.owner.map(ToOwned::to_owned),
        caller_class: site.class,
        out: Vec::new(),
    };
    for body_stmt in &site.func.body {
        visitor.visit_stmt(body_stmt);
    }
    out.extend(visitor.out);
}

struct FunctionBodyCallVisitor<'f, 'a, 'b> {
    file: &'f FileFacts<'a, 'b>,
    caller_qualified_name: String,
    caller_owner: Option<String>,
    caller_class: Option<&'f StmtClassDef>,
    out: Vec<CallShape>,
}

/// What `super().name(...)` in a method names.
enum SuperCallee<'e> {
    /// `name` looked up from the class's first base, written as a path
    /// (`Base`, `mod.Base`).
    Base { base: &'e Expr, path: Vec<String> },
    /// The class has no explicit base: `name` is a method of `object`.
    Object,
}

impl<'ast> Visitor<'ast> for FunctionBodyCallVisitor<'_, '_, '_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            let shape = self.call_shape(call);
            self.out.push(shape);
        }
        walk_expr(self, expr);
    }
}

impl FunctionBodyCallVisitor<'_, '_, '_> {
    /// The visitor already owns every caller-side fact a call shape
    /// needs, so this reads them off `self` instead of taking them as a
    /// parameter list.
    fn call_shape(&self, call: &ExprCall) -> CallShape {
        // `super().close()` is `Base.close(self)`: read it as that path so
        // it binds through `Base`, and never as a receiver call that the
        // name fallback could bind to the caller's own override.
        let super_callee = self.super_callee(&call.func);
        let (facts, head) = match &super_callee {
            Some((SuperCallee::Base { base, path }, method)) => {
                let mut path = path.clone();
                path.push((*method).to_owned());
                let facts = CalleeFacts {
                    name: Some((*method).to_owned()),
                    path_segments: Some(path),
                    receiver: ReceiverExprKind::None,
                };
                (facts, callee_head(base))
            }
            _ => {
                // `extract = mi.extract; extract(...)` calls `mi.extract`.
                let callee = self.aliased_callee(&call.func).unwrap_or(&call.func);
                (
                    callee_facts(callee, self.file.namespace_aliases),
                    callee_head(callee),
                )
            }
        };
        let line = self.file.line_index.line(call.range.start().to_u32());
        let referent = head.and_then(|head| self.file.bindings.referent(head));
        let segments = facts.path_segments.as_deref();
        // A local — a parameter, an assignment, a nested `def` — shadows
        // every definition outside the function, but only a bare call
        // names the binding itself: `emit.run()` names a method on
        // whatever the local holds.
        let callee_is_locally_bound =
            matches!(segments, Some([_])) && referent == Some(Referent::Local);
        // `client.connect()` on a parameter or local, or on a module-level
        // value, calls a method on that value — never a free function
        // reachable by name — even when the value's name is uppercase or
        // shadows an import.
        let receiver = match (facts.receiver, referent, segments) {
            (
                ReceiverExprKind::None | ReceiverExprKind::Expression,
                Some(Referent::Local | Referent::ModuleValue),
                Some([_, _, ..]),
            ) => ReceiverExprKind::LocalValue,
            (receiver, ..) => receiver,
        };
        let callee_binding = if matches!(super_callee, Some((SuperCallee::Object, _))) {
            SyntaxFact::Known(CalleeBinding::External)
        } else {
            match (referent, segments) {
                // `s.send()` on `s: Session` or `s = Session()` calls
                // `Session.send`, bound through `Session`.
                (Some(Referent::Local | Referent::ModuleValue), Some([_, method])) => head
                    .and_then(|head| self.file.bindings.declared_class(head))
                    .map_or(SyntaxFact::Unknown, |class| {
                        self.class_member_binding(class, method)
                    }),
                _ => self.path_binding(referent, segments),
            }
        };
        // Positional arguments first, then keywords — the order Python
        // syntax itself enforces at a call site.
        let mut arguments: Vec<ArgumentShape> = call
            .arguments
            .args
            .iter()
            .map(|arg| argument_shape(self.file.source, arg))
            .collect();
        arguments.extend(call.arguments.keywords.iter().map(|keyword| {
            match keyword.arg.as_ref() {
                Some(name) => ArgumentShape::Keyword {
                    name: name.as_str().to_owned(),
                    value: Box::new(argument_shape(self.file.source, &keyword.value)),
                },
                // `**kwargs` unpacking: no positions line up past it.
                None => ArgumentShape::Spread,
            }
        }));
        CallShape {
            caller_qualified_name: SyntaxFact::Known(Some(self.caller_qualified_name.clone())),
            caller_module: SyntaxFact::Known(self.file.module.to_owned()),
            caller_owner: SyntaxFact::Known(self.caller_owner.clone()),
            callee_display_name: SyntaxFact::Known(facts.name),
            callee_path_segments: facts
                .path_segments
                .map_or(SyntaxFact::Unknown, SyntaxFact::Known),
            receiver_expr_kind: SyntaxFact::Known(receiver),
            arguments: SyntaxFact::Known(arguments),
            callee_is_locally_bound: SyntaxFact::Known(callee_is_locally_bound),
            callee_binding,
            lexical_resolution: LexicalResolutionStatus::NotAttempted,
            visible_imports: self.file.imports.to_vec(),
            line,
        }
    }
}

impl<'f, 'b> FunctionBodyCallVisitor<'f, '_, 'b> {
    /// What a callee path whose head is `referent` is bound to: a builtin
    /// is external, an import expands to the imported path, and a
    /// module-level `def` or `class` qualifies at this module.
    fn path_binding(
        &self,
        referent: Option<Referent<'_>>,
        segments: Option<&[String]>,
    ) -> SyntaxFact<CalleeBinding> {
        match (referent, segments) {
            (Some(Referent::Builtin), _) => SyntaxFact::Known(CalleeBinding::External),
            (Some(Referent::Import(target)), Some([_, tail @ ..])) => {
                let mut path = target.to_owned();
                for segment in tail {
                    path.push_str("::");
                    path.push_str(segment);
                }
                SyntaxFact::Known(CalleeBinding::Declaration(path))
            }
            (Some(Referent::ModuleDefinition), Some(segments)) => SyntaxFact::Known(
                CalleeBinding::Declaration(qualify_module(self.file.module, &segments.join("::"))),
            ),
            _ => SyntaxFact::Unknown,
        }
    }

    /// The path a bare callee names through an alias: `mi.extract` for
    /// `extract()` after `extract = mi.extract` in the same function, when
    /// that assignment is the name's only binding and the path starts at an import, a
    /// module-level definition or a builtin. `None` otherwise, which
    /// leaves a local callee shadowing every definition of its name.
    fn aliased_callee<'e>(&self, callee: &'e Expr) -> Option<&'e Expr>
    where
        'b: 'e,
    {
        let Expr::Name(name) = callee else {
            return None;
        };
        // Only a function's local: a module-level assignment runs before
        // the module finishes binding, so its path may name something
        // other than what the module's final scope says
        // (`_issubclass = issubclass` ahead of `def issubclass`).
        if self.file.bindings.referent(name) != Some(Referent::Local) {
            return None;
        }
        let target = self.file.bindings.alias_of(name)?;
        let head = callee_head(target)?;
        matches!(
            self.file.bindings.referent(head),
            Some(Referent::Import(_) | Referent::ModuleDefinition | Referent::Builtin)
        )
        .then_some(target)
    }

    /// The binding of `method` on an instance of `class`, a plain path
    /// the value's declaration names.
    fn class_member_binding(&self, class: &Expr, method: &str) -> SyntaxFact<CalleeBinding> {
        let Some(mut segments) = expression_path(class) else {
            return SyntaxFact::Unknown;
        };
        segments.push(method.to_owned());
        let referent = callee_head(class).and_then(|head| self.file.bindings.referent(head));
        self.path_binding(referent, Some(&segments))
    }

    /// For a callee `super().name` (or `super(Cls, self).name`) inside a
    /// method, where `super` is the builtin: what `super()` reaches, and
    /// `name`. `None` for any other callee, and for a class whose first
    /// base is no plain path (`namedtuple(...)`), which leaves the call
    /// an ordinary receiver call.
    fn super_callee<'e>(&self, callee: &'e Expr) -> Option<(SuperCallee<'f>, &'e str)> {
        let Expr::Attribute(ExprAttribute { value, attr, .. }) = callee else {
            return None;
        };
        let Expr::Call(ExprCall { func, .. }) = value.as_ref() else {
            return None;
        };
        let Expr::Name(name) = func.as_ref() else {
            return None;
        };
        if name.id.as_str() != "super"
            || self.file.bindings.referent(name) != Some(Referent::Builtin)
        {
            return None;
        }
        let class = self.caller_class?;
        let base = class
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.args.first());
        let callee = match base {
            None => SuperCallee::Object,
            Some(base) => {
                // `class C(Base[T])`: the base is `Base`.
                let base = match base {
                    Expr::Subscript(subscript) => &subscript.value,
                    base => base,
                };
                SuperCallee::Base {
                    base,
                    path: expression_path(base)?,
                }
            }
        };
        Some((callee, attr.as_str()))
    }
}

/// Classify one call argument. Literals (`None`, `True`, `...`, and
/// f-strings without substitutions included) carry their source text;
/// an uppercase-initial name or attribute chain (`Color.RED`,
/// `DEFAULTS`) gets the weaker [`ArgumentShape::Const`] on the same
/// naming convention the callee classifier above already trusts —
/// Python locals are lowercase by convention, so the same uppercase
/// text denotes the same module-level thing.
fn argument_shape(source: &str, expr: &Expr) -> ArgumentShape {
    let literal = || ArgumentShape::Literal {
        text: source[expr.range()].to_owned(),
    };
    match expr {
        Expr::NumberLiteral(_)
        | Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::EllipsisLiteral(_) => literal(),
        Expr::UnaryOp(unary)
            if unary.op == UnaryOp::USub && matches!(&*unary.operand, Expr::NumberLiteral(_)) =>
        {
            literal()
        }
        Expr::Name(name) => {
            let text = name.id.to_string();
            if starts_uppercase(&text) {
                ArgumentShape::Const { text }
            } else {
                ArgumentShape::Identifier { text }
            }
        }
        Expr::Attribute(_) => match expression_path(expr) {
            Some(segments) if segments.first().is_some_and(|s| starts_uppercase(s)) => {
                ArgumentShape::Const {
                    text: segments.join("."),
                }
            }
            _ => ArgumentShape::Other,
        },
        Expr::Starred(_) => ArgumentShape::Spread,
        _ => ArgumentShape::Other,
    }
}

struct CalleeFacts {
    name: Option<String>,
    path_segments: Option<Vec<String>>,
    receiver: ReceiverExprKind,
}

fn callee_facts(callee: &Expr, namespace_aliases: &HashSet<String>) -> CalleeFacts {
    match callee {
        Expr::Name(ExprName { id, .. }) => CalleeFacts {
            name: Some(id.to_string()),
            path_segments: Some(vec![id.to_string()]),
            receiver: ReceiverExprKind::None,
        },
        Expr::Attribute(ExprAttribute { value, attr, .. }) => {
            let mut segments = expression_path(value).unwrap_or_default();
            segments.push(attr.as_str().to_owned());
            let receiver = if matches!(
                value.as_ref(),
                Expr::Name(name) if name.id.as_str() == "self"
            ) {
                ReceiverExprKind::SelfValue
            } else if segments
                .first()
                .is_some_and(|first| namespace_aliases.contains(first) || starts_uppercase(first))
            {
                ReceiverExprKind::None
            } else {
                ReceiverExprKind::Expression
            };
            CalleeFacts {
                name: Some(attr.as_str().to_owned()),
                path_segments: (!segments.is_empty()).then_some(segments),
                receiver,
            }
        }
        _ => CalleeFacts {
            name: None,
            path_segments: None,
            receiver: ReceiverExprKind::Expression,
        },
    }
}

/// The name a callee expression starts from: `f` in `f()`, `a` in
/// `a.b.c()`; `None` for a computed callee.
fn callee_head(callee: &Expr) -> Option<&ExprName> {
    match callee {
        Expr::Name(name) => Some(name),
        Expr::Attribute(attr) => callee_head(&attr.value),
        _ => None,
    }
}

fn expression_path(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::Name(name) => Some(vec![name.id.to_string()]),
        Expr::Attribute(attr) => {
            let mut segments = expression_path(&attr.value)?;
            segments.push(attr.attr.as_str().to_owned());
            Some(segments)
        }
        _ => None,
    }
}

fn collect_imports(body: &[Stmt], module: &str) -> Vec<ImportShape> {
    let mut out = Vec::new();
    for stmt in body {
        match stmt {
            Stmt::Import(StmtImport { names, .. }) => {
                out.extend(names.iter().map(module_import_shape));
            }
            Stmt::ImportFrom(from) => {
                let Some(base) =
                    resolve_from_base(module, from.level, from.module.as_ref().map(|m| m.as_str()))
                else {
                    continue;
                };
                for alias in &from.names {
                    let imported = alias.name.as_str();
                    if imported == "*" {
                        continue;
                    }
                    let local = alias
                        .asname
                        .as_ref()
                        .map(|n| n.as_str().to_owned())
                        .unwrap_or_else(|| imported.to_owned());
                    let target = if base.is_empty() {
                        imported.to_owned()
                    } else {
                        format!("{base}::{imported}")
                    };
                    out.push(import_shape(Some(local), target, Some(imported.to_owned())));
                }
            }
            _ => {}
        }
    }
    out
}

/// `import a.b` binds `a`, the top-level package, so the alias names
/// `a`; `import a.b as c` binds `a.b` to `c`.
fn module_import_shape(alias: &Alias) -> ImportShape {
    let imported = alias.name.as_str();
    let (local, target) = match &alias.asname {
        Some(asname) => (asname.as_str(), dotted_to_module_path(imported)),
        None => {
            let top = top_segment(imported);
            (top, top.to_owned())
        }
    };
    import_shape(Some(local.to_owned()), target, None)
}

/// Every import in this language names its module, alias, and symbol
/// outright, so the three facts are always known.
fn import_shape(
    local_alias: Option<String>,
    imported_module: String,
    exported_symbol: Option<String>,
) -> ImportShape {
    ImportShape::known(imported_module, local_alias, exported_symbol)
}

fn function_span(func: &StmtFunctionDef, lines: &LineIndex) -> SourceSpan {
    let start_line = lines.line(func.range.start().to_u32());
    // `range.end()` lands at the position just past the last byte of the
    // body; the line that byte sits on is the closing line.
    let end_offset = func.range.end().to_u32().saturating_sub(1);
    let end_line = lines.line(end_offset);
    SourceSpan {
        start_line,
        end_line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn shapes(src: &str, module: &str) -> Vec<FunctionShape> {
        extract_function_shapes_with_module(src, module).unwrap()
    }

    fn calls(src: &str, module: &str) -> Vec<CallShape> {
        extract_call_shapes_with_module(src, module).unwrap()
    }

    /// The call's binding as text: the declaration path, `<external>`,
    /// or `None` when unknown.
    fn binding_of(call: &CallShape) -> Option<&str> {
        call.callee_binding().map(|binding| match binding {
            CalleeBinding::Declaration(target) => target.as_str(),
            CalleeBinding::External => "<external>",
        })
    }

    /// A callee bound in the caller's own scope — a nested `def`, a
    /// `lambda`, any parameter or local — is shadowed: the resolver must
    /// be told so it does not fall back to a same-named module function.
    /// A pytest fixture parameter (`httpbin("get")`) calls the fixture's
    /// return value, not the fixture function.
    #[rstest]
    #[case::nested_def("def caller():\n    def emit(x):\n        pass\n    emit(1)\n", true)]
    #[case::lambda_local("def caller():\n    emit = lambda x: x\n    emit(1)\n", true)]
    #[case::annotated_lambda(
        "def caller():\n    emit: Callable[[int], None] = lambda x: x\n    emit(1)\n",
        true
    )]
    #[case::callable_param("def caller(emit: Callable[[int], None]):\n    emit(1)\n", true)]
    #[case::qualified_callable_param("def caller(emit: typing.Callable):\n    emit(1)\n", true)]
    #[case::string_annotated_param(
        "def caller(emit: \"Callable[[int], None]\"):\n    emit(1)\n",
        true
    )]
    #[case::annotated_only("def caller():\n    emit: Callable[[int], None]\n    emit(1)\n", true)]
    #[case::binding_in_nested_block(
        "def caller(flag):\n    if flag:\n        emit = lambda x: x\n    emit(1)\n",
        true
    )]
    #[case::call_before_binding("def caller():\n    emit(1)\n    emit = lambda x: x\n", true)]
    #[case::closure_sees_outer_local(
        "def caller():\n    emit = lambda x: x\n    def inner():\n        emit(1)\n",
        true
    )]
    #[case::nonlocal_keeps_outer_binding(
        "def outer():\n    emit = lambda x: x\n    def inner():\n        nonlocal emit\n        emit = compute()\n        emit(1)\n",
        true
    )]
    #[case::module_lambda("emit = lambda x: x\ndef caller():\n    emit(1)\n", false)]
    #[case::plain_local("def caller():\n    emit = compute()\n    emit(1)\n", true)]
    #[case::value_param("def caller(emit: int):\n    emit(1)\n", true)]
    #[case::fixture_param("def test_get(emit):\n    emit(\"get\")\n", true)]
    #[case::tuple_target(
        "def caller(self):\n    emit, kw = self.emit, self.kw\n    emit(1)\n",
        true
    )]
    #[case::loop_target("def caller(hooks):\n    for emit in hooks:\n        emit(1)\n", true)]
    #[case::unbound_name("def caller():\n    emit(1)\n", false)]
    fn local_callable_bindings_shadow_bare_calls(#[case] src: &str, #[case] expected: bool) {
        let call = calls(src, "m")
            .into_iter()
            .find(|call| call.callee_name() == Some("emit"))
            .expect("emit call site");
        assert_eq!(call.callee_is_locally_bound(), expected);
    }

    /// The callee's leading name is bound through the module's scopes:
    /// imports expand to the imported path, module-level `def`s and
    /// classes qualify at the module, builtins are external, and a
    /// local, a later module-level rebinding, or a name nothing binds
    /// leaves the binding unknown.
    #[rstest]
    #[case::from_import(
        "from pkg.sub import run\ndef caller():\n    run()\n",
        "run",
        Some("pkg::sub::run")
    )]
    #[case::aliased_from_import(
        "from pkg import run as go\ndef caller():\n    go()\n",
        "go",
        Some("pkg::run")
    )]
    #[case::relative_import(
        "from . import util\ndef caller():\n    util.run()\n",
        "run",
        Some("pkg::util::run")
    )]
    #[case::module_import(
        "import pkg.sub as s\ndef caller():\n    s.run()\n",
        "run",
        Some("pkg::sub::run")
    )]
    #[case::submodule_import(
        "import pkg.sub\ndef caller():\n    pkg.sub.run()\n",
        "run",
        Some("pkg::sub::run")
    )]
    #[case::import_in_body(
        "def caller():\n    import pkg\n    pkg.run()\n",
        "run",
        Some("pkg::run")
    )]
    #[case::module_def(
        "def caller():\n    helper()\ndef helper():\n    pass\n",
        "helper",
        Some("pkg::main::helper")
    )]
    #[case::class_path(
        "def caller():\n    Svc.create()\nclass Svc:\n    pass\n",
        "create",
        Some("pkg::main::Svc::create")
    )]
    #[case::builtin("def caller(xs):\n    len(xs)\n", "len", Some("<external>"))]
    #[case::shadowed_builtin(
        "def len(x):\n    pass\ndef caller(xs):\n    len(xs)\n",
        "len",
        Some("pkg::main::len")
    )]
    #[case::param_shadows_import("from pkg import run\ndef caller(run):\n    run()\n", "run", None)]
    #[case::module_value("run = make()\ndef caller():\n    run()\n", "run", None)]
    #[case::global_statement(
        "def caller():\n    global run\n    run = make()\n    run()\ndef run():\n    pass\n",
        "run",
        Some("pkg::main::run")
    )]
    #[case::class_scope_invisible_to_methods(
        "class C:\n    def run(self):\n        pass\n    def caller(self):\n        run()\n",
        "run",
        None
    )]
    #[case::comprehension_target("def caller(fs):\n    [run() for run in fs]\n", "run", None)]
    #[case::unbound("def caller():\n    run()\n", "run", None)]
    #[case::walrus_in_comprehension_binds_the_function(
        "def run():\n    pass\ndef caller(fs):\n    [(run := f) for f in fs]\n    run()\n",
        "run",
        None
    )]
    #[case::lambda_default_runs_outside(
        "def run():\n    pass\ndef caller():\n    go(lambda cb=run(): cb())\n",
        "run",
        Some("pkg::main::run")
    )]
    #[case::lambda_parameter_shadows(
        "def run():\n    pass\ndef caller():\n    go(lambda run: run())\n",
        "run",
        None
    )]
    #[case::except_body_resolves(
        "def run():\n    pass\ndef caller():\n    try:\n        pass\n    except E as err:\n        run()\n",
        "run",
        Some("pkg::main::run")
    )]
    #[case::except_name_shadows(
        "def run():\n    pass\ndef caller():\n    try:\n        pass\n    except E as run:\n        run()\n",
        "run",
        None
    )]
    #[case::match_capture_shadows(
        "def run():\n    pass\ndef caller(x):\n    match x:\n        case run:\n            run()\n",
        "run",
        None
    )]
    #[case::match_star_shadows(
        "def run():\n    pass\ndef caller(x):\n    match x:\n        case [*run]:\n            run()\n",
        "run",
        None
    )]
    #[case::match_rest_shadows(
        "def run():\n    pass\ndef caller(x):\n    match x:\n        case {**run}:\n            run()\n",
        "run",
        None
    )]
    fn callee_bindings(#[case] src: &str, #[case] callee: &str, #[case] expected: Option<&str>) {
        let call = calls(src, "pkg::main")
            .into_iter()
            .find(|call| call.callee_name() == Some(callee))
            .expect("call site");
        assert_eq!(binding_of(&call), expected);
    }

    /// A local assigned once from a path that starts at an import or a
    /// module-level definition is an alias: calling it calls that path.
    /// Any other local still shadows the name. A module-level alias is
    /// not followed: its path is read before the module finishes binding.
    #[rstest]
    #[case::module_attribute(
        "import more_itertools as mi\ndef caller():\n    extract = mi.extract\n    extract(1)\n",
        Some("more_itertools::extract"),
        false
    )]
    #[case::module_def(
        "def run():\n    pass\ndef caller():\n    extract = run\n    extract(1)\n",
        Some("pkg::main::run"),
        false
    )]
    #[case::builtin(
        "def caller():\n    extract = len\n    extract(1)\n",
        Some("<external>"),
        false
    )]
    #[case::module_level_alias(
        "extract = run\ndef run():\n    pass\ndef caller():\n    extract(1)\n",
        None,
        false
    )]
    #[case::attribute_of_self(
        "class C:\n    def caller(self):\n        extract = self.extract\n        extract(1)\n",
        None,
        true
    )]
    #[case::rebound(
        "import mi\ndef caller(flag):\n    extract = mi.extract\n    if flag:\n        extract = mi.other\n    extract(1)\n",
        None,
        true
    )]
    fn local_aliases_call_their_path(
        #[case] src: &str,
        #[case] expected: Option<&str>,
        #[case] locally_bound: bool,
    ) {
        let call = calls(src, "pkg::main")
            .into_iter()
            .find(|call| call.line == src.lines().count())
            .expect("last-line call site");
        assert_eq!(binding_of(&call), expected);
        assert_eq!(call.callee_is_locally_bound(), locally_bound);
    }

    /// `super().name()` reads as `Base.name()` through the class's first
    /// base, so it binds where the base does and never to the caller's
    /// own override; with no base it is `object`'s method.
    #[rstest]
    #[case::imported_base(
        "from pkg.core import Group\nclass C(Group):\n    def run(self):\n        super().run()\n",
        Some("pkg::core::Group::run")
    )]
    #[case::module_base(
        "class B:\n    pass\nclass C(B):\n    def run(self):\n        super().run()\n",
        Some("pkg::main::B::run")
    )]
    #[case::generic_base(
        "from pkg import B\nclass C(B[int]):\n    def run(self):\n        super().run()\n",
        Some("pkg::B::run")
    )]
    #[case::explicit_arguments(
        "from pkg import B\nclass C(B):\n    def run(self):\n        super(C, self).run()\n",
        Some("pkg::B::run")
    )]
    #[case::no_base(
        "class C:\n    def run(self):\n        super().run()\n",
        Some("<external>")
    )]
    #[case::builtin_base(
        "class C(dict):\n    def run(self):\n        super().run()\n",
        Some("<external>")
    )]
    #[case::computed_base("class C(make()):\n    def run(self):\n        super().run()\n", None)]
    #[case::shadowed_super("class C:\n    def run(self, super):\n        super().run()\n", None)]
    fn super_calls_bind_through_the_first_base(#[case] src: &str, #[case] expected: Option<&str>) {
        let call = calls(src, "pkg::main")
            .into_iter()
            .find(|call| call.callee_name() == Some("run"))
            .expect("run call site");
        assert_eq!(binding_of(&call), expected);
    }

    /// A receiver whose class its declaration names — an annotation, or a
    /// constructor call assigned once — binds the method through that
    /// class; anything less certain leaves the binding unknown.
    #[rstest]
    #[case::annotated_param(
        "from pkg import Session\ndef caller(s: Session):\n    s.send()\n",
        Some("pkg::Session::send")
    )]
    #[case::optional_param(
        "from typing import Optional\nfrom pkg import Session\ndef caller(s: Optional[Session]):\n    s.send()\n",
        Some("pkg::Session::send")
    )]
    #[case::union_with_none(
        "from pkg import Session\ndef caller(s: Session | None):\n    s.send()\n",
        Some("pkg::Session::send")
    )]
    #[case::annotated_local(
        "from pkg import Session\ndef caller():\n    s: Session = make()\n    s.send()\n",
        Some("pkg::Session::send")
    )]
    #[case::constructed_local(
        "import threading\ndef caller():\n    s = threading.Event()\n    s.send()\n",
        Some("threading::Event::send")
    )]
    #[case::constructed_module_value(
        "class Session:\n    pass\ns = Session()\ndef caller():\n    s.send()\n",
        Some("pkg::main::Session::send")
    )]
    #[case::builtin_annotation("def caller(s: str):\n    s.send()\n", Some("<external>"))]
    #[case::generic_origin(
        "from typing import List\ndef caller(s: List[int]):\n    s.send()\n",
        Some("typing::List::send")
    )]
    #[case::annotated(
        "from typing import Annotated\nfrom pkg import Session\ndef caller(s: Annotated[Session, 1]):\n    s.send()\n",
        Some("pkg::Session::send")
    )]
    #[case::other_operator(
        "from pkg import Session\ndef caller(s: Session & None):\n    s.send()\n",
        None
    )]
    #[case::constructed_by_a_call_result(
        "def caller():\n    s = make().Session()\n    s.send()\n",
        None
    )]
    #[case::union(
        "from typing import Union\ndef caller(s: Union[A, B]):\n    s.send()\n",
        None
    )]
    #[case::type_of_class("def caller(s: type[A]):\n    s.send()\n", None)]
    #[case::string_annotation("def caller(s: \"Session\"):\n    s.send()\n", None)]
    #[case::lowercase_factory("def caller():\n    s = make()\n    s.send()\n", None)]
    #[case::rebound(
        "from pkg import Session\ndef caller(s: Session):\n    s = wrap(s)\n    s.send()\n",
        None
    )]
    #[case::attribute_chain(
        "from pkg import Session\ndef caller(s: Session):\n    s.adapter.send()\n",
        None
    )]
    fn declared_classes_bind_receiver_methods(#[case] src: &str, #[case] expected: Option<&str>) {
        let call = calls(src, "pkg::main")
            .into_iter()
            .find(|call| call.callee_name() == Some("send"))
            .expect("send call site");
        assert_eq!(binding_of(&call), expected);
        assert_eq!(
            call.receiver_expr_kind,
            SyntaxFact::Known(ReceiverExprKind::LocalValue)
        );
    }

    /// A comprehension's target binds in the comprehension's own scope,
    /// so it neither leaks into the function nor hides a call inside the
    /// comprehension from the module it resolves to.
    #[rstest]
    #[case::list("[run for run in fs]")]
    #[case::set("{run for run in fs}")]
    #[case::generator("list(run for run in fs)")]
    #[case::dict("{run: 1 for run in fs}")]
    fn comprehension_targets_stay_in_their_scope(#[case] comprehension: &str) {
        let src = format!(
            "def run():\n    pass\ndef caller(fs):\n    {comprehension}\n    [run() for f in fs]\n    run()\n"
        );
        let bindings: Vec<_> = calls(&src, "m")
            .into_iter()
            .filter(|call| call.callee_name() == Some("run"))
            .map(|call| call.callee_binding().cloned())
            .collect();
        let module_run = Some(CalleeBinding::Declaration("m::run".to_owned()));
        assert_eq!(bindings, [module_run.clone(), module_run]);
    }

    /// A receiver bound to a value — a parameter, a local, a module-level
    /// assignment — is a method call whatever its case; an import or a
    /// class keeps the path reading.
    #[rstest]
    #[case::uppercase_local(
        "def caller():\n    Cls = pick()\n    Cls.run()\n",
        ReceiverExprKind::LocalValue
    )]
    #[case::param_shadows_namespace(
        "import graph\ndef caller(graph):\n    graph.run()\n",
        ReceiverExprKind::LocalValue
    )]
    #[case::module_value(
        "client = Client()\ndef caller():\n    client.run()\n",
        ReceiverExprKind::LocalValue
    )]
    #[case::namespace_import(
        "import graph\ndef caller():\n    graph.run()\n",
        ReceiverExprKind::None
    )]
    #[case::module_class(
        "class Svc:\n    pass\ndef caller():\n    Svc.run()\n",
        ReceiverExprKind::None
    )]
    #[case::unbound_lowercase("def caller():\n    client.run()\n", ReceiverExprKind::Expression)]
    #[case::self_receiver(
        "class S:\n    def caller(self):\n        self.run()\n",
        ReceiverExprKind::SelfValue
    )]
    fn receivers_follow_their_binding(#[case] src: &str, #[case] expected: ReceiverExprKind) {
        let call = calls(src, "m")
            .into_iter()
            .find(|call| call.callee_name() == Some("run"))
            .expect("run call site");
        assert_eq!(call.receiver_expr_kind, SyntaxFact::Known(expected));
    }

    /// A `lambda`'s own parameters bind in the lambda's scope, not the
    /// enclosing function's, so they must not shadow calls around it.
    #[test]
    fn lambda_parameters_do_not_shadow_the_enclosing_scope() {
        let src = "def caller():\n    run(lambda emit: emit(1))\n    emit(2)\n";
        let bound: Vec<_> = calls(src, "m")
            .into_iter()
            .filter(|call| call.callee_name() == Some("emit"))
            .map(|call| call.callee_is_locally_bound())
            .collect();
        assert_eq!(bound, [true, false]);
    }

    /// Only the shadowed name is affected.
    #[test]
    fn other_calls_in_a_shadowing_function_are_untouched() {
        let src = "def caller():\n    emit = lambda x: x\n    emit(1)\n    helper()\n";
        let flags: Vec<_> = calls(src, "m")
            .iter()
            .map(|call| {
                (
                    call.callee_name().map(ToOwned::to_owned),
                    call.callee_is_locally_bound(),
                )
            })
            .collect();
        assert_eq!(
            flags,
            [
                (Some("emit".to_owned()), true),
                (Some("helper".to_owned()), false),
            ]
        );
    }

    /// The non-constant boundary of the classifier: negation of
    /// anything but a number literal and an attribute chain on a
    /// lowercase local stay opaque.
    #[rstest]
    #[case::negated_identifier("def a(x):\n    f(-x)\n", ArgumentShape::Other)]
    #[case::lowercase_attribute("def a(obj):\n    f(obj.attr)\n", ArgumentShape::Other)]
    fn argument_shape_edge_cases(#[case] src: &str, #[case] expected: ArgumentShape) {
        let call = calls(src, "m")
            .into_iter()
            .find(|call| call.callee_name() == Some("f"))
            .expect("f call site");
        assert_eq!(
            call.arguments.known_value().cloned().expect("known"),
            vec![expected],
        );
    }

    /// Argument shapes: literals (`None`, `True`, a negative number)
    /// carry source text, uppercase names and attribute chains are
    /// consts, keywords carry their name and value shape, and `*`/`**`
    /// unpacking is a spread.
    #[test]
    fn call_arguments_are_classified_by_shape() {
        let src = "def caller(x, xs, kw):\n    f(1, -2, \"s\", None, True, Color.RED, MAX, x, g(), *xs, mode=\"w\", **kw)\n";
        let call = calls(src, "m")
            .into_iter()
            .find(|call| call.callee_name() == Some("f"))
            .expect("f call site");
        let text = |t: &str| t.to_owned();
        assert_eq!(
            call.arguments.known_value().cloned().expect("known"),
            vec![
                ArgumentShape::Literal { text: text("1") },
                ArgumentShape::Literal { text: text("-2") },
                ArgumentShape::Literal {
                    text: text("\"s\"")
                },
                ArgumentShape::Literal { text: text("None") },
                ArgumentShape::Literal { text: text("True") },
                ArgumentShape::Const {
                    text: text("Color.RED")
                },
                ArgumentShape::Const { text: text("MAX") },
                ArgumentShape::Identifier { text: text("x") },
                ArgumentShape::Other,
                ArgumentShape::Spread,
                ArgumentShape::Keyword {
                    name: text("mode"),
                    value: Box::new(ArgumentShape::Literal {
                        text: text("\"w\"")
                    }),
                },
                ArgumentShape::Spread,
            ],
        );
    }

    /// The function-shape signature carries the declared parameters —
    /// what the parameters analyzer lines call-site arguments up
    /// against — with `self` projected as the receiver, not a slot.
    #[test]
    fn function_shapes_carry_parameter_names() {
        let src = "class S:\n    def run(self, a, b=1):\n        return a\n";
        let funcs = shapes(src, "m");
        let signature = funcs[0].signature_shape().expect("signature extracted");
        let names: Vec<&str> = signature.parameter_names().collect();
        assert_eq!(names, ["a", "b"]);
    }

    #[test]
    fn extracts_module_qualified_names_for_free_and_class_methods() {
        let src = "
def helper():
    return 1

class Service:
    def run(self):
        return helper()
";
        let funcs = shapes(src, "pkg::main");
        let names: Vec<_> = funcs
            .iter()
            .map(|f| f.qualified_name.known_value().unwrap().as_str())
            .collect();
        assert_eq!(names, ["pkg::main::helper", "pkg::main::Service::run"]);

        let owner = funcs[1]
            .owner
            .known_value()
            .unwrap()
            .as_ref()
            .map(|o| (o.display_name.clone(), o.kind));
        assert_eq!(owner, Some(("Service".to_owned(), OwnerKind::Class)));
    }

    #[test]
    fn empty_module_qualifies_with_bare_name() {
        let funcs = shapes("def f():\n    return 1\n", "");
        assert_eq!(
            funcs[0].qualified_name.known_value().map(String::as_str),
            Some("f"),
        );
    }

    #[test]
    fn drops_stub_and_protocol_subtrees() {
        let src = "
from typing import Protocol

class P(Protocol):
    def f(self): ...

def stub(): ...

def real():
    return 1
";
        let funcs = shapes(src, "m");
        let names: Vec<_> = funcs.iter().map(|f| f.display_name.clone()).collect();
        assert_eq!(names, ["real"]);
    }

    #[test]
    fn bare_call_shape_records_caller_module_and_imports() {
        let src = "
from helper import helper

def caller():
    helper()
";
        let calls = calls(src, "main");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].caller_qualified_name(), Some("main::caller"));
        assert_eq!(calls[0].callee_name(), Some("helper"));
        assert_eq!(
            calls[0].visible_imports[0]
                .imported_module
                .known_value()
                .map(String::as_str),
            Some("helper::helper"),
        );
    }

    #[test]
    fn namespace_import_member_calls_are_path_calls() {
        let src = "
import graph

def caller():
    graph.create_view()
";
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("create_view"));
        assert_eq!(call.callee_path().as_deref(), Some("graph::create_view"));
        assert!(!call.has_receiver_expression());
    }

    #[test]
    fn aliased_namespace_import_keeps_alias_segment() {
        let src = "
import graph as g

def caller():
    g.create_view()
";
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_path().as_deref(), Some("g::create_view"));
        assert!(!call.has_receiver_expression());
        let import = &call.visible_imports[0];
        assert_eq!(
            import.local_alias.known_value().and_then(Option::as_ref),
            Some(&"g".to_owned()),
        );
        assert_eq!(
            import.imported_module.known_value().map(String::as_str),
            Some("graph"),
        );
    }

    #[test]
    fn self_method_calls_remain_unresolved_self_value() {
        let src = "
class Service:
    def helper(self):
        return 1
    def caller(self):
        return self.helper()
";
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("helper"));
        assert!(call.has_receiver_expression());
    }

    #[test]
    fn class_static_calls_are_path_calls() {
        let src = "
class Helper:
    @staticmethod
    def run():
        return 1

def caller():
    Helper.run()
";
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("run"));
        assert_eq!(call.callee_path().as_deref(), Some("Helper::run"));
        assert!(!call.has_receiver_expression());
    }

    #[test]
    fn lowercase_member_calls_remain_receiver_calls() {
        let src = "
def caller(client):
    client.connect()
";
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("connect"));
        assert_eq!(call.callee_path().as_deref(), Some("client::connect"));
        assert!(call.has_receiver_expression());
    }

    #[test]
    fn from_import_targets_are_qualified_to_module_segment() {
        let src = "
from pkg.sub import name

def caller():
    name()
";
        let call = &calls(src, "main")[0];
        let import = &call.visible_imports[0];
        assert_eq!(
            import.imported_module.known_value().map(String::as_str),
            Some("pkg::sub::name"),
        );
        assert_eq!(
            import
                .exported_symbol
                .known_value()
                .and_then(Option::as_ref)
                .map(String::as_str),
            Some("name"),
        );
    }

    #[test]
    fn relative_from_import_climbs_module_segments() {
        let src = "
from .. import util

def caller():
    util.run()
";
        let call = &calls(src, "pkg::sub::main")[0];
        let import = &call.visible_imports[0];
        assert_eq!(
            import.imported_module.known_value().map(String::as_str),
            Some("pkg::util"),
        );
    }

    #[test]
    fn relative_import_outrunning_depth_is_dropped() {
        let src = "
from ... import util

def caller():
    util.run()
";
        let call = &calls(src, "main")[0];
        assert!(call.visible_imports.is_empty());
    }

    #[test]
    fn star_imports_are_skipped() {
        let src = "
from helpers import *

def caller():
    helper()
";
        let call = &calls(src, "main")[0];
        assert!(call.visible_imports.is_empty());
    }

    #[test]
    fn anonymous_callee_expressions_are_recorded_without_a_name() {
        let src = "
def caller():
    (lambda: 1)()
";
        let call = &calls(src, "main")[0];
        assert!(call.callee_name().is_none());
        assert!(call.callee_path().is_none());
    }

    #[test]
    fn nested_attribute_call_preserves_full_path_segments() {
        // Catches a regression in `expression_path` where dropping the
        // recursive `Attribute` arm would shorten `a.b.c()` to just `c`.
        let src = "
def caller():
    a.b.c()
";
        let call = &calls(src, "main")[0];
        assert_eq!(call.callee_name(), Some("c"));
        assert_eq!(call.callee_path().as_deref(), Some("a::b::c"));
    }

    #[test]
    fn is_test_propagates_from_test_class_to_inner_methods() {
        // `helper` is not test-named on its own, but the enclosing
        // `TestThing` class is — the `||` between owner and self must
        // surface that, otherwise mutation testing flags the propagation
        // as a no-op.
        let src = "
class TestThing:
    def helper(self):
        assert True
";
        let funcs = shapes(src, "pkg::main");
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].display_name, "helper");
        assert!(
            funcs[0].is_test,
            "method on a Test* class must inherit is_test=true",
        );
    }

    #[test]
    fn single_dot_import_at_module_root_yields_bare_target() {
        // `from . import util` at a top-level file — `pops == segments.len()`
        // must still resolve (`>` not `>=`), producing a bare `util` target.
        let src = "
from . import util

def caller():
    util.run()
";
        let call = &calls(src, "main")[0];
        let import = &call.visible_imports[0];
        assert_eq!(
            import.imported_module.known_value().map(String::as_str),
            Some("util"),
        );
    }

    #[test]
    fn single_dot_import_in_nested_module_keeps_parent_path() {
        // `from . import util` from `pkg::sub::main` resolves to
        // `pkg::sub::util` (drop one segment, then append the import).
        // Differentiates `len - pops` from `len / pops`, which would
        // collapse to `pkg::sub::main::util` here.
        let src = "
from . import util

def caller():
    util.run()
";
        let call = &calls(src, "pkg::sub::main")[0];
        let import = &call.visible_imports[0];
        assert_eq!(
            import.imported_module.known_value().map(String::as_str),
            Some("pkg::sub::util"),
        );
    }
}
