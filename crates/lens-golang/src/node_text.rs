//! Checked source-text extraction for tree-sitter nodes.
//!
//! `Node::utf8_text` is fallible: it slices the source buffer by the
//! node's byte range and validates the result as UTF-8. In practice the
//! buffer always comes from a `&str`, so the failure is unreachable —
//! but the three copies of `node_text` this module replaces reacted to
//! it with `unwrap_or_default()`, which turns a parse failure into an
//! *empty identifier in the report*. An agent reading that output has
//! no way to tell an empty name from a name that legitimately isn't
//! there.
//!
//! So every read goes through [`node_str`], which logs the failure to
//! stderr through `tracing` and hands back `None`. Callers then decide
//! explicitly: drop the finding (most of them), or fall back to an
//! empty value where empty is already the meaningful answer (only
//! `TreeNode::value`, where non-identifier nodes carry no text either).

use tree_sitter::Node;

/// Source text of `node`, or `None` when its byte range is not valid
/// UTF-8 — in which case the failure is logged rather than swallowed.
pub(crate) fn node_str<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    match node.utf8_text(source) {
        Ok(text) => Some(text),
        Err(error) => {
            let range = node.byte_range();
            tracing::warn!(
                kind = node.kind(),
                line = node.start_position().row + 1,
                start_byte = range.start,
                end_byte = range.end,
                %error,
                "go: node text is not valid UTF-8; dropping it from the analysis",
            );
            None
        }
    }
}

/// Owned source text of `node`, empty when the text is unreadable.
///
/// Only for callers where an empty string is already a meaningful
/// answer. Anything that names something in a report should use
/// [`node_str`] and skip the item instead.
pub(crate) fn node_text_or_empty(node: Node<'_>, source: &[u8]) -> String {
    node_str(node, source).unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_tree;
    use crate::test_support::capture_logs;

    fn tree_root(source: &str) -> (tree_sitter::Tree, String) {
        let tree = parse_tree(source).expect("parse");
        (tree, source.to_owned())
    }

    #[test]
    fn node_str_returns_text_for_valid_utf8() {
        let source = "package main\n\nfunc Ünïcode() {}\n";
        let (tree, owned) = tree_root(source);
        let decl = tree
            .root_node()
            .named_child(1)
            .expect("function declaration");
        let name = decl.child_by_field_name("name").expect("name");

        assert_eq!(node_str(name, owned.as_bytes()), Some("Ünïcode"));
        assert_eq!(node_text_or_empty(name, owned.as_bytes()), "Ünïcode");
    }

    #[test]
    fn node_str_reports_none_and_logs_when_the_byte_range_is_not_utf8() {
        // Same byte length as the parsed source so the node's range is
        // in bounds, but the identifier bytes are not valid UTF-8.
        let source = "package main\n\nfunc Ünïcode() {}\n";
        let (tree, _) = tree_root(source);
        let decl = tree
            .root_node()
            .named_child(1)
            .expect("function declaration");
        let name = decl.child_by_field_name("name").expect("name");

        let mut corrupted = source.as_bytes().to_vec();
        for byte in &mut corrupted[name.byte_range()] {
            *byte = 0xFF;
        }

        let (text, logs) = capture_logs(|| node_str(name, &corrupted));

        assert_eq!(text, None);
        assert!(logs.contains("WARN"), "expected a warning, got: {logs}");
        assert!(
            logs.contains("not valid UTF-8"),
            "expected the UTF-8 diagnostic, got: {logs}"
        );
        assert!(
            logs.contains("line=3"),
            "expected the source line in the diagnostic, got: {logs}"
        );
    }

    /// The empty-string fallback is only acceptable because it is
    /// announced: an unannounced empty identifier is exactly the
    /// silent degradation this module exists to remove.
    #[test]
    fn node_text_or_empty_still_logs_before_falling_back() {
        let source = "package main\n\nfunc Ünïcode() {}\n";
        let (tree, _) = tree_root(source);
        let decl = tree
            .root_node()
            .named_child(1)
            .expect("function declaration");
        let name = decl.child_by_field_name("name").expect("name");

        let mut corrupted = source.as_bytes().to_vec();
        for byte in &mut corrupted[name.byte_range()] {
            *byte = 0xFF;
        }

        let (text, logs) = capture_logs(|| node_text_or_empty(name, &corrupted));

        assert_eq!(text, "");
        assert!(logs.contains("WARN"), "expected a warning, got: {logs}");
    }
}
