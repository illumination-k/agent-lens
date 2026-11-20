//! Syntax-only static types of Go values, for binding receiver calls.
//!
//! `x.M()` names a method by its last segment only, which the call graph
//! can resolve by name alone when one workspace method is called `M`.
//! Go spells most types out, though: a receiver (`func (s *T)`), a
//! parameter (`p cmp.Path`), a composite literal (`&T{}`), a function's
//! declared result (`func NewFlagSet(...) *FlagSet`), a struct field.
//! This module reads those declarations into [`ValueTypeHint`]s:
//!
//! * per call site, the hint for the receiver value, from the caller's own
//!   scope ([`TypeScope`]);
//! * per file, the package-level declarations a hint may refer to — types
//!   with their fields, function results, package variables
//!   ([`GoTypeFacts`]).
//!
//! A pass that sees every file joins the two (the call graph's
//! `go_types`). Nothing here infers across an assignment chain or a
//! generic instantiation; a name the scope binds to values of two
//! different types gets no hint at all, so a missing fact leaves the call
//! to the name-based resolution it had before.

use std::collections::{HashMap, HashSet};

use lens_domain::{ImportShape, ValueTypeHint, qualify_module};
use tree_sitter::Node;

use crate::call_index::expression_list_children;
use crate::node_text::node_str;
use crate::parser::{GoParseError, parse_tree};

/// Go's predeclared type names: none of them has a method a workspace
/// declares (`error`'s `Error` is the interface's own).
pub(crate) const PREDECLARED_TYPES: &[&str] = &[
    "any",
    "bool",
    "byte",
    "comparable",
    "complex128",
    "complex64",
    "error",
    "float32",
    "float64",
    "int",
    "int16",
    "int32",
    "int64",
    "int8",
    "rune",
    "string",
    "uint",
    "uint16",
    "uint32",
    "uint64",
    "uint8",
    "uintptr",
];

/// What kind of type a `type` declaration introduces, as far as method
/// lookup cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoTypeKind {
    Struct,
    Interface,
    /// A defined type over anything else (`type Values []Value`).
    Other,
}

/// One `type` declaration at package scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoTypeDecl {
    /// `module::Name`.
    pub qualified_name: String,
    pub kind: GoTypeKind,
    /// `type A = B`: the aliased type, whose methods `A` has.
    pub alias_of: Option<ValueTypeHint>,
    /// Every struct field by name, with its type hint where the type
    /// gives one (a func-typed field has none, and still shadows a
    /// promoted method of its name).
    pub fields: Vec<Binding>,
    /// Embedded fields, whose methods and fields are promoted. Each is
    /// also a field named after its type.
    pub embedded: Vec<ValueTypeHint>,
}

/// The package-level declarations of one file that a [`ValueTypeHint`]
/// can refer to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoTypeFacts {
    pub types: Vec<GoTypeDecl>,
    /// `module::f` / `module::T::m` → its declared results, one entry per
    /// result slot (`None` where the type gives no hint).
    pub function_results: Vec<(String, Vec<Option<ValueTypeHint>>)>,
    /// `module::v` → the hint for a package variable.
    pub package_vars: Vec<(String, ValueTypeHint)>,
}

/// Extract the package-level type facts of one Go file.
pub fn extract_type_facts_with_module(
    source: &str,
    module: &str,
) -> Result<GoTypeFacts, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let root = tree.root_node();
    let imports = crate::call_index::collect_imports(root, bytes);
    let mut facts = GoTypeFacts::default();
    let mut cursor = root.walk();
    for decl in root.named_children(&mut cursor) {
        match decl.kind() {
            "type_declaration" => collect_type_decls(decl, bytes, module, &imports, &mut facts),
            "function_declaration" | "method_declaration" => {
                collect_function_results(decl, bytes, module, &imports, &mut facts);
            }
            "var_declaration" => {
                let scope = TypeScope::new(bytes, module, &imports, HashSet::new());
                let mut bindings = Vec::new();
                scope.var_declaration_bindings(decl, &mut bindings);
                facts.package_vars.extend(
                    bindings
                        .into_iter()
                        .filter_map(|(name, hint)| Some((qualify_module(module, &name), hint?))),
                );
            }
            _ => {}
        }
    }
    Ok(facts)
}

fn collect_type_decls(
    decl: Node<'_>,
    source: &[u8],
    module: &str,
    imports: &[ImportShape],
    facts: &mut GoTypeFacts,
) {
    let mut cursor = decl.walk();
    for spec in decl.named_children(&mut cursor) {
        if !matches!(spec.kind(), "type_spec" | "type_alias") {
            continue;
        }
        let Some(name) = spec
            .child_by_field_name("name")
            .and_then(|n| node_str(n, source))
        else {
            continue;
        };
        let Some(underlying) = spec.child_by_field_name("type") else {
            continue;
        };
        let scope = TypeScope::new(source, module, imports, type_parameter_names(spec, source));
        let mut type_decl = GoTypeDecl {
            qualified_name: qualify_module(module, name),
            kind: GoTypeKind::Other,
            alias_of: None,
            fields: Vec::new(),
            embedded: Vec::new(),
        };
        if spec.kind() == "type_alias" {
            type_decl.alias_of = scope.type_hint(underlying);
        } else {
            match underlying.kind() {
                "struct_type" => {
                    type_decl.kind = GoTypeKind::Struct;
                    scope.struct_fields(underlying, &mut type_decl);
                }
                "interface_type" => type_decl.kind = GoTypeKind::Interface,
                _ => {}
            }
        }
        facts.types.push(type_decl);
    }
}

fn collect_function_results(
    decl: Node<'_>,
    source: &[u8],
    module: &str,
    imports: &[ImportShape],
    facts: &mut GoTypeFacts,
) {
    let Some(name) = crate::parser::function_name_text(decl, source) else {
        return;
    };
    let qualified = if decl.kind() == "method_declaration" {
        let Some(owner) = crate::parser::method_receiver_type(decl, source) else {
            return;
        };
        qualify_module(module, &format!("{owner}::{name}"))
    } else {
        qualify_module(module, name)
    };
    let mut type_params = type_parameter_names(decl, source);
    if let Some(receiver) = decl.child_by_field_name("receiver") {
        type_params.extend(receiver_type_parameter_names(receiver, source));
    }
    let scope = TypeScope::new(source, module, imports, type_params);
    let results = decl
        .child_by_field_name("result")
        .map(|result| scope.result_hints(result))
        .unwrap_or_default();
    if results.iter().any(Option::is_some) {
        facts.function_results.push((qualified, results));
    }
}

/// Names a declaration's `[T any, K comparable]` introduces.
pub(crate) fn type_parameter_names(node: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut names = HashSet::new();
    let Some(params) = node.child_by_field_name("type_parameters") else {
        return names;
    };
    let mut cursor = params.walk();
    for decl in params.named_children(&mut cursor) {
        let mut inner = decl.walk();
        for child in decl.children_by_field_name("name", &mut inner) {
            if let Some(text) = node_str(child, source) {
                names.insert(text.to_owned());
            }
        }
    }
    names
}

/// The type arguments a generic receiver (`func (s *Set[K]) ...`) names:
/// type parameters of the method's body.
pub(crate) fn receiver_type_parameter_names(receiver: Node<'_>, source: &[u8]) -> HashSet<String> {
    let mut names = HashSet::new();
    collect_receiver_type_args(receiver, source, &mut names);
    names
}

fn collect_receiver_type_args(node: Node<'_>, source: &[u8], out: &mut HashSet<String>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if node.kind() == "type_arguments" {
            out.extend(type_argument_name(child, source));
        } else {
            collect_receiver_type_args(child, source, out);
        }
    }
}

/// The bare name a receiver's type argument declares (`K` in `Set[K]`).
fn type_argument_name(arg: Node<'_>, source: &[u8]) -> Option<String> {
    let ident = if arg.kind() == "type_elem" {
        let mut cursor = arg.walk();
        arg.named_children(&mut cursor).next()?
    } else {
        arg
    };
    (ident.kind() == "type_identifier")
        .then(|| node_str(ident, source).map(str::to_owned))
        .flatten()
}

/// What one name in a function's scope is bound to: a single type hint,
/// or `None` when some binding of the name gives none or two bindings
/// disagree.
pub(crate) type LocalTypes = HashMap<String, Option<ValueTypeHint>>;

/// One declared name and the hint its declaration gives.
type Binding = (String, Option<ValueTypeHint>);

/// The facts a hint is read against: the file's module and imports, the
/// type parameters in scope, and the names the enclosing function binds.
pub(crate) struct TypeScope<'a> {
    source: &'a [u8],
    module: &'a str,
    /// Namespace alias → imported module path.
    aliases: HashMap<&'a str, &'a str>,
    type_params: HashSet<String>,
    locals: LocalTypes,
}

impl<'a> TypeScope<'a> {
    pub(crate) fn new(
        source: &'a [u8],
        module: &'a str,
        imports: &'a [ImportShape],
        type_params: HashSet<String>,
    ) -> Self {
        let aliases = imports
            .iter()
            .filter_map(|import| {
                let alias = import.local_alias.known_value()?.as_deref()?;
                let module = import.imported_module.known_value()?;
                Some((alias, module.as_str()))
            })
            .collect();
        Self {
            source,
            module,
            aliases,
            type_params,
            locals: HashMap::new(),
        }
    }

    /// The scope of one function or method: its receiver, parameters and
    /// every name its body (closures included) declares. Go scopes a
    /// `:=` to its block; reading the whole body as one scope costs the
    /// hint of a name redeclared with another type, never a wrong one.
    pub(crate) fn for_function(
        source: &'a [u8],
        module: &'a str,
        imports: &'a [ImportShape],
        decl: Node<'_>,
        body: Node<'_>,
    ) -> Self {
        let mut type_params = type_parameter_names(decl, source);
        if let Some(receiver) = decl.child_by_field_name("receiver") {
            type_params.extend(receiver_type_parameter_names(receiver, source));
        }
        let mut scope = Self::new(source, module, imports, type_params);
        let mut bindings = Vec::new();
        for field in ["receiver", "parameters"] {
            if let Some(params) = decl.child_by_field_name(field) {
                scope.parameter_bindings(params, &mut bindings);
            }
        }
        scope.bind_all(bindings);
        scope.collect_body_bindings(body);
        scope
    }

    fn bind_all(&mut self, bindings: Vec<Binding>) {
        for (name, hint) in bindings {
            if name == "_" {
                continue;
            }
            self.locals
                .entry(name)
                .and_modify(|existing| {
                    if *existing != hint {
                        *existing = None;
                    }
                })
                .or_insert(hint);
        }
    }

    /// Walk a body in source order, binding every declared name with the
    /// hint its declaration gives, read against the names bound so far.
    fn collect_body_bindings(&mut self, node: Node<'_>) {
        let bindings = self.declared_bindings(node);
        self.bind_all(bindings);
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.collect_body_bindings(child);
        }
    }

    /// The names `node` itself declares, each with the hint its
    /// declaration gives.
    fn declared_bindings(&self, node: Node<'_>) -> Vec<Binding> {
        let mut out = Vec::new();
        // Values no declaration spells the type of: a constant, a range
        // or receive variable, a type switch's per-case value.
        let unhinted = match node.kind() {
            "short_var_declaration" => {
                self.paired_bindings(node, &mut out);
                None
            }
            "var_declaration" => {
                self.var_declaration_bindings(node, &mut out);
                None
            }
            "func_literal" => {
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.parameter_bindings(params, &mut out);
                }
                None
            }
            "const_spec" => {
                let mut cursor = node.walk();
                Some(node.children_by_field_name("name", &mut cursor).collect())
            }
            "range_clause" | "receive_statement" => node
                .child_by_field_name("left")
                .map(expression_list_children),
            "type_switch_statement" => node
                .child_by_field_name("alias")
                .map(expression_list_children),
            _ => None,
        };
        for name in unhinted.unwrap_or_default() {
            self.push_name(name, None, &mut out);
        }
        out
    }

    fn push_name(&self, name: Node<'_>, hint: Option<ValueTypeHint>, out: &mut Vec<Binding>) {
        if name.kind() == "identifier"
            && let Some(text) = node_str(name, self.source)
        {
            out.push((text.to_owned(), hint));
        }
    }

    fn parameter_bindings(&self, params: Node<'_>, out: &mut Vec<Binding>) {
        let mut cursor = params.walk();
        for decl in params.named_children(&mut cursor) {
            // `xs ...T` holds a slice, which has no methods.
            let hint = match decl.kind() {
                "parameter_declaration" => decl
                    .child_by_field_name("type")
                    .and_then(|ty| self.type_hint(ty)),
                "variadic_parameter_declaration" => None,
                _ => continue,
            };
            let mut inner = decl.walk();
            for name in decl.children_by_field_name("name", &mut inner) {
                self.push_name(name, hint.clone(), out);
            }
        }
    }

    /// `var a, b T`, `var a = f()`, `var a, err = f()`.
    pub(crate) fn var_declaration_bindings(&self, decl: Node<'_>, out: &mut Vec<Binding>) {
        let mut specs = Vec::new();
        collect_var_specs(decl, &mut specs);
        for spec in specs {
            if let Some(ty) = spec.child_by_field_name("type") {
                let hint = self.type_hint(ty);
                let mut inner = spec.walk();
                for name in spec.children_by_field_name("name", &mut inner) {
                    self.push_name(name, hint.clone(), out);
                }
            } else {
                let mut inner = spec.walk();
                let names: Vec<Node<'_>> =
                    spec.children_by_field_name("name", &mut inner).collect();
                let values = spec
                    .child_by_field_name("value")
                    .map(expression_list_children)
                    .unwrap_or_default();
                self.pair(&names, &values, out);
            }
        }
    }

    /// `a, b := x, y` pairs by position; `a, err := f()` reads result `i`
    /// of the one call for the `i`-th name.
    fn paired_bindings(&self, node: Node<'_>, out: &mut Vec<Binding>) {
        let names = node
            .child_by_field_name("left")
            .map(expression_list_children)
            .unwrap_or_default();
        let values = node
            .child_by_field_name("right")
            .map(expression_list_children)
            .unwrap_or_default();
        self.pair(&names, &values, out);
    }

    fn pair(&self, names: &[Node<'_>], values: &[Node<'_>], out: &mut Vec<Binding>) {
        for (index, name) in names.iter().enumerate() {
            let hint = if names.len() == values.len() {
                values
                    .get(index)
                    .and_then(|value| self.expr_hint(*value, 0))
            } else if let [value] = values {
                self.expr_hint(*value, index)
            } else {
                None
            };
            self.push_name(*name, hint, out);
        }
    }

    /// The hint for each result slot of a `result:` node: a bare type, or
    /// a parameter list (`(n int, err error)` gives one slot per name).
    fn result_hints(&self, result: Node<'_>) -> Vec<Option<ValueTypeHint>> {
        if result.kind() != "parameter_list" {
            return vec![self.type_hint(result)];
        }
        let mut out = Vec::new();
        let mut cursor = result.walk();
        for decl in result.named_children(&mut cursor) {
            if decl.kind() != "parameter_declaration" {
                continue;
            }
            let hint = decl
                .child_by_field_name("type")
                .and_then(|ty| self.type_hint(ty));
            let mut inner = decl.walk();
            let slots = decl
                .children_by_field_name("name", &mut inner)
                .count()
                .max(1);
            out.extend(std::iter::repeat_n(hint, slots));
        }
        out
    }

    fn struct_fields(&self, struct_type: Node<'_>, out: &mut GoTypeDecl) {
        let mut cursor = struct_type.walk();
        for list in struct_type.named_children(&mut cursor) {
            if list.kind() != "field_declaration_list" {
                continue;
            }
            let mut field_cursor = list.walk();
            for field in list.named_children(&mut field_cursor) {
                if field.kind() == "field_declaration" {
                    self.struct_field(field, out);
                }
            }
        }
    }

    /// One `field_declaration`: its names (`out, err *Writer`), or an
    /// embedded type, which is also a field named after the type.
    fn struct_field(&self, field: Node<'_>, out: &mut GoTypeDecl) {
        let hint = field
            .child_by_field_name("type")
            .and_then(|ty| self.type_hint(ty));
        let mut cursor = field.walk();
        let names: Vec<&str> = field
            .children_by_field_name("name", &mut cursor)
            .filter_map(|name| node_str(name, self.source))
            .collect();
        if !names.is_empty() {
            out.fields.extend(
                names
                    .into_iter()
                    .map(|name| (name.to_owned(), hint.clone())),
            );
            return;
        }
        let Some(hint) = hint else {
            return;
        };
        if let Some(name) = hint_type_name(&hint) {
            out.fields.push((name.to_owned(), Some(hint.clone())));
        }
        out.embedded.push(hint);
    }

    /// A named type, through pointers, parentheses and instantiation:
    /// `T`, `*T`, `pkg.T`, `G[int]`. A type parameter or a predeclared
    /// type gives no hint, and neither does a slice, map, channel or
    /// function type.
    pub(crate) fn type_hint(&self, ty: Node<'_>) -> Option<ValueTypeHint> {
        match ty.kind() {
            "type_identifier" => {
                let name = node_str(ty, self.source)?;
                if self.type_params.contains(name) || PREDECLARED_TYPES.contains(&name) {
                    return None;
                }
                Some(ValueTypeHint::Named(qualify_module(self.module, name)))
            }
            "qualified_type" => {
                let package = node_str(ty.child_by_field_name("package")?, self.source)?;
                let name = node_str(ty.child_by_field_name("name")?, self.source)?;
                let imported = self.aliases.get(package)?;
                Some(ValueTypeHint::Named(format!("{imported}::{name}")))
            }
            "generic_type" => self.type_hint(ty.child_by_field_name("type")?),
            "pointer_type" | "parenthesized_type" | "type_elem" => {
                let mut cursor = ty.walk();
                let inner = ty.named_children(&mut cursor).next()?;
                self.type_hint(inner)
            }
            _ => None,
        }
    }

    /// The hint for result `index` of an expression. Only a call yields
    /// more than one value; any other expression has a hint at index 0
    /// alone (`y, ok := x.(T)` binds `T` to `y`).
    pub(crate) fn expr_hint(&self, expr: Node<'_>, index: usize) -> Option<ValueTypeHint> {
        match expr.kind() {
            "call_expression" => self.call_hint(expr, index),
            _ if index > 0 => None,
            "identifier" => {
                let name = node_str(expr, self.source)?;
                match self.locals.get(name) {
                    Some(hint) => hint.clone(),
                    None if self.aliases.contains_key(name) => None,
                    None => Some(ValueTypeHint::PackageVar(qualify_module(self.module, name))),
                }
            }
            "selector_expression" => {
                let operand = expr.child_by_field_name("operand")?;
                let field = node_str(expr.child_by_field_name("field")?, self.source)?;
                if let Some(imported) = self.alias_operand(operand) {
                    return Some(ValueTypeHint::PackageVar(format!("{imported}::{field}")));
                }
                Some(ValueTypeHint::Field {
                    owner: Box::new(self.expr_hint(operand, 0)?),
                    field: field.to_owned(),
                })
            }
            "composite_literal" => self.type_hint(expr.child_by_field_name("type")?),
            "type_assertion_expression" => self.type_hint(expr.child_by_field_name("type")?),
            "unary_expression" => {
                let operator = expr.child_by_field_name("operator")?;
                matches!(operator.kind(), "&" | "*").then_some(())?;
                self.expr_hint(expr.child_by_field_name("operand")?, 0)
            }
            "parenthesized_expression" => {
                let mut cursor = expr.walk();
                let inner = expr.named_children(&mut cursor).next()?;
                self.expr_hint(inner, 0)
            }
            _ => None,
        }
    }

    fn call_hint(&self, call: Node<'_>, index: usize) -> Option<ValueTypeHint> {
        let mut function = call.child_by_field_name("function")?;
        // `F[T](x)`: the instantiated function.
        if function.kind() == "index_expression" {
            function = function.child_by_field_name("operand")?;
        }
        match function.kind() {
            "identifier" => {
                let name = node_str(function, self.source)?;
                if self.locals.contains_key(name) {
                    return None;
                }
                if name == "new" {
                    return (index == 0).then(|| self.new_hint(call)).flatten();
                }
                Some(ValueTypeHint::CallResult {
                    callee: qualify_module(self.module, name),
                    index,
                })
            }
            "selector_expression" => {
                let operand = function.child_by_field_name("operand")?;
                let method = node_str(function.child_by_field_name("field")?, self.source)?;
                if let Some(imported) = self.alias_operand(operand) {
                    return Some(ValueTypeHint::CallResult {
                        callee: format!("{imported}::{method}"),
                        index,
                    });
                }
                Some(ValueTypeHint::MethodResult {
                    receiver: Box::new(self.expr_hint(operand, 0)?),
                    method: method.to_owned(),
                    index,
                })
            }
            "parenthesized_expression" if index == 0 => self.conversion_hint(function),
            _ => None,
        }
    }

    /// `new(T)`: a pointer to `T`.
    fn new_hint(&self, call: Node<'_>) -> Option<ValueTypeHint> {
        let args = call.child_by_field_name("arguments")?;
        let mut cursor = args.walk();
        let ty = args.named_children(&mut cursor).next()?;
        self.type_hint(ty)
    }

    /// `(*T)(p)`, `(T)(x)`: a conversion to the parenthesised type, which
    /// the cross-file pass reads as a call to a name that is a type.
    fn conversion_hint(&self, function: Node<'_>) -> Option<ValueTypeHint> {
        let mut cursor = function.walk();
        let inner = function.named_children(&mut cursor).next()?;
        let ty = match inner.kind() {
            "unary_expression" => inner.child_by_field_name("operand")?,
            _ => inner,
        };
        (ty.kind() == "identifier").then_some(())?;
        let name = node_str(ty, self.source)?;
        (!self.locals.contains_key(name)).then(|| ValueTypeHint::CallResult {
            callee: qualify_module(self.module, name),
            index: 0,
        })
    }

    /// The imported module a bare operand names, when it is a namespace
    /// alias the scope does not shadow.
    fn alias_operand(&self, operand: Node<'_>) -> Option<&'a str> {
        if !matches!(operand.kind(), "identifier" | "package_identifier") {
            return None;
        }
        let name = node_str(operand, self.source)?;
        if self.locals.contains_key(name) {
            return None;
        }
        self.aliases.get(name).copied()
    }
}

/// The last segment of a [`ValueTypeHint::Named`] path: the name an
/// embedded field is accessed by.
fn hint_type_name(hint: &ValueTypeHint) -> Option<&str> {
    match hint {
        ValueTypeHint::Named(path) => path.rsplit("::").next(),
        _ => None,
    }
}

fn collect_var_specs<'t>(node: Node<'t>, out: &mut Vec<Node<'t>>) {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            "var_spec" => out.push(child),
            "var_spec_list" => collect_var_specs(child, out),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract_call_shapes_with_module;
    use rstest::rstest;

    fn named(path: &str) -> ValueTypeHint {
        ValueTypeHint::Named(path.to_owned())
    }

    fn call_result(callee: &str, index: usize) -> ValueTypeHint {
        ValueTypeHint::CallResult {
            callee: callee.to_owned(),
            index,
        }
    }

    /// The receiver hint of the call to `method` inside `body`, a function
    /// of package `m` that imports `x/cmp`.
    fn receiver_hint(body: &str, method: &str) -> Option<ValueTypeHint> {
        let source = format!("package m\n\nimport \"x/cmp\"\n\n{body}\n");
        extract_call_shapes_with_module(&source, "m")
            .unwrap()
            .into_iter()
            .find(|call| call.callee_name() == Some(method))
            .unwrap_or_else(|| panic!("no call to {method}"))
            .receiver_type
            .known_value()
            .cloned()
    }

    #[rstest]
    #[case::receiver("func (s *S) a() { s.M() }", Some(named("m::S")))]
    #[case::generic_receiver("func (s *Set[K]) a() { s.M() }", Some(named("m::Set")))]
    #[case::parameter("func a(p cmp.Path) { p.M() }", Some(named("x::cmp::Path")))]
    #[case::pointer_parameter("func a(p *T) { p.M() }", Some(named("m::T")))]
    #[case::type_parameter("func a[T any](p T) { p.M() }", None)]
    #[case::predeclared("func a(e error) { e.M() }", None)]
    #[case::variadic("func a(xs ...T) { xs.M() }", None)]
    #[case::composite("func a() { x := &T{}; x.M() }", Some(named("m::T")))]
    #[case::new("func a() { x := new(T); x.M() }", Some(named("m::T")))]
    #[case::var_typed("func a() { var x cmp.Path; x.M() }", Some(named("x::cmp::Path")))]
    #[case::assertion("func a(v any) { x, ok := v.(T); _ = ok; x.M() }", Some(named("m::T")))]
    #[case::call_result("func a() { x := New(); x.M() }", Some(call_result("m::New", 0)))]
    #[case::second_result("func a() { _, x := New(); x.M() }", Some(call_result("m::New", 1)))]
    #[case::conversion("func a(p any) { (*T)(p).M() }", Some(call_result("m::T", 0)))]
    #[case::instantiated_call(
        "func a() { x := New[int](); x.M() }",
        Some(call_result("m::New", 0))
    )]
    #[case::imported_call(
        "func a() { x := cmp.New(); x.M() }",
        Some(call_result("x::cmp::New", 0))
    )]
    #[case::package_var("func a() { CommandLine.M() }", Some(ValueTypeHint::PackageVar("m::CommandLine".to_owned())))]
    #[case::imported_var("func a() { cmp.Default.M() }", Some(ValueTypeHint::PackageVar("x::cmp::Default".to_owned())))]
    #[case::namespace_call("func a() { cmp.M() }", None)]
    #[case::field(
        "func (s *S) a() { s.out.M() }",
        Some(ValueTypeHint::Field { owner: Box::new(named("m::S")), field: "out".to_owned() })
    )]
    #[case::method_result(
        "func (s *S) a() { s.Get().M() }",
        Some(ValueTypeHint::MethodResult { receiver: Box::new(named("m::S")), method: "Get".to_owned(), index: 0 })
    )]
    #[case::two_types_one_name("func a() { x := &T{}; if true { x := &U{}; _ = x }; x.M() }", None)]
    #[case::same_type_twice(
        "func a() { x := &T{}; x = &T{}; if true { x := T{}; _ = x }; x.M() }",
        Some(named("m::T"))
    )]
    #[case::range_variable("func a(xs []T) { for _, x := range xs { x.M() } }", None)]
    #[case::closure_parameter("func a() { f := func(x T) { x.M() }; f(T{}) }", Some(named("m::T")))]
    fn receiver_calls_carry_the_declared_type(
        #[case] body: &str,
        #[case] expected: Option<ValueTypeHint>,
    ) {
        assert_eq!(receiver_hint(body, "M"), expected);
    }

    #[test]
    fn package_facts_cover_types_results_and_variables() {
        let source = concat!(
            "package m\n\n",
            "import \"x/cmp\"\n\n",
            "type S struct {\n    out, err *Writer\n    cmp.Path\n    fn func()\n}\n\n",
            "type I interface{ M() }\n\n",
            "type A = S\n\n",
            "type List[T any] struct{ head T }\n\n",
            "var CommandLine = New()\n",
            "var a, b *S\n",
            "var (\n    c = &S{}\n    d int\n)\n\n",
            "func New() (*S, error) { return nil, nil }\n\n",
            "func (s *S) Pair() (x, y Writer) { return }\n\n",
            "func Nothing() {}\n",
        );
        let facts = extract_type_facts_with_module(source, "m").unwrap();
        let s = &facts.types[0];
        assert_eq!(s.qualified_name, "m::S");
        assert_eq!(s.kind, GoTypeKind::Struct);
        assert_eq!(
            s.fields,
            [
                ("out".to_owned(), Some(named("m::Writer"))),
                ("err".to_owned(), Some(named("m::Writer"))),
                ("Path".to_owned(), Some(named("x::cmp::Path"))),
                ("fn".to_owned(), None),
            ]
        );
        assert_eq!(s.embedded, [named("x::cmp::Path")]);
        assert_eq!(facts.types[1].kind, GoTypeKind::Interface);
        assert_eq!(facts.types[2].alias_of, Some(named("m::S")));
        // A field of a type-parameter type gives no hint.
        assert_eq!(facts.types[3].fields, [("head".to_owned(), None)]);
        assert_eq!(
            facts.function_results,
            [
                ("m::New".to_owned(), vec![Some(named("m::S")), None]),
                (
                    "m::S::Pair".to_owned(),
                    vec![Some(named("m::Writer")), Some(named("m::Writer"))]
                ),
            ]
        );
        assert_eq!(
            facts.package_vars,
            [
                ("m::CommandLine".to_owned(), call_result("m::New", 0)),
                ("m::a".to_owned(), named("m::S")),
                ("m::b".to_owned(), named("m::S")),
                ("m::c".to_owned(), named("m::S")),
            ]
        );
    }
}
