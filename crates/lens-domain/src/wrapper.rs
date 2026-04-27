//! Language-agnostic wrapper-detection result type.
//!
//! Each language adapter (e.g. `lens-rust`, `lens-ts`) defines what
//! counts as a "trivial adapter" in its own grammar, but the shape of
//! the result — name, line range, callee, and the chain of adapters
//! that were peeled off the body — is the same everywhere. Keeping the
//! struct here lets `agent-lens` dispatch on language without having to
//! convert between near-identical per-adapter types.

/// One thin-wrapper finding: a function whose body, after peeling a
/// short chain of trivial adapters, is just a forwarding call to
/// `callee` with the function's parameters passed straight through.
///
/// `adapters` is rendered in source order (innermost wrapper first), so
/// the final body reads as `callee(args)` followed by each adapter in
/// `adapters` joined together. An empty `adapters` means the body was a
/// bare `callee(args)` call.
///
/// The remaining fields surface the three "wrapper-ness" axes an agent
/// uses to triage findings:
///
/// * **Thin** — `statement_count` (always 1 today, but explicit so a
///   future relaxation of detection can vary it).
/// * **Low semantic delta** — `adapters` (empty = pure delegation;
///   non-empty = a short chain of trivial coercions / unwraps).
/// * **Low reuse** — `reuse`, populated only when the analyzer ran
///   over a directory and could enumerate cross-file call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapperFinding {
    pub name: String,
    /// 1-based inclusive start line of the function signature.
    pub start_line: usize,
    /// 1-based inclusive end line of the function body.
    pub end_line: usize,
    pub callee: String,
    pub adapters: Vec<String>,
    /// Number of statements in the function body. Today the detector
    /// only accepts single-statement bodies, so this is always 1; the
    /// field is exposed so agents reading the output can confirm the
    /// "thin" axis without having to re-derive it from line ranges.
    pub statement_count: usize,
    /// Workspace-wide usage of this wrapper, if measured. `None` when
    /// the analyzer ran on a single file (the call-site universe was
    /// not enumerated).
    pub reuse: Option<ReuseMetrics>,
}

/// "Low reuse" axis for a wrapper: how many places call it, how many
/// distinct callers there are, and whether every caller lives in the
/// wrapper's own file.
///
/// All three numbers are computed by name-matching across the walked
/// directory, so they are heuristic — same-named methods on different
/// types collapse into the same bucket. Treat the result as guidance
/// for an agent, not a precise call graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReuseMetrics {
    /// Total call sites of this wrapper across the walked directory,
    /// excluding calls written inside the wrapper itself.
    pub call_sites: usize,
    /// Distinct caller functions across those call sites. Top-level
    /// references (e.g. inside a `const` initialiser) are counted as
    /// one anonymous caller per file.
    pub unique_callers: usize,
    /// `true` iff every call site was found in the same file as the
    /// wrapper definition. `true` with `call_sites == 0` means the
    /// wrapper is unused inside the walked tree.
    pub same_file_only: bool,
}

/// True iff `args` are exactly `params` in order, each appearing once.
///
/// `extract` pulls a bare identifier out of each language's argument
/// shape; non-identifier expressions (literals, casts, derefs, etc.)
/// must return `None` so the caller treats them as "not a pass-through".
///
/// Used by `lens-rust`, `lens-ts`, and `lens-py` `wrapper` extractors so
/// the structural check (size match, ident-position match, no
/// duplicates) is implemented once.
pub fn args_pass_through_by<T>(
    args: &[T],
    params: &[String],
    extract: impl Fn(&T) -> Option<String>,
) -> bool {
    if args.len() != params.len() {
        return false;
    }
    let mut seen = vec![false; params.len()];
    for arg in args {
        let Some(name) = extract(arg) else {
            return false;
        };
        let Some(pos) = params.iter().position(|p| p == &name) else {
            return false;
        };
        if seen[pos] {
            return false;
        }
        seen[pos] = true;
    }
    seen.iter().all(|hit| *hit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(s: &&str) -> Option<String> {
        Some((*s).to_owned())
    }

    #[test]
    fn matches_exact_passthrough() {
        let params = vec!["a".to_owned(), "b".to_owned()];
        assert!(args_pass_through_by(&["a", "b"], &params, ident));
    }

    #[test]
    fn allows_reordered_args() {
        // The structural check is set-based: `b, a` is still treated as
        // a pass-through of `[a, b]` because each param appears once
        // and no duplicates were seen. Pre-existing language adapters
        // rely on this — wrappers that swap argument order are still
        // considered trivial forwards.
        let params = vec!["a".to_owned(), "b".to_owned()];
        assert!(args_pass_through_by(&["b", "a"], &params, ident));
    }

    #[test]
    fn rejects_when_lengths_differ() {
        let params = vec!["a".to_owned()];
        assert!(!args_pass_through_by(&["a", "b"], &params, ident));
    }

    #[test]
    fn rejects_duplicates() {
        let params = vec!["a".to_owned(), "b".to_owned()];
        assert!(!args_pass_through_by(&["a", "a"], &params, ident));
    }

    #[test]
    fn rejects_when_extract_returns_none() {
        let params = vec!["a".to_owned()];
        assert!(!args_pass_through_by(
            &["literal-1"],
            &params,
            |_: &&str| None,
        ));
    }
}
