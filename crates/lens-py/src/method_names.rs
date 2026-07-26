//! Python method names that carry no attribution evidence.
//!
//! `rows.append(x)` says nothing about `rows`, so the call-graph
//! resolver can only match `append` against workspace function names —
//! and one workspace `append` will absorb every list mutation in the
//! tree. The table below lists the names where that match is worthless:
//! the `list`, `dict`, `set`, `str`, `bytes`, file-object, and `re`
//! match-object methods that appear in nearly every function body.
//!
//! Names a project invented stay out: matching those by name is the
//! resolver's main source of true positives.

use lens_domain::UbiquitousMethodNames;

/// Python's ubiquitous method names, sorted for binary search.
pub const UBIQUITOUS_METHOD_NAMES: UbiquitousMethodNames = UbiquitousMethodNames::new(&[
    "add",
    "append",
    "capitalize",
    "casefold",
    "clear",
    "close",
    "copy",
    "count",
    "decode",
    "difference",
    "discard",
    "encode",
    "endswith",
    "extend",
    "find",
    "finditer",
    "flush",
    "format",
    "format_map",
    "fromkeys",
    "get",
    "getvalue",
    "group",
    "groupdict",
    "groups",
    "index",
    "intersection",
    "isalnum",
    "isalpha",
    "isdigit",
    "islower",
    "isnumeric",
    "isspace",
    "isupper",
    "items",
    "join",
    "keys",
    "ljust",
    "lower",
    "lstrip",
    "match",
    "partition",
    "pop",
    "popitem",
    "read",
    "readline",
    "readlines",
    "remove",
    "replace",
    "reverse",
    "rfind",
    "rindex",
    "rjust",
    "rpartition",
    "rsplit",
    "rstrip",
    "search",
    "seek",
    "setdefault",
    "sort",
    "split",
    "splitlines",
    "startswith",
    "strip",
    "sub",
    "subn",
    "symmetric_difference",
    "tell",
    "title",
    "translate",
    "union",
    "update",
    "upper",
    "values",
    "write",
    "writelines",
    "zfill",
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
    #[case::list("append", true)]
    #[case::dict("items", true)]
    #[case::string("startswith", true)]
    #[case::regex("group", true)]
    #[case::project_specific("visit_expr", false)]
    #[case::project_specific_short("walk", false)]
    fn table_separates_builtin_names_from_project_names(
        #[case] name: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(UBIQUITOUS_METHOD_NAMES.contains(name), expected);
    }
}
