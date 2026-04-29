//! Language-agnostic complexity metrics for individual functions.
//!
//! Adapters (e.g. `lens-rust`) walk AST nodes and populate
//! [`FunctionComplexity`] instances; this module only owns the raw shapes
//! and the *derived* metrics computed from them (Halstead Volume,
//! Maintainability Index). Everything is intentionally free of any
//! language-specific concept so the same struct can later be filled in
//! from tree-sitter for TS/Python.
//!
//! Two metric families coexist here:
//!
//! * **Counts** — Cyclomatic and Cognitive complexity, max nesting depth,
//!   and Halstead operator/operand counts. The adapter is responsible for
//!   deciding what counts as a branch or an operator in its language; this
//!   module trusts those numbers verbatim.
//! * **Derived** — Halstead Volume and Maintainability Index. Computed on
//!   demand because they involve floats, can be undefined, and would
//!   otherwise force every adapter to re-implement the same formula.

/// Halstead operator/operand counts collected by the language adapter.
///
/// `distinct_*` are the number of unique tokens in each category (`n1`,
/// `n2` in Halstead's notation); `total_*` are the running totals
/// (`N1`, `N2`). The adapter chooses what counts as an operator vs. an
/// operand for its language.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HalsteadCounts {
    /// `n1`: number of distinct operators.
    pub distinct_operators: usize,
    /// `n2`: number of distinct operands.
    pub distinct_operands: usize,
    /// `N1`: total operator occurrences.
    pub total_operators: usize,
    /// `N2`: total operand occurrences.
    pub total_operands: usize,
}

impl HalsteadCounts {
    /// `n = n1 + n2`. Vocabulary size.
    pub fn vocabulary(&self) -> usize {
        self.distinct_operators + self.distinct_operands
    }

    /// `N = N1 + N2`. Program length.
    pub fn length(&self) -> usize {
        self.total_operators + self.total_operands
    }

    /// Halstead Volume: `V = N * log2(n)`.
    ///
    /// Returns `None` when the function has no operators or no operands —
    /// `log2(0)` is undefined and the resulting MI would be meaningless.
    ///
    /// # Examples
    ///
    /// ```
    /// use lens_domain::HalsteadCounts;
    ///
    /// // N = 8, n = 4 → V = 8 * log2(4) = 16.
    /// let h = HalsteadCounts {
    ///     distinct_operators: 2,
    ///     distinct_operands: 2,
    ///     total_operators: 4,
    ///     total_operands: 4,
    /// };
    /// assert_eq!(h.volume(), Some(16.0));
    ///
    /// // No tokens → vocabulary too small → undefined.
    /// assert_eq!(HalsteadCounts::default().volume(), None);
    /// ```
    pub fn volume(&self) -> Option<f64> {
        let n = self.vocabulary();
        let len = self.length();
        if n < 2 || len == 0 {
            return None;
        }
        Some((len as f64) * (n as f64).log2())
    }
}

/// Complexity metrics for a single function-shaped item (free function,
/// method, or trait default).
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionComplexity {
    pub name: String,
    /// 1-based inclusive start line of the function signature.
    pub start_line: usize,
    /// 1-based inclusive end line of the function body.
    pub end_line: usize,
    /// McCabe Cyclomatic Complexity. Starts at 1 (linear path) and is
    /// incremented for each branching construct.
    pub cyclomatic: u32,
    /// Sonar-style Cognitive Complexity. Adds nesting penalties so that
    /// deeply-nested control flow scores higher than the same number of
    /// flat branches.
    pub cognitive: u32,
    /// Maximum nesting depth reached inside the function body.
    pub max_nesting: u32,
    /// Halstead operator/operand counts. Used to derive Volume and MI.
    pub halstead: HalsteadCounts,
}

impl FunctionComplexity {
    /// Lines of code occupied by the function (signature through closing
    /// brace, inclusive). 1-based, so a one-liner is `1`.
    pub fn loc(&self) -> usize {
        self.end_line.saturating_sub(self.start_line) + 1
    }

    /// Maintainability Index, normalised to `[0, 100]`.
    ///
    /// Computes the original Coleman-Oman formula
    /// `171 - 5.2 ln V - 0.23 CC - 16.2 ln LOC`, scales the `[0, 171]`
    /// range to `[0, 100]`, and clamps. Returns `None` when Halstead
    /// Volume is undefined or LOC is zero — both inputs to `ln`.
    pub fn maintainability_index(&self) -> Option<f64> {
        let v = self.halstead.volume()?;
        let loc = self.loc();
        if v <= 0.0 || loc == 0 {
            return None;
        }
        let raw =
            171.0 - 5.2 * v.ln() - 0.23 * f64::from(self.cyclomatic) - 16.2 * (loc as f64).ln();
        let scaled = raw * 100.0 / 171.0;
        Some(scaled.clamp(0.0, 100.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    fn fc(
        cc: u32,
        cog: u32,
        nest: u32,
        halstead: HalsteadCounts,
        lines: (usize, usize),
    ) -> FunctionComplexity {
        FunctionComplexity {
            name: "f".into(),
            start_line: lines.0,
            end_line: lines.1,
            cyclomatic: cc,
            cognitive: cog,
            max_nesting: nest,
            halstead,
        }
    }

    #[test]
    fn loc_is_inclusive_of_both_endpoints() {
        let f = fc(1, 0, 0, HalsteadCounts::default(), (10, 12));
        assert_eq!(f.loc(), 3);
    }

    #[test]
    fn loc_is_one_for_a_single_line_function() {
        let f = fc(1, 0, 0, HalsteadCounts::default(), (5, 5));
        assert_eq!(f.loc(), 1);
    }

    #[test]
    fn halstead_volume_is_undefined_for_empty_counts() {
        assert!(HalsteadCounts::default().volume().is_none());
    }

    #[test]
    fn halstead_volume_is_undefined_for_a_single_token_kind() {
        // n = 1 → log2(1) = 0 would collapse the metric. Guard.
        let h = HalsteadCounts {
            distinct_operators: 1,
            distinct_operands: 0,
            total_operators: 4,
            total_operands: 0,
        };
        assert!(h.volume().is_none());
    }

    #[test]
    fn vocabulary_sums_distinct_operators_and_operands() {
        // Use unequal counts so the sum and the product can never collapse
        // onto the same value.
        let h = HalsteadCounts {
            distinct_operators: 3,
            distinct_operands: 5,
            total_operators: 0,
            total_operands: 0,
        };
        assert_eq!(h.vocabulary(), 8);
    }

    #[test]
    fn length_sums_total_operators_and_operands() {
        // Same shape: pick coprime values so + and * give different totals.
        let h = HalsteadCounts {
            distinct_operators: 0,
            distinct_operands: 0,
            total_operators: 3,
            total_operands: 5,
        };
        assert_eq!(h.length(), 8);
    }

    #[test]
    fn halstead_volume_uses_n_log2_n() {
        // N = 8, n = 4 → V = 8 * log2(4) = 16
        let h = HalsteadCounts {
            distinct_operators: 2,
            distinct_operands: 2,
            total_operators: 4,
            total_operands: 4,
        };
        let v = h.volume().unwrap();
        assert!(approx(v, 16.0), "got {v}");
    }

    #[test]
    fn maintainability_index_is_undefined_when_halstead_is() {
        let f = fc(1, 0, 0, HalsteadCounts::default(), (1, 10));
        assert!(f.maintainability_index().is_none());
    }

    #[test]
    fn maintainability_index_stays_within_zero_to_one_hundred() {
        // High volume + high CC + huge LOC → raw MI goes negative; clamp
        // should pin it at 0 rather than emitting a negative score.
        let h = HalsteadCounts {
            distinct_operators: 50,
            distinct_operands: 50,
            total_operators: 5_000,
            total_operands: 5_000,
        };
        let f = fc(200, 0, 0, h, (1, 5_000));
        let mi = f.maintainability_index().unwrap();
        assert!((0.0..=100.0).contains(&mi), "MI out of bounds: {mi}");
        assert!(approx(mi, 0.0), "expected clamped to 0, got {mi}");
    }

    #[test]
    fn simple_function_scores_high_maintainability() {
        // Tiny function with low complexity should land near the top of
        // the MI range.
        let h = HalsteadCounts {
            distinct_operators: 2,
            distinct_operands: 2,
            total_operators: 4,
            total_operands: 4,
        };
        let f = fc(1, 0, 0, h, (1, 3));
        let mi = f.maintainability_index().unwrap();
        assert!(mi > 80.0, "expected high MI for trivial function, got {mi}");
    }

    use proptest::prelude::*;

    /// Random Halstead counts. Distinct counts are kept small (`0..8`) so
    /// that the `n < 2` boundary fires often enough for the iff-property
    /// to exercise both branches; totals are larger so `len == 0` still
    /// only triggers when both totals happen to be zero.
    fn arb_halstead() -> impl Strategy<Value = HalsteadCounts> {
        (0usize..8, 0usize..8, 0usize..32, 0usize..32).prop_map(|(n1, n2, big_n1, big_n2)| {
            HalsteadCounts {
                distinct_operators: n1,
                distinct_operands: n2,
                total_operators: big_n1,
                total_operands: big_n2,
            }
        })
    }

    proptest! {
        /// `volume()` is `None` exactly when vocabulary is below 2 or
        /// length is zero — both inputs to `log2` would be undefined or
        /// collapse the metric.
        #[test]
        fn halstead_volume_is_none_iff_vocabulary_below_two_or_length_zero(
            h in arb_halstead(),
        ) {
            let undefined = h.vocabulary() < 2 || h.length() == 0;
            prop_assert_eq!(h.volume().is_none(), undefined);
        }

        /// When defined, Halstead Volume is non-negative: `len * log2(n)`
        /// with `n >= 2` and `len >= 1` cannot produce a negative number.
        #[test]
        fn halstead_volume_is_non_negative_when_defined(h in arb_halstead()) {
            if let Some(v) = h.volume() {
                prop_assert!(v >= 0.0, "expected non-negative volume, got {}", v);
            }
        }

        /// `loc()` is at least 1 regardless of how `start_line` and
        /// `end_line` relate — `saturating_sub` collapses the inverted
        /// case rather than underflowing.
        #[test]
        fn loc_is_always_at_least_one(
            start in 0usize..10_000,
            end in 0usize..10_000,
        ) {
            let f = fc(1, 0, 0, HalsteadCounts::default(), (start, end));
            prop_assert!(f.loc() >= 1);
        }

        /// When `end_line >= start_line`, `loc()` is exactly the
        /// inclusive 1-based span `end - start + 1`.
        #[test]
        fn loc_equals_end_minus_start_plus_one_when_well_ordered(
            start in 0usize..10_000,
            delta in 0usize..10_000,
        ) {
            let end = start + delta;
            let f = fc(1, 0, 0, HalsteadCounts::default(), (start, end));
            prop_assert_eq!(f.loc(), delta + 1);
        }

        /// When defined, MI is clamped into `[0, 100]` for any combination
        /// of Halstead counts, cyclomatic complexity, and LOC.
        #[test]
        fn maintainability_index_stays_in_zero_to_one_hundred(
            h in arb_halstead(),
            cc in 0u32..512,
            start in 1usize..10_000,
            delta in 0usize..10_000,
        ) {
            let f = fc(cc, 0, 0, h, (start, start + delta));
            if let Some(mi) = f.maintainability_index() {
                prop_assert!(
                    (0.0..=100.0).contains(&mi),
                    "MI out of range: {}",
                    mi,
                );
            }
        }

        /// MI is `None` exactly when Halstead Volume is `None` or LOC is
        /// zero — the two `ln` inputs in the Coleman-Oman formula. `loc()`
        /// is always >= 1 so in practice the disjunction reduces to the
        /// volume guard, and the property pins both halves of that link.
        #[test]
        fn maintainability_index_is_none_iff_volume_none_or_loc_zero(
            h in arb_halstead(),
            cc in 0u32..512,
            start in 1usize..10_000,
            delta in 0usize..10_000,
        ) {
            let f = fc(cc, 0, 0, h, (start, start + delta));
            let undefined = f.halstead.volume().is_none() || f.loc() == 0;
            prop_assert_eq!(f.maintainability_index().is_none(), undefined);
        }
    }
}
