//! Naming-convention helpers for filtering Go test scaffolding.
//!
//! Go's `go test` discovers tests by name: a free function whose name
//! begins with `Test`, `Benchmark`, `Example`, or `Fuzz` and whose first
//! letter after the prefix is upper-case (or end-of-name) is treated as
//! a test entry point. The path filter at the analyzer layer already
//! drops `*_test.go` files; this name-level filter is the backup that
//! catches test-shaped definitions that slip through (e.g. inlined into
//! production source for some reason).

/// True iff `name` follows Go's `go test` discovery convention for a
/// test-flavoured top-level function.
///
/// Recognised prefixes are `Test`, `Benchmark`, `Example`, and `Fuzz`.
/// A name that *starts* with one of those prefixes only counts as a
/// test if the next character is an upper-case ASCII letter, an
/// underscore, or end-of-name — otherwise common production names like
/// `Tester` or `Examplify` would be filtered.
pub(crate) fn name_looks_like_test_function(name: &str) -> bool {
    for prefix in ["Test", "Benchmark", "Example", "Fuzz"] {
        if let Some(rest) = name.strip_prefix(prefix)
            && (rest.is_empty()
                || rest.starts_with('_')
                || rest.chars().next().is_some_and(|c| c.is_ascii_uppercase()))
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::test_capitalised("TestSomething")]
    #[case::test_underscore("Test_under")]
    #[case::test_bare("Test")]
    #[case::benchmark("BenchmarkAdd")]
    #[case::example("ExampleHello")]
    #[case::fuzz("FuzzParser")]
    fn matches_go_test_conventions(#[case] name: &str) {
        assert!(name_looks_like_test_function(name));
    }

    #[rstest]
    #[case::lowercase_test_prefix("tester")]
    #[case::test_followed_by_lowercase("Tester")]
    #[case::example_followed_by_lowercase("Examplify")]
    #[case::no_prefix("Foo")]
    #[case::empty("")]
    fn rejects_non_test_names(#[case] name: &str) {
        assert!(!name_looks_like_test_function(name));
    }
}
