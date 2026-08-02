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

use lens_domain::{BuiltinFunctionNames, InertAttributeNames, UbiquitousMethodNames};

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

/// Python builtins that are effectively never redefined at module
/// scope, sorted for binary search.
///
/// Deliberately a fraction of `builtins`: Python lets a module define
/// its own `filter`, `format`, `sum`, or `list`, and such a definition
/// is a genuine call target the resolver should keep matching. The bar
/// for an entry is that redefining the name at module scope would be
/// pathological, so a bare call to it is never a workspace call.
pub const BUILTIN_FUNCTION_NAMES: BuiltinFunctionNames = BuiltinFunctionNames::new(&[
    "enumerate",
    "getattr",
    "hasattr",
    "isinstance",
    "issubclass",
    "len",
    "print",
    "range",
    "repr",
    "setattr",
    "super",
    "zip",
]);

/// Decorator names that cannot make a function reachable.
///
/// Empty because the adapter does not extract decorators yet: it reports
/// [`lens_domain::SyntaxFact::Unknown`] for a function's annotations, so
/// nothing ever reaches this table. It exists so the per-language lookup
/// is total, and an empty table is the safe reading — every decorator
/// would count as "something may call this".
pub const INERT_ATTRIBUTE_NAMES: InertAttributeNames = InertAttributeNames::new(&[]);

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn table_is_sorted_and_deduped() {
        assert!(UBIQUITOUS_METHOD_NAMES.is_sorted_and_deduped());
        assert!(BUILTIN_FUNCTION_NAMES.is_sorted_and_deduped());
        assert!(INERT_ATTRIBUTE_NAMES.is_sorted_and_deduped());
    }

    #[rstest]
    #[case::length("len", true)]
    #[case::iteration("enumerate", true)]
    #[case::attribute_access("getattr", true)]
    #[case::shadowable_builtin("filter", false)]
    #[case::shadowable_constructor("dict", false)]
    #[case::project_specific("build_index", false)]
    fn builtin_table_keeps_shadowable_names_out(#[case] name: &str, #[case] expected: bool) {
        assert_eq!(BUILTIN_FUNCTION_NAMES.contains(name), expected);
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
