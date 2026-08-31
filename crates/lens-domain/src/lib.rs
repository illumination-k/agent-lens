//! Shared analysis primitives for `agent-lens`.
//!
//! This crate holds language-agnostic building blocks that each
//! language-specific crate (currently [`lens-rust`](../lens_rust/index.html))
//! plugs into:
//!
//! * [`TreeNode`] — a small labelled tree used as a common currency for AST
//!   comparison.
//! * [`apted`] — tree edit distance (Zhang-Shasha-style with configurable
//!   operation costs), modelled after `similarity-ts-core`'s APTED.
//! * [`tsed`] — a normalised similarity score derived from the edit distance,
//!   with an optional size penalty for short functions.
//! * [`function`] — the [`LanguageParser`] trait, [`FunctionDef`] type, the
//!   [`find_similar_functions`] helper that drives pairwise comparison, and
//!   [`cluster_similar_pairs`] for collapsing pairs into complete-link
//!   clusters.
//! * [`line_index`] — byte offset → 1-based line number mapping, shared by
//!   every adapter whose parser reports positions as byte offsets.
//! * [`lsh`] — MinHash + banded LSH used to pre-filter candidate pairs once
//!   the corpus grows past a couple hundred functions, replacing the
//!   quadratic cartesian product with a near-linear pass.
//! * [`block_shape`] — statement-sequence windows inside function
//!   bodies, the unit compared by `similarity --target blocks`. Adapters
//!   supply positioned statements; the windowing that turns them into
//!   comparison units lives here so every language sees the same rule.
//! * [`cohesion`] — LCOM4-style cohesion metric over method graphs that the
//!   language adapters (e.g. `lens-rust`) populate.
//! * [`communities`] — deterministic community detection over a module
//!   dependency graph, scored against the grouping the repository
//!   declares. Answers whether the directory structure matches the
//!   clustering the dependencies actually form.
//! * [`complexity`] — per-function Cyclomatic / Cognitive / Nesting / Halstead
//!   counts, plus derived Maintainability Index. Adapters fill in the counts;
//!   the derived metrics live here so every language goes through the same
//!   formula.
//! * [`coupling`] — module-level Number of Couplings / Fan-In / Fan-Out /
//!   Henry-Kafura IFC / Inter-module coupling / Instability / dependency
//!   cycles. Adapters produce [`CouplingEdge`]s; this module folds them
//!   into the report.
//! * [`context_span`] — for each module, the transitive closure of its
//!   outgoing dependencies. Reuses the [`CouplingEdge`] graph and
//!   answers "how many other modules must I read to fully understand
//!   this one".
//! * [`hotspot`] — `commits × cognitive_max` scoring per file. Adapters
//!   feed in per-file complexity rollups and a churn table; this module
//!   merges them into a ranked list.
//! * [`change_entropy`] — Hassan-style change scatter: how spread out
//!   change activity was in each period, and how much of that scatter
//!   accumulates onto each file as history complexity. Also measures a
//!   single pending change against the distribution of commits it is
//!   about to join.
//! * [`cochange`] — temporal (logical) coupling: which files change in
//!   the same commit, how reliably in each direction, and whether the
//!   pairing beats what two files that busy would do by chance. The CLI
//!   supplies per-commit file sets; the association-rule arithmetic
//!   (support / confidence / lift) lives here.
//! * [`search`] — BM25F retrieval over function-level documents, with a
//!   character n-gram fallback for query terms the corpus never spells.
//!   Built per run from the corpus the caller already parsed, so there is
//!   no index to invalidate.
//! * [`risk`] — the churn × blast-radius sibling of [`hotspot`]: the
//!   same churn table joined with per-file call-graph centrality by
//!   rank product, for "how carefully should I treat this edit?".
//! * [`method_names`] — the [`UbiquitousMethodNames`],
//!   [`BuiltinFunctionNames`] and [`InertAttributeNames`] lookup shapes.
//!   Adapters own the actual name tables (`.clone()`, `.map()`,
//!   `append(…)`, `len(…)`, `#[inline]`, …); the call-graph resolver
//!   consults the first two to avoid attributing a call to a workspace
//!   function that merely shares a standard-library method name or a
//!   language builtin, and reachability analysis consults the third to
//!   tell a harmless annotation from one that can register a definition
//!   with machinery no call site names.
//! * [`wrapper`] — thin-wrapper finding shape. Adapters decide what
//!   counts as a trivial adapter in their grammar; the result type is
//!   shared so `agent-lens` can dispatch on language without per-adapter
//!   conversion.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod apted;
pub mod block_shape;
pub mod change_entropy;
pub mod cochange;
pub mod cohesion;
pub mod communities;
pub mod complexity;
pub mod context_span;
pub mod coupling;
pub mod function;
pub mod hotspot;
pub mod line_index;
pub mod lsh;
pub mod method_names;
pub mod naming;
pub mod risk;
pub mod search;
pub mod syntax;
pub mod tree;
pub mod tsed;
pub mod type_shape;
pub mod wrapper;

pub use apted::{
    APTEDOptions, SubtreeSizes, collect_subtree_sizes, compute_edit_distance,
    compute_edit_distance_with_subtree_sizes,
};
pub use block_shape::{
    BlockShape, BlockWindowOptions, DEFAULT_MAX_WINDOW_STATEMENTS, StatementSeq, StatementUnit,
    block_windows,
};
pub use change_entropy::{
    ChangeEntropyReport, ChangeEntropyThresholds, CommitChanges, DEFAULT_MIN_COMMITS_PER_PERIOD,
    EntropyDistribution, FileChange, FileEntropy, FilePeriodContribution, Period, PeriodEntropy,
    Scatter, compute_change_entropy, module_of,
};
pub use cochange::{
    CoChangeCounts, CoChangePair, CoChangeReport, CoChangeThresholds, CoChangeTotals, CommitFiles,
    DEFAULT_MAX_COMMIT_FILES, DEFAULT_MIN_CONFIDENCE, DEFAULT_MIN_SUPPORT, PairSupport,
    compute_cochange, rank_cochange_pairs, tally_cochange,
};
pub use cohesion::{
    CohesionUnit, CohesionUnitKind, MethodCohesion, compute_components, compute_lcom96,
};
pub use communities::{
    Community, CommunityEdge, CommunityNode, CommunityReport, DEFAULT_MIN_COMMUNITY, DeclaredShare,
    MisfiledMember, SpanningCommunity, detect_communities,
};
pub use complexity::{ComplexityCounters, FunctionComplexity, HalsteadAcc, HalsteadCounts};
pub use context_span::{ContextSpanReport, ModuleContextSpan, compute_context_spans};
pub use coupling::{
    CouplingEdge, CouplingReport, DependencyCycle, EdgeKind, ModuleMetrics, ModulePath,
    PairCoupling, compute_report,
};
pub use function::{
    CandidateStrategy, FunctionDef, FunctionSignature, LanguageParseError, LanguageParser,
    ReceiverShape, SimilarCluster, SimilarPair, cluster_similar_pairs, find_similar_functions,
    find_similar_pair_indices, find_similar_pair_indices_with_strategy,
};
pub use hotspot::{FileChurn, FileComplexity, HotspotEntry, compute_hotspots};
pub use line_index::LineIndex;
pub use lsh::{LshOptions, lsh_candidate_pairs, lsh_candidate_pairs_for_trees};
pub use method_names::{BuiltinFunctionNames, InertAttributeNames, UbiquitousMethodNames};
pub use naming::{identifier_tokens, path_segments, qualify, qualify_module, starts_uppercase};
pub use risk::{FileCentrality, RiskEntry, compute_risk};
pub use search::{
    Bm25Options, FuzzyOptions, IndexOptions, SearchDocument, SearchField, SearchHit, SearchIndex,
    TermScore,
};
pub use syntax::{
    ArgumentShape, BodyShape, CallShape, FunctionShape, ImportShape, InterfaceMethodShape,
    InterfaceShape, LexicalResolutionStatus, OwnerKind, OwnerShape, ParameterShape,
    ReceiverExprKind, SignatureShape, SourceSpan, SyntaxFact, TraitDeclShape, TraitImplShape,
    VisibilityShape, callee_names_local_binding,
};
pub use tree::TreeNode;
pub use tsed::{TSEDOptions, calculate_tsed, calculate_tsed_with_subtree_sizes};
pub use type_shape::{
    TypeDefKind, TypeMemberShape, TypeShape, TypeVariantShape, normalize_type_text,
};
pub use wrapper::{ReuseMetrics, WrapperFinding, args_pass_through_by};
