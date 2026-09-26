//! TypeScript / JavaScript function and call-shape extraction for the
//! function graph analyzer.
//!
//! No type inference is attempted, but names are bound: `oxc_semantic`
//! settles what each callee's leading identifier refers to — a local, a
//! declaration at module scope, an import, or nothing the file declares
//! (a global) — and, given an [`ImportContext`], `oxc_resolver` settles
//! which file an import names. Together they turn a call into
//! [`CalleeBinding`] facts the graph resolves without name matching.
//! Receiver calls on values stay unresolved unless they look like
//! namespace/static calls.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use lens_domain::{
    ArgumentShape, BodyShape, CallShape, CalleeBinding, FunctionShape, ImportShape,
    LexicalResolutionStatus, LineIndex, OwnerKind, OwnerShape, ParameterShape, ReceiverExprKind,
    ReceiverShape, SignatureShape, SourceSpan, SyntaxFact, qualify_module, starts_uppercase,
};
use oxc_allocator::Allocator;
use oxc_ast::ast::*;
use oxc_ast_visit::{Visit, walk};
use oxc_semantic::{Scoping, SemanticBuilder};
use oxc_syntax::scope::ScopeFlags;
use oxc_syntax::symbol::SymbolId;

use crate::parser::{Dialect, MODULE_EXTENSIONS, TsParseError, is_test_item};
use crate::resolver::{ModuleResolver, is_relative_specifier};
use crate::tree::function_body_tree;
use crate::walk::{FunctionItem, FunctionVisitor, walk_program};

/// Extract neutral function-shape facts for TypeScript / JavaScript.
pub fn extract_function_shapes_with_module(
    source: &str,
    dialect: Dialect,
    module: &str,
) -> Result<Vec<FunctionShape>, TsParseError> {
    let alloc = Allocator::default();
    let ret = dialect.parse(&alloc, source);
    if !ret.diagnostics.is_empty() {
        return Err(TsParseError::from_diagnostics(
            ret.diagnostics
                .iter()
                .map(|e| e.message.as_ref().to_owned()),
        ));
    }

    let line_index = LineIndex::new(source);
    let mut collector = FunctionShapeCollector {
        module: module.to_owned(),
        out: Vec::new(),
        jsdoc_by_attach: crate::parser::jsdoc_by_attach_offset(source, &ret.program.comments),
    };
    walk_program(&ret.program, &line_index, &mut collector);
    Ok(collector.out)
}

/// Where one file's import specifiers lead: the file itself, the
/// project's resolver, and the module path the caller names a resolved
/// source file by (`None` for a file outside the analysis).
pub struct ImportContext<'a> {
    pub file: &'a Path,
    pub resolver: &'a ModuleResolver,
    pub module_of: &'a dyn Fn(&Path) -> Option<String>,
}

/// Extract neutral call-shape facts for TypeScript / JavaScript.
///
/// Without an [`ImportContext`] only relative imports are followed, and
/// lexically (`./util` next to `app::main` is `app::util`).
pub fn extract_call_shapes_with_module(
    source: &str,
    dialect: Dialect,
    module: &str,
) -> Result<Vec<CallShape>, TsParseError> {
    extract_call_shapes(source, dialect, module, None)
}

/// [`extract_call_shapes_with_module`], resolving every import the way
/// the TypeScript toolchain does — tsconfig `paths`, `exports` maps,
/// workspace members — and treating an import that resolves to no
/// analysed file as external.
pub fn extract_call_shapes_with_imports(
    source: &str,
    dialect: Dialect,
    module: &str,
    imports: &ImportContext<'_>,
) -> Result<Vec<CallShape>, TsParseError> {
    extract_call_shapes(source, dialect, module, Some(imports))
}

fn extract_call_shapes(
    source: &str,
    dialect: Dialect,
    module: &str,
    context: Option<&ImportContext<'_>>,
) -> Result<Vec<CallShape>, TsParseError> {
    let alloc = Allocator::default();
    let ret = dialect.parse(&alloc, source);
    if !ret.diagnostics.is_empty() {
        return Err(TsParseError::from_diagnostics(
            ret.diagnostics
                .iter()
                .map(|e| e.message.as_ref().to_owned()),
        ));
    }

    let line_index = LineIndex::new(source);
    // Reference binding only: no AST node table, no syntax re-check.
    let semantic = SemanticBuilder::new()
        .with_build_nodes(false)
        .build(&ret.program)
        .semantic;
    let bindings = collect_imports(&ret.program, module, context);
    let imports = bindings.iter().filter_map(ImportBinding::shape).collect();
    let namespace_aliases = bindings
        .iter()
        .filter(|binding| {
            binding.name != ImportedName::Named && matches!(binding.module, Imported::Workspace(_))
        })
        .map(|binding| binding.local.clone())
        .collect();
    let mut dynamic = DynamicImportCollector {
        module,
        context,
        out: Vec::new(),
    };
    dynamic.visit_program(&ret.program);
    let mut collector = CallShapeCollector {
        module: module.to_owned(),
        line_index: &line_index,
        scoping: semantic.scoping(),
        imports,
        bindings: bindings
            .into_iter()
            .chain(dynamic.out)
            .filter_map(|binding| Some((binding.symbol?, binding)))
            .collect(),
        namespace_aliases,
        out: Vec::new(),
    };
    walk_program(&ret.program, &line_index, &mut collector);
    Ok(collector.out)
}

struct FunctionShapeCollector {
    module: String,
    out: Vec<FunctionShape>,
    jsdoc_by_attach: std::collections::HashMap<u32, String>,
}

impl FunctionVisitor for FunctionShapeCollector {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        let (owner, display_name) = split_owner(&item.name);
        let qualified_name = qualify_module(&self.module, &item.name);
        let doc = item
            .doc_attach_start
            .and_then(|attach| self.jsdoc_by_attach.get(&attach).cloned());
        self.out.push(FunctionShape {
            display_name,
            qualified_name: SyntaxFact::Known(qualified_name),
            module_path: SyntaxFact::Known(self.module.clone()),
            owner: SyntaxFact::Known(owner.map(|owner| OwnerShape {
                display_name: owner,
                kind: OwnerKind::Class,
            })),
            // Export status is not extracted yet, so stay honest with
            // `Unknown` instead of hardcoding `Unexported`.
            visibility: SyntaxFact::Unknown,
            signature: SyntaxFact::Known(parameter_signature(item.params)),
            doc,
            // Decorators are not extracted yet; `Unknown` keeps a
            // framework-registered function from reading as unannotated.
            attributes: SyntaxFact::Unknown,
            body: BodyShape {
                tree: function_body_tree(item.body),
            },
            span: SourceSpan {
                start_line: item.start_line,
                end_line: item.end_line,
            },
            // Syntactic evidence only: a `describe`/`it` callback or an
            // xUnit-style name. The graph ORs this with the file's path,
            // so a test that lives in a conventionally named file is
            // still marked when its name says nothing.
            is_test: is_test_item(&item.name),
        });
    }
}

/// Project the declared parameter list into a [`SignatureShape`] that
/// carries one slot per parameter, positionally: a destructuring
/// pattern (`{a, b}`) has no single binding name and stays `None`
/// rather than expanding into several misaligned slots. TS/JS have no
/// syntactic receiver, so [`ReceiverShape::None`] is always correct.
/// Every other signature fact stays [`SyntaxFact::Unknown`] rather
/// than being half-extracted here.
fn parameter_signature(params: &FormalParameters) -> SignatureShape {
    let slot = |pattern: &BindingPattern| ParameterShape {
        name: SyntaxFact::Known(
            pattern
                .get_binding_identifier()
                .map(|id| id.name.to_string()),
        ),
        type_annotation: SyntaxFact::Unknown,
        type_paths: Vec::new(),
    };
    let mut slots: Vec<ParameterShape> = params.items.iter().map(|p| slot(&p.pattern)).collect();
    if let Some(rest) = &params.rest {
        slots.push(slot(&rest.rest.argument));
    }
    SignatureShape {
        name_tokens: SyntaxFact::Unknown,
        params: slots,
        return_type: SyntaxFact::Unknown,
        return_type_paths: Vec::new(),
        receiver: SyntaxFact::Known(ReceiverShape::None),
        generics: SyntaxFact::Unknown,
        bounds: SyntaxFact::Unknown,
    }
}

struct CallShapeCollector<'a> {
    module: String,
    line_index: &'a LineIndex,
    scoping: &'a Scoping,
    imports: Vec<ImportShape>,
    /// Import bindings by the symbol they declare: static imports, and
    /// locals destructured from a dynamic `import()`.
    bindings: HashMap<SymbolId, ImportBinding>,
    namespace_aliases: HashSet<String>,
    out: Vec<CallShape>,
}

impl FunctionVisitor for CallShapeCollector<'_> {
    fn on_function(&mut self, item: FunctionItem<'_>) {
        let (owner, _) = split_owner(&item.name);
        let caller = qualify_module(&self.module, &item.name);
        let mut visitor = FunctionBodyCallVisitor {
            collector: self,
            caller_qualified_name: caller,
            caller_owner: owner,
            out: Vec::new(),
        };
        visitor.visit_function_body(item.body);
        let calls = visitor.out;
        self.out.extend(calls);
    }
}

struct FunctionBodyCallVisitor<'c, 'a> {
    collector: &'c CallShapeCollector<'a>,
    caller_qualified_name: String,
    caller_owner: Option<String>,
    out: Vec<CallShape>,
}

impl<'a> Visit<'a> for FunctionBodyCallVisitor<'_, '_> {
    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        let arguments = it.arguments.iter().map(argument_shape).collect();
        let line = self.collector.line_index.line(it.span.start);
        let shape = self.call_shape(&it.callee, arguments, line);
        self.out.push(shape);
        walk::walk_call_expression(self, it);
    }

    // A call site belongs to exactly one function: its nearest enclosing
    // one. The walker already emits each nested function as its own
    // `<parent>::closure#N` unit, so stop the descent here rather than
    // re-attributing a callback's calls to the function that registered
    // it (`el.onclick = () => help()` is a call by the closure, not by
    // the enclosing function).
    fn visit_function(&mut self, _it: &Function<'a>, _flags: ScopeFlags) {}

    fn visit_arrow_function_expression(&mut self, _it: &ArrowFunctionExpression<'a>) {}
}

/// What a callee's leading identifier refers to, per `oxc_semantic`.
enum Referent<'b> {
    /// Declared in a function, block, or parameter list — any scope
    /// below the module's.
    Local,
    /// Declared at module scope by this file.
    ModuleScope,
    /// An import binding.
    Import(&'b ImportBinding),
    /// Declared nowhere in the file: a global.
    Global,
}

impl FunctionBodyCallVisitor<'_, '_> {
    /// The visitor already owns every caller-side fact a call shape
    /// needs, so this reads them off `self` instead of taking them as a
    /// parameter list.
    fn call_shape(
        &self,
        callee_expr: &Expression,
        arguments: Vec<ArgumentShape>,
        line: usize,
    ) -> CallShape {
        let collector = self.collector;
        let callee = callee_facts(callee_expr, &collector.namespace_aliases);
        let referent = root_identifier(callee_expr).and_then(|root| self.referent(root));
        // A local binding shadows every definition outside the function,
        // but only a bare call names the binding itself: `local.run()`
        // names a method on whatever the local holds.
        let bare = matches!(callee.path_segments.as_deref(), Some([_]));
        let callee_is_locally_bound = bare && matches!(referent, Some(Referent::Local));
        let callee_binding = match (&referent, callee.path_segments.as_deref()) {
            (Some(referent), Some(segments)) => self.binding(referent, segments),
            _ => SyntaxFact::Unknown,
        };
        CallShape {
            caller_qualified_name: SyntaxFact::Known(Some(self.caller_qualified_name.clone())),
            caller_module: SyntaxFact::Known(collector.module.clone()),
            caller_owner: SyntaxFact::Known(self.caller_owner.clone()),
            callee_display_name: SyntaxFact::Known(callee.name),
            callee_path_segments: callee
                .path_segments
                .map_or(SyntaxFact::Unknown, SyntaxFact::Known),
            receiver_expr_kind: SyntaxFact::Known(callee.receiver),
            arguments: SyntaxFact::Known(arguments),
            callee_is_locally_bound: SyntaxFact::Known(callee_is_locally_bound),
            callee_binding,
            lexical_resolution: LexicalResolutionStatus::NotAttempted,
            visible_imports: collector.imports.clone(),
            line,
        }
    }

    fn referent(&self, root: &IdentifierReference) -> Option<Referent<'_>> {
        let scoping = self.collector.scoping;
        let reference = scoping.get_reference(root.reference_id.get()?);
        let Some(symbol) = reference.symbol_id() else {
            return Some(Referent::Global);
        };
        if let Some(import) = self.collector.bindings.get(&symbol) {
            return Some(Referent::Import(import));
        }
        if scoping.symbol_scope_id(symbol) != scoping.root_scope_id() {
            return Some(Referent::Local);
        }
        // An import form not collected (`import x = require(...)`).
        if scoping.symbol_flags(symbol).is_import() {
            return None;
        }
        Some(Referent::ModuleScope)
    }

    /// The declaration `segments` (the callee path, its first segment
    /// being the bound identifier) names, when the binding settles it.
    fn binding(&self, referent: &Referent<'_>, segments: &[String]) -> SyntaxFact<CalleeBinding> {
        let rest = segments[1..].join("::");
        let declaration = |target: String| SyntaxFact::Known(CalleeBinding::Declaration(target));
        match referent {
            Referent::Local => SyntaxFact::Unknown,
            Referent::Global => SyntaxFact::Known(CalleeBinding::External),
            Referent::ModuleScope => {
                declaration(qualify_module(&self.collector.module, &segments.join("::")))
            }
            Referent::Import(import) => match (&import.module, import.name) {
                (Imported::External, _) => SyntaxFact::Known(CalleeBinding::External),
                (Imported::Workspace(module), ImportedName::Named) => {
                    let exported = import.exported.as_deref().unwrap_or_default();
                    let target = qualify_module(module, exported);
                    declaration(if rest.is_empty() {
                        target
                    } else {
                        qualify_module(&target, &rest)
                    })
                }
                // `ns.helper()` names `helper` in the module; `ns()` names
                // the module object, which no function node is.
                (Imported::Workspace(module), ImportedName::Namespace) if !rest.is_empty() => {
                    declaration(qualify_module(module, &rest))
                }
                // A default export's node is named after its declaration,
                // which the importer does not know.
                (Imported::Workspace(_) | Imported::Unknown, _) => SyntaxFact::Unknown,
            },
        }
    }
}

/// The identifier a callee path starts at: `helper` in `helper()`,
/// `api` in `api.users.list()`. `None` for `this.x()`, `f()()`, and
/// other callees not rooted at a name.
fn root_identifier<'b>(callee: &'b Expression<'b>) -> Option<&'b IdentifierReference<'b>> {
    match callee {
        Expression::Identifier(id) => Some(id),
        Expression::StaticMemberExpression(member) => root_identifier(&member.object),
        Expression::ParenthesizedExpression(expr) => root_identifier(&expr.expression),
        _ => None,
    }
}

/// Classify one call argument. Only literals — and `undefined`, which
/// is an identifier syntactically but a value nothing sane shadows —
/// can claim "the same text is the same value"; an uppercase-initial
/// name or member chain (`Color.Red`, `DEFAULTS`) gets the weaker
/// [`ArgumentShape::Const`] on the same naming convention the callee
/// classifier above already trusts. TS-only wrappers (`x as T`, `x!`,
/// parens) are peeled first.
fn argument_shape(arg: &Argument) -> ArgumentShape {
    match arg {
        Argument::SpreadElement(_) => ArgumentShape::Spread,
        _ => arg
            .as_expression()
            .map_or(ArgumentShape::Other, expression_argument_shape),
    }
}

fn expression_argument_shape(expr: &Expression) -> ArgumentShape {
    let literal = |text: String| ArgumentShape::Literal { text };
    match expr {
        Expression::BooleanLiteral(lit) => literal(lit.value.to_string()),
        Expression::NullLiteral(_) => literal("null".to_owned()),
        Expression::NumericLiteral(lit) => literal(
            lit.raw
                .as_ref()
                .map_or_else(|| lit.value.to_string(), ToString::to_string),
        ),
        Expression::StringLiteral(lit) => literal(format!("\"{}\"", lit.value)),
        Expression::TemplateLiteral(template) if template.expressions.is_empty() => {
            match template
                .quasis
                .first()
                .and_then(|q| q.value.cooked.as_ref())
            {
                Some(text) => literal(format!("\"{text}\"")),
                None => ArgumentShape::Other,
            }
        }
        Expression::Identifier(id) if id.name == "undefined" => literal("undefined".to_owned()),
        Expression::Identifier(id) => {
            let text = id.name.to_string();
            if starts_uppercase(&text) {
                ArgumentShape::Const { text }
            } else {
                ArgumentShape::Identifier { text }
            }
        }
        Expression::UnaryExpression(unary) if unary.operator == UnaryOperator::UnaryNegation => {
            match expression_argument_shape(&unary.argument) {
                ArgumentShape::Literal { text } => literal(format!("-{text}")),
                _ => ArgumentShape::Other,
            }
        }
        Expression::StaticMemberExpression(_) => match expression_path(expr) {
            Some(segments) if segments.first().is_some_and(|s| starts_uppercase(s)) => {
                ArgumentShape::Const {
                    text: segments.join("."),
                }
            }
            _ => ArgumentShape::Other,
        },
        Expression::ParenthesizedExpression(inner) => expression_argument_shape(&inner.expression),
        Expression::TSAsExpression(inner) => expression_argument_shape(&inner.expression),
        Expression::TSNonNullExpression(inner) => expression_argument_shape(&inner.expression),
        Expression::TSSatisfiesExpression(inner) => expression_argument_shape(&inner.expression),
        _ => ArgumentShape::Other,
    }
}

struct CalleeFacts {
    name: Option<String>,
    path_segments: Option<Vec<String>>,
    receiver: ReceiverExprKind,
}

fn callee_facts(callee: &Expression, namespace_aliases: &HashSet<String>) -> CalleeFacts {
    match callee {
        Expression::Identifier(id) => CalleeFacts {
            name: Some(id.name.to_string()),
            path_segments: Some(vec![id.name.to_string()]),
            receiver: ReceiverExprKind::None,
        },
        Expression::StaticMemberExpression(member) => {
            let mut segments = expression_path(&member.object).unwrap_or_default();
            segments.push(member.property.name.to_string());
            let receiver = if matches!(member.object, Expression::ThisExpression(_)) {
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
                name: Some(member.property.name.to_string()),
                path_segments: (!segments.is_empty()).then_some(segments),
                receiver,
            }
        }
        Expression::ParenthesizedExpression(expr) => {
            callee_facts(&expr.expression, namespace_aliases)
        }
        _ => CalleeFacts {
            name: None,
            path_segments: None,
            receiver: ReceiverExprKind::Expression,
        },
    }
}

fn expression_path(expr: &Expression) -> Option<Vec<String>> {
    match expr {
        Expression::Identifier(id) => Some(vec![id.name.to_string()]),
        Expression::StaticMemberExpression(member) => {
            let mut segments = expression_path(&member.object)?;
            segments.push(member.property.name.to_string());
            Some(segments)
        }
        Expression::ParenthesizedExpression(expr) => expression_path(&expr.expression),
        _ => None,
    }
}

/// Where an import's specifier leads.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Imported {
    /// A module of the analysis, by its module path.
    Workspace(String),
    /// A package or file outside the analysis.
    External,
    /// Not settled: no [`ImportContext`] to resolve a bare specifier, or
    /// a relative one whose target is missing.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImportedName {
    Named,
    Default,
    Namespace,
}

/// One local name an `import` declaration, or a destructured dynamic
/// `import()`, binds.
#[derive(Debug, Clone)]
struct ImportBinding {
    local: String,
    symbol: Option<SymbolId>,
    module: Imported,
    name: ImportedName,
    /// The exported name, for [`ImportedName::Named`].
    exported: Option<String>,
}

impl ImportBinding {
    /// The lexical import fact, for an import into the workspace.
    fn shape(&self) -> Option<ImportShape> {
        let Imported::Workspace(module) = &self.module else {
            return None;
        };
        let target = self
            .exported
            .as_deref()
            .map_or_else(|| module.clone(), |name| qualify_module(module, name));
        Some(import_shape(
            Some(self.local.clone()),
            target,
            self.exported.clone(),
        ))
    }
}

fn collect_imports(
    program: &Program,
    module: &str,
    context: Option<&ImportContext<'_>>,
) -> Vec<ImportBinding> {
    let mut out = Vec::new();
    for stmt in &program.body {
        let Statement::ImportDeclaration(import) = stmt else {
            continue;
        };
        let Some(specifiers) = &import.specifiers else {
            continue;
        };
        let imported = imported_module(module, import.source.value.as_str(), context);
        for specifier in specifiers {
            let (local, name, exported) = match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(specifier) => (
                    &specifier.local,
                    ImportedName::Named,
                    module_export_name(&specifier.imported),
                ),
                ImportDeclarationSpecifier::ImportDefaultSpecifier(specifier) => {
                    (&specifier.local, ImportedName::Default, None)
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(specifier) => {
                    (&specifier.local, ImportedName::Namespace, None)
                }
            };
            out.push(ImportBinding {
                local: local.name.to_string(),
                symbol: local.symbol_id.get(),
                module: imported.clone(),
                name,
                exported,
            });
        }
    }
    out
}

/// Locals bound from a dynamic import: `const { a, b: c } = await
/// import("./m")` binds `a` and `c` as named imports of `./m`, and
/// `const m = await import("./m")` binds `m` as its namespace. Only the
/// declaring symbol is recorded, so such a local binds the import in its
/// own scope and nowhere else.
struct DynamicImportCollector<'m, 'c> {
    module: &'m str,
    context: Option<&'c ImportContext<'c>>,
    out: Vec<ImportBinding>,
}

impl<'a> Visit<'a> for DynamicImportCollector<'_, '_> {
    fn visit_variable_declarator(&mut self, it: &VariableDeclarator<'a>) {
        if let Some(specifier) = it.init.as_ref().and_then(dynamic_import_specifier) {
            let module = imported_module(self.module, specifier, self.context);
            let mut bind = |local: &BindingIdentifier, name, exported| {
                self.out.push(ImportBinding {
                    local: local.name.to_string(),
                    symbol: local.symbol_id.get(),
                    module: module.clone(),
                    name,
                    exported,
                });
            };
            match &it.id {
                BindingPattern::BindingIdentifier(id) => bind(id, ImportedName::Namespace, None),
                BindingPattern::ObjectPattern(pattern) => {
                    for property in &pattern.properties {
                        if let (Some(key), Some(local)) = (
                            property.key.static_name(),
                            property.value.get_binding_identifier(),
                        ) && !property.computed
                        {
                            bind(local, ImportedName::Named, Some(key.to_string()));
                        }
                    }
                }
                _ => {}
            }
        }
        walk::walk_variable_declarator(self, it);
    }
}

/// The static specifier of `import("…")` or `await import("…")`.
fn dynamic_import_specifier<'b>(init: &'b Expression<'b>) -> Option<&'b str> {
    match init.without_parentheses() {
        Expression::AwaitExpression(inner) => dynamic_import_specifier(&inner.argument),
        Expression::ImportExpression(import) => {
            crate::coupling::static_string_value(&import.source)
        }
        _ => None,
    }
}

/// Settle where `specifier`, imported by `module`, leads. With a context
/// the resolver decides, and a specifier it cannot map onto an analysed
/// file is external; a relative one it cannot find falls back to the
/// lexical guess, as without a context.
fn imported_module(module: &str, specifier: &str, context: Option<&ImportContext<'_>>) -> Imported {
    if let Some(context) = context {
        if let Some(target) = context
            .resolver
            .resolve(context.file, specifier)
            .and_then(|file| (context.module_of)(&file))
        {
            return Imported::Workspace(target);
        }
        if !is_relative_specifier(specifier) {
            return Imported::External;
        }
    }
    resolve_import_module(module, specifier).map_or(Imported::Unknown, Imported::Workspace)
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

fn module_export_name(name: &ModuleExportName) -> Option<String> {
    match name {
        ModuleExportName::IdentifierName(id) => Some(id.name.to_string()),
        ModuleExportName::IdentifierReference(id) => Some(id.name.to_string()),
        ModuleExportName::StringLiteral(s) => Some(s.value.to_string()),
    }
}

fn resolve_import_module(module: &str, source: &str) -> Option<String> {
    if !source.starts_with('.') {
        return None;
    }
    let mut segments = module
        .split("::")
        .filter(|segment| !segment.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    segments.pop();
    for raw in source.split('/') {
        match raw {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(strip_ts_extension(segment).to_owned()),
        }
    }
    // `./geo/index` is the module `./geo` names: a directory's
    // `index.ts` takes the directory's module path (see
    // `module_path::module_segments`), so the import must too.
    if segments.last().is_some_and(|last| last == "index") {
        segments.pop();
    }
    (!segments.is_empty()).then(|| segments.join("::"))
}

fn strip_ts_extension(segment: &str) -> &str {
    segment
        .rsplit_once('.')
        .filter(|(_, ext)| MODULE_EXTENSIONS.contains(ext))
        .map_or(segment, |(stem, _)| stem)
}

fn split_owner(name: &str) -> (Option<String>, String) {
    name.rsplit_once("::").map_or_else(
        || (None, name.to_owned()),
        |(owner, name)| (Some(owner.to_owned()), name.to_owned()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    /// A bare callee bound anywhere below module scope — a closure, a
    /// nested `function`, a parameter, any local — is shadowed: the
    /// resolver must be told so it does not fall back to a same-named
    /// function exported elsewhere. The walker names such a closure
    /// `pump::closure#N`, never `emit`, so nothing legitimate resolves.
    #[rstest]
    #[case::arrow_local("function pump() { const emit = (e: number) => {}; emit(1); }", true)]
    #[case::function_expression_local(
        "function pump() { const emit = function () {}; emit(1); }",
        true
    )]
    #[case::let_arrow_local("function pump() { let emit = () => {}; emit(1); }", true)]
    #[case::nested_declaration("function pump() { function emit() {} emit(1); }", true)]
    #[case::function_typed_param("function pump(emit: (e: number) => void) { emit(1); }", true)]
    #[case::defaulted_param("function pump(emit = () => {}) { emit(1); }", true)]
    #[case::binding_in_nested_block(
        "function pump(flag: boolean) { if (flag) { const emit = () => {}; emit(1); } }",
        true
    )]
    // Scope binding, not syntax, decides: whatever the local holds, it is
    // not a workspace declaration named `emit`.
    #[case::plain_local("function pump() { const emit = compute(); emit(1); }", true)]
    #[case::untyped_param("function pump(emit) { emit(1); }", true)]
    #[case::unbound_name("function pump() { emit(1); }", false)]
    #[case::module_scope("const emit = () => {}; function pump() { emit(1); }", false)]
    fn local_callable_bindings_shadow_bare_calls(#[case] source: &str, #[case] expected: bool) {
        let call = extract_call_shapes_with_module(source, Dialect::Ts, "src::m")
            .unwrap()
            .into_iter()
            .find(|call| {
                call.callee_name() == Some("emit")
                    && call.caller_qualified_name() == Some("src::m::pump")
            })
            .expect("emit call site in pump");
        assert_eq!(call.callee_is_locally_bound(), expected);
    }

    fn binding_of(source: &str, callee: &str) -> SyntaxFact<CalleeBinding> {
        extract_call_shapes_with_module(source, Dialect::Ts, "app::main")
            .unwrap()
            .into_iter()
            .find(|call| call.callee_path().as_deref() == Some(callee))
            .unwrap_or_else(|| panic!("no call to {callee}"))
            .callee_binding
    }

    /// `oxc_semantic` settles the callee's leading name: a module-scope
    /// declaration, a relative import (aliased, namespace, or through a
    /// class), or a global. Without an [`ImportContext`] a bare import
    /// and a default import stay unsettled.
    #[rstest]
    #[case::module_function(
        "function helper() {} function f() { helper(); }",
        "helper",
        Some("app::main::helper")
    )]
    #[case::module_class_static(
        "class Api { static create() {} } function f() { Api.create(); }",
        "Api::create",
        Some("app::main::Api::create")
    )]
    #[case::aliased_import(
        "import { helper as h } from './util'; function f() { h(); }",
        "h",
        Some("app::util::helper")
    )]
    #[case::namespace_import(
        "import * as util from './util'; function f() { util.helper(); }",
        "util::helper",
        Some("app::util::helper")
    )]
    #[case::imported_class(
        "import { Api } from '../api'; function f() { Api.create(); }",
        "Api::create",
        Some("api::Api::create")
    )]
    #[case::dynamic_import_destructured(
        "async function f() { const { helper: h } = await import('./util'); h(); }",
        "h",
        Some("app::util::helper")
    )]
    #[case::dynamic_import_namespace(
        "async function f() { const util = await import('./util'); util.helper(); }",
        "util::helper",
        Some("app::util::helper")
    )]
    #[case::parenthesized_callee(
        "function helper() {} function f() { (helper)(); }",
        "helper",
        Some("app::main::helper")
    )]
    // Calling the namespace object itself names no function.
    #[case::namespace_called_directly(
        "import * as util from './util'; function f() { util(); }",
        "util",
        None
    )]
    #[case::default_import("import h from './util'; function f() { h(); }", "h", None)]
    #[case::bare_import_without_context(
        "import { h } from 'lib'; function f() { h(); }",
        "h",
        None
    )]
    #[case::local("function f() { const h = () => {}; h(); }", "h", None)]
    fn callee_bindings(#[case] source: &str, #[case] callee: &str, #[case] expected: Option<&str>) {
        let expected = expected.map_or(SyntaxFact::Unknown, |target| {
            SyntaxFact::Known(CalleeBinding::Declaration(target.to_owned()))
        });
        assert_eq!(binding_of(source, callee), expected);
    }

    /// Only a default or namespace import into the workspace is a
    /// namespace alias, making `alias.member()` a path call; a named
    /// import's `.member()` is a call on a value, and an unsettled
    /// namespace is no known module.
    #[rstest]
    #[case::namespace("import * as api from './api'; function f() { api.get(); }", false)]
    #[case::default("import api from './api'; function f() { api.get(); }", false)]
    #[case::named_value("import { api } from './api'; function f() { api.get(); }", true)]
    #[case::unsettled_namespace("import * as api from 'lib'; function f() { api.get(); }", true)]
    fn namespace_aliases_are_workspace_default_or_namespace_imports(
        #[case] source: &str,
        #[case] receiver_call: bool,
    ) {
        let call = extract_call_shapes_with_module(source, Dialect::Ts, "app::main")
            .unwrap()
            .into_iter()
            .find(|call| call.callee_name() == Some("get"))
            .expect("get call");
        assert_eq!(call.has_receiver_expression(), receiver_call);
    }

    #[rstest]
    #[case::global_function("function f() { fetch('/'); }", "fetch")]
    #[case::global_object("function f() { JSON.parse('1'); }", "JSON::parse")]
    #[case::test_global("function f() { expect(1); }", "expect")]
    fn globals_bind_external(#[case] source: &str, #[case] callee: &str) {
        assert_eq!(
            binding_of(source, callee),
            SyntaxFact::Known(CalleeBinding::External)
        );
    }

    /// With an [`ImportContext`], imports resolve through tsconfig paths
    /// and workspace members, and an import that lands on no analysed
    /// file is external.
    #[test]
    fn import_context_resolves_aliases_and_marks_packages_external() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for (rel, content) in [
            (
                "tsconfig.json",
                r#"{"compilerOptions": {"paths": {"@/*": ["./src/*"]}}}"#,
            ),
            ("src/main.ts", ""),
            ("src/lib/db.ts", "export function query() {}"),
        ] {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let resolver = ModuleResolver::new(root);
        let module_of = |file: &Path| {
            file.strip_prefix(root)
                .ok()
                .map(|rel| crate::module_segments(rel).join("::"))
        };
        let file = root.join("src/main.ts");
        let context = ImportContext {
            file: &file,
            resolver: &resolver,
            module_of: &module_of,
        };
        let source = "import { query } from '@/lib/db';\n\
                      import { useState } from 'react';\n\
                      import { gone } from './missing';\n\
                      import { lost } from '../missing';\n\
                      function f() { query(); useState(); gone(); lost(); }\n";
        let calls =
            extract_call_shapes_with_imports(source, Dialect::Ts, "src::main", &context).unwrap();
        let binding = |name: &str| {
            calls
                .iter()
                .find(|call| call.callee_name() == Some(name))
                .map(|call| call.callee_binding.clone())
        };
        assert_eq!(
            binding("query"),
            Some(SyntaxFact::Known(CalleeBinding::Declaration(
                "src::lib::db::query".to_owned()
            )))
        );
        assert_eq!(
            binding("useState"),
            Some(SyntaxFact::Known(CalleeBinding::External))
        );
        // A relative import the resolver cannot find is not external: it
        // keeps the lexical guess, as without a context.
        for (callee, target) in [("gone", "src::missing::gone"), ("lost", "missing::lost")] {
            assert_eq!(
                binding(callee),
                Some(SyntaxFact::Known(CalleeBinding::Declaration(
                    target.to_owned()
                ))),
                "{callee}",
            );
        }
        let imports = &calls[0].visible_imports;
        assert_eq!(
            imports[0],
            ImportShape::known(
                "src::lib::db::query".to_owned(),
                Some("query".to_owned()),
                Some("query".to_owned()),
            ),
        );
        assert!(
            !imports
                .iter()
                .any(|import| import.local_alias == SyntaxFact::Known(Some("useState".to_owned()))),
            "an external import is no lexical import fact",
        );
    }

    /// A binding in one function does not shadow the same name in
    /// another, and a top-level `const emit = () => {}` is a real module
    /// node that must keep resolving.
    #[test]
    fn bindings_do_not_leak_across_functions() {
        let source = concat!(
            "const emit = (e: number) => {};\n",
            "function pump() { const emit = () => {}; emit(1); }\n",
            "function drain() { emit(2); }\n",
        );
        let flags: Vec<_> = extract_call_shapes_with_module(source, Dialect::Ts, "src::m")
            .unwrap()
            .into_iter()
            .filter(|call| call.callee_name() == Some("emit"))
            .map(|call| {
                (
                    call.caller_qualified_name().map(ToOwned::to_owned),
                    call.callee_is_locally_bound(),
                )
            })
            .collect();
        assert_eq!(
            flags,
            [
                (Some("src::m::pump".to_owned()), true),
                (Some("src::m::drain".to_owned()), false),
            ]
        );
    }

    /// A closure's own parameters bind in the closure, which the walker
    /// emits as its own unit — they must not shadow the parent's calls.
    #[test]
    fn closure_parameters_do_not_shadow_the_enclosing_scope() {
        let source = "function pump() { run((emit: () => void) => emit()); emit(); }";
        let call = extract_call_shapes_with_module(source, Dialect::Ts, "src::m")
            .unwrap()
            .into_iter()
            .find(|call| {
                call.callee_name() == Some("emit")
                    && call.caller_qualified_name() == Some("src::m::pump")
            })
            .expect("emit call site in pump");
        assert!(!call.callee_is_locally_bound());
    }

    /// The non-constant boundary of the classifier: a template with
    /// substitutions, negation of anything but a literal, and a member
    /// chain on a lowercase local stay opaque, while parens and the
    /// TS-only wrappers peel to the value inside.
    #[rstest]
    #[case::template_with_substitution(
        "function a(x: number) { f(`v${x}`); }",
        ArgumentShape::Other
    )]
    #[case::negated_identifier("function a(x: number) { f(-x); }", ArgumentShape::Other)]
    #[case::not_a_negation("function a() { f(!0); }", ArgumentShape::Other)]
    #[case::lowercase_member("function a(obj: T) { f(obj.prop); }", ArgumentShape::Other)]
    #[case::parenthesised(
        "function a(x: number) { f((x)); }",
        ArgumentShape::Identifier { text: "x".to_owned() }
    )]
    #[case::as_cast(
        "function a(x: number) { f(x as unknown); }",
        ArgumentShape::Identifier { text: "x".to_owned() }
    )]
    #[case::non_null(
        "function a(x?: number) { f(x!); }",
        ArgumentShape::Identifier { text: "x".to_owned() }
    )]
    #[case::satisfies(
        "function a(x: number) { f(x satisfies number); }",
        ArgumentShape::Identifier { text: "x".to_owned() }
    )]
    fn argument_shape_edge_cases(#[case] source: &str, #[case] expected: ArgumentShape) {
        let call = extract_call_shapes_with_module(source, Dialect::Ts, "src::m")
            .unwrap()
            .into_iter()
            .find(|call| call.callee_name() == Some("f"))
            .expect("f call site");
        assert_eq!(
            call.arguments.known_value().cloned().expect("known"),
            vec![expected],
        );
    }

    /// Argument shapes: literals (with `undefined` and a plain template
    /// string) carry text, uppercase-initial names and member chains are
    /// consts, lowercase identifiers stay identifiers, spreads and
    /// arbitrary expressions are opaque.
    #[test]
    fn call_arguments_are_classified_by_shape() {
        let source = "function pump(x: number, xs: number[]) {\n\
             f(1, -2, \"s\", `t`, true, null, undefined, Color.Red, MAX, x, g(), ...xs);\n\
             }\n";
        let call = extract_call_shapes_with_module(source, Dialect::Ts, "src::m")
            .unwrap()
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
                ArgumentShape::Literal {
                    text: text("\"t\"")
                },
                ArgumentShape::Literal { text: text("true") },
                ArgumentShape::Literal { text: text("null") },
                ArgumentShape::Literal {
                    text: text("undefined")
                },
                ArgumentShape::Const {
                    text: text("Color.Red")
                },
                ArgumentShape::Const { text: text("MAX") },
                ArgumentShape::Identifier { text: text("x") },
                ArgumentShape::Other,
                ArgumentShape::Spread,
            ],
        );
    }

    /// The function-shape signature carries one slot per declared
    /// parameter, positionally: a destructuring pattern is a nameless
    /// slot rather than several misaligned ones, and a rest parameter
    /// is a slot of its own.
    #[test]
    fn function_shapes_carry_positional_parameter_slots() {
        let source = "function pump(a: number, {b, c}: Opts, ...rest: number[]) {}\n";
        let functions = extract_function_shapes_with_module(source, Dialect::Ts, "src::m").unwrap();
        let signature = functions[0].signature_shape().expect("signature extracted");
        let names: Vec<Option<&str>> = signature
            .params
            .iter()
            .map(|p| {
                p.name
                    .known_value()
                    .and_then(Option::as_ref)
                    .map(String::as_str)
            })
            .collect();
        assert_eq!(names, [Some("a"), None, Some("rest")]);
    }

    #[test]
    fn extracts_functions_with_module_qualified_names() {
        let source = "class Service { run() { helper(); } }\nfunction helper() {}\n";

        let functions =
            extract_function_shapes_with_module(source, Dialect::Ts, "src::service").unwrap();

        assert_eq!(functions[0].display_name, "run");
        assert_eq!(
            functions[0]
                .qualified_name
                .known_value()
                .map(String::as_str),
            Some("src::service::Service::run"),
        );
        assert_eq!(functions[1].display_name, "helper");
        assert_eq!(
            functions[1]
                .qualified_name
                .known_value()
                .map(String::as_str),
            Some("src::service::helper"),
        );
    }

    #[test]
    fn extracts_bare_and_imported_call_shapes() {
        let source = "import { helper } from './helper';\nfunction caller() { helper(); }\n";

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].caller_qualified_name(), Some("src::main::caller"));
        assert_eq!(calls[0].callee_name(), Some("helper"));
        assert_eq!(
            calls[0].visible_imports[0]
                .imported_module
                .known_value()
                .map(String::as_str),
            Some("src::helper::helper"),
        );
    }

    /// A directory's `index.ts` has the directory's module path, so an
    /// import spelling the file out must land on that same module.
    /// Regression for #579: `'./geo/index'` resolved to `src::geo::index`,
    /// which no node carries.
    #[rstest]
    #[case::directory("./geo", "src::geo")]
    #[case::index_file("./geo/index", "src::geo")]
    #[case::index_file_with_extension("./geo/index.ts", "src::geo")]
    #[case::sibling_index("./index", "src")]
    #[case::index_named_directory("./index/geo", "src::index::geo")]
    fn index_file_imports_resolve_to_the_directory_module(
        #[case] specifier: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(
            resolve_import_module("src::main", specifier).as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn namespace_import_member_calls_are_path_calls() {
        let source =
            "import * as graph from '../graph';\nfunction caller() { graph.createGraphView(); }\n";

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "routes::index").unwrap();

        assert_eq!(calls[0].callee_name(), Some("createGraphView"));
        assert_eq!(
            calls[0].callee_path().as_deref(),
            Some("graph::createGraphView"),
        );
        assert!(!calls[0].has_receiver_expression());
        assert_eq!(
            calls[0].visible_imports[0]
                .imported_module
                .known_value()
                .map(String::as_str),
            Some("graph"),
        );
    }

    #[test]
    fn parenthesized_bare_callees_keep_the_inner_name() {
        let source = "function caller() { (helper)(); }\nfunction helper() {}\n";

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();

        assert_eq!(calls[0].callee_name(), Some("helper"));
        assert_eq!(calls[0].callee_path().as_deref(), Some("helper"));
        assert!(!calls[0].has_receiver_expression());
    }

    #[test]
    fn parenthesized_static_member_objects_keep_the_object_path() {
        let source = "function caller() { (Api).create(); }\n";

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();

        assert_eq!(calls[0].callee_name(), Some("create"));
        assert_eq!(calls[0].callee_path().as_deref(), Some("Api::create"));
        assert!(!calls[0].has_receiver_expression());
    }

    #[test]
    fn nested_static_member_paths_are_preserved() {
        let source = "function caller() { Api.Services.create(); }\n";

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();

        assert_eq!(calls[0].callee_name(), Some("create"));
        assert_eq!(
            calls[0].callee_path().as_deref(),
            Some("Api::Services::create"),
        );
        assert!(!calls[0].has_receiver_expression());
    }

    #[test]
    fn lowercase_member_calls_remain_receiver_calls() {
        let source = "function caller(client) { client.connect(); }\n";

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();

        assert_eq!(calls[0].callee_name(), Some("connect"));
        assert_eq!(calls[0].callee_path().as_deref(), Some("client::connect"));
        assert!(calls[0].has_receiver_expression());
    }

    #[test]
    fn nested_functions_get_their_own_function_shapes() {
        let source = "function setup() { const handler = () => {}; }\n";
        let functions =
            extract_function_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();
        let qualified: Vec<&str> = functions
            .iter()
            .filter_map(|f| f.qualified_name.known_value().map(String::as_str))
            .collect();
        assert!(qualified.contains(&"src::main::setup"), "got {qualified:?}");
        assert!(
            qualified.contains(&"src::main::setup::closure#1"),
            "got {qualified:?}",
        );
    }

    #[test]
    fn calls_inside_a_nested_function_are_owned_by_the_closure() {
        // The `helper()` call is made by the callback, not by the
        // function that defines it: the call shape's caller is the
        // closure, and `setup` itself contributes no calls.
        let source = "function setup() { const run = () => helper(); }\nfunction helper() {}\n";
        let calls = extract_call_shapes_with_module(source, Dialect::Ts, "src::main").unwrap();

        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].callee_name(), Some("helper"));
        assert_eq!(
            calls[0].caller_qualified_name(),
            Some("src::main::setup::closure#1"),
        );
    }

    #[test]
    fn a_test_case_body_owns_the_calls_it_makes() {
        // Every call a vitest suite makes lives in a harness callback. If
        // those bodies are not units, the call graph has no test node to
        // start a reachability walk from — the shape of issue #424.
        let source = "import { checkConsistency } from \"./integrity\";\n\
             describe(\"checkConsistency\", () => {\n\
                 it(\"accepts agreeing numbers\", () => {\n\
                     checkConsistency(counted);\n\
                 });\n\
             });\n";
        let module = "src::integrity_test";

        let functions = extract_function_shapes_with_module(source, Dialect::Ts, module).unwrap();
        let case = functions
            .iter()
            .find(|f| {
                f.qualified_name
                    .known_value()
                    .is_some_and(|name| name.ends_with("it#1(\"accepts agreeing numbers\")"))
            })
            .expect("the case must be its own function shape");
        assert!(case.is_test, "a harness callback is test code");

        let calls = extract_call_shapes_with_module(source, Dialect::Ts, module).unwrap();
        let covered = calls
            .iter()
            .find(|c| c.callee_name() == Some("checkConsistency"))
            .expect("the call under test must be recorded");
        assert_eq!(
            covered.caller_qualified_name(),
            case.qualified_name.known_value().map(String::as_str),
        );
    }
}
