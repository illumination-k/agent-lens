//! Language-agnostic error-handling shape metrics for individual
//! functions.
//!
//! Adapters (e.g. `lens-rust`) walk AST nodes and populate
//! [`FunctionErrorShape`] instances; this module only owns the raw
//! counts and the *derived* ratio computed from them. Everything is
//! intentionally free of any language-specific concept so the same
//! struct can be filled in from syn, oxc, ruff, or tree-sitter.
//!
//! The counts describe the *shape* of a function's error handling, not
//! a verdict. Whether a high error-line ratio or a pile of
//! rethrow-only handlers is a problem depends on the function's role
//! (an RPC boundary legitimately spends most of its lines on error
//! paths), so thresholding is left to the reader.
//!
//! Terminology shared by every adapter:
//!
//! * **Error region** — a syntactic region whose execution implies an
//!   error occurred: a `catch` / `except` handler body, an `Err(..)`
//!   match-arm body, a `map_err` closure body, or the body of a Go
//!   `if err != nil`-style block. Regions nested inside another error
//!   region are not double-counted in [`error_loc`].
//! * **Propagate-only handler** — a handler that does nothing except
//!   (optionally wrap and) re-raise the error: `catch (e) { throw e }`,
//!   a lone `raise ... from e`, `Err(e) => return Err(e.into())`,
//!   `if err != nil { return fmt.Errorf("...: %w", err) }`.
//! * **Log-and-rethrow handler** — a handler whose statements are all
//!   logging calls followed by a final propagation, with no recovery.
//!
//! [`error_loc`]: FunctionErrorShape::error_loc

/// Error-handling shape for a single function-shaped item (free
/// function, method, or trait default).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionErrorShape {
    pub name: String,
    /// 1-based inclusive start line of the function signature.
    pub start_line: usize,
    /// 1-based inclusive end line of the function body.
    pub end_line: usize,
    /// Number of error-region entry points: `catch` clauses, `except`
    /// handlers, `Err(..)` match arms / `if let Err` blocks, `map_err`
    /// calls, or Go error-check `if` blocks.
    pub error_branch_count: u32,
    /// Lines occupied by error regions. Regions nested inside another
    /// error region are counted once (by the outermost region).
    pub error_loc: usize,
    /// Number of disjoint `try` statements in the function (0 for
    /// languages without `try`, i.e. Rust and Go).
    pub disjoint_try_count: u32,
    /// `try` statements whose protected body is a single statement —
    /// the "wrap every call individually" shape.
    pub single_stmt_try_count: u32,
    /// Handlers that only (optionally wrap and) propagate the error.
    pub rethrow_only_handlers: u32,
    /// Handlers that only log and then propagate the error.
    pub log_and_rethrow_handlers: u32,
    /// True when the function propagates every error it touches —
    /// every handler is propagate-only (or, in Rust, the error path is
    /// only `?` / `map_err`) — with no recovery and no logging. Such a
    /// function is a pure link in a wrap chain. False when the
    /// function has no error constructs at all.
    pub wrap_only_error_path: bool,
}

impl FunctionErrorShape {
    /// Lines of code occupied by the function (signature through the
    /// last body line, inclusive). 1-based, so a one-liner is `1`.
    pub fn loc(&self) -> usize {
        self.end_line.saturating_sub(self.start_line) + 1
    }

    /// Share of the function's lines spent inside error regions, in
    /// `[0.0, 1.0]`. Clamped: adapters may attribute a multi-region
    /// line more than once, and the signature line is part of `loc`
    /// but never part of a region.
    pub fn error_loc_ratio(&self) -> f64 {
        let ratio = self.error_loc as f64 / self.loc() as f64;
        ratio.clamp(0.0, 1.0)
    }

    /// True when the function contains any error-handling construct
    /// worth reporting. Functions without one are dropped by the
    /// analyzer so reports stay signal-dense.
    pub fn has_error_handling(&self) -> bool {
        self.error_branch_count > 0 || self.disjoint_try_count > 0 || self.wrap_only_error_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(start: usize, end: usize, error_loc: usize) -> FunctionErrorShape {
        FunctionErrorShape {
            name: "f".into(),
            start_line: start,
            end_line: end,
            error_branch_count: 0,
            error_loc,
            disjoint_try_count: 0,
            single_stmt_try_count: 0,
            rethrow_only_handlers: 0,
            log_and_rethrow_handlers: 0,
            wrap_only_error_path: false,
        }
    }

    #[test]
    fn loc_is_inclusive_of_both_endpoints() {
        assert_eq!(shape(10, 12, 0).loc(), 3);
    }

    #[test]
    fn loc_is_one_for_a_single_line_function() {
        assert_eq!(shape(5, 5, 0).loc(), 1);
    }

    #[test]
    fn ratio_is_error_lines_over_loc() {
        let s = shape(1, 10, 4);
        assert!((s.error_loc_ratio() - 0.4).abs() < 1e-9);
    }

    #[test]
    fn ratio_is_clamped_to_one() {
        // Adapters can over-attribute lines (e.g. two sibling regions
        // sharing a line); the ratio must still read as a share.
        let s = shape(1, 2, 10);
        assert_eq!(s.error_loc_ratio(), 1.0);
    }

    #[test]
    fn ratio_is_zero_without_error_lines() {
        assert_eq!(shape(1, 10, 0).error_loc_ratio(), 0.0);
    }

    #[test]
    fn has_error_handling_reflects_branches_tries_and_wrap_only() {
        let none = shape(1, 10, 0);
        assert!(!none.has_error_handling());

        let mut branchy = shape(1, 10, 2);
        branchy.error_branch_count = 1;
        assert!(branchy.has_error_handling());

        let mut trying = shape(1, 10, 0);
        trying.disjoint_try_count = 1;
        assert!(trying.has_error_handling());

        // Rust `?`-only functions have no branch and no try but are
        // still a link in a propagation chain.
        let mut wrap_only = shape(1, 10, 0);
        wrap_only.wrap_only_error_path = true;
        assert!(wrap_only.has_error_handling());
    }

    use proptest::prelude::*;

    proptest! {
        /// The ratio is always inside `[0, 1]` no matter how the raw
        /// counts relate.
        #[test]
        fn ratio_stays_in_unit_interval(
            start in 0usize..10_000,
            delta in 0usize..10_000,
            error_loc in 0usize..50_000,
        ) {
            let s = shape(start, start + delta, error_loc);
            let r = s.error_loc_ratio();
            prop_assert!((0.0..=1.0).contains(&r), "ratio out of range: {r}");
        }

        /// When the adapter attributes no more lines than the function
        /// has, the ratio is exactly `error_loc / loc`.
        #[test]
        fn ratio_is_exact_when_within_loc(
            start in 0usize..10_000,
            delta in 0usize..10_000,
            error_loc in 0usize..10_000,
        ) {
            let s = shape(start, start + delta, error_loc.min(delta + 1));
            let expected = s.error_loc as f64 / s.loc() as f64;
            prop_assert!((s.error_loc_ratio() - expected).abs() < 1e-12);
        }
    }
}
