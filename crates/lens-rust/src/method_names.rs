//! Rust method names that carry no attribution evidence.
//!
//! `recv.clone()` says nothing about `recv`'s type, so the call-graph
//! resolver can only match `clone` against workspace function names —
//! and a workspace `Foo::clone` will match every `.clone()` in the tree.
//! The table below lists the names where that match is worthless:
//! `std`/`core` trait methods (`Clone`, `Ord`, `AsRef`, `Iterator`,
//! `Display`, `Deref`, …) and the inherent methods `Option`, `Result`,
//! `Vec`, `HashMap`, `str`, `String`, and `Path` define on values that
//! appear in nearly every function body.
//!
//! Names a workspace invented (`with_children`, `known_value`) stay out:
//! matching those by name is the resolver's main source of true
//! positives. The bar for adding an entry is that `std` defines it on
//! several unrelated types, so a receiver call is far more likely to
//! reach `std` than the workspace match.
//!
//! The table only gates *receiver* calls. A typed path call
//! (`Foo::clone(x)`) carries the owner in the path, so it resolves
//! normally regardless of what is listed here.

use lens_domain::UbiquitousMethodNames;

/// Rust's ubiquitous method names, sorted for binary search.
pub const UBIQUITOUS_METHOD_NAMES: UbiquitousMethodNames = UbiquitousMethodNames::new(&[
    "and_then",
    "any",
    "append",
    "as_bytes",
    "as_deref",
    "as_mut",
    "as_path",
    "as_ref",
    "as_slice",
    "as_str",
    "binary_search",
    "borrow",
    "borrow_mut",
    "by_ref",
    "bytes",
    "canonicalize",
    "capacity",
    "chain",
    "char_indices",
    "chars",
    "clear",
    "clone",
    "clone_from",
    "cloned",
    "cmp",
    "collect",
    "concat",
    "contains",
    "contains_key",
    "copied",
    "count",
    "dedup",
    "deref",
    "deref_mut",
    "display",
    "drain",
    "drop",
    "ends_with",
    "entry",
    "enumerate",
    "eq",
    "err",
    "exists",
    "expect",
    "expect_err",
    "extend",
    "extend_from_slice",
    "file_name",
    "filter",
    "filter_map",
    "find",
    "find_map",
    "first",
    "flat_map",
    "flatten",
    "flush",
    "fmt",
    "fold",
    "for_each",
    "get",
    "get_mut",
    "get_or_insert",
    "get_or_insert_with",
    "hash",
    "insert",
    "insert_str",
    "into",
    "into_iter",
    "into_keys",
    "into_values",
    "is_dir",
    "is_empty",
    "is_err",
    "is_file",
    "is_none",
    "is_none_or",
    "is_ok",
    "is_some",
    "is_some_and",
    "iter",
    "iter_mut",
    "join",
    "keys",
    "last",
    "len",
    "lines",
    "map",
    "map_err",
    "map_or",
    "map_or_else",
    "map_while",
    "matches",
    "max",
    "metadata",
    "min",
    "ne",
    "next",
    "next_back",
    "nth",
    "ok",
    "ok_or",
    "ok_or_else",
    "or_default",
    "or_else",
    "or_insert",
    "or_insert_with",
    "parent",
    "parse",
    "partial_cmp",
    "peek",
    "peekable",
    "pop",
    "position",
    "push",
    "push_str",
    "remove",
    "repeat",
    "replace",
    "reserve",
    "retain",
    "rev",
    "reverse",
    "rsplit",
    "rsplit_once",
    "skip",
    "skip_while",
    "sort",
    "sort_by",
    "sort_by_key",
    "sort_unstable",
    "sort_unstable_by",
    "source",
    "split",
    "split_at",
    "split_off",
    "split_once",
    "split_whitespace",
    "splitn",
    "starts_with",
    "step_by",
    "strip_prefix",
    "strip_suffix",
    "sum",
    "swap",
    "take",
    "take_while",
    "to_ascii_lowercase",
    "to_ascii_uppercase",
    "to_lowercase",
    "to_owned",
    "to_path_buf",
    "to_str",
    "to_string",
    "to_string_lossy",
    "to_uppercase",
    "to_vec",
    "trim",
    "trim_end",
    "trim_start",
    "truncate",
    "try_into",
    "unwrap",
    "unwrap_err",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "unzip",
    "values",
    "values_mut",
    "windows",
    "with_extension",
    "write",
    "write_all",
    "write_fmt",
    "write_str",
    "zip",
]);

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn table_is_sorted_and_deduped() {
        assert!(UBIQUITOUS_METHOD_NAMES.is_sorted_and_deduped());
    }

    #[rstest]
    #[case::clone("clone", true)]
    #[case::get("get", true)]
    #[case::cmp("cmp", true)]
    #[case::as_ref("as_ref", true)]
    #[case::parent("parent", true)]
    #[case::workspace_builder("with_children", false)]
    #[case::workspace_accessor("known_value", false)]
    #[case::workspace_leaf("leaf", false)]
    #[case::workspace_line("line", false)]
    fn table_separates_std_names_from_workspace_names(#[case] name: &str, #[case] expected: bool) {
        assert_eq!(UBIQUITOUS_METHOD_NAMES.contains(name), expected);
    }
}
