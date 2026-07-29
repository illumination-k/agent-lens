//! Single-pass walk over the function-shaped declarations at the top
//! level of a Go file: `func name(...)` (`function_declaration`) and
//! `func (r T) name(...)` (`method_declaration`).
//!
//! Every extractor in this crate — parser, call index (twice), complexity,
//! wrapper, cohesion — used to spell out the same ladder: iterate the
//! root's named children, match those two node kinds, and resolve the
//! receiver type for the method arm. Six copies of one traversal meant six
//! places to update when the shape changed. Consumers now implement the
//! leaf instead, which is the only part that differed.
//!
//! Declarations without a body (`func f()` bodies supplied by assembly)
//! or without a resolvable name are skipped, matching what every consumer
//! did on its own: each one bailed out of the same two `Option`s.
//!
//! Closures (`func_literal`) are deliberately not emitted — their
//! containing function is the unit of analysis, as documented in
//! [`crate::parser`]. This walker is the counterpart of `lens-ts`'s
//! `walk.rs` and `lens-rust`'s `common::walk_fn_items`; new analyzers
//! should call it rather than rewriting the ladder.

use tree_sitter::Node;

use crate::parser::{function_name_text, method_receiver_type};

/// One function-shaped declaration found by [`walk_top_level_fns`].
pub(crate) struct FnSite<'tree, 'src> {
    /// The `function_declaration` / `method_declaration` node itself.
    pub node: Node<'tree>,
    /// Its `body` block. Guaranteed present.
    pub body: Node<'tree>,
    /// The declared name, unqualified (`Get`, not `Client::Get`).
    pub name: &'src str,
    /// Receiver type for a method (`*Client` and `Client` both fold to
    /// `Client`). `None` for free functions, and also for a method whose
    /// receiver type cannot be resolved — consumers that need to tell
    /// those apart read [`is_method`](Self::is_method).
    pub owner: Option<String>,
    /// True for `method_declaration` nodes.
    pub is_method: bool,
}

/// Walk `root`'s named children and emit one [`FnSite`] per top-level
/// function or method, in source order.
pub(crate) fn walk_top_level_fns<'tree, 'src, F>(
    root: Node<'tree>,
    source: &'src [u8],
    visit: &mut F,
) where
    F: FnMut(FnSite<'tree, 'src>),
{
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        let is_method = match child.kind() {
            "function_declaration" => false,
            "method_declaration" => true,
            _ => continue,
        };
        let Some(body) = child.child_by_field_name("body") else {
            continue;
        };
        let Some(name) = function_name_text(child, source) else {
            continue;
        };
        let owner = is_method
            .then(|| method_receiver_type(child, source))
            .flatten();
        visit(FnSite {
            node: child,
            body,
            name,
            owner,
            is_method,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_tree;

    fn sites(source: &str) -> Vec<(String, Option<String>, bool)> {
        let tree = parse_tree(source).expect("parses");
        let bytes = source.as_bytes();
        let mut out = Vec::new();
        walk_top_level_fns(tree.root_node(), bytes, &mut |site| {
            out.push((site.name.to_owned(), site.owner.clone(), site.is_method));
        });
        out
    }

    #[test]
    fn emits_functions_and_methods_in_source_order() {
        let source = r#"
package main

func Alpha() {}

func (c *Client) Beta() {}

func (c Client) Gamma() {}

type Client struct{}
"#;
        assert_eq!(
            sites(source),
            [
                ("Alpha".to_owned(), None, false),
                ("Beta".to_owned(), Some("Client".to_owned()), true),
                ("Gamma".to_owned(), Some("Client".to_owned()), true),
            ]
        );
    }

    #[test]
    fn skips_declarations_without_a_body() {
        // Bodies supplied elsewhere (assembly) parse fine but have no
        // `body` field, and no consumer can do anything with them.
        let source = "package main\n\nfunc asmOnly(x int) int\n\nfunc withBody() {}\n";
        assert_eq!(sites(source), [("withBody".to_owned(), None, false)]);
    }

    #[test]
    fn skips_non_function_top_level_declarations() {
        let source = "package main\n\nimport \"fmt\"\n\nvar x = 1\n\ntype T struct{}\n";
        assert!(sites(source).is_empty());
    }

    #[test]
    fn nested_closures_are_left_to_their_enclosing_function() {
        let source = "package main\n\nfunc Outer() {\n\tinner := func() {}\n\t_ = inner\n}\n";
        assert_eq!(sites(source), [("Outer".to_owned(), None, false)]);
    }
}
