//! Generic hook runners.
//!
//! Each agent (Claude Code / Codex) provides a [`HookEnvelope`] that
//! adapts their input/output shapes to the engine-agnostic
//! `Vec<EditedSource> -> Option<String>` flow used by the cores. Each
//! analyser ([`SimilarityCore`], [`WrapperCore`], [`ComplexityCore`],
//! [`CohesionCore`]) implements [`HookCore`] so a single
//! [`CoreHook<C, E>`] handles all 4 × 2 combinations and the per-hook
//! types are just type aliases.

use std::marker::PhantomData;
use std::path::Path;

use agent_hooks::Hook;

use crate::hooks::core::cohesion::CohesionCore;
use crate::hooks::core::complexity::ComplexityCore;
use crate::hooks::core::session_summary::{SessionSummaryError, render_summary};
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

/// One analyser core (similarity, wrapper, complexity, cohesion) reduced
/// to the surface the runner needs.
///
/// Implementing this on each `…Core` lets [`CoreHook`] be generic over
/// the analyser as well as the engine envelope, so the per-hook structs
/// collapse to a single struct plus type aliases.
pub trait HookCore: Default + Clone + std::fmt::Debug {
    /// Analyse every prepared source and produce a single report, or
    /// `None` when there is nothing to surface.
    fn run(&self, sources: &[EditedSource]) -> Result<Option<String>, HookError>;
}

impl HookCore for SimilarityCore {
    fn run(&self, sources: &[EditedSource]) -> Result<Option<String>, HookError> {
        SimilarityCore::run(self, sources)
    }
}

impl HookCore for WrapperCore {
    fn run(&self, sources: &[EditedSource]) -> Result<Option<String>, HookError> {
        WrapperCore::run(self, sources)
    }
}

impl HookCore for ComplexityCore {
    fn run(&self, sources: &[EditedSource]) -> Result<Option<String>, HookError> {
        ComplexityCore::run(self, sources)
    }
}

impl HookCore for CohesionCore {
    fn run(&self, sources: &[EditedSource]) -> Result<Option<String>, HookError> {
        CohesionCore::run(self, sources)
    }
}

/// Hook generic over both the analyser core and the engine envelope.
///
/// One implementation drives all 4 × 2 hook combinations: each per-hook
/// struct is a type alias over `CoreHook<TheirCore, TheirEnvelope>`.
pub struct CoreHook<C: HookCore, E: HookEnvelope> {
    core: C,
    _envelope: PhantomData<fn() -> E>,
}

impl<C: HookCore, E: HookEnvelope> CoreHook<C, E> {
    pub fn new() -> Self {
        Self {
            core: C::default(),
            _envelope: PhantomData,
        }
    }
}

impl<C: HookCore, E: HookEnvelope> Default for CoreHook<C, E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: HookCore, E: HookEnvelope> std::fmt::Debug for CoreHook<C, E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CoreHook")
            .field("core", &self.core)
            .finish()
    }
}

impl<C: HookCore, E: HookEnvelope> Clone for CoreHook<C, E> {
    fn clone(&self) -> Self {
        Self {
            core: self.core.clone(),
            _envelope: PhantomData,
        }
    }
}

impl<C: HookCore, E: HookEnvelope> Hook for CoreHook<C, E> {
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

/// Similarity hook generic over the engine envelope.
pub type SimilarityHook<E> = CoreHook<SimilarityCore, E>;
/// Wrapper-detection hook generic over the engine envelope.
pub type WrapperHook<E> = CoreHook<WrapperCore, E>;
/// Per-function complexity hook generic over the engine envelope.
pub type ComplexityHook<E> = CoreHook<ComplexityCore, E>;
/// Cohesion hook generic over the engine envelope.
pub type CohesionHook<E> = CoreHook<CohesionCore, E>;

impl<E: HookEnvelope> CoreHook<SimilarityCore, E> {
    /// Override the similarity threshold. Useful for tests; the binary
    /// currently always uses the default.
    pub fn with_threshold(mut self, threshold: f64) -> Self {
        self.core = self.core.with_threshold(threshold);
        self
    }
}

/// Engine-specific glue between an agent's SessionStart payload and the
/// engine-agnostic summary renderer.
///
/// SessionStart has no `EditedSource` list to prepare — the whole input
/// contract is "where is the session anchored" — so it gets its own
/// envelope rather than piggybacking on [`HookEnvelope`].
pub trait SessionStartEnvelope {
    /// Hook input type as it arrives from the agent.
    type Input: serde::de::DeserializeOwned;
    /// Hook output type the agent expects back. `Default` is the "no
    /// signal, stay silent" response.
    type Output: serde::Serialize + Default;

    /// Working directory the session is anchored at.
    fn cwd(input: &Self::Input) -> &Path;

    /// Wrap a rendered summary body in the envelope shape this agent
    /// uses (both agents inject via `additionalContext` today, but the
    /// concrete output types are per-engine).
    fn wrap_summary(body: String) -> Self::Output;
}

/// SessionStart summary hook generic over the engine envelope.
///
/// Renders [`render_summary`] for the session's cwd and wraps the body
/// via the envelope, or returns the default no-op output when neither
/// summary section produces signal.
pub struct SummaryHook<E: SessionStartEnvelope> {
    _envelope: PhantomData<fn() -> E>,
}

impl<E: SessionStartEnvelope> SummaryHook<E> {
    pub fn new() -> Self {
        Self {
            _envelope: PhantomData,
        }
    }
}

impl<E: SessionStartEnvelope> Default for SummaryHook<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: SessionStartEnvelope> std::fmt::Debug for SummaryHook<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SummaryHook").finish()
    }
}

impl<E: SessionStartEnvelope> Clone for SummaryHook<E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E: SessionStartEnvelope> Copy for SummaryHook<E> {}

impl<E: SessionStartEnvelope> Hook for SummaryHook<E> {
    type Input = E::Input;
    type Output = E::Output;
    type Error = SessionSummaryError;

    fn handle(&self, input: Self::Input) -> Result<Self::Output, Self::Error> {
        match render_summary(E::cwd(&input))? {
            Some(body) => Ok(E::wrap_summary(body)),
            None => Ok(E::Output::default()),
        }
    }
}

/// The engine-agnostic half of every [`SessionStartEnvelope`] test suite.
///
/// [`SummaryHook`] decides both things a session-start adapter is
/// answerable for — stay silent when neither summary section has signal,
/// and inject the rendered body otherwise — so the assertions belong to
/// the runner, not to each envelope. The envelope supplies only its own
/// input value.
///
/// Both agents inject through the same wire fields
/// (`hookSpecificOutput.additionalContext`), so these check the
/// serialized envelope rather than the concrete output type. That is the
/// shape the agent actually reads, and it needs no bound the trait does
/// not already carry.
#[cfg(test)]
pub(crate) mod session_start_conformance {
    use std::path::Path;

    use agent_hooks::Hook as _;
    use serde_json::Value;

    use super::{SessionStartEnvelope, SummaryHook};

    /// Run the summary hook for `cwd` and return its serialized output.
    fn run<E: SessionStartEnvelope>(cwd: &Path, input: impl FnOnce(&Path) -> E::Input) -> Value {
        let out = SummaryHook::<E>::new().handle(input(cwd)).unwrap();
        serde_json::to_value(&out).unwrap()
    }

    /// A cwd that is neither a git working tree nor a recognised module
    /// root produces no signal, and the hook must then emit the default
    /// no-op response rather than an empty summary.
    pub(crate) fn stays_silent_without_repo_or_crate<E, F>(input: F)
    where
        E: SessionStartEnvelope,
        F: FnOnce(&Path) -> E::Input,
    {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            run::<E>(dir.path(), input),
            serde_json::to_value(E::Output::default()).unwrap(),
        );
    }

    /// A cwd with both halves available is injected as
    /// `hookSpecificOutput.additionalContext`, carrying the hotspot and
    /// coupling sections [`render_summary`] renders.
    pub(crate) fn injects_summary_via_additional_context<E, F>(input: F)
    where
        E: SessionStartEnvelope,
        F: FnOnce(&Path) -> E::Input,
    {
        let dir = tempfile::tempdir().unwrap();
        crate::test_support::init_repo_with_crate_for_session_summary(dir.path());

        let out = run::<E>(dir.path(), input);
        let extra = &out["hookSpecificOutput"];
        assert_eq!(extra["hookEventName"], "SessionStart", "got {out}");
        let body = extra["additionalContext"]
            .as_str()
            .unwrap_or_else(|| panic!("expected additionalContext, got {out}"));

        assert!(body.starts_with("# agent-lens session-start"), "got {body}");
        assert!(body.contains("## Hotspots"), "want hotspot: {body}");
        assert!(body.contains("## Coupling"), "want coupling: {body}");
    }
}
