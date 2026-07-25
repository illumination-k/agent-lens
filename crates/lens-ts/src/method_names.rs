//! TypeScript / JavaScript method names that carry no attribution
//! evidence.
//!
//! `xs.map(f)` says nothing about `xs`, so the call-graph resolver can
//! only match `map` against workspace function names — and one
//! workspace `map` will absorb every array traversal in the tree. The
//! table below lists the names where that match is worthless: the
//! `Array`, `String`, `Object`, `Map`, `Set`, `Promise`, `RegExp`,
//! `JSON`, and `console` members that appear in nearly every function
//! body, including the static ones (`Object.assign`, `Promise.all`)
//! since those parse as receiver calls too.
//!
//! Names a project invented stay out: matching those by name is the
//! resolver's main source of true positives.

use lens_domain::UbiquitousMethodNames;

/// TypeScript / JavaScript's ubiquitous method names, sorted for binary
/// search.
pub const UBIQUITOUS_METHOD_NAMES: UbiquitousMethodNames = UbiquitousMethodNames::new(&[
    "add",
    "all",
    "apply",
    "assign",
    "at",
    "bind",
    "call",
    "catch",
    "charAt",
    "charCodeAt",
    "clear",
    "codePointAt",
    "concat",
    "copyWithin",
    "create",
    "debug",
    "delete",
    "dir",
    "endsWith",
    "entries",
    "error",
    "every",
    "fill",
    "filter",
    "finally",
    "find",
    "findIndex",
    "findLast",
    "findLastIndex",
    "flat",
    "flatMap",
    "forEach",
    "freeze",
    "from",
    "fromEntries",
    "get",
    "getTime",
    "groupBy",
    "has",
    "hasOwnProperty",
    "includes",
    "indexOf",
    "info",
    "isArray",
    "join",
    "keys",
    "lastIndexOf",
    "localeCompare",
    "log",
    "map",
    "match",
    "matchAll",
    "next",
    "of",
    "padEnd",
    "padStart",
    "parse",
    "pop",
    "push",
    "race",
    "reduce",
    "reduceRight",
    "reject",
    "repeat",
    "replace",
    "replaceAll",
    "resolve",
    "reverse",
    "search",
    "set",
    "shift",
    "slice",
    "some",
    "sort",
    "splice",
    "split",
    "startsWith",
    "stringify",
    "substring",
    "test",
    "then",
    "toFixed",
    "toISOString",
    "toJSON",
    "toLocaleString",
    "toLowerCase",
    "toString",
    "toUpperCase",
    "trim",
    "trimEnd",
    "trimStart",
    "unshift",
    "valueOf",
    "values",
    "warn",
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
    #[case::array("map", true)]
    #[case::promise("then", true)]
    #[case::map_get("get", true)]
    #[case::console("log", true)]
    #[case::project_specific("renderDiagnostics", false)]
    #[case::project_specific_short("lineFor", false)]
    fn table_separates_builtin_names_from_project_names(
        #[case] name: &str,
        #[case] expected: bool,
    ) {
        assert_eq!(UBIQUITOUS_METHOD_NAMES.contains(name), expected);
    }
}
