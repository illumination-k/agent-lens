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
    pub fn known(value: T) -> Self {
        Self::Known(value)
    }

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
            body: BodyShape { tree: body_tree },
            span: SourceSpan {
                start_line: def.start_line,
                end_line: def.end_line,
            },
            is_test: def.is_test,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerShape {
    pub display_name: String,
    pub kind: OwnerKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerKind {
    Class,
    Impl,
    Trait,
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
    pub receiver: SyntaxFact<ReceiverKind>,
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

    pub fn receiver_kind(&self) -> Option<ReceiverKind> {
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
            receiver: SyntaxFact::Known(signature.receiver.into()),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReceiverKind {
    None,
    Value,
    Ref,
    RefMut,
}

impl From<ReceiverShape> for ReceiverKind {
    fn from(receiver: ReceiverShape) -> Self {
        match receiver {
            ReceiverShape::None => Self::None,
            ReceiverShape::Value => Self::Value,
            ReceiverShape::Ref => Self::Ref,
            ReceiverShape::RefMut => Self::RefMut,
        }
    }
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
