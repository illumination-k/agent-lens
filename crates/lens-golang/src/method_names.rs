//! Go method names that carry no attribution evidence.
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

use lens_domain::UbiquitousMethodNames;

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

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn table_is_sorted_and_deduped() {
        assert!(UBIQUITOUS_METHOD_NAMES.is_sorted_and_deduped());
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
