//! Generic PostToolUse hook runners.
//!
//! Each agent (Claude Code / Codex) provides a [`HookEnvelope`] that
//! adapts their input/output shapes to the engine-agnostic
//! `(Vec<EditedSource>, Option<String>)` flow used by the cores. The
//! [`SimilarityHook`] / [`WrapperHook`] structs here are then a single
//! generic implementation parameterised on the envelope, replacing the
//! near-identical per-agent boilerplate.

use std::marker::PhantomData;

use agent_hooks::Hook;

use crate::hooks::core::similarity::SimilarityCore;
use crate::hooks::core::wrapper::WrapperCore;
use crate::hooks::core::{EditedSource, HookError, ReadEditedSourceError};

/// Engine-specific glue between an agent's hook payload and the
/// engine-agnostic `(EditedSource, report)` core flow.
pub trait HookEnvelope {
    /// Hook input type as it arrives from the agent (e.g. Claude Code's
    /// or Codex's `PostToolUseInput`). The `DeserializeOwned` bound
    /// matches the underlying [`agent_hooks::Hook`] contract so the CLI
    /// glue can deserialize stdin into this type.
    type Input: serde::de::DeserializeOwned;
    /// Hook output type the agent expects back. `Serialize` matches the
    /// `Hook` contract; `Default` is how the runner produces a "no-op"
    /// response when the core finds nothing to report.
    type Output: serde::Serialize + Default;

    /// Convert an agent payload into the list of files that should be
    /// analysed. Returning an empty list means "out of scope" and short
    /// circuits to `Self::Output::default()`.
    fn prepare_sources(input: &Self::Input) -> Result<Vec<EditedSource>, ReadEditedSourceError>;

    /// Wrap a non-empty report string in the envelope shape this agent
    /// uses (e.g. Claude Code's `systemMessage`, Codex's
    /// `additionalContext`).
    fn wrap_report(report: String) -> Self::Output;
}

/// Similarity hook generic over the engine envelope.
pub struct SimilarityHook<E: HookEnvelope> {
    core: SimilarityCore,
    _envelope: PhantomData<fn() -> E>,
}

impl<E: HookEnvelope> SimilarityHook<E> {
    /// Construct a handler with the default similarity threshold and TSED
    /// options.
    pub fn new() -> Self {
        Self {
            core: SimilarityCore::new(),
            _envelope: PhantomData,
        }
    }

    /// Override the similarity threshold. Useful for tests; the binary
    /// currently always uses the default.
    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.core = self.core.with_threshold(threshold);
        self
    }
}

impl<E: HookEnvelope> Default for SimilarityHook<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: HookEnvelope> std::fmt::Debug for SimilarityHook<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimilarityHook")
            .field("core", &self.core)
            .finish()
    }
}

impl<E: HookEnvelope> Clone for SimilarityHook<E> {
    fn clone(&self) -> Self {
        Self {
            core: self.core.clone(),
            _envelope: PhantomData,
        }
    }
}

impl<E: HookEnvelope> Hook for SimilarityHook<E> {
    type Input = E::Input;
    type Output = E::Output;
    type Error = HookError;

    fn handle(&self, input: Self::Input) -> Result<Self::Output, Self::Error> {
        let sources = E::prepare_sources(&input)?;
        match self.core.run(&sources)? {
            Some(report) => Ok(E::wrap_report(report)),
            None => Ok(E::Output::default()),
        }
    }
}

/// Wrapper-detection hook generic over the engine envelope.
pub struct WrapperHook<E: HookEnvelope> {
    core: WrapperCore,
    _envelope: PhantomData<fn() -> E>,
}

impl<E: HookEnvelope> WrapperHook<E> {
    pub fn new() -> Self {
        Self {
            core: WrapperCore::new(),
            _envelope: PhantomData,
        }
    }
}

impl<E: HookEnvelope> Default for WrapperHook<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: HookEnvelope> std::fmt::Debug for WrapperHook<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WrapperHook")
            .field("core", &self.core)
            .finish()
    }
}

impl<E: HookEnvelope> Clone for WrapperHook<E> {
    fn clone(&self) -> Self {
        Self {
            core: self.core.clone(),
            _envelope: PhantomData,
        }
    }
}

impl<E: HookEnvelope> Hook for WrapperHook<E> {
    type Input = E::Input;
    type Output = E::Output;
    type Error = HookError;

    fn handle(&self, input: Self::Input) -> Result<Self::Output, Self::Error> {
        let sources = E::prepare_sources(&input)?;
        match self.core.run(&sources)? {
            Some(report) => Ok(E::wrap_report(report)),
            None => Ok(E::Output::default()),
        }
    }
}
