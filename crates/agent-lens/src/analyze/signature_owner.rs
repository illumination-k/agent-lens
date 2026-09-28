//! Exclusions shared by `analyze single-use` and `analyze parameters`:
//! functions whose signature or call sites belong to something other
//! than their in-tree callers, so neither "inline it" nor "drop the
//! parameter" is an edit on the function alone.

use serde::Serialize;

use super::call_graph::model::{CallGraphNode, GraphLanguage};
use super::export_lang::{ExportLang, InterfaceIndex};

/// Why a node's signature is owned outside its in-tree callers.
#[derive(Debug, Clone, Copy)]
pub(super) enum SignatureOwner {
    TraitMethod,
    InterfaceMethod,
    Annotated,
}

impl SignatureOwner {
    /// The owner of `node`'s signature, if anything besides its callers
    /// has a say in it. `None` also covers "could not tell": a node
    /// without attribute extraction (TypeScript, Python) is *not*
    /// excluded — excluding on that would empty both languages.
    pub(super) fn of(node: &CallGraphNode, interfaces: &InterfaceIndex) -> Option<Self> {
        if matches!(
            node.owner_kind,
            Some(lens_domain::OwnerKind::TraitImpl | lens_domain::OwnerKind::Trait)
        ) {
            return Some(Self::TraitMethod);
        }
        if ExportLang::of(node) == Some(ExportLang::Go)
            && !interfaces.matching(node, ExportLang::Go).is_empty()
        {
            return Some(Self::InterfaceMethod);
        }
        // A known non-inert annotation may itself be a caller, or mean a
        // framework owns both the signature and the call sites.
        if node.attributes.as_ref().is_some_and(|attributes| {
            let inert = node
                .graph_language()
                .unwrap_or(GraphLanguage::TypeScript)
                .inert_attribute_names();
            attributes
                .iter()
                .any(|attribute| !inert.contains(attribute))
        }) {
            return Some(Self::Annotated);
        }
        None
    }

    pub(super) fn count_slot(self, counts: &mut SignatureOwnerCounts) -> &mut usize {
        match self {
            Self::TraitMethod => &mut counts.trait_method_count,
            Self::InterfaceMethod => &mut counts.interface_method_count,
            Self::Annotated => &mut counts.annotated_count,
        }
    }
}

/// Functions excluded per [`SignatureOwner`], flattened into each
/// analyzer's `excluded` section.
#[derive(Debug, Default, Serialize)]
pub(super) struct SignatureOwnerCounts {
    /// Rust trait `impl` methods and trait default bodies: the trait
    /// owns the signature, and callers can name the trait.
    pub(super) trait_method_count: usize,
    /// Go methods matching an in-scope interface's method set by name
    /// and arity: the interface owns the signature, and calls can
    /// dispatch through it.
    pub(super) interface_method_count: usize,
    /// Carries an annotation not on the language's inert list: the
    /// annotation itself may be a caller, and a framework may call it
    /// with anything and often owns the signature too.
    pub(super) annotated_count: usize,
}
