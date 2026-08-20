//! In-memory analysis index shared by the analyzers of one run.
//!
//! A profile run drives many analyzers over the same tree, and each of
//! them used to re-read and re-parse every source file: `run self` built
//! the same call graph five times because cycles, layers, hubs, risk,
//! and delegation each asked for their own. The index memoizes the two
//! layers where that work repeats:
//!
//! * **Per-file facts** — function shapes, call shapes, complexity
//!   units, wrapper findings, interface shapes — keyed by the file's
//!   *content* (language + text hash), so any two analyzers that need
//!   the same fact about the same text share one parse, even when their
//!   graph-level configurations differ.
//! * **Assembled graphs** — the function call graph and the module
//!   dependency graph — keyed by the full builder configuration, so
//!   analyzers with identical scope skip even the walk and re-assembly.
//!
//! The index only exists inside an [`AnalysisIndexScope`]: the profile
//! runner (`agent-lens run`, `baseline create/compare`) activates one
//! around its tool loop, and every lookup outside a scope is a straight
//! passthrough to the underlying extraction. A single `agent-lens
//! analyze <tool>` invocation therefore pays nothing for the machinery.
//!
//! Keys are content-addressed on purpose: a file whose text changes
//! under an active scope hashes to a new key and misses, so the index
//! can never serve stale facts — at worst it holds a dead entry until
//! the scope ends. Entries live for the scope's lifetime, which is one
//! CLI invocation; nothing is persisted.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lens_domain::{
    CallShape, FileChurn, FunctionComplexity, FunctionShape, InterfaceShape, WrapperFinding,
};

use super::call_graph::{CallGraph, CallGraphBuilder};
use super::diff::{DiffScope, LineRange};
use super::module_graph::{GraphPolicy, ModuleGraph};
use super::roots::AnalyzeRoots;
use super::{AnalyzerError, SourceLang, dispatch_lens};
use std::path::PathBuf;

/// Identity of one file's parsed text: the language it is parsed as
/// plus a hash of the source. Content-addressed rather than
/// path-addressed so identical text shares one entry wherever it lives,
/// and so an edit under an active scope misses instead of going stale.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct SourceKey {
    lang: SourceLang,
    hash: u64,
}

impl SourceKey {
    pub(crate) fn new(lang: SourceLang, source: &str) -> Self {
        let mut hasher = DefaultHasher::new();
        source.hash(&mut hasher);
        Self {
            lang,
            hash: hasher.finish(),
        }
    }
}

/// Identity of one assembled call graph: the builder's whole
/// configuration plus the root set it was pointed at. Two analyzers
/// build the same graph exactly when both halves agree.
pub(crate) type CallGraphKey = (CallGraphBuilder, AnalyzeRoots);

/// Identity of one assembled module graph: the crate root plus the
/// policy it was built under.
pub(crate) type ModuleGraphKey = (PathBuf, GraphPolicy);

/// The memoized fact tables. One instance lives for one
/// [`AnalysisIndexScope`]; every table maps an identity key to the
/// immutable, shared result of the extraction that would otherwise run
/// again.
#[derive(Debug, Default)]
pub struct AnalysisIndex {
    function_shapes: Table<(SourceKey, String), Vec<FunctionShape>>,
    call_shapes: Table<(SourceKey, String, bool), Vec<CallShape>>,
    complexity_units: Table<SourceKey, Vec<FunctionComplexity>>,
    wrapper_findings: Table<SourceKey, Vec<WrapperFinding>>,
    interface_shapes: Table<(SourceKey, String), Vec<InterfaceShape>>,
    call_graphs: Table<CallGraphKey, CallGraph>,
    module_graphs: Table<ModuleGraphKey, ModuleGraph>,
    /// Enclosing working-tree root per directory (`None`: outside any
    /// repository), so the batch diff resolves each directory once.
    repo_roots: Table<PathBuf, Option<PathBuf>>,
    /// One whole-repository `git diff` per (root, scope), split into
    /// per-file changed ranges keyed by canonical absolute path.
    repo_changed_ranges: Table<(PathBuf, DiffScope), HashMap<PathBuf, Vec<LineRange>>>,
    /// Per-file commit counts per (repository root, targets, `--since`
    /// window) — `hotspot` and `risk` read the same `git log`.
    churn: Table<(PathBuf, Vec<PathBuf>, Option<String>), Vec<FileChurn>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

type Table<K, V> = Mutex<HashMap<K, Arc<V>>>;

impl AnalysisIndex {
    /// The index installed by the innermost live [`AnalysisIndexScope`]
    /// on this thread, if any.
    pub(crate) fn active() -> Option<Arc<Self>> {
        ACTIVE.with_borrow(Clone::clone)
    }

    /// Hit and miss counts across every table, in that order. A hit is
    /// an extraction some earlier analyzer already paid for.
    pub fn stats(&self) -> (u64, u64) {
        (
            self.hits.load(Ordering::Relaxed),
            self.misses.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn function_shapes<E>(
        &self,
        key: SourceKey,
        module: &str,
        compute: impl FnOnce() -> Result<Vec<FunctionShape>, E>,
    ) -> Result<Arc<Vec<FunctionShape>>, E> {
        self.memoize(&self.function_shapes, (key, module.to_owned()), compute)
    }

    pub(crate) fn call_shapes<E>(
        &self,
        key: SourceKey,
        module: &str,
        include_cfg_test_blocks: bool,
        compute: impl FnOnce() -> Result<Vec<CallShape>, E>,
    ) -> Result<Arc<Vec<CallShape>>, E> {
        self.memoize(
            &self.call_shapes,
            (key, module.to_owned(), include_cfg_test_blocks),
            compute,
        )
    }

    pub(crate) fn complexity_units<E>(
        &self,
        key: SourceKey,
        compute: impl FnOnce() -> Result<Vec<FunctionComplexity>, E>,
    ) -> Result<Arc<Vec<FunctionComplexity>>, E> {
        self.memoize(&self.complexity_units, key, compute)
    }

    pub(crate) fn wrapper_findings<E>(
        &self,
        key: SourceKey,
        compute: impl FnOnce() -> Result<Vec<WrapperFinding>, E>,
    ) -> Result<Arc<Vec<WrapperFinding>>, E> {
        self.memoize(&self.wrapper_findings, key, compute)
    }

    pub(crate) fn interface_shapes<E>(
        &self,
        key: SourceKey,
        module: &str,
        compute: impl FnOnce() -> Result<Vec<InterfaceShape>, E>,
    ) -> Result<Arc<Vec<InterfaceShape>>, E> {
        self.memoize(&self.interface_shapes, (key, module.to_owned()), compute)
    }

    pub(crate) fn call_graph<E>(
        &self,
        key: CallGraphKey,
        compute: impl FnOnce() -> Result<CallGraph, E>,
    ) -> Result<Arc<CallGraph>, E> {
        self.memoize(&self.call_graphs, key, compute)
    }

    pub(crate) fn module_graph<E>(
        &self,
        key: ModuleGraphKey,
        compute: impl FnOnce() -> Result<ModuleGraph, E>,
    ) -> Result<Arc<ModuleGraph>, E> {
        self.memoize(&self.module_graphs, key, compute)
    }

    pub(crate) fn repo_root(
        &self,
        dir: PathBuf,
        compute: impl FnOnce() -> Option<PathBuf>,
    ) -> Arc<Option<PathBuf>> {
        self.memoize_ok(&self.repo_roots, dir, compute)
    }

    pub(crate) fn repo_changed_ranges(
        &self,
        key: (PathBuf, DiffScope),
        compute: impl FnOnce() -> HashMap<PathBuf, Vec<LineRange>>,
    ) -> Arc<HashMap<PathBuf, Vec<LineRange>>> {
        self.memoize_ok(&self.repo_changed_ranges, key, compute)
    }

    pub(crate) fn churn<E>(
        &self,
        key: (PathBuf, Vec<PathBuf>, Option<String>),
        compute: impl FnOnce() -> Result<Vec<FileChurn>, E>,
    ) -> Result<Arc<Vec<FileChurn>>, E> {
        self.memoize(&self.churn, key, compute)
    }

    /// [`Self::memoize`] for computations that cannot fail.
    fn memoize_ok<K: Eq + Hash, V>(
        &self,
        table: &Table<K, V>,
        key: K,
        compute: impl FnOnce() -> V,
    ) -> Arc<V> {
        match self.memoize::<_, _, std::convert::Infallible>(table, key, || Ok(compute())) {
            Ok(value) => value,
            Err(never) => match never {},
        }
    }

    /// Look `key` up in `table`, running `compute` on a miss. The lock
    /// is never held across `compute`, so a memoized computation can
    /// itself consult other tables; the cost of that choice is that two
    /// racing threads may both compute, with the first insertion
    /// winning — duplicated work, never a wrong answer.
    fn memoize<K: Eq + Hash, V, E>(
        &self,
        table: &Table<K, V>,
        key: K,
        compute: impl FnOnce() -> Result<V, E>,
    ) -> Result<Arc<V>, E> {
        if let Some(value) = lock(table).get(&key) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(Arc::clone(value));
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let value = Arc::new(compute()?);
        Ok(Arc::clone(lock(table).entry(key).or_insert(value)))
    }
}

/// Complexity units for one file's text, shared through the active
/// index. This is the *same fact* whether the caller is `analyze
/// complexity` or a call-graph build attaching node weights, so both go
/// through here and the file is parsed once per run.
///
/// The owned variant for callers that filter the list in place; a
/// caller that only reads takes [`shared_complexity_units`] and skips
/// the clone.
pub(crate) fn indexed_complexity_units(
    lang: SourceLang,
    source: &str,
) -> Result<Vec<FunctionComplexity>, AnalyzerError> {
    shared_complexity_units(lang, source).map(|units| units.as_ref().clone())
}

pub(crate) fn shared_complexity_units(
    lang: SourceLang,
    source: &str,
) -> Result<Arc<Vec<FunctionComplexity>>, AnalyzerError> {
    match AnalysisIndex::active() {
        Some(index) => index.complexity_units(SourceKey::new(lang, source), || {
            raw_complexity_units(lang, source)
        }),
        None => raw_complexity_units(lang, source).map(Arc::new),
    }
}

fn raw_complexity_units(
    lang: SourceLang,
    source: &str,
) -> Result<Vec<FunctionComplexity>, AnalyzerError> {
    dispatch_lens!(lang, source, extract_complexity_units).map_err(AnalyzerError::Parse)
}

/// Thin-wrapper findings for one file's text, shared through the active
/// index — the fact `analyze wrapper` reports and a delegation-facts
/// call-graph build attaches to its nodes.
///
/// The owned variant for callers that filter the list in place; a
/// caller that only reads takes [`shared_wrapper_findings`] and skips
/// the clone.
pub(crate) fn indexed_wrapper_findings(
    lang: SourceLang,
    source: &str,
) -> Result<Vec<WrapperFinding>, AnalyzerError> {
    shared_wrapper_findings(lang, source).map(|findings| findings.as_ref().clone())
}

pub(crate) fn shared_wrapper_findings(
    lang: SourceLang,
    source: &str,
) -> Result<Arc<Vec<WrapperFinding>>, AnalyzerError> {
    match AnalysisIndex::active() {
        Some(index) => index.wrapper_findings(SourceKey::new(lang, source), || {
            raw_wrapper_findings(lang, source)
        }),
        None => raw_wrapper_findings(lang, source).map(Arc::new),
    }
}

fn raw_wrapper_findings(
    lang: SourceLang,
    source: &str,
) -> Result<Vec<WrapperFinding>, AnalyzerError> {
    dispatch_lens!(lang, source, find_wrappers).map_err(AnalyzerError::Parse)
}

/// Lock a table, treating a poisoned mutex as usable: the tables hold
/// plain data whose invariants a panicked writer cannot have broken
/// half-way (insertions are single `HashMap::entry` calls).
fn lock<K, V>(table: &Table<K, V>) -> std::sync::MutexGuard<'_, HashMap<K, Arc<V>>> {
    table.lock().unwrap_or_else(PoisonError::into_inner)
}

thread_local! {
    static ACTIVE: std::cell::RefCell<Option<Arc<AnalysisIndex>>> =
        const { std::cell::RefCell::new(None) };
}

/// RAII activation of a fresh [`AnalysisIndex`] on the current thread.
/// Analyzers run inside the scope share extractions; dropping the scope
/// discards the index (restoring any outer scope, so nesting is safe).
///
/// The guard is `!Send` because activation and deactivation must happen
/// on the same thread the analyzers run on.
#[derive(Debug)]
pub struct AnalysisIndexScope {
    index: Arc<AnalysisIndex>,
    previous: Option<Arc<AnalysisIndex>>,
    _not_send: PhantomData<*const ()>,
}

impl AnalysisIndexScope {
    /// Install a fresh index as the active one for this thread.
    pub fn activate() -> Self {
        let index = Arc::new(AnalysisIndex::default());
        let previous = ACTIVE.replace(Some(Arc::clone(&index)));
        Self {
            index,
            previous,
            _not_send: PhantomData,
        }
    }

    /// The index this scope installed — for reading [`AnalysisIndex::stats`].
    pub fn index(&self) -> &AnalysisIndex {
        &self.index
    }
}

impl Drop for AnalysisIndexScope {
    fn drop(&mut self) {
        let (hits, misses) = self.index.stats();
        tracing::debug!(hits, misses, "analysis index scope closed");
        ACTIVE.set(self.previous.take());
    }
}

/// Run `f` with `index` installed as this thread's active index,
/// restoring the previous state afterwards (also on panic).
///
/// The active index lives in a thread local, so a rayon worker does not
/// see the scope its caller activated. A parallel walk captures
/// [`AnalysisIndex::active`] once on the calling thread and re-installs
/// it around each unit of work with this helper; `None` is accepted so
/// the capture can be passed through unconditionally.
pub(crate) fn with_installed<R>(index: Option<&Arc<AnalysisIndex>>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<Arc<AnalysisIndex>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ACTIVE.set(self.0.take());
        }
    }
    let _restore = Restore(ACTIVE.replace(index.map(Arc::clone)));
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::{
        ComplexityAnalyzer, CouplingAnalyzer, CyclesAnalyzer, DelegationAnalyzer, OutputFormat,
        WrapperAnalyzer,
    };
    use crate::test_support::write_file;

    fn key(source: &str) -> SourceKey {
        SourceKey::new(SourceLang::Rust, source)
    }

    /// The property the whole index stands on: running the analyzers
    /// under a scope changes nothing about their reports, only how
    /// often files are parsed. Exercises the per-file facts (complexity,
    /// wrappers), the call graph (cycles, delegation), and the module
    /// graph (coupling) against the same fixture, scoped and unscoped.
    #[test]
    fn scoped_reports_match_unscoped_reports_and_share_work() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "mod a;\nmod b;\n");
        write_file(
            dir.path(),
            "src/a.rs",
            "pub fn helper(n: i32) -> i32 { if n > 0 { n } else { -n } }\n",
        );
        write_file(
            dir.path(),
            "src/b.rs",
            "pub fn forward(n: i32) -> i32 { crate::a::helper(n) }\n",
        );

        let run_all = || -> Vec<String> {
            vec![
                ComplexityAnalyzer::new()
                    .analyze(dir.path(), OutputFormat::Json)
                    .unwrap(),
                WrapperAnalyzer::new()
                    .analyze(dir.path(), OutputFormat::Json)
                    .unwrap(),
                CyclesAnalyzer::new()
                    .analyze(dir.path(), OutputFormat::Json)
                    .unwrap(),
                DelegationAnalyzer::new()
                    .analyze(dir.path(), OutputFormat::Json)
                    .unwrap(),
                CouplingAnalyzer::new()
                    .analyze(dir.path(), OutputFormat::Json)
                    .unwrap(),
            ]
        };

        let unscoped = run_all();
        let scope = AnalysisIndexScope::activate();
        let scoped = run_all();
        assert_eq!(unscoped, scoped);

        let (hits, _) = scope.index().stats();
        assert!(hits > 0, "the analyzers above share at least one fact");
    }

    /// Two identical builds resolve to one assembled graph: the second
    /// is a single graph-table hit, with no new extraction misses.
    #[test]
    fn an_identical_call_graph_build_is_one_hit_and_no_new_misses() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "fn a() { b(); }\nfn b() {}\n");
        let roots = AnalyzeRoots::from(dir.path());

        let scope = AnalysisIndexScope::activate();
        let builder = CallGraphBuilder::new().with_exclude_tests(true);
        let first = builder.build(&roots).unwrap();
        let (hits_after_first, misses_after_first) = scope.index().stats();
        assert_eq!(hits_after_first, 0);

        let second = builder.build(&roots).unwrap();
        let (hits, misses) = scope.index().stats();
        assert_eq!(hits, 1, "the whole second build is one graph hit");
        assert_eq!(misses, misses_after_first);
        assert_eq!(first.nodes.len(), second.nodes.len());
        assert_eq!(first.edges.len(), second.edges.len());
    }

    /// A build that differs only in a fact flag misses the graph table
    /// but re-parses nothing: every base fact is served from the index,
    /// and only the flag's own extraction (wrappers here) is new.
    #[test]
    fn a_delegation_facts_build_reuses_every_base_fact() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "src/lib.rs", "fn a() { b(); }\nfn b() {}\n");
        let roots = AnalyzeRoots::from(dir.path());

        let scope = AnalysisIndexScope::activate();
        CallGraphBuilder::new().build(&roots).unwrap();
        let (_, misses_before) = scope.index().stats();

        CallGraphBuilder::new()
            .with_delegation_facts(true)
            .build(&roots)
            .unwrap();
        let (hits, misses) = scope.index().stats();
        assert_eq!(
            hits, 3,
            "function shapes, call shapes, and complexity come from the index"
        );
        assert_eq!(
            misses - misses_before,
            2,
            "only the new graph key and the wrapper facts are computed"
        );
    }

    /// Interface facts flow through the index like every other fact: a
    /// Go interface declared in the fixture must come back on the
    /// graph, and a second identical build must serve it as a hit. An
    /// index that swallowed interface shapes would report every
    /// interface-dispatched Go method as uncalled in `visibility`.
    #[test]
    fn an_interface_facts_build_serves_interfaces_through_the_index() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.go",
            "package lib\n\ntype Named interface {\n\tName() string\n}\n\nfunc Use(n Named) string { return n.Name() }\n",
        );
        let roots = AnalyzeRoots::from(dir.path());

        let scope = AnalysisIndexScope::activate();
        let builder = CallGraphBuilder::new().with_interface_facts(true);
        let first = builder.build(&roots).unwrap();
        assert_eq!(
            first
                .interfaces
                .iter()
                .map(|i| i.display_name.as_str())
                .collect::<Vec<_>>(),
            ["Named"],
        );
        let (_, misses_before) = scope.index().stats();

        let second = CallGraphBuilder::new()
            .with_interface_facts(true)
            .with_delegation_facts(true)
            .build(&roots)
            .unwrap();
        let (hits, misses) = scope.index().stats();
        assert_eq!(second.interfaces.len(), 1);
        assert_eq!(hits, 4, "interfaces join the three base facts as hits");
        assert_eq!(
            misses - misses_before,
            2,
            "only the new graph key and the wrapper facts are computed"
        );
    }

    /// `with_installed` must restore the thread's previous active index
    /// when it returns — a worker task that left its capture installed
    /// would leak one build's index into whatever runs next on that
    /// rayon thread.
    #[test]
    fn with_installed_swaps_the_active_index_and_restores_it() {
        let scope = AnalysisIndexScope::activate();
        let scope_ptr = std::ptr::from_ref(scope.index());
        let other = Arc::new(AnalysisIndex::default());

        with_installed(Some(&other), || {
            assert_eq!(
                AnalysisIndex::active().map(|idx| Arc::as_ptr(&idx)),
                Some(Arc::as_ptr(&other)),
            );
        });
        assert_eq!(
            AnalysisIndex::active().map(|idx| Arc::as_ptr(&idx)),
            Some(scope_ptr),
            "returning restores the scope's index",
        );

        with_installed(None, || {
            assert!(
                AnalysisIndex::active().is_none(),
                "a None capture uninstalls the index for the task",
            );
        });
        assert!(AnalysisIndex::active().is_some(), "and restores it after");
    }

    #[test]
    fn memoize_computes_once_per_key_and_tracks_stats() {
        let index = AnalysisIndex::default();
        let mut computed = 0;
        for _ in 0..3 {
            let units = index
                .complexity_units::<std::convert::Infallible>(key("fn a() {}"), || {
                    computed += 1;
                    Ok(Vec::new())
                })
                .unwrap();
            assert!(units.is_empty());
        }
        assert_eq!(computed, 1);
        assert_eq!(index.stats(), (2, 1));
    }

    #[test]
    fn different_content_or_language_is_a_different_key() {
        assert_ne!(key("fn a() {}"), key("fn b() {}"));
        assert_ne!(
            SourceKey::new(SourceLang::Python, "x = 1"),
            SourceKey::new(SourceLang::Go, "x = 1"),
        );
    }

    #[test]
    fn compute_errors_are_not_cached() {
        let index = AnalysisIndex::default();
        let err = index
            .complexity_units(key("fn a() {}"), || Err("boom"))
            .unwrap_err();
        assert_eq!(err, "boom");
        // The failed computation left no entry, so the next call
        // computes again — and a success then sticks.
        let units = index
            .complexity_units::<&str>(key("fn a() {}"), || Ok(Vec::new()))
            .unwrap();
        assert!(units.is_empty());
        assert_eq!(index.stats(), (0, 2));
    }

    #[test]
    fn active_reflects_scope_nesting() {
        assert!(AnalysisIndex::active().is_none());
        let outer = AnalysisIndexScope::activate();
        let outer_ptr = Arc::as_ptr(&AnalysisIndex::active().unwrap());
        assert_eq!(outer_ptr, std::ptr::from_ref(outer.index()));
        {
            let inner = AnalysisIndexScope::activate();
            let inner_ptr = Arc::as_ptr(&AnalysisIndex::active().unwrap());
            assert_eq!(inner_ptr, std::ptr::from_ref(inner.index()));
            assert_ne!(inner_ptr, outer_ptr);
        }
        assert_eq!(
            Arc::as_ptr(&AnalysisIndex::active().unwrap()),
            outer_ptr,
            "dropping the inner scope restores the outer one",
        );
        drop(outer);
        assert!(AnalysisIndex::active().is_none());
    }
}
