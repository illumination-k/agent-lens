//! Textual scan of the `pub use` items in a crate root: the names and
//! glob prefixes it re-exports.

use std::collections::BTreeSet;

use crate::analyze::call_graph::model::name_last_segment;

/// Collect the identifiers and glob prefixes a source file re-exports
/// with `pub use`. Statements are taken line-first so a `pub use` inside
/// a doc comment or string cannot contribute.
pub(super) fn parse_pub_use(source: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut names = BTreeSet::new();
    let mut globs = BTreeSet::new();
    let mut statement: Option<String> = None;
    for line in source.lines() {
        let line = line.trim();
        let body = match statement.as_mut() {
            Some(pending) => {
                pending.push(' ');
                pending.push_str(line);
                pending
            }
            None => {
                let Some(rest) = line.strip_prefix("pub use ") else {
                    continue;
                };
                statement.insert(rest.to_owned())
            }
        };
        let Some(end) = body.find(';') else {
            continue;
        };
        let body = body[..end].to_owned();
        statement = None;
        collect_use_tree(&body, "", &mut names, &mut globs);
    }
    (names, globs)
}

/// Split one `pub use` body into the names and glob prefixes it exposes,
/// recursing so a nested group (`a::{b, c::{d, e}}`) contributes its
/// leaves rather than the raw group text.
fn collect_use_tree(
    body: &str,
    prefix: &str,
    names: &mut BTreeSet<String>,
    globs: &mut BTreeSet<String>,
) {
    let body = body.trim();
    let Some(open) = body.find('{') else {
        collect_use_leaf(body, prefix, names, globs);
        return;
    };
    let inner_prefix = join_use_path(prefix, body[..open].trim().trim_end_matches("::"));
    for item in split_top_level(brace_group(&body[open..])) {
        collect_use_tree(item, &inner_prefix, names, globs);
    }
}

/// The text inside the brace group starting at `body`, which begins with
/// `{`, up to its matching close brace.
fn brace_group(body: &str) -> &str {
    let mut depth = 0usize;
    for (offset, ch) in body.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return &body[1..offset];
                }
            }
            _ => {}
        }
    }
    &body[1..]
}

fn join_use_path(prefix: &str, rest: &str) -> String {
    match (prefix.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_owned(),
        (_, true) => prefix.to_owned(),
        _ => format!("{prefix}::{rest}"),
    }
}

/// One leaf of a use tree: `foo`, `foo as bar`, `a::b`, or `a::*`.
fn collect_use_leaf(
    leaf: &str,
    prefix: &str,
    names: &mut BTreeSet<String>,
    globs: &mut BTreeSet<String>,
) {
    let path = leaf.split(" as ").next().unwrap_or(leaf).trim();
    if path.is_empty() {
        return;
    }
    let full = join_use_path(prefix, path);
    match full.strip_suffix("::*").or(full.strip_suffix('*')) {
        Some(glob) => {
            globs.insert(glob.trim_end_matches("::").to_owned());
        }
        None => {
            names.insert(name_last_segment(&full).to_owned());
        }
    }
}

/// Split a brace group on top-level commas, keeping nested groups whole.
fn split_top_level(inner: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (offset, ch) in inner.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(&inner[start..offset]);
                start = offset + 1;
            }
            _ => {}
        }
    }
    items.push(&inner[start..]);
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::plain("pub use inner::target;", &["target"], &[])]
    #[case::alias("pub use inner::target as renamed;", &["target"], &[])]
    #[case::group("pub use inner::{one, two as three};", &["one", "two"], &[])]
    #[case::glob("pub use inner::*;", &[], &["inner"])]
    #[case::group_with_glob("pub use inner::{one, deep::*};", &["one"], &["inner::deep"])]
    #[case::nested_path("pub use a::b::target;", &["target"], &[])]
    #[case::crate_prefix("pub use crate::inner::*;", &[], &["crate::inner"])]
    #[case::nested_group("pub use a::{b, c::{d, e}};", &["b", "d", "e"], &[])]
    #[case::nested_group_with_glob("pub use a::{b, c::{d, *}};", &["b", "d"], &["a::c"])]
    #[case::sibling_after_a_nested_group("pub use a::{b::{c}, d};", &["c", "d"], &[])]
    #[case::not_public("use inner::target;", &[], &[])]
    #[case::in_a_comment("// pub use inner::target;", &[], &[])]
    fn pub_use_statements_yield_their_names_and_globs(
        #[case] source: &str,
        #[case] expected_names: &[&str],
        #[case] expected_globs: &[&str],
    ) {
        let (names, globs) = parse_pub_use(source);
        assert_eq!(
            names.iter().map(String::as_str).collect::<Vec<_>>(),
            expected_names
        );
        assert_eq!(
            globs.iter().map(String::as_str).collect::<Vec<_>>(),
            expected_globs
        );
    }

    #[rstest]
    #[case::both("a", "b", "a::b")]
    #[case::no_prefix("", "b", "b")]
    #[case::no_rest("a", "", "a")]
    #[case::neither("", "", "")]
    fn use_paths_join_without_dangling_separators(
        #[case] prefix: &str,
        #[case] rest: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(join_use_path(prefix, rest), expected);
    }

    #[rstest]
    #[case::flat("a, b", &["a", " b"])]
    #[case::nested_group_stays_whole("a, b::{c, d}, e", &["a", " b::{c, d}", " e"])]
    #[case::single_item("only", &["only"])]
    #[case::empty("", &[""])]
    fn brace_groups_split_on_top_level_commas_only(#[case] inner: &str, #[case] expected: &[&str]) {
        assert_eq!(split_top_level(inner), expected);
    }

    /// A brace group split across lines is one statement, and the
    /// terminating `;` is what ends it.
    #[test]
    fn a_multi_line_pub_use_is_read_as_one_statement() {
        let (names, globs) = parse_pub_use(
            "pub use inner::{\n    one,\n    two as renamed,\n};\npub fn not_a_use() {}\n",
        );
        assert_eq!(
            names.iter().map(String::as_str).collect::<Vec<_>>(),
            ["one", "two"]
        );
        assert!(globs.is_empty());
    }
}
