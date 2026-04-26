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
