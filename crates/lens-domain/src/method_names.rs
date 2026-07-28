//! Name tables that switch the resolver's name matching off.
//!
//! A syntax-only pipeline resolves a callee by name, which is sound for
//! names a workspace invented and worthless for two families of name it
//! never owns:
//!
//! * **Ubiquitous method names.** A receiver call (`recv.foo()`) carries
//!   no type information, so `.clone()`, `.get()`, `.map()`,
//!   `.append()`, `.String()` can only be matched by name — and nearly
//!   every such site targets the standard library.
//! * **Builtin function names.** A builtin is called bare
//!   (`append(xs, x)`, `len(s)`, `parseInt(s)`), so it never reaches the
//!   receiver table, yet the same name match applies.
//!
//! Either way a single workspace symbol sharing the name absorbs every
//! call site in the corpus and becomes a phantom hub.
//!
//! The tables themselves are language conventions, so they live next to
//! the adapters ([`lens_rust`], [`lens_ts`], [`lens_py`],
//! [`lens_golang`]); this module only holds the shared lookup shape so
//! every adapter answers the question the same way.

/// Sorted, deduplicated name table backing the public tables.
///
/// Private so the two tables stay distinct types at the call site: a
/// builtin table must not be accepted where a receiver-call table is
/// expected, because the resolver consults them at different points and
/// for different reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SortedNames(&'static [&'static str]);

impl SortedNames {
    const fn new(names: &'static [&'static str]) -> Self {
        Self(names)
    }

    fn contains(self, name: &str) -> bool {
        self.0.binary_search(&name).is_ok()
    }

    fn is_sorted_and_deduped(self) -> bool {
        self.0.windows(2).all(|pair| pair[0] < pair[1])
    }
}

/// Declare a public newtype over [`SortedNames`] with the same lookup
/// surface, so each table documents what its entries mean without
/// re-implementing the search.
macro_rules! name_table {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $name(SortedNames);

        impl $name {
            #[must_use]
            pub const fn new(names: &'static [&'static str]) -> Self {
                Self(SortedNames::new(names))
            }

            /// Whether `name` is in the table.
            #[must_use]
            pub fn contains(self, name: &str) -> bool {
                self.0.contains(name)
            }

            /// Table invariant [`Self::contains`] depends on. Adapters
            /// assert this in their own tests so a hand-edited table
            /// cannot silently start missing entries.
            #[must_use]
            pub fn is_sorted_and_deduped(self) -> bool {
                self.0.is_sorted_and_deduped()
            }
        }
    };
}

name_table!(
    /// Method names whose presence in a workspace is no evidence that a
    /// receiver call targets them.
    ///
    /// Construct with [`UbiquitousMethodNames::new`] from a `const`
    /// slice; [`UbiquitousMethodNames::contains`] binary-searches it, so
    /// the slice must be sorted.
    UbiquitousMethodNames
);

name_table!(
    /// Names the language itself defines as bare-callable functions, so
    /// a call site spelling one of them is not a workspace call however
    /// the workspace happens to name its own symbols.
    ///
    /// Entries are limited to names the language reserves or that no
    /// project realistically redefines at module scope; a name a project
    /// might plausibly own belongs in neither table, since name matching
    /// is the resolver's main source of true positives.
    BuiltinFunctionNames
);

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const TABLE: UbiquitousMethodNames = UbiquitousMethodNames::new(&["clone", "get", "map"]);
    const BUILTINS: BuiltinFunctionNames = BuiltinFunctionNames::new(&["append", "len"]);

    #[rstest]
    #[case::first("clone", true)]
    #[case::middle("get", true)]
    #[case::last("map", true)]
    #[case::absent("with_children", false)]
    #[case::empty_name("", false)]
    fn contains_matches_table_entries(#[case] name: &str, #[case] expected: bool) {
        assert_eq!(TABLE.contains(name), expected);
    }

    #[test]
    fn sortedness_check_rejects_unsorted_and_duplicated_tables() {
        assert!(TABLE.is_sorted_and_deduped());
        assert!(UbiquitousMethodNames::new(&[]).is_sorted_and_deduped());
        assert!(!UbiquitousMethodNames::new(&["get", "clone"]).is_sorted_and_deduped());
        assert!(!UbiquitousMethodNames::new(&["get", "get"]).is_sorted_and_deduped());
    }

    #[test]
    fn empty_table_contains_nothing() {
        assert!(!UbiquitousMethodNames::new(&[]).contains("clone"));
    }

    #[rstest]
    #[case::present("append", true)]
    #[case::present_last("len", true)]
    #[case::absent("clone", false)]
    fn builtin_table_shares_the_lookup_surface(#[case] name: &str, #[case] expected: bool) {
        assert_eq!(BUILTINS.contains(name), expected);
        assert!(BUILTINS.is_sorted_and_deduped());
    }
}
