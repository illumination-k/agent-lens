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

use crate::hooks::core::checkpoint::{CheckpointError, session_delta, take_snapshot};
use crate::hooks::core::cohesion::CohesionCore;
use crate::hooks::core::complexity::ComplexityCore;
use crate::hooks::core::footprint::FootprintCore;
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

    /// Working directory the session is anchored at. Hooks that look
    /// past the edited files — the footprint of the whole pending diff —
    /// start from here.
    fn cwd(input: &Self::Input) -> &Path;

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

/// Pending-diff footprint hook generic over the engine envelope.
///
/// Not a [`HookCore`]: the report is about the whole diff under the
/// session's directory, so the core needs the cwd as well as the edited
/// files, which only narrow what gets reported.
pub struct FootprintHook<E: HookEnvelope> {
    core: FootprintCore,
    _envelope: PhantomData<fn() -> E>,
}

impl<E: HookEnvelope> FootprintHook<E> {
    pub fn new() -> Self {
        Self {
            core: FootprintCore,
            _envelope: PhantomData,
        }
    }
}

impl<E: HookEnvelope> Default for FootprintHook<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: HookEnvelope> std::fmt::Debug for FootprintHook<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FootprintHook").finish()
    }
}

impl<E: HookEnvelope> Hook for FootprintHook<E> {
    type Input = E::Input;
    type Output = E::Output;
    type Error = HookError;

    fn handle(&self, input: Self::Input) -> Result<Self::Output, Self::Error> {
        let sources = E::prepare_sources(&input)?;
        match self.core.run(E::cwd(&input), &sources)? {
            Some(report) => Ok(E::wrap_report(report)),
            None => Ok(E::Output::default()),
        }
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

    /// The agent's id for the session, which names its checkpoint.
    fn session_id(input: &Self::Input) -> &str;

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

/// SessionStart hook that records the session checkpoint snapshot.
///
/// Silent by design: the snapshot is for the stop hooks to compare
/// against, and the session's context has no use for "a file was
/// written". A snapshot that already exists (a resumed session) is kept.
pub struct SnapshotHook<E: SessionStartEnvelope> {
    _envelope: PhantomData<fn() -> E>,
}

impl<E: SessionStartEnvelope> SnapshotHook<E> {
    pub fn new() -> Self {
        Self {
            _envelope: PhantomData,
        }
    }
}

impl<E: SessionStartEnvelope> Default for SnapshotHook<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: SessionStartEnvelope> std::fmt::Debug for SnapshotHook<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SnapshotHook").finish()
    }
}

impl<E: SessionStartEnvelope> Hook for SnapshotHook<E> {
    type Input = E::Input;
    type Output = E::Output;
    type Error = CheckpointError;

    fn handle(&self, input: Self::Input) -> Result<Self::Output, Self::Error> {
        take_snapshot(E::cwd(&input), E::session_id(&input))?;
        Ok(E::Output::default())
    }
}

/// Engine-specific glue between a Stop / SubagentStop payload and the
/// session checkpoint.
pub trait StopEnvelope {
    type Input: serde::de::DeserializeOwned;
    /// `Default` is the "nothing got worse" response.
    type Output: serde::Serialize + Default;

    fn cwd(input: &Self::Input) -> &Path;
    fn session_id(input: &Self::Input) -> &str;
    /// Whether this stop is already the continuation a previous stop
    /// hook forced. Blocking again would loop.
    fn stop_hook_active(input: &Self::Input) -> bool;

    /// Hand the report back. With `block`, the agent is kept going with
    /// the report as the reason it reads; without, the report is only a
    /// message.
    fn wrap_delta(report: String, block: bool) -> Self::Output;
}

/// Stop / SubagentStop hook that reports what got worse since the
/// session's snapshot.
///
/// Blocks — hands the report to the agent as a reason to keep going —
/// only when a finding is new since the last stop and this stop is not
/// itself a forced continuation; otherwise the report is a message.
/// `with_block(false)` never blocks.
pub struct DeltaHook<E: StopEnvelope> {
    block: bool,
    _envelope: PhantomData<fn() -> E>,
}

impl<E: StopEnvelope> DeltaHook<E> {
    pub fn new() -> Self {
        Self {
            block: true,
            _envelope: PhantomData,
        }
    }

    pub fn with_block(mut self, block: bool) -> Self {
        self.block = block;
        self
    }
}

impl<E: StopEnvelope> Default for DeltaHook<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: StopEnvelope> std::fmt::Debug for DeltaHook<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeltaHook")
            .field("block", &self.block)
            .finish()
    }
}

impl<E: StopEnvelope> Hook for DeltaHook<E> {
    type Input = E::Input;
    type Output = E::Output;
    type Error = CheckpointError;

    fn handle(&self, input: Self::Input) -> Result<Self::Output, Self::Error> {
        let Some(delta) = session_delta(E::cwd(&input), E::session_id(&input))? else {
            return Ok(E::Output::default());
        };
        let block = self.block && delta.new_findings > 0 && !E::stop_hook_active(&input);
        Ok(E::wrap_delta(delta.report, block))
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

    /// The snapshot hook writes the checkpoint under the session's id
    /// and injects nothing.
    pub(crate) fn snapshot_is_recorded_silently<E, F>(input: F)
    where
        E: SessionStartEnvelope,
        F: FnOnce(&Path) -> E::Input,
    {
        let dir = tempfile::tempdir().unwrap();
        crate::test_support::init_checkpoint_fixture(dir.path());
        let input = input(dir.path());
        let path = crate::hooks::core::checkpoint::snapshot_path(dir.path(), E::session_id(&input));
        let out = super::SnapshotHook::<E>::new().handle(input).unwrap();
        assert_eq!(
            serde_json::to_value(&out).unwrap(),
            serde_json::to_value(E::Output::default()).unwrap(),
        );
        assert!(path.is_file(), "no snapshot at {}", path.display());
    }
}

/// The engine-agnostic half of every [`StopEnvelope`] test suite:
/// [`DeltaHook`] decides when to block, the envelope only how. Checked
/// on the serialized output, which is what the agent reads.
#[cfg(test)]
pub(crate) mod stop_conformance {
    use std::path::Path;

    use agent_hooks::Hook as _;
    use serde_json::Value;

    use super::{DeltaHook, StopEnvelope};
    use crate::hooks::core::checkpoint::take_snapshot;
    use crate::test_support::{init_checkpoint_fixture, regress_checkpoint_fixture};

    fn run<E: StopEnvelope>(hook: &DeltaHook<E>, input: E::Input) -> Value {
        serde_json::to_value(hook.handle(input).unwrap()).unwrap()
    }

    /// A new regression blocks, with the report as the reason; the same
    /// regression at the next stop is only a message.
    pub(crate) fn blocks_once_then_messages<E, F>(input: F)
    where
        E: StopEnvelope,
        F: Fn(&Path, &str, bool) -> E::Input,
    {
        let dir = tempfile::tempdir().unwrap();
        init_checkpoint_fixture(dir.path());
        let hook = DeltaHook::<E>::new();
        assert_eq!(
            run(&hook, input(dir.path(), "s", false)),
            serde_json::to_value(E::Output::default()).unwrap(),
            "no snapshot, no opinion",
        );
        take_snapshot(dir.path(), "s").unwrap();
        assert_eq!(
            run(&hook, input(dir.path(), "s", false)),
            serde_json::to_value(E::Output::default()).unwrap(),
            "nothing changed, nothing to say",
        );
        regress_checkpoint_fixture(dir.path());

        let first = run(&hook, input(dir.path(), "s", false));
        assert_eq!(first["decision"], "block", "got {first}");
        let reason = first["reason"].as_str().unwrap();
        assert!(reason.contains("## New wrappers (1)"), "got {reason}");
        assert!(reason.contains("`outer` (modified)"), "got {reason}");

        let second = run(&hook, input(dir.path(), "s", false));
        assert!(second.get("decision").is_none(), "got {second}");
        assert!(
            second["systemMessage"]
                .as_str()
                .is_some_and(|m| m.contains("`outer`")),
            "got {second}",
        );
    }

    /// A stop that is already a forced continuation never blocks, and
    /// neither does a hook built with `with_block(false)`.
    pub(crate) fn never_blocks_a_continuation_or_when_told_not_to<E, F>(input: F)
    where
        E: StopEnvelope,
        F: Fn(&Path, &str, bool) -> E::Input,
    {
        let dir = tempfile::tempdir().unwrap();
        init_checkpoint_fixture(dir.path());
        take_snapshot(dir.path(), "a").unwrap();
        take_snapshot(dir.path(), "b").unwrap();
        regress_checkpoint_fixture(dir.path());

        let active = run(&DeltaHook::<E>::new(), input(dir.path(), "a", true));
        assert!(active.get("decision").is_none(), "got {active}");
        assert!(active["systemMessage"].is_string(), "got {active}");

        let advisory = run(
            &DeltaHook::<E>::new().with_block(false),
            input(dir.path(), "b", false),
        );
        assert!(advisory.get("decision").is_none(), "got {advisory}");
        assert!(advisory["systemMessage"].is_string(), "got {advisory}");
    }
}
