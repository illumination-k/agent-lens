//! Language-neutral shape of a type definition (struct / class / enum /
//! alias), the unit compared by `analyze similarity --target types`.
//!
//! Adapters fill a [`TypeShape`] per definition; the shape renders itself
//! into the [`TreeNode`] comparison currency and into a synthesized
//! [`SignatureShape`] so the existing body/signature scoring blend works
//! unchanged. Rendering lives here — not in the adapters — so every
//! language emits the same label vocabulary and cross-language pairs pay
//! no spurious edit cost.

use crate::function::ReceiverShape;
use crate::naming::identifier_tokens;
use crate::syntax::{
    BodyShape, FunctionShape, ParameterShape, SignatureShape, SourceSpan, SyntaxFact,
};
use crate::tree::TreeNode;

/// Neutral kind of a type definition. Used as the comparison tree's root
/// label so a Rust `struct` and a TS `interface` compare as the same kind
/// of thing; the language-specific spelling stays in
/// [`TypeShape::kind_label`] for reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeDefKind {
    /// Named fields or properties: struct, interface, dataclass, TypedDict.
    Record,
    /// Tagged variants: Rust/TS enums, Python `Enum` subclasses.
    Enum,
    /// A name for another type: `type X = …`.
    Alias,
}

impl TypeDefKind {
    fn root_label(self) -> &'static str {
        match self {
            Self::Record => "Record",
            Self::Enum => "Enum",
            Self::Alias => "Alias",
        }
    }
}

/// One field / property / variant payload of a type definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeMemberShape {
    /// Declared member name; `None` for positional members (tuple-struct
    /// fields, alias targets).
    pub name: Option<String>,
    /// Type annotation as written, before [`normalize_type_text`].
    /// `None` when the language leaves the member untyped.
    pub type_text: Option<String>,
    /// Named type paths referenced by the annotation, for type-overlap
    /// scoring (same currency as `SignatureShape::type_paths`).
    pub type_paths: Vec<String>,
}

/// One variant of an [`TypeDefKind::Enum`] definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeVariantShape {
    pub name: String,
    /// Payload members; empty for unit variants.
    pub members: Vec<TypeMemberShape>,
}

/// Neutral representation of a type definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeShape {
    pub display_name: String,
    pub kind: TypeDefKind,
    /// Language-facing kind for reports: `"struct"`, `"enum"`,
    /// `"type_alias"`, `"interface"`, `"class"`, `"dataclass"`. Kept a
    /// small fixed vocabulary so agents can switch on it.
    pub kind_label: &'static str,
    /// Fields/properties for [`TypeDefKind::Record`], the single target
    /// member for [`TypeDefKind::Alias`]. Empty for enums.
    pub members: Vec<TypeMemberShape>,
    /// Variants for [`TypeDefKind::Enum`]; empty otherwise.
    pub variants: Vec<TypeVariantShape>,
    pub generics: Vec<String>,
    /// Documentation text, comment markers stripped.
    pub doc: Option<String>,
    pub span: SourceSpan,
    pub is_test: bool,
}

impl TypeShape {
    /// Whether the definition reduces to a bare root node: no members and
    /// no variants.
    ///
    /// Such a shape carries no evidence at all, so it does not compare
    /// *highly* against another shapeless definition — it compares
    /// *vacuously*, at 1.0, against every one of them. A marker
    /// interface, a unit struct, and an empty enum would form a single
    /// cluster whose only shared property is that nothing was extracted.
    /// The similarity corpus drops these before pairing.
    pub fn is_shapeless(&self) -> bool {
        self.members.is_empty() && self.variants.is_empty()
    }

    /// Render the comparison tree. Member facts go into node *labels*
    /// (`Field(user_id: Vec<String>)`), never into `value`: APTED,
    /// token profiles, and exact-match hashing all compare labels only.
    /// Member and variant names are case-folded so `userId` and
    /// `user_id` land on the same label across languages; type text is
    /// whitespace-normalized for the same reason.
    pub fn tree(&self) -> TreeNode {
        let children = match self.kind {
            TypeDefKind::Record | TypeDefKind::Alias => {
                self.members.iter().map(member_node).collect()
            }
            TypeDefKind::Enum => self.variants.iter().map(variant_node).collect(),
        };
        TreeNode::with_children(self.kind.root_label(), "", children)
    }

    /// Synthesize a [`SignatureShape`] from the member list so the
    /// signature component of the similarity blend (identifier overlap,
    /// type overlap, member count) scores type pairs the same way it
    /// scores function pairs. Enums contribute one parameter per
    /// variant; records one per member.
    pub fn member_signature(&self) -> SignatureShape {
        let params = match self.kind {
            TypeDefKind::Record | TypeDefKind::Alias => {
                self.members.iter().map(member_param).collect()
            }
            TypeDefKind::Enum => self.variants.iter().map(variant_param).collect(),
        };
        SignatureShape {
            name_tokens: SyntaxFact::Known(identifier_tokens(&self.display_name)),
            params,
            return_type: SyntaxFact::Known(None),
            return_type_paths: Vec::new(),
            receiver: SyntaxFact::Known(ReceiverShape::None),
            generics: SyntaxFact::Known(self.generics.clone()),
            bounds: SyntaxFact::Unknown,
        }
    }

    /// Lower into the [`FunctionShape`] corpus currency used by the
    /// similarity pipeline: the comparison tree becomes the body, the
    /// synthesized member signature the signature. Callers that need the
    /// language-facing kind must read [`TypeShape::kind_label`] first —
    /// the lowering does not carry it.
    pub fn into_function_shape(self) -> FunctionShape {
        let tree = self.tree();
        let signature = self.member_signature();
        FunctionShape {
            display_name: self.display_name,
            qualified_name: SyntaxFact::Unknown,
            module_path: SyntaxFact::Unknown,
            owner: SyntaxFact::Known(None),
            visibility: SyntaxFact::Unknown,
            signature: SyntaxFact::Known(signature),
            doc: self.doc,
            attributes: SyntaxFact::Unknown,
            body: BodyShape { tree },
            span: self.span,
            is_test: self.is_test,
        }
    }
}

fn member_node(member: &TypeMemberShape) -> TreeNode {
    let name = member.name.as_deref().map(fold_identifier);
    let ty = member.type_text.as_deref().map(normalize_type_text);
    let label = match (name, ty) {
        (Some(name), Some(ty)) => format!("Field({name}: {ty})"),
        (Some(name), None) => format!("Field({name})"),
        (None, Some(ty)) => format!("Field({ty})"),
        (None, None) => "Field".to_owned(),
    };
    TreeNode::leaf(label)
}

fn variant_node(variant: &TypeVariantShape) -> TreeNode {
    let label = format!("Variant({})", fold_identifier(&variant.name));
    TreeNode::with_children(label, "", variant.members.iter().map(member_node).collect())
}

fn member_param(member: &TypeMemberShape) -> ParameterShape {
    ParameterShape {
        name: SyntaxFact::Known(member.name.clone()),
        type_annotation: SyntaxFact::Known(member.type_text.as_deref().map(normalize_type_text)),
        type_paths: member.type_paths.clone(),
    }
}

fn variant_param(variant: &TypeVariantShape) -> ParameterShape {
    ParameterShape {
        name: SyntaxFact::Known(Some(variant.name.clone())),
        type_annotation: SyntaxFact::Known(None),
        type_paths: variant
            .members
            .iter()
            .flat_map(|member| member.type_paths.iter().cloned())
            .collect(),
    }
}

/// Fold an identifier to lowercase `_`-joined tokens so naming
/// conventions don't register as member renames. Identifiers with no
/// alphanumeric tokens pass through unchanged.
fn fold_identifier(name: &str) -> String {
    let tokens = identifier_tokens(name);
    if tokens.is_empty() {
        name.to_owned()
    } else {
        tokens.join("_")
    }
}

/// Normalize a type annotation's spelling so tokenizer-dependent spacing
/// (`Vec < String >` from `syn`, `Vec<String>` from a source slice) maps
/// to one text. Whitespace runs collapse; a single space survives only
/// between two word characters (`dyn Error`), everything else joins.
pub fn normalize_type_text(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            let prev_is_word = out.chars().next_back().is_some_and(is_word_char);
            if prev_is_word && is_word_char(ch) {
                out.push(' ');
            }
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn member(name: Option<&str>, type_text: Option<&str>, type_paths: &[&str]) -> TypeMemberShape {
        TypeMemberShape {
            name: name.map(str::to_owned),
            type_text: type_text.map(str::to_owned),
            type_paths: type_paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    fn record(name: &str, members: Vec<TypeMemberShape>) -> TypeShape {
        TypeShape {
            display_name: name.to_owned(),
            kind: TypeDefKind::Record,
            kind_label: "struct",
            members,
            variants: Vec::new(),
            generics: Vec::new(),
            doc: None,
            span: SourceSpan {
                start_line: 1,
                end_line: 4,
            },
            is_test: false,
        }
    }

    /// Both an empty record and an empty enum render as a bare root, so
    /// they score 1.0 against each other. The flag is what lets the
    /// corpus drop them before that happens.
    #[rstest]
    #[case::empty_record(TypeDefKind::Record, false, false, true)]
    #[case::empty_enum(TypeDefKind::Enum, false, false, true)]
    #[case::has_member(TypeDefKind::Record, true, false, false)]
    #[case::has_variant(TypeDefKind::Enum, false, true, false)]
    fn is_shapeless_when_no_member_and_no_variant(
        #[case] kind: TypeDefKind,
        #[case] with_member: bool,
        #[case] with_variant: bool,
        #[case] expected: bool,
    ) {
        let mut shape = record("Thing", Vec::new());
        shape.kind = kind;
        if with_member {
            shape.members.push(member(Some("id"), Some("u64"), &[]));
        }
        if with_variant {
            shape.variants.push(TypeVariantShape {
                name: "Only".to_owned(),
                members: Vec::new(),
            });
        }

        assert_eq!(shape.is_shapeless(), expected);
        assert_eq!(shape.tree().subtree_size() == 1, expected);
    }

    #[test]
    fn record_tree_renders_member_labels() {
        let shape = record(
            "User",
            vec![
                member(Some("userId"), Some("Vec < String >"), &["Vec", "String"]),
                member(Some("name"), None, &[]),
                member(None, Some("u32"), &[]),
            ],
        );

        let tree = shape.tree();

        assert_eq!(tree.label, "Record");
        assert_eq!(
            tree.children
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            ["Field(user_id: Vec<String>)", "Field(name)", "Field(u32)"],
        );
        assert!(tree.children.iter().all(|c| c.children.is_empty()));
    }

    #[test]
    fn enum_tree_renders_variants_with_payload_children() {
        let shape = TypeShape {
            display_name: "Event".to_owned(),
            kind: TypeDefKind::Enum,
            kind_label: "enum",
            members: Vec::new(),
            variants: vec![
                TypeVariantShape {
                    name: "Created".to_owned(),
                    members: vec![member(Some("id"), Some("u64"), &[])],
                },
                TypeVariantShape {
                    name: "Deleted".to_owned(),
                    members: Vec::new(),
                },
            ],
            generics: Vec::new(),
            doc: None,
            span: SourceSpan {
                start_line: 1,
                end_line: 5,
            },
            is_test: false,
        };

        let tree = shape.tree();

        assert_eq!(tree.label, "Enum");
        assert_eq!(tree.children[0].label, "Variant(created)");
        assert_eq!(tree.children[0].children[0].label, "Field(id: u64)");
        assert_eq!(tree.children[1].label, "Variant(deleted)");
        assert!(tree.children[1].children.is_empty());
    }

    #[test]
    fn alias_tree_renders_target_member() {
        let mut shape = record("UserId", vec![member(None, Some("Uuid"), &["Uuid"])]);
        shape.kind = TypeDefKind::Alias;
        shape.kind_label = "type_alias";

        let tree = shape.tree();

        assert_eq!(tree.label, "Alias");
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].label, "Field(Uuid)");
    }

    /// The same record spelled with different naming and spacing
    /// conventions must render identical trees — that is the whole
    /// point of folding member facts before they become labels.
    #[test]
    fn cross_convention_records_render_identical_trees() {
        let rust_side = record(
            "Summary",
            vec![member(Some("user_id"), Some("Vec < String >"), &[])],
        );
        let ts_side = record(
            "Summary",
            vec![member(Some("userId"), Some("Vec<String>"), &[])],
        );

        assert_eq!(rust_side.tree(), ts_side.tree());
    }

    #[test]
    fn member_signature_projects_members_as_parameters() {
        let shape = record(
            "UserProfile",
            vec![
                member(Some("id"), Some("UserId"), &["UserId"]),
                member(Some("emails"), Some("Vec<Email>"), &["Vec", "Email"]),
            ],
        );

        let sig = shape.member_signature();

        assert_eq!(sig.name_tokens().collect::<Vec<_>>(), ["user", "profile"]);
        assert_eq!(sig.parameter_count(), 2);
        assert_eq!(sig.parameter_names().collect::<Vec<_>>(), ["id", "emails"]);
        assert_eq!(
            sig.parameter_type_paths().collect::<Vec<_>>(),
            ["UserId", "Vec", "Email"],
        );
        assert_eq!(sig.receiver_shape(), Some(ReceiverShape::None));
    }

    #[test]
    fn enum_member_signature_projects_variants_as_parameters() {
        let shape = TypeShape {
            display_name: "Event".to_owned(),
            kind: TypeDefKind::Enum,
            kind_label: "enum",
            members: Vec::new(),
            variants: vec![TypeVariantShape {
                name: "Created".to_owned(),
                members: vec![member(Some("id"), Some("EventId"), &["EventId"])],
            }],
            generics: Vec::new(),
            doc: None,
            span: SourceSpan {
                start_line: 1,
                end_line: 3,
            },
            is_test: false,
        };

        let sig = shape.member_signature();

        assert_eq!(sig.parameter_names().collect::<Vec<_>>(), ["Created"]);
        assert_eq!(sig.parameter_type_paths().collect::<Vec<_>>(), ["EventId"]);
    }

    #[test]
    fn into_function_shape_carries_tree_signature_and_metadata() {
        let mut shape = record("User", vec![member(Some("id"), Some("u64"), &[])]);
        shape.doc = Some("A user.".to_owned());

        let expected_tree = shape.tree();
        let function = shape.into_function_shape();

        assert_eq!(function.display_name, "User");
        assert_eq!(function.body_tree(), &expected_tree);
        assert_eq!(function.doc.as_deref(), Some("A user."));
        assert_eq!(function.span.line_count(), 4);
        assert_eq!(
            function.signature_shape().map(|sig| sig.parameter_count()),
            Some(1),
        );
    }

    #[rstest]
    #[case::syn_token_spacing("Vec < String >", "Vec<String>")]
    #[case::already_tight("Vec<String>", "Vec<String>")]
    #[case::word_boundary_kept("dyn Error + Send", "dyn Error+Send")]
    #[case::reference("& str", "&str")]
    #[case::newlines_collapse("HashMap<\n  String,\n  u32,\n>", "HashMap<String,u32,>")]
    #[case::leading_trailing_ws("  u32  ", "u32")]
    #[case::empty("", "")]
    fn normalize_type_text_folds_spacing(#[case] raw: &str, #[case] expected: &str) {
        assert_eq!(normalize_type_text(raw), expected);
    }

    use proptest::collection::vec as prop_vec;
    use proptest::prelude::*;

    fn arb_member() -> impl Strategy<Value = TypeMemberShape> {
        (
            proptest::option::of("[A-Za-z_][A-Za-z0-9_]{0,8}"),
            proptest::option::of("[A-Za-z<>,& ]{0,12}"),
            prop_vec("[A-Za-z]{1,6}", 0..3),
        )
            .prop_map(|(name, type_text, type_paths)| TypeMemberShape {
                name,
                type_text,
                type_paths,
            })
    }

    fn arb_record() -> impl Strategy<Value = TypeShape> {
        ("[A-Za-z][A-Za-z0-9]{0,8}", prop_vec(arb_member(), 0..6)).prop_map(|(name, members)| {
            let mut shape = record("placeholder", members);
            shape.display_name = name;
            shape
        })
    }

    proptest! {
        /// The tree is exactly root + one node per member, and every
        /// label is non-empty — the invariants scoring relies on.
        #[test]
        fn record_tree_size_is_one_plus_member_count(shape in arb_record()) {
            let tree = shape.tree();
            prop_assert_eq!(tree.subtree_size(), 1 + shape.members.len());
            let mut stack = vec![&tree];
            while let Some(node) = stack.pop() {
                prop_assert!(!node.label.is_empty());
                stack.extend(node.children.iter());
            }
        }

        /// Normalization is idempotent and never yields leading or
        /// trailing whitespace.
        #[test]
        fn normalize_type_text_is_idempotent(raw in "[A-Za-z<>,&_ \n\t]{0,24}") {
            let once = normalize_type_text(&raw);
            prop_assert_eq!(&normalize_type_text(&once), &once);
            prop_assert!(once.trim() == once);
        }
    }
}
