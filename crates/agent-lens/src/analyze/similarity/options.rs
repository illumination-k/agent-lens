//! The `analyze similarity` vocabulary: what to compare
//! ([`SimilarityTarget`]), how to score it ([`SimilarityMethod`]), how to
//! pair by name ([`PairKey`]), the CLI/profile option
//! table ([`SimilarityOptions`]) and their defaults.
//!
//! A leaf of the `similarity` module tree: the pipeline stages
//! (`corpus`, `candidates`, `score`, `report`) read these types, so they
//! live below all of them rather than in the parent that drives them.

use crate::analyze::{DiffScope, parse_diff_range};

/// Default similarity threshold. Picked to match the cutoff used by the
/// PostToolUse `similarity` hook so the on-demand analyzer reports the
/// same pairs that show up in the hook's transcript message.
pub const DEFAULT_THRESHOLD: f64 = 0.85;

/// Default minimum line count for a function to be considered. Mirrors the
/// `--min-lines` default in `similarity-ts`: tiny functions (one-liners,
/// trivial getters) form too many spurious matches.
pub const DEFAULT_MIN_LINES: usize = 5;

/// Default minimum line count for `--target types`. Type definitions run
/// shorter than function bodies — a meaningful struct is often 3-4 lines —
/// so the function default would drop most of the corpus, while 1-2 lines
/// is unit structs and one-line aliases whose trees are too small to
/// score meaningfully.
pub const DEFAULT_TYPE_MIN_LINES: usize = 3;

/// Default minimum line count for `--target blocks`.
///
/// Held at the same 5 lines as functions, and for the same reason read
/// from the other end. A window of three or four lines is a guard clause
/// and at most one statement beside it, and every language writes its
/// guard clause the same way everywhere it appears: at a floor of 3 the
/// top of a Go report is nothing but `if err != nil { return nil, err }`
/// (issue #441). That is a real repetition, but not one anybody can
/// extract a helper for, and it crowds out the findings that are. The
/// boilerplate this target exists to find — a mapped error tail, a
/// URL-assembly preamble — clears five lines; `--min-lines` is there for
/// a deliberate hunt below it.
pub const DEFAULT_BLOCK_MIN_LINES: usize = 5;

/// Default floor for `--paired-by`. A name-matched pair scoring below
/// this shares a name and essentially nothing else, which in practice
/// means two unrelated functions that happen to be called the same thing
/// (every analyzer's own `format_markdown`, every type's `new`) rather
/// than one implementation that drifted from another. Drift lives in the
/// band between this floor and the threshold; below it the report would
/// be dominated by namesakes and the ascending sort would put them first.
/// Pass `--drift-floor 0` to see them anyway.
pub const DEFAULT_DRIFT_FLOOR: f64 = 0.3;

/// Algorithm used to score how similar two function bodies are.
///
/// Every method feeds the same `0.8 * body + 0.2 * signature` blend and
/// the same clustering, so only the body score differs. The choice is
/// about what counts as "the same body" — its syntax tree, its token
/// stream, or its dependence structure — not a different report shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SimilarityMethod {
    /// APTED tree-edit distance over the body AST. Precise and the most
    /// faithful to structural change, but the costliest to compute.
    #[default]
    Tsed,
    /// Weighted Jaccard overlap of the body's preorder token k-grams.
    /// Cheaper than TSED and more tolerant of reordered code, at the
    /// cost of some precision.
    Token,
    /// Longest common subsequence of the body's preorder token sequence,
    /// over the longer sequence's length. Order-aware where `token` is
    /// not, and tolerant of a large inserted or deleted gap, so it
    /// catches the Type-3 clone that had a block of statements added in
    /// the middle (NIL, ESEC/FSE 2021).
    Lcs,
    /// Weisfeiler-Lehman kernel over the body's program dependence
    /// graph: statements as nodes, control and data dependences as
    /// edges. Invariant to reordering independent statements and to
    /// renaming locals, so it finds the semantic clone a tree-edit
    /// distance scores low on; a body with the same statements wired
    /// differently scores lower than TSED would give it.
    Pdg,
}

impl SimilarityMethod {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Tsed => "tsed",
            Self::Token => "token",
            Self::Lcs => "lcs",
            Self::Pdg => "pdg",
        }
    }

    /// Whether the TSED cheap candidate filters (size / label / arity /
    /// shingle bounds on the tree-edit distance) are sound for this
    /// method's score. Only TSED's own score is bounded by them; the
    /// other methods must score every enumerated pair.
    pub(super) fn uses_tree_filters(self) -> bool {
        matches!(self, Self::Tsed)
    }
}

/// Which definitions the analyzer compares.
///
/// Every target shares the corpus, pairing, scoring, and report
/// pipeline; the choice decides what each unit *is* — a function body, a
/// type definition's member shape (struct/class/interface fields, enum
/// variants, alias targets), or a run of statements inside a function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SimilarityTarget {
    /// Function-shaped units: free functions, methods, bound closures.
    #[default]
    Functions,
    /// Type definitions: Rust struct/enum/type alias, TS interface/type
    /// alias/enum, Python annotated classes and Enum subclasses, Go
    /// struct/alias.
    Types,
    /// Statement runs *inside* function bodies. Finds the copy-paste
    /// that whole-definition comparison structurally cannot see: an
    /// error-mapping tail repeated at 55 call sites, a URL-assembly
    /// preamble repeated in every endpoint method. The enclosing
    /// functions differ, so they never cluster; the repeated fragment
    /// does.
    Blocks,
}

impl SimilarityTarget {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Functions => "functions",
            Self::Types => "types",
            Self::Blocks => "blocks",
        }
    }

    /// Singular noun for markdown headings and counts.
    pub(super) fn noun(self) -> &'static str {
        match self {
            Self::Functions => "function",
            Self::Types => "type",
            Self::Blocks => "block",
        }
    }
}

/// `analyze similarity` flags, and the `[profile.<name>.similarity]` table.
///
/// Written out rather than generated by
/// `analyzer_options!` because several of these flags
/// carry real clap defaults: `Default` and the `default_value_t`
/// attributes must agree, so both read the same consts here.
#[derive(Debug, Clone, clap::Args, serde::Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct SimilarityOptions {
    /// Cap the markdown ranking to the top-N entries. JSON output always
    /// carries the full list.
    #[arg(long)]
    pub top: Option<usize>,
    /// Restrict the report to units touching unstaged changed lines in
    /// `git diff -U0`.
    #[arg(long)]
    pub diff_only: bool,
    /// Restrict the report to units touching lines changed in the given
    /// git revision range, as `git diff -U0 <range>` (`HEAD~1..HEAD`,
    /// `main...topic`). Reads committed history instead of the working
    /// tree.
    #[arg(
        long,
        value_name = "RANGE",
        conflicts_with = "diff_only",
        value_parser = parse_diff_range,
    )]
    pub diff_range: Option<String>,
    /// Similarity threshold in [0.0, 1.0]. Pairs scoring at or above
    /// this value are eligible for clustering, and the same threshold
    /// is the complete-link cut so every pair inside a reported cluster
    /// stays at or above it. Defaults to the same cutoff used by the
    /// PostToolUse `similarity` hook.
    #[arg(long, visible_alias = "min-score", default_value_t = DEFAULT_THRESHOLD)]
    pub threshold: f64,
    /// Multi-threshold sweep: a comma-separated ascending ladder of
    /// thresholds, e.g. `--sweep 0.6,0.75,0.85`. Pairs are scored and
    /// clustered once at the lowest rung, and every reported cluster is
    /// annotated with the highest rung at which its complete-link structure
    /// survives intact — a coarse dendrogram in one run that separates
    /// verbatim clones from merely structural parallels. Supersedes the
    /// threshold option, which it conflicts with.
    #[arg(long, value_delimiter = ',', conflicts_with = "threshold")]
    pub sweep: Vec<f64>,
    /// Invert the report: match functions by name *first*, score second,
    /// and list every cross-file match regardless of threshold, most
    /// drifted first. Threshold clustering can only report what is still
    /// similar, so two parallel implementations that have drifted apart —
    /// the likeliest missed sync — silently drop out of it. `qualified`
    /// (alias `name`) keys on the normalized owner-qualified name, so
    /// `Summary::from` matches `JsSummary::from`; `method` keys on the
    /// method segment alone, which finds siblings whose types were
    /// renamed at the cost of grouping unrelated same-named functions.
    #[arg(long, value_enum, value_name = "KEY", conflicts_with = "sweep")]
    pub paired_by: Option<PairKey>,
    /// Floor for name-anchored pairing: name matches scoring below this
    /// are dropped as unrelated namesakes rather than reported as drift.
    /// Drift lives between this floor and the threshold; below it a
    /// shared name means two different functions that happen to be
    /// called the same thing. Pass `0` to report every match. No effect
    /// unless pairing is enabled.
    #[arg(long, default_value_t = DEFAULT_DRIFT_FLOOR, requires = "paired_by")]
    pub drift_floor: f64,
    /// Minimum source line count for a unit to be considered. Units
    /// shorter than this are dropped before pairwise comparison; keeps
    /// trivial getters / one-liners out of the report. Defaults per
    /// target: 5 for `functions` and `blocks`, 3 for `types`.
    #[arg(long)]
    pub min_lines: Option<usize>,
    /// Comparison unit. `functions` (default) compares function bodies.
    /// `types` compares type definitions instead — Rust struct/enum/type
    /// alias, TS interface/type alias/enum, Python annotated classes /
    /// dataclasses / Enum subclasses, Go struct/alias — by their member
    /// shape (field names and types, enum variants, alias targets), so
    /// duplicated DTOs and drifted mirror structs surface the same way
    /// duplicated functions do. With name-anchored pairing, only the
    /// `qualified`/`name` key applies; `method` has no meaning for a
    /// type and is rejected.
    #[arg(long, value_enum, default_value_t = SimilarityTarget::Functions)]
    pub target: SimilarityTarget,
    /// Body-scoring algorithm. `tsed` (default) uses APTED tree-edit
    /// distance over the body AST. `token` compares preorder token
    /// k-gram multisets — faster and more tolerant of reordered code,
    /// but less precise. `lcs` scores the longest common subsequence
    /// of the preorder token sequences — order-aware, and tolerant of a
    /// large inserted gap that splits a copied body. `pdg` compares program dependence graphs
    /// (statements wired by control and data dependence) with a
    /// Weisfeiler-Lehman kernel — invariant to statement reordering
    /// and local renaming, so it finds semantic clones the tree shape
    /// hides. Scores from different methods are not directly
    /// comparable.
    #[arg(long, value_enum, default_value_t = SimilarityMethod::Tsed)]
    pub method: SimilarityMethod,
    /// Weight each token k-gram by its inverse document frequency over
    /// the analyzed corpus, so boilerplate every body repeats (logging,
    /// error plumbing, builder chains) counts for less than the logic a
    /// body does not share with the rest of the corpus. Only affects
    /// `--method token`.
    #[arg(long)]
    pub idf: bool,
    /// Roll the per-pair doc-comment overlap up into the markdown
    /// report, as a range plus how many of the cluster's pairs carried
    /// doc text on both sides. Diagnostic only — it never feeds the
    /// similarity score. High overlap on a high-similarity cluster means
    /// the *stated intent* matches too (a strong merge candidate, often
    /// a copy-paste that took the doc with it); low overlap flags a
    /// structural coincidence that usually should not be merged. JSON
    /// output always carries the per-pair values, with or without this
    /// flag.
    #[arg(long)]
    pub doc_overlap: bool,
}

impl Default for SimilarityOptions {
    fn default() -> Self {
        Self {
            top: None,
            diff_only: false,
            diff_range: None,
            threshold: DEFAULT_THRESHOLD,
            sweep: Vec::new(),
            paired_by: None,
            drift_floor: DEFAULT_DRIFT_FLOOR,
            min_lines: None,
            target: SimilarityTarget::Functions,
            method: SimilarityMethod::Tsed,
            idf: false,
            doc_overlap: false,
        }
    }
}

impl SimilarityOptions {
    /// The diff gate these options ask for. Hand-written to match what
    /// `analyzer_options!` generates for the analyzers whose option
    /// types it does build, so the two spellings stay interchangeable.
    pub fn diff_scope(&self) -> DiffScope {
        DiffScope::new(self.diff_only, self.diff_range.clone())
    }

    /// Whether both diff flags were set. clap rejects the combination
    /// on the CLI, so this is what a `[profile.<name>.similarity]`
    /// table is checked against.
    pub fn has_diff_conflict(&self) -> bool {
        self.diff_only && self.diff_range.is_some()
    }
}

/// How two functions are decided to be siblings — i.e. parallel
/// implementations that ought to stay in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PairKey {
    /// Owner-qualified name, normalized: `Summary::from` and
    /// `JsSummary::from` share the key `summary::from` because case and
    /// separator conventions are folded away and binding affixes
    /// (`Js`, `Py`, `Wasm`, `Napi`, `Node`, `Ts`) are stripped from the
    /// owner. The tight key: few, mostly meaningful matches.
    #[default]
    #[value(alias = "name")]
    #[serde(alias = "name")]
    Qualified,
    /// The method segment alone, normalized: every `::from` in the tree
    /// keys as `from`. The loose key — it finds siblings whose owning
    /// types were renamed past recognition, at the cost of grouping
    /// unrelated functions that merely share a common method name.
    Method,
}

impl PairKey {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Qualified => "qualified",
            Self::Method => "method",
        }
    }
}
