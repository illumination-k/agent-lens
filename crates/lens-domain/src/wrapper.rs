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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapperFinding {
    pub name: String,
    /// 1-based inclusive start line of the function signature.
    pub start_line: usize,
    /// 1-based inclusive end line of the function body.
    pub end_line: usize,
    pub callee: String,
    pub adapters: Vec<String>,
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
