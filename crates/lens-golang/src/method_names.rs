//! Go names that carry no attribution evidence: ubiquitous methods and
//! the predeclared builtins.
//!
//! `v.String()` says nothing about `v`, so the call-graph resolver can
//! only match `String` against workspace function names — and one
//! workspace `String` will absorb every `fmt.Stringer` call in the
//! module. The table below lists the names where that match is
//! worthless: the methods of the standard interfaces every Go package
//! implements or consumes (`fmt.Stringer`, `error`, `io`,
//! `encoding/json`, `sort.Interface`, `sync`, `context`, `testing.TB`).
//!
//! Package-qualified calls (`fmt.Sprintf`) and type-qualified calls
//! (`Foo.Method`) are not receiver calls in this adapter's call shapes,
//! so they are unaffected by the table.
//!
//! Names a project invented stay out: matching those by name is the
//! resolver's main source of true positives. Reflection-flavoured
//! accessors (`Name`, `Kind`, `Type`, `Field`, `Value`) are deliberately
//! absent — they are ubiquitous only in reflection-heavy code and are
//! far more often a project's own accessor.

use lens_domain::{BuiltinFunctionNames, UbiquitousMethodNames};

/// Go's ubiquitous method names, sorted for binary search.
pub const UBIQUITOUS_METHOD_NAMES: UbiquitousMethodNames = UbiquitousMethodNames::new(&[
    "As",
    "Bytes",
    "Cap",
    "Cleanup",
    "Close",
    "Deadline",
    "Done",
    "Err",
    "Error",
    "Errorf",
    "Fatal",
    "Fatalf",
    "Flush",
    "Grow",
    "Helper",
    "Is",
    "Len",
    "Less",
    "Lock",
    "Log",
    "Logf",
    "MarshalJSON",
    "MarshalText",
    "Next",
    "Parallel",
    "RLock",
    "RUnlock",
    "Read",
    "ReadFrom",
    "Reset",
    "Scan",
    "Seek",
    "Setenv",
    "Skip",
    "String",
    "Swap",
    "TempDir",
    "Unlock",
    "UnmarshalJSON",
    "UnmarshalText",
    "Unwrap",
    "Wait",
    "Write",
    "WriteString",
    "WriteTo",
]);

/// Go's predeclared functions, sorted for binary search.
///
/// These are called bare (`append(xs, x)`, `len(s)`), so they never
/// reach [`UBIQUITOUS_METHOD_NAMES`] — but the resolver's name fallback
/// still applies, and a module that happens to define a function or
/// method called `append` would otherwise absorb every builtin call site
/// in the corpus. The list is the Go spec's predeclared function set,
/// which is closed: a project cannot add to it, and shadowing one is
/// legal but never makes a call in *another* package mean the shadow.
pub const BUILTIN_FUNCTION_NAMES: BuiltinFunctionNames = BuiltinFunctionNames::new(&[
    "append", "cap", "clear", "close", "complex", "copy", "delete", "imag", "len", "make", "max",
    "min", "new", "panic", "print", "println", "real", "recover",
]);

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn table_is_sorted_and_deduped() {
        assert!(UBIQUITOUS_METHOD_NAMES.is_sorted_and_deduped());
        assert!(BUILTIN_FUNCTION_NAMES.is_sorted_and_deduped());
    }

    #[rstest]
    #[case::slice_growth("append", true)]
    #[case::allocation("make", true)]
    #[case::length("len", true)]
    #[case::channel_close("close", true)]
    #[case::stdlib_method_not_a_builtin("String", false)]
    #[case::project_specific("ServeIndex", false)]
    fn builtin_table_covers_the_predeclared_functions(#[case] name: &str, #[case] expected: bool) {
        assert_eq!(BUILTIN_FUNCTION_NAMES.contains(name), expected);
    }

    #[rstest]
    #[case::stringer("String", true)]
    #[case::error("Error", true)]
    #[case::io("Write", true)]
    #[case::sort_interface("Less", true)]
    #[case::project_specific("ServeIndex", false)]
    #[case::reflection_accessor("Kind", false)]
    fn table_separates_stdlib_names_from_project_names(#[case] name: &str, #[case] expected: bool) {
        assert_eq!(UBIQUITOUS_METHOD_NAMES.contains(name), expected);
    }
}
