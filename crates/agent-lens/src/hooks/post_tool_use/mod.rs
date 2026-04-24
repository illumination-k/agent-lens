//! `PostToolUse` hook handlers and name-based dispatch.

pub mod rust_similarity;

use agent_hooks::Hook;
use agent_hooks::claude_code::{PostToolUseInput, PostToolUseOutput};

/// Failures raised while dispatching a `PostToolUse` hook by name.
#[derive(Debug)]
pub enum DispatchError {
    /// No handler is registered under `name`.
    UnknownHandler(String),
    /// The named handler ran but returned an error.
    Handler(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownHandler(name) => {
                write!(f, "unknown post-tool-use hook handler: {name}")
            }
            Self::Handler(e) => write!(f, "post-tool-use hook handler failed: {e}"),
        }
    }
}

impl std::error::Error for DispatchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnknownHandler(_) => None,
            Self::Handler(e) => Some(e.as_ref()),
        }
    }
}

/// Run the handler registered under `name` against `input`.
///
/// Unknown names produce [`DispatchError::UnknownHandler`]; handler errors
/// are wrapped in [`DispatchError::Handler`] so the binary can log them
/// without caring which handler ran.
pub fn dispatch(name: &str, input: PostToolUseInput) -> Result<PostToolUseOutput, DispatchError> {
    match name {
        "rust-similarity" => rust_similarity::RustSimilarityHook::new()
            .handle(input)
            .map_err(|e| DispatchError::Handler(Box::new(e))),
        other => Err(DispatchError::UnknownHandler(other.to_owned())),
    }
}

/// Names of every `PostToolUse` handler registered in this binary.
///
/// Exposed so the CLI can surface them (e.g. in help text or error
/// messages) without hard-coding the list in two places.
pub const HANDLER_NAMES: &[&str] = &["rust-similarity"];
