//! Ubiquitous method-name tables.
//!
//! A receiver call (`recv.foo()`) carries no type information in a
//! syntax-only pipeline, so the call-graph resolver can only match the
//! callee by name. That is sound for names a workspace invented and
//! useless for names the language's standard library defines on nearly
//! every value: `.clone()`, `.get()`, `.map()`, `.append()`, `.String()`.
//! Matching those against a workspace function turns each of them into a
//! phantom hub.
//!
//! The tables themselves are language conventions, so they live next to
//! the adapters ([`lens_rust`], [`lens_ts`], [`lens_py`],
//! [`lens_golang`]); this module only holds the shared lookup shape so
//! every adapter answers the question the same way.

/// A sorted, deduplicated table of method names whose presence in a
/// workspace is no evidence that a receiver call targets it.
///
/// Construct with [`UbiquitousMethodNames::new`] from a `const` slice;
/// [`UbiquitousMethodNames::contains`] binary-searches it, so the slice
/// must be sorted. [`UbiquitousMethodNames::is_sorted_and_deduped`]
/// lets adapters assert that in a unit test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UbiquitousMethodNames(&'static [&'static str]);

impl UbiquitousMethodNames {
    #[must_use]
    pub const fn new(names: &'static [&'static str]) -> Self {
        Self(names)
    }

    /// Whether `name` is one of the language's ubiquitous method names.
    #[must_use]
    pub fn contains(self, name: &str) -> bool {
        self.0.binary_search(&name).is_ok()
    }

    /// Table invariant [`Self::contains`] depends on. Adapters assert
    /// this in their own tests so a hand-edited table cannot silently
    /// start missing entries.
    #[must_use]
    pub fn is_sorted_and_deduped(self) -> bool {
        self.0.windows(2).all(|pair| pair[0] < pair[1])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    const TABLE: UbiquitousMethodNames = UbiquitousMethodNames::new(&["clone", "get", "map"]);

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
}
