//! Language-neutral syntax facts shared by graph and similarity analyzers.
//!
//! These types are deliberately syntax-only. Language adapters should fill
//! facts they can read cheaply from the parser they already use, mark facts
//! as [`SyntaxFact::Unknown`] when they cannot, and leave semantic enrichment
//! (type inference, language servers, cross-package resolution) as a later
//! optional pass.

use crate::function::{FunctionDef, FunctionSignature, ReceiverShape};
use crate::tree::TreeNode;

/// A fact that may be unavailable for a language or parser backend.
///
/// Optional facts use `Known(None)` when the adapter knows the concept is
/// absent, and `Unknown` when it did not or could not determine the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyntaxFact<T> {
    Known(T),
    Unknown,
}

impl<T> SyntaxFact<T> {
    pub fn as_ref(&self) -> SyntaxFact<&T> {
        match self {
            Self::Known(value) => SyntaxFact::Known(value),
            Self::Unknown => SyntaxFact::Unknown,
        }
    }

    pub fn known_value(&self) -> Option<&T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unknown => None,
        }
    }
}

/// 1-based inclusive source line span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSpan {
    pub start_line: usize,
    pub end_line: usize,
}

impl SourceSpan {
    pub fn line_count(self) -> usize {
        self.end_line.saturating_sub(self.start_line) + 1
    }
}

/// Neutral representation of a function-like definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionShape {
    pub display_name: String,
    pub qualified_name: SyntaxFact<String>,
    pub module_path: SyntaxFact<String>,
    pub owner: SyntaxFact<Option<OwnerShape>>,
    pub visibility: SyntaxFact<VisibilityShape>,
    pub signature: SyntaxFact<SignatureShape>,
    /// Documentation text attached to the definition, comment markers
    /// stripped. `None` when absent or not extracted by the adapter.
    pub doc: Option<String>,
    /// Non-doc annotations attached to the definition, as written and
    /// without their arguments: Rust attribute paths (`no_mangle`,
    /// `tokio::main`), Go compiler directives read off the doc comment
    /// (`go:linkname`, `export`). `Known(vec![])` means the adapter
    /// looked and found none; `Unknown` means it does not extract them,
    /// which reachability analysis must read as "an entry marker may be
    /// hiding here" rather than "there is none".
    pub attributes: SyntaxFact<Vec<String>>,
    pub body: BodyShape,
    pub span: SourceSpan,
    pub is_test: bool,
}

impl FunctionShape {
    pub fn line_count(&self) -> usize {
        self.span.line_count()
    }

    pub fn body_tree(&self) -> &TreeNode {
        &self.body.tree
    }

    pub fn signature_shape(&self) -> Option<&SignatureShape> {
        self.signature.known_value()
    }
}

impl From<FunctionDef> for FunctionShape {
    fn from(def: FunctionDef) -> Self {
        let body_tree = def.body_tree().clone();
        Self {
            display_name: def.name,
            qualified_name: SyntaxFact::Unknown,
            module_path: SyntaxFact::Unknown,
            owner: SyntaxFact::Unknown,
            visibility: SyntaxFact::Unknown,
            signature: def
                .signature
                .map(SignatureShape::from)
                .map_or(SyntaxFact::Unknown, SyntaxFact::Known),
            doc: def.doc,
            attributes: SyntaxFact::Unknown,
            body: BodyShape { tree: body_tree },
            span: SourceSpan {
                start_line: def.start_line,
                end_line: def.end_line,
            },
            is_test: def.is_test,
        }
    }
}

/// Neutral representation of an interface-like declaration: the named
/// method set a concrete type satisfies structurally (Go `interface`;
/// a Rust `trait` would project the same way).
///
/// Only directly declared methods are carried. Embedded interfaces are
/// not expanded — an embedded interface's methods are collected from
/// its own declaration when that declaration is in scope, and an
/// out-of-scope embed has no method set to expand anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceShape {
    pub display_name: String,
    pub qualified_name: SyntaxFact<String>,
    pub methods: Vec<InterfaceMethodShape>,
}

/// One method an interface declares directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceMethodShape {
    pub name: String,
    /// Parameter slots, with grouped names expanded: `Do(a, b int)`
    /// declares 2, `Do(int)` declares 1, a variadic slot counts as 1.
    pub param_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerShape {
    pub display_name: String,
    pub kind: OwnerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerKind {
    Class,
    /// An inherent `impl` block: the methods are reached by name.
    Impl,
    /// A trait declaration: the methods are the interface itself.
    Trait,
    /// A trait `impl` block (`impl Trait for Type`): the methods are
    /// reachable through the trait, so a call site can name the trait
    /// method and never this definition.
    TraitImpl,
    Receiver,
    Namespace,
    Module,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibilityShape {
    Public,
    Restricted(String),
    Private,
    Exported,
    Unexported,
}

/// Neutral representation of function signature facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureShape {
    pub name_tokens: SyntaxFact<Vec<String>>,
    pub params: Vec<ParameterShape>,
    pub return_type: SyntaxFact<Option<String>>,
    pub return_type_paths: Vec<String>,
    pub receiver: SyntaxFact<ReceiverShape>,
    pub generics: SyntaxFact<Vec<String>>,
    pub bounds: SyntaxFact<Vec<String>>,
}

impl SignatureShape {
    pub fn parameter_count(&self) -> usize {
        self.params.len()
    }

    pub fn parameter_names(&self) -> impl Iterator<Item = &str> {
        self.params
            .iter()
            .filter_map(|param| param.name.known_value().and_then(Option::as_ref))
            .map(String::as_str)
    }

    pub fn parameter_type_paths(&self) -> impl Iterator<Item = &str> {
        self.params
            .iter()
            .flat_map(|param| param.type_paths.iter().map(String::as_str))
    }

    pub fn name_tokens(&self) -> impl Iterator<Item = &str> {
        self.name_tokens
            .known_value()
            .into_iter()
            .flat_map(|tokens| tokens.iter().map(String::as_str))
    }

    pub fn generics(&self) -> impl Iterator<Item = &str> {
        self.generics
            .known_value()
            .into_iter()
            .flat_map(|items| items.iter().map(String::as_str))
    }

    pub fn receiver_shape(&self) -> Option<ReceiverShape> {
        self.receiver.known_value().copied()
    }
}

impl From<FunctionSignature> for SignatureShape {
    fn from(signature: FunctionSignature) -> Self {
        let mut params = Vec::with_capacity(signature.parameter_count);
        let mut names = signature.parameter_names.into_iter();
        let mut types = signature.parameter_type_paths.into_iter();
        for _ in 0..signature.parameter_count {
            let name = names.next();
            let ty = types.next();
            params.push(ParameterShape {
                name: SyntaxFact::Known(name),
                type_annotation: SyntaxFact::Known(ty.clone()),
                type_paths: ty.into_iter().collect(),
            });
        }
        Self {
            name_tokens: SyntaxFact::Known(signature.name_tokens),
            params,
            return_type: SyntaxFact::Unknown,
            return_type_paths: signature.return_type_paths,
            receiver: SyntaxFact::Known(signature.receiver),
            generics: SyntaxFact::Known(signature.generics),
            bounds: SyntaxFact::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterShape {
    pub name: SyntaxFact<Option<String>>,
    pub type_annotation: SyntaxFact<Option<String>>,
    pub type_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyShape {
    pub tree: TreeNode,
}

/// Neutral representation of a call expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallShape {
    pub caller_qualified_name: SyntaxFact<Option<String>>,
    pub caller_module: SyntaxFact<String>,
    pub caller_owner: SyntaxFact<Option<String>>,
    pub callee_display_name: SyntaxFact<Option<String>>,
    pub callee_path_segments: SyntaxFact<Vec<String>>,
    pub receiver_expr_kind: SyntaxFact<ReceiverExprKind>,
    /// Whether the callee name is bound in the caller's own local scope
    /// — a closure or nested function assigned to a local name, or a
    /// function-typed parameter. Such a call targets the local binding,
    /// which shadows anything the workspace defines under that name, so
    /// resolution must not attribute it to a global definition.
    ///
    /// `Unknown` when the adapter does not track local scopes; consumers
    /// then behave as if the callee were not locally bound.
    pub callee_is_locally_bound: SyntaxFact<bool>,
    pub lexical_resolution: LexicalResolutionStatus,
    pub visible_imports: Vec<ImportShape>,
    pub line: usize,
}

impl CallShape {
    pub fn callee_name(&self) -> Option<&str> {
        self.callee_display_name
            .known_value()
            .and_then(Option::as_ref)
            .map(String::as_str)
    }

    pub fn callee_path(&self) -> Option<String> {
        self.callee_path_segments
            .known_value()
            .map(|segments| segments.join("::"))
    }

    pub fn caller_qualified_name(&self) -> Option<&str> {
        self.caller_qualified_name
            .known_value()
            .and_then(Option::as_ref)
            .map(String::as_str)
    }

    pub fn caller_module(&self) -> Option<&str> {
        self.caller_module.known_value().map(String::as_str)
    }

    pub fn caller_owner(&self) -> Option<&str> {
        self.caller_owner
            .known_value()
            .and_then(Option::as_ref)
            .map(String::as_str)
    }

    /// True only when the adapter positively determined that the callee
    /// name is bound in the caller's local scope. [`SyntaxFact::Unknown`]
    /// reads as "not locally bound", keeping adapters that do not track
    /// scopes on the pre-existing resolution path.
    pub fn callee_is_locally_bound(&self) -> bool {
        matches!(self.callee_is_locally_bound, SyntaxFact::Known(true))
    }

    pub fn has_receiver_expression(&self) -> bool {
        matches!(
            self.receiver_expr_kind,
            SyntaxFact::Known(ReceiverExprKind::Expression | ReceiverExprKind::SelfValue)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverExprKind {
    None,
    SelfValue,
    Expression,
}

/// Whether a call site's callee is a bare name that the caller's own
/// scope binds to a callable — the fact [`CallShape::callee_is_locally_bound`]
/// carries.
///
/// Only single-segment plain calls qualify. A receiver call (`emit.run()`)
/// names a method, not the binding, and a multi-segment path
/// (`pkg::run()`) is already anchored by its prefix, so neither is
/// shadowed by a local of that name.
pub fn callee_names_local_binding(
    receiver: ReceiverExprKind,
    callee_path_segments: Option<&[String]>,
    locally_bound: &std::collections::HashSet<String>,
) -> bool {
    if receiver != ReceiverExprKind::None || locally_bound.is_empty() {
        return false;
    }
    matches!(callee_path_segments, Some([only]) if locally_bound.contains(only))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexicalResolutionStatus {
    NotAttempted,
    Resolved,
    Unresolved,
    Ambiguous,
}

/// Neutral representation of an import/export fact visible in a scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportShape {
    pub imported_module: SyntaxFact<String>,
    pub local_alias: SyntaxFact<Option<String>>,
    pub exported_symbol: SyntaxFact<Option<String>>,
}

impl ImportShape {
    /// An import whose three parts the adapter read straight off the
    /// syntax tree, so every field is [`SyntaxFact::Known`].
    ///
    /// This is the shape a language with explicit import statements
    /// produces — TypeScript and Python both build every import this
    /// way. It lives here rather than in each adapter because "all three
    /// facts are known" is a statement about [`ImportShape`], not about
    /// any one language.
    pub fn known(
        imported_module: String,
        local_alias: Option<String>,
        exported_symbol: Option<String>,
    ) -> Self {
        Self {
            imported_module: SyntaxFact::Known(imported_module),
            local_alias: SyntaxFact::Known(local_alias),
            exported_symbol: SyntaxFact::Known(exported_symbol),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FunctionDef, FunctionSignature};

    #[test]
    fn syntax_fact_known_value_distinguishes_known_from_unknown() {
        let known = SyntaxFact::Known("value".to_owned());
        let unknown: SyntaxFact<String> = SyntaxFact::Unknown;

        assert_eq!(known.known_value().map(String::as_str), Some("value"));
        assert_eq!(unknown.known_value(), None);
    }

    #[test]
    fn import_shape_known_marks_every_field_as_known() {
        let shape = ImportShape::known(
            "./mod".to_owned(),
            Some("alias".to_owned()),
            Some("symbol".to_owned()),
        );
        assert_eq!(
            shape,
            ImportShape {
                imported_module: SyntaxFact::Known("./mod".to_owned()),
                local_alias: SyntaxFact::Known(Some("alias".to_owned())),
                exported_symbol: SyntaxFact::Known(Some("symbol".to_owned())),
            }
        );
    }

    /// An absent alias or symbol is a *known* absence — a default import
    /// really has no named symbol — not an unread fact.
    #[test]
    fn import_shape_known_treats_none_as_a_known_absence() {
        let shape = ImportShape::known("./mod".to_owned(), None, None);
        assert_eq!(shape.local_alias, SyntaxFact::Known(None));
        assert_eq!(shape.exported_symbol, SyntaxFact::Known(None));
    }

    #[test]
    fn source_span_and_function_shape_line_counts_are_inclusive() {
        let span = SourceSpan {
            start_line: 10,
            end_line: 14,
        };
        let shape = FunctionShape {
            display_name: "f".to_owned(),
            qualified_name: SyntaxFact::Unknown,
            module_path: SyntaxFact::Unknown,
            owner: SyntaxFact::Known(None),
            visibility: SyntaxFact::Unknown,
            signature: SyntaxFact::Unknown,
            doc: None,
            attributes: SyntaxFact::Unknown,
            body: BodyShape {
                tree: TreeNode::leaf("Block"),
            },
            span,
            is_test: false,
        };

        assert_eq!(span.line_count(), 5);
        assert_eq!(shape.line_count(), 5);
    }

    #[test]
    fn function_shape_from_function_def_preserves_body_and_signature() {
        let def = FunctionDef {
            name: "parse_user".to_owned(),
            start_line: 3,
            end_line: 8,
            is_test: true,
            signature: Some(signature()),
            doc: Some("Parse a user from its id.".to_owned()),
            implements: None,
            tree: TreeNode::with_children(
                "Function",
                "",
                vec![TreeNode::leaf("FnSignature"), TreeNode::leaf("Block")],
            ),
        };

        let shape = FunctionShape::from(def);

        assert_eq!(shape.display_name, "parse_user");
        assert_eq!(shape.span.line_count(), 6);
        assert!(shape.is_test);
        assert_eq!(shape.body_tree().label, "Block");
        assert_eq!(shape.doc.as_deref(), Some("Parse a user from its id."));
        assert_eq!(
            shape
                .signature_shape()
                .map(|sig| sig.name_tokens().collect::<Vec<_>>()),
            Some(vec!["parse", "user"]),
        );
    }

    #[test]
    fn signature_shape_projects_comparable_signature_facts() {
        let sig = SignatureShape::from(signature());

        assert_eq!(sig.parameter_count(), 2);
        assert_eq!(sig.name_tokens().collect::<Vec<_>>(), ["parse", "user"]);
        assert_eq!(
            sig.parameter_names().collect::<Vec<_>>(),
            ["id", "fallback"]
        );
        assert_eq!(
            sig.parameter_type_paths().collect::<Vec<_>>(),
            ["UserId", "Option<User>"]
        );
        assert_eq!(sig.return_type_paths, ["Result<User>"]);
        assert_eq!(sig.generics().collect::<Vec<_>>(), ["T: Clone"]);
        assert_eq!(sig.receiver_shape(), Some(ReceiverShape::Ref));
    }

    #[test]
    fn call_shape_accessors_return_known_call_facts() {
        let call = CallShape {
            caller_qualified_name: SyntaxFact::Known(Some("crate::m::S::caller".to_owned())),
            caller_module: SyntaxFact::Known("crate::m".to_owned()),
            caller_owner: SyntaxFact::Known(Some("S".to_owned())),
            callee_display_name: SyntaxFact::Known(Some("parse".to_owned())),
            callee_path_segments: SyntaxFact::Known(vec![
                "crate".to_owned(),
                "m".to_owned(),
                "parse".to_owned(),
            ]),
            receiver_expr_kind: SyntaxFact::Known(ReceiverExprKind::Expression),
            callee_is_locally_bound: SyntaxFact::Known(false),
            lexical_resolution: LexicalResolutionStatus::NotAttempted,
            visible_imports: Vec::new(),
            line: 12,
        };
        let path_call = CallShape {
            receiver_expr_kind: SyntaxFact::Known(ReceiverExprKind::None),
            ..call.clone()
        };

        assert_eq!(call.callee_name(), Some("parse"));
        assert_eq!(call.callee_path().as_deref(), Some("crate::m::parse"));
        assert_eq!(call.caller_qualified_name(), Some("crate::m::S::caller"));
        assert_eq!(call.caller_module(), Some("crate::m"));
        assert_eq!(call.caller_owner(), Some("S"));
        assert!(call.has_receiver_expression());
        assert!(!path_call.has_receiver_expression());
    }

    /// `Unknown` means "the adapter does not track scopes", which must
    /// read as not-shadowed — the same as an explicit `Known(false)`.
    #[test]
    fn callee_is_locally_bound_treats_unknown_as_not_bound() {
        let call = CallShape {
            callee_is_locally_bound: SyntaxFact::Known(true),
            ..call_shape()
        };
        let not_bound = CallShape {
            callee_is_locally_bound: SyntaxFact::Known(false),
            ..call_shape()
        };
        let unknown = CallShape {
            callee_is_locally_bound: SyntaxFact::Unknown,
            ..call_shape()
        };

        assert!(call.callee_is_locally_bound());
        assert!(!not_bound.callee_is_locally_bound());
        assert!(!unknown.callee_is_locally_bound());
    }

    #[test]
    fn callee_names_local_binding_matches_only_bare_calls_on_a_bound_name() {
        let bound: std::collections::HashSet<String> = ["emit".to_owned()].into_iter().collect();
        let empty = std::collections::HashSet::new();
        let bare = ["emit".to_owned()];
        let other = ["send".to_owned()];
        let path = ["pkg".to_owned(), "emit".to_owned()];

        // A bare call on a bound name is the one shadowed shape.
        assert!(callee_names_local_binding(
            ReceiverExprKind::None,
            Some(&bare),
            &bound
        ));
        // A receiver call names a method on the receiver's type, and a
        // multi-segment path is anchored by its prefix — a local of the
        // same name shadows neither.
        assert!(!callee_names_local_binding(
            ReceiverExprKind::Expression,
            Some(&bare),
            &bound
        ));
        assert!(!callee_names_local_binding(
            ReceiverExprKind::SelfValue,
            Some(&bare),
            &bound
        ));
        assert!(!callee_names_local_binding(
            ReceiverExprKind::None,
            Some(&path),
            &bound
        ));
        // Nothing bound under that name, or nothing bound at all.
        assert!(!callee_names_local_binding(
            ReceiverExprKind::None,
            Some(&other),
            &bound
        ));
        assert!(!callee_names_local_binding(
            ReceiverExprKind::None,
            Some(&bare),
            &empty
        ));
        // An anonymous callee has no name to shadow.
        assert!(!callee_names_local_binding(
            ReceiverExprKind::None,
            None,
            &bound
        ));
    }

    fn call_shape() -> CallShape {
        CallShape {
            caller_qualified_name: SyntaxFact::Known(Some("crate::m::caller".to_owned())),
            caller_module: SyntaxFact::Known("crate::m".to_owned()),
            caller_owner: SyntaxFact::Known(None),
            callee_display_name: SyntaxFact::Known(Some("emit".to_owned())),
            callee_path_segments: SyntaxFact::Known(vec!["emit".to_owned()]),
            receiver_expr_kind: SyntaxFact::Known(ReceiverExprKind::None),
            callee_is_locally_bound: SyntaxFact::Unknown,
            lexical_resolution: LexicalResolutionStatus::NotAttempted,
            visible_imports: Vec::new(),
            line: 1,
        }
    }

    fn signature() -> FunctionSignature {
        FunctionSignature {
            name_tokens: vec!["parse".to_owned(), "user".to_owned()],
            parameter_count: 2,
            parameter_names: vec!["id".to_owned(), "fallback".to_owned()],
            parameter_type_paths: vec!["UserId".to_owned(), "Option<User>".to_owned()],
            return_type_paths: vec!["Result<User>".to_owned()],
            generics: vec!["T: Clone".to_owned()],
            receiver: ReceiverShape::Ref,
        }
    }
}
