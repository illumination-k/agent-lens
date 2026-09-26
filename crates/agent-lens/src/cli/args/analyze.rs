//! `agent-lens analyze` subcommands and their argument structs.

use std::path::PathBuf;

use agent_lens::analyze::change_entropy::ChangeEntropyOptions;
use agent_lens::analyze::co_change::CoChangeOptions;
use agent_lens::analyze::cohesion::CohesionOptions;
use agent_lens::analyze::communities::CommunitiesOptions;
use agent_lens::analyze::complexity::ComplexityOptions;
use agent_lens::analyze::context_span::ContextSpanOptions;
use agent_lens::analyze::coupling::CouplingOptions;
use agent_lens::analyze::delegation::DelegationOptions;
use agent_lens::analyze::footprint::FootprintOptions;
use agent_lens::analyze::graph_query::GraphQueryOptions;
use agent_lens::analyze::hotspot::HotspotOptions;
use agent_lens::analyze::hubs::HubsOptions;
use agent_lens::analyze::impact::ImpactOptions;
use agent_lens::analyze::layers::LayersOptions;
use agent_lens::analyze::narrowable::NarrowableOptions;
use agent_lens::analyze::reach::ReachOptions;
use agent_lens::analyze::risk::RiskOptions;
use agent_lens::analyze::search::SearchOptions;
use agent_lens::analyze::similarity::SimilarityOptions;
use agent_lens::analyze::test_redundancy::TestRedundancyOptions;
use agent_lens::analyze::wrapper::WrapperOptions;
use agent_lens::analyze::{AnalyzeRoots, OutputFormat};
use clap::{Args, Subcommand};

use crate::cli::examples;

#[derive(Debug, Subcommand)]
pub(in crate::cli) enum AnalyzeCommand {
    /// Report LCOM4 cohesion units (`impl` blocks, classes, or module
    /// units).
    ///
    /// Accepts source files or directories, and more than one of
    /// either — several paths are walked into one report. In directory
    /// mode the analyzer walks recursively (respecting `.gitignore` like
    /// ripgrep) and groups findings per file. The parser is chosen from
    /// each file extension (Rust, TypeScript/JavaScript, Python, or Go).
    /// The JSON format is the default machine-readable output;
    /// `--format md` emits a compact summary tuned for LLM context.
    #[command(after_long_help = examples::COHESION)]
    Cohesion(AnalyzeCohesionArgs),
    /// Report per-function complexity metrics (Cyclomatic, Cognitive,
    /// Max Nesting, Halstead Volume, Maintainability Index).
    ///
    /// Accepts source files or directories, and more than one of
    /// either — several paths are walked into one report. In directory
    /// mode the analyzer walks recursively (respecting `.gitignore` like
    /// ripgrep), groups findings per file, and aggregates the top-level
    /// summary across the whole corpus. The parser is chosen from each
    /// file extension (Rust, TypeScript/JavaScript, Python, or Go).
    /// The JSON format is the default machine-readable output;
    /// `--format md` emits a compact summary tuned for LLM context.
    #[command(after_long_help = examples::COMPLEXITY)]
    Complexity(AnalyzeComplexityArgs),
    /// Report module-level coupling metrics for a Rust crate, a
    /// TypeScript / JavaScript module graph, a Go module, or a Python
    /// package tree.
    ///
    /// Number of Couplings, Fan-In, Fan-Out, simplified Henry-Kafura
    /// IFC ((fan_in*fan_out)^2), per-pair shared-symbol counts,
    /// Robert C. Martin's Instability `Ce/(Ca+Ce)`, and the strongly
    /// connected components of the dependency graph (cycles). `path`
    /// may be a `.rs` crate root (e.g. `src/lib.rs`) or a directory
    /// containing one, a TypeScript / JavaScript entry file
    /// (`.ts` / `.tsx` / `.mts` / `.cts` / `.js` / `.jsx` / `.mjs` /
    /// `.cjs`, or an experimental `.astro` component) whose relative
    /// imports define the module graph, a
    /// `.go` file or Go module directory (containing `go.mod`), or a
    /// `.py` file or package directory whose in-tree imports define the
    /// module graph. The graph grows outwards from that one entry, so
    /// unlike the file-walking analyzers this takes exactly one `path`.
    /// JSON is the default and carries every module; `--format md` caps
    /// the module table at `--top` (default 20) and the coupled-pair
    /// list at `--top` or 10, whichever is smaller. Dependency cycles
    /// are never truncated.
    #[command(after_long_help = examples::COUPLING)]
    Coupling(AnalyzeCouplingArgs),
    /// Compare the module clusters the dependencies form against the
    /// module boundaries the repository declares.
    ///
    /// `layers` answers whether the dependency *direction* is sane; this
    /// answers whether the *grouping* is. The headline is a pair of
    /// modularity scores over one graph: `Q` for the partition detected
    /// from the edges, and `Q` for the declared partition — each member
    /// grouped by the module it is filed under. A declared score close to
    /// the detected one means the directory structure already is the
    /// clustering. Two listings carry the gap: *misfiled members*, a
    /// member whose community is dominated by another declared module and
    /// which has more edge weight to that module than to its own, and
    /// *spanning communities*, a cluster spread over several declared
    /// modules with none of them owning a majority — a feature that never
    /// got a home. Ranking is by how lopsided a member's in/out edge
    /// counts are, never by community size.
    ///
    /// `--granularity file` (default) makes one module-graph node one
    /// member, so a finding names a file; `--granularity module`
    /// collapses files into their containing module first, so a finding
    /// names a directory. The partition is deterministic: greedy
    /// modularity agglomeration over a canonically ordered graph, with
    /// merge ties broken by node id, so the same tree always produces the
    /// same report. Modularity has a resolution limit — a small genuine
    /// cluster can be absorbed into a larger neighbour, which is why
    /// every community reports its size — and on a small or densely
    /// connected tree everything lands in one community, which is said
    /// outright rather than split into noise. `path` is a single entry
    /// point, the same one `coupling` takes. JSON is the default;
    /// `--format md` caps each listing at `--top` (default 20).
    #[command(after_long_help = examples::COMMUNITIES)]
    Communities(AnalyzeCommunitiesArgs),
    /// Report function-level call cycles: groups of 2+ functions that
    /// call each other, directly or transitively, with advisory
    /// cheapest-cut suggestions for breaking each group.
    ///
    /// Builds the same heuristic static call graph as `analyze
    /// function-graph` and reports its strongly connected components
    /// over resolved call edges only. Each tangle lists its members
    /// with file:line, whether it stays inside one file (likely
    /// intentional mutual recursion — parsers, tree walkers — and
    /// ranked below cross-file tangles), its internal call-site count,
    /// and the number of nearby ambiguous edges as a confidence
    /// warning. Break suggestions name the cheapest internal edges (by
    /// static call-site count, greedy feedback-arc heuristic) whose
    /// removal would break the cycle, with call lines as evidence —
    /// advisory only, since a cheap edge can still be load-bearing.
    /// The parser is chosen from each file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go); other extensions are
    /// ignored silently. JSON is the default; `--format md` emits a
    /// compact summary tuned for LLM context.
    #[command(after_long_help = examples::CYCLES)]
    Cycles(AnalyzeCommonArgs),
    /// Report chains of functions that only forward, and the modules
    /// built out of them.
    ///
    /// Builds the same heuristic call graph as `analyze function-graph`
    /// and walks the subgraph of functions that add nothing of their
    /// own: exactly one resolved outgoing target, no other call site
    /// beyond the language's own trivial adapters (`.clone()`,
    /// `.into()`, builtins), at most three body statements, and
    /// `cyclomatic == 1`. `analyze wrapper` reports the one-hop case
    /// with argument-level evidence; this reports what happens when it
    /// stacks — `api::save -> service::save -> repo::save ->
    /// db::insert`, where an agent opens four files to reach the one
    /// doing the work, so the terminus is the headline of every row. A
    /// module roll-up adds the "lasagna layer" half: how much of a
    /// module is forwarding and how much of that forwarding points at
    /// one other module. Classification under-reports on purpose — a
    /// forwarder that also logs, locks, or validates is not a middle
    /// man, and a function whose body facts were unavailable is
    /// reported as unclassified rather than assumed thin. Test
    /// functions, a module's sole public surface (a facade; Rust and Go
    /// only, the two adapters that extract export status), and doc
    /// comments saying "deprecated" are exempt, and a chain running
    /// through an exempt function is cut there. Chains follow resolved
    /// edges only, so depths are lower bounds; forwarding cycles have
    /// no head to walk from and are counted rather than listed. The
    /// parser is chosen from each file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go); other extensions are
    /// ignored silently. JSON is the default; `--format md` caps each
    /// listing at `--top` (default 20).
    #[command(after_long_help = examples::DELEGATION)]
    Delegation(AnalyzeDelegationArgs),
    /// Emit a static function call graph as visualization-ready data.
    ///
    /// The graph is heuristic and current-source only: nodes are functions,
    /// edges are syntactic call sites, and callee resolution is limited to
    /// exact extracted names or unique last-segment matches. The parser is
    /// chosen from each file extension (Rust, TypeScript/JavaScript,
    /// Python, or Go); other extensions are ignored silently. JSON is the
    /// default; `--format md` emits a compact sanity summary.
    #[command(after_long_help = examples::FUNCTION_GRAPH)]
    FunctionGraph(AnalyzeCommonArgs),
    /// Run one canned traversal on the static function call graph:
    /// callers, callees, neighborhood, or path.
    ///
    /// Builds the same heuristic call graph as `analyze function-graph`
    /// and answers one structural question per invocation. `--query
    /// callers|callees|neighborhood` walks resolved call edges from the
    /// function named by `--symbol` up to `--depth` (default 1;
    /// `--direction in|out|both` picks the neighborhood orientation).
    /// `--query path` reports the shortest call chain from `--symbol`
    /// to `--to`, with the call lines of every hop as evidence.
    /// Symbols match by `::`-segment suffix on the qualified name
    /// (e.g. `Resolver::resolve`) or an exact node id; ambiguous
    /// matches are listed, never guessed. Traversal follows resolved
    /// edges only, so results are lower bounds — every row carries the
    /// node's unresolved/ambiguous outgoing call-site counts — and the
    /// result set is capped by node count (`--limit`, default 50). The
    /// parser is chosen from each file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go); other extensions are
    /// ignored silently. JSON is the default; `--format md` renders
    /// span and module detail for small result sets and compact id
    /// rows for larger ones.
    #[command(after_long_help = examples::GRAPH_QUERY)]
    GraphQuery(AnalyzeGraphQueryArgs),
    /// Report each module's transitive outgoing dependency closure
    /// (its "context span").
    ///
    /// For every module in the graph, lists the directly-depended
    /// modules, the modules reachable through one or more outgoing
    /// edges, and the count of distinct source files those modules
    /// span. Useful as an "onboarding cost" estimate — how many files
    /// an agent must open to reason about a given module. `path` may
    /// be a Rust crate root (or a directory containing one), a
    /// TypeScript/JavaScript entry file, a Python file/directory, or a
    /// Go file or module directory (containing `go.mod`). Frameworks
    /// with many implicit entries (Next.js App Router, file-routed
    /// Remix / Astro) can pass `--entry-glob` repeatedly to merge
    /// several TS/JS entry trees into one report; in that mode `path`
    /// must be a directory and the patterns are evaluated relative to
    /// it.
    #[command(after_long_help = examples::CONTEXT_SPAN)]
    ContextSpan(AnalyzeContextSpanArgs),
    /// Report hub smells on the static function call graph: god
    /// functions, load-bearing utilities, bottlenecks, and misplaced
    /// functions.
    ///
    /// Builds the same call graph as `function-graph` and flags, per
    /// function: outlier fan-out (god functions, defect-prone), outlier
    /// fan-in (load-bearing blast-radius signal — check callers before
    /// editing, not a defect), Henry-Kafura information-flow spikes
    /// (`loc × (fan_in × fan_out)²`), and cross-module pull (most
    /// resolved call traffic lands in a different module). Fan-in is
    /// split into prod vs test callers, and each function carries a
    /// deterministic PageRank-importance percentile (damping 0.85,
    /// fixed 100 iterations, call-count weights). Outliers are chosen
    /// by a robust quartile rule on log-scaled metrics, never absolute
    /// thresholds. Degrees count resolved edges only, so they are
    /// lower bounds; the report cites per-module resolution confidence.
    /// The parser is chosen from each file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). JSON is the default;
    /// `--format md` emits ranked lists capped at `--top` (default 20).
    #[command(after_long_help = examples::HUBS)]
    Hubs(AnalyzeHubsArgs),
    /// Report the blast radius of a change: which functions
    /// transitively call the changed ones, which tests reach them, and
    /// where the impact concentrates.
    ///
    /// Builds the same heuristic call graph as `analyze function-graph`
    /// and walks callers backwards from each seed over resolved call
    /// edges, on the SCC condensation (a call cycle counts as one hop),
    /// up to `--depth` hops (default 5). Seeds default to the functions
    /// whose spans intersect the unstaged working-tree diff (`git diff
    /// -U0`); pass `--function <symbol>` (repeatable) to query a
    /// planned edit before making it. Symbols match by `::`-segment
    /// suffix on the qualified name or an exact node id; ambiguous
    /// matches are listed, never guessed. Per changed function the
    /// report lists direct callers verbatim, folds deeper callers to
    /// per-depth per-module counts, lists reachable test functions as a
    /// verification checklist, and states the caller total (VFI) with
    /// modules spanned. Counts follow resolved edges only and are
    /// labeled as bounds: ambiguous and caller-unattributed call sites
    /// are excluded and their counts reported. The parser is chosen
    /// from each file extension (Rust, TypeScript/JavaScript, Python,
    /// or Go); other extensions are ignored silently. JSON is the
    /// default; `--format md` caps caller and test lists at `--top`
    /// (default 20).
    #[command(after_long_help = examples::IMPACT)]
    Impact(AnalyzeImpactArgs),
    /// Report the shape of the pending diff: how many functions it
    /// touched, which edits sit outside its impact closure, and what it
    /// left behind.
    ///
    /// Reads the diff once, both sides — the index against the working
    /// tree by default (the same `git diff` every `--diff-only` reads),
    /// or the two sides of `--diff-range`. A function counts as touched
    /// when its body text changed; reindents and line shifts do not.
    /// Reports added and deleted lines and their ratio; the cognitive
    /// complexity of every touched function before and after; functions
    /// the diff turned into forwarding-only wrappers; added functions
    /// with no resolved or ambiguous caller outside tests (and how many
    /// tests call them — code written only for its test); and scatter:
    /// touched functions are grouped by overlapping blast radius (their
    /// callers within `--depth` hops, default 2), and the ones outside
    /// the largest group are listed as outside the change's impact
    /// closure. The parser is chosen from each file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go); other files count toward
    /// `skipped_file_count` only. JSON is the default; `--format md`
    /// caps each list at `--top` (default 15).
    #[command(after_long_help = examples::FOOTPRINT)]
    Footprint(AnalyzeFootprintArgs),
    /// Report an inferred layer map: what level each function and module
    /// sits on, which modules are mutually dependent, and which
    /// cross-module calls skip a level.
    ///
    /// Builds the same heuristic call graph as `analyze function-graph`
    /// and levelizes it Lakos-style over resolved call edges, at two
    /// granularities. A function level (`L`) is
    /// `1 + max(level of its callees)`, computed on the SCC condensation
    /// so a call cycle collapses to one node and its members share a
    /// level. A module level (`M`) is the same computation on the module
    /// graph induced by cross-module calls — levelizing that graph
    /// directly, rather than averaging its members' function levels,
    /// keeps module levels consistent with module edges, so a module's
    /// level need not match its members'. Level 1 is leaf code that
    /// calls nothing; the highest level is the entry side. Nothing is
    /// declared — both layerings are inferred from the code. The
    /// listings are structural facts, not errors, since callbacks and
    /// dependency injection shape the graph the same way: module cycles
    /// (mutually dependent modules, with the concrete call sites that
    /// realise each cycle), skip-level calls (a downward call passing
    /// over at least one module level), and modules whose members span
    /// many function levels (a vertical cohesion smell). Zero-fan-in
    /// `main`/exported functions are reported as the entry-point
    /// orientation set; visibility is only extracted for Rust and Go, so
    /// TypeScript and Python entries rest on zero fan-in alone. Levels
    /// follow resolved edges only, so they are lower bounds and one
    /// mis-resolved edge can lift a whole chain — per-level function and
    /// edge counts, name-fallback provenance per call site, and
    /// per-module resolution confidence are reported alongside. The
    /// parser is chosen from each file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go); other extensions are
    /// ignored silently. JSON is the default; `--format md` caps each
    /// listing at `--top` (default 20).
    #[command(after_long_help = examples::LAYERS)]
    Layers(AnalyzeLayersArgs),
    /// Rank files by `commits × cognitive_max` to surface hotspots.
    ///
    /// Walks `path` for supported source files (Rust,
    /// TypeScript/JavaScript, Python, or Go), asks `git` how many
    /// commits each file has been touched in
    /// (optionally scoped by `--since`), and joins the two with
    /// cognitive complexity. The resulting ranking points at
    /// "frequently changed *and* complex" code — where bugs concentrate
    /// and where a refactor is most likely to pay off. `path` must be
    /// inside a git working tree.
    #[command(after_long_help = examples::HOTSPOT)]
    Hotspot(AnalyzeHotspotArgs),
    /// Rank files by churn × blast radius: where an edit is most likely
    /// to be both frequent and far-reaching.
    ///
    /// The blast-radius sibling of `hotspot`. Where `hotspot` multiplies
    /// churn by intra-function complexity — which cannot separate "hot
    /// but leaf" from "hot and load-bearing" — this joins the same git
    /// churn (`--since` window included) with call-graph centrality:
    /// the max and sum of PageRank importance over each file's
    /// functions, from the same deterministic pass `analyze hubs`
    /// reports, plus transitive caller counts (VFI) as a second raw
    /// component. The composite is a rank product
    /// (`churn_rank × centrality_rank`), so no scale normalisation is
    /// needed and **lower is riskier**; every raw component is printed
    /// alongside it, together with the file's highest-PageRank function
    /// as the concrete reason it ranks. This is a blast-radius signal,
    /// not a defect signal: a high row means check callers and tests
    /// before editing. Ranking granularity is per file, since git
    /// attributes commits to files. Centrality follows resolved call
    /// edges only, so it is a lower bound and the report cites
    /// per-module resolution confidence. `path` must be inside a git
    /// working tree. The parser is chosen from each file extension
    /// (Rust, TypeScript/JavaScript, Python, or Go). JSON is the
    /// default; `--format md` caps the table at `--top` (default 20).
    #[command(after_long_help = examples::RISK)]
    Risk(AnalyzeRiskArgs),
    /// Report file pairs that git history says change together, so an
    /// agent knows what else an edit will pull in.
    ///
    /// Reads `git log` and nothing else, so unlike every other analyzer
    /// here it has no language matrix: `.toml`, `.md`, workflow YAML and
    /// fixtures are covered, which is coupling the AST-based analyzers
    /// cannot see by construction. Each pair is described as an
    /// association rule over the commits in the window (`--since`):
    /// `cochanges` is how many commits touched both, and the two
    /// `confidence` figures are the conditionals in each direction —
    /// the edge is symmetric but the conditional is not, so both are
    /// reported. `lift` guards the metric's usual false positive, a pair
    /// that only looks coupled because both files are hot; at lift near
    /// 1 the co-occurrence is what independence would predict. Ranking
    /// is `support × max(confidence)`, and `last_cochange` dates the
    /// pattern so a dead one is visibly dead. Renames are followed, so a
    /// rename mid-history does not split a pair's evidence in two.
    /// Commits touching more than `--max-commit-files` files are dropped
    /// whole as tangled, and how often that fired is reported. This is
    /// correlation, never causation: a pair changed together, and
    /// nothing here says why or which way a dependency runs. `path` must
    /// be inside a git working tree, and a shallow clone is warned about
    /// on stderr rather than reported as "no coupling". JSON is the
    /// default; `--format md` caps the table at `--top` (default 20).
    #[command(name = "co-change", after_long_help = examples::CO_CHANGE)]
    CoChange(AnalyzeCoChangeArgs),
    /// Report how scattered change activity has been, and how scattered
    /// the pending change is.
    ///
    /// `hotspot` says a file changes a lot. This says whether the change
    /// activity around it was focused or smeared, which is the part
    /// Hassan (2009) found predictive. Entropy is Shannon entropy over
    /// each file's share of the changed lines in a change set, divided
    /// by `log2(files)` so change sets of different size compare: 0 is
    /// all of it in one file, 1 is spread perfectly evenly. History is
    /// bucketed into ISO weeks or calendar months (`--period`), both in
    /// UTC and calendar-anchored rather than measured back from now, so
    /// two runs a day apart agree about which commits share a bucket. A
    /// period's entropy is attributed to its files by Hassan's weighted
    /// variant — each file takes its share of the period's changed lines
    /// times the period's entropy — and summed per file into a history
    /// complexity that ranks files against each other. `--diff-only`
    /// turns the report around: it measures the pending working-tree
    /// change as one change set and says where its scatter sits among
    /// the commits this repository actually makes, which is the form
    /// worth having before a commit exists rather than after. Like
    /// `co-change` it reads `git log` and never parses a file, so it has
    /// no language matrix. Commits touching more than
    /// `--max-commit-files` files take part in nothing, and periods
    /// under `--min-commits` are omitted rather than reported from two
    /// commits. This is a prior, not a gate: a high row says change
    /// around a file was unfocused, never that the file is wrong. `path`
    /// must be inside a git working tree, and a shallow clone is warned
    /// about on stderr. JSON is the default; `--format md` caps its
    /// tables at `--top` (default 20).
    #[command(name = "change-entropy", after_long_help = examples::CHANGE_ENTROPY)]
    ChangeEntropy(AnalyzeChangeEntropyArgs),
    /// Report where history and the code disagree: pairs that co-change
    /// with nothing declaring the dependency, and declared dependencies
    /// the window never exercised.
    ///
    /// The differential between `co-change` and the static graphs, and
    /// the one thing neither view can say alone. `hidden_coupling` is
    /// the high-value bucket: two files keep changing together and no
    /// file-level dependency runs between them either way, so something
    /// undeclared holds them together — a shared literal, a
    /// serialization format, a duplicated constant, a generated file
    /// with no regeneration step. Find that contract and make it
    /// explicit, or delete it. `suspect_dependencies` is the weaker
    /// half and ranks below it: a declared dependency whose endpoints
    /// both moved inside the window but never moved together. Over one
    /// window a stable, correct dependency looks exactly like a dead
    /// one, so a row is a question about whether the edge still carries
    /// weight, never a verdict. The two buckets are never merged into
    /// one score. The static side is the file-level projection of the
    /// module graph (`use` / `import` edges) plus resolved call edges —
    /// no new extractor — which makes it a lower bound: an unresolved
    /// call site is an edge nobody can see, so "no static path" is an
    /// upper bound on "undeclared" and the report cites per-module
    /// resolution confidence. Two classifications are kept out of both
    /// buckets and reported separately, because both have no static
    /// edge by construction: pairs with a test-like path on either side
    /// (a test co-changes with its subject by definition) and pairs
    /// where no language backend reads one side (`.md`, `.toml`,
    /// workflow YAML, fixtures — no static view rather than no
    /// dependency, and a doc that always moves with a source file is
    /// still a real contract). History flags (`--since`,
    /// `--min-support`, `--min-confidence`, `--max-commit-files`) mean
    /// exactly what they mean for `co-change`. `path` must be inside a
    /// git working tree. JSON is the default; `--format md` caps each
    /// bucket at `--top` (default 20).
    #[command(name = "hidden-coupling", after_long_help = examples::HIDDEN_COUPLING)]
    HiddenCoupling(AnalyzeHiddenCouplingArgs),
    /// Rank functions by how well they match a query.
    ///
    /// The retrieval unit is the function, not the line: every hit is a
    /// definition with its span, the line inside it that matched best,
    /// and a per-term breakdown of why it ranked — so a broad query
    /// costs a bounded, ranked result list instead of an unranked wall
    /// of line matches. Each function is indexed as five separately
    /// weighted fields (name, file path, signature, doc comment, body)
    /// and scored with BM25F, so a function *named* after the query
    /// outranks one that merely mentions it. Queries are tokenized the
    /// way identifiers are written, splitting on `_` and camelCase and
    /// also indexing the joined form, so `parse_diff_range`,
    /// `parseDiffRange` and `parse diff range` are one query. A term
    /// the corpus never spells is expanded to nearby vocabulary by
    /// character-trigram overlap (`--fuzzy`, default `missing`), which
    /// covers misspellings and — because a script without word
    /// boundaries tokenizes to one long term — substrings of a longer
    /// token; an expanded term is always worth less than a literal one.
    /// `--rank graph` scales relevance by a call-graph importance prior
    /// (`1 + ln(1 + fan_in)`), re-ordering the top candidates so
    /// load-bearing matches lead. The index is built per run and never
    /// persisted, so results are never stale. The parser is chosen from
    /// each file extension (Rust, TypeScript/JavaScript, Python, or
    /// Go); other extensions are ignored silently. JSON is the default;
    /// `--format md` emits a compact ranked listing.
    #[command(after_long_help = examples::SEARCH)]
    Search(AnalyzeSearchArgs),
    /// Report clusters of near-duplicate functions.
    ///
    /// Accepts source files or directories, and more than one of
    /// either — several paths are walked into one corpus, so a cluster
    /// spanning two of them is found where per-path runs would miss it.
    /// In directory mode the analyzer walks recursively (respecting
    /// `.gitignore` like ripgrep) and reports cross-file clusters
    /// alongside in-file ones.
    /// Function bodies are compared via TSED on their normalised AST;
    /// pairs scoring at or above `--threshold` are folded into complete-link
    /// clusters where every member is similar to every other (no chaining
    /// through weaker links). Each reported pair also carries diagnostic
    /// components that never feed the score, among them `doc_overlap` —
    /// the word-level overlap of the two doc comments, which separates
    /// "same stated intent" clones from functions that merely share a
    /// shape. The parser is chosen from each file extension
    /// (Rust, TypeScript/JavaScript, Python, or Go). The JSON format is
    /// the default machine-readable output and always carries the
    /// per-pair components; `--format md` emits a compact summary tuned
    /// for LLM context, with the doc overlap rolled in under
    /// `--doc-overlap`.
    #[command(after_long_help = examples::SIMILARITY)]
    Similarity(AnalyzeSimilarityArgs),
    /// Report declarations wider than their uses: single-use functions,
    /// single-impl abstractions, constant or dead parameters, and
    /// over-exposed visibility.
    ///
    /// Builds the same heuristic call graph as `analyze function-graph`
    /// and reports four kinds of candidate edit, each a section and a
    /// full report with its own caveats. `single-use`: a non-test
    /// function exactly one resolved caller needs, small and simple
    /// enough to inline (`--max-loc`, `--max-cyclomatic`), with a
    /// calibration section of the loc and cyclomatic distribution over
    /// every single-caller function. `single-impl`: a Rust `trait` or Go
    /// `interface` with at most one production implementor (`impl Trait
    /// for Type` blocks; in-tree Go types whose method sets cover the
    /// interface), caveated when the indirection is deliberate — a test
    /// implementor, `dyn` references, public visibility. `parameters`:
    /// a parameter every resolved production call site passes the same
    /// literal or constant path (from `--min-call-sites`, default 2), a
    /// parameter no call site passes, or one the body never reads.
    /// `visibility`: a `pub` / exported function whose resolved callers
    /// all sit in a narrower module, with the visibility they would
    /// still permit; narrowing is compiler-verified, so a wrong row
    /// costs a failed build. Fan-in counts resolved edges only, so
    /// every section backs its rows with a raw-name scan and demotes
    /// with a caveat rather than dropping; trait/interface methods and
    /// annotated functions are excluded outright where their signature
    /// is not theirs to change. `single-impl` and `visibility` read
    /// Rust and Go only. The sections share one graph build.
    /// `--section` picks the sections (default all four). JSON nests
    /// each section's report under its key and is the default;
    /// `--format md` stacks them and caps each listing at `--top`
    /// (default 20).
    #[command(after_long_help = examples::NARROWABLE)]
    Narrowable(AnalyzeNarrowableArgs),
    /// Report who reaches each production function: untested,
    /// test-only, and unreachable code.
    ///
    /// Builds the same heuristic call graph as `analyze function-graph`
    /// and places every production function in one cell of
    /// reached-from-entries × reached-from-tests, where entries are
    /// `main`, Go `init`, `pub` / exported declarations, and anything
    /// carrying a non-inert annotation. Each off-diagonal cell is a
    /// section, and each section is a full report with its own bounds.
    /// `untested`: entries reach it, no test does — a forward walk from
    /// every test function over resolved edges, grouped by module and
    /// ranked by untested LOC; structural, not coverage, and an upper
    /// bound, since integration tests that drive the built binary have
    /// no in-graph caller. `test-only`: a test reaches it, no entry does
    /// — the candidate edit is moving it into the language's test
    /// scope; public declarations whose resolved callers are all tests
    /// are a separate, weaker list. `unreachable`: neither does — after
    /// a raw identifier-reference scan each row lands in a tier:
    /// `confirmed` (private, unreferenced, deletable on this evidence),
    /// `likely` (the declaration reaches outside the analyzed path),
    /// `unknown` (trait or interface dispatch, an annotation, an
    /// ambiguous call site, a raw reference); clusters that only call
    /// each other are islands with a deletion order. Export status is
    /// extracted for Rust and Go only, so `test-only` and `unreachable`
    /// judge those languages and count the rest. `--exclude-tests`
    /// removes the test roots and every section says so. The sections
    /// share one graph build. `--section` picks the cells (default all
    /// three). JSON nests each section's report under its key and is
    /// the default; `--format md` stacks them, caps each listing at
    /// `--top` (default 20), and renders `unreachable` from `confirmed`
    /// up (`--tier` widens it).
    #[command(after_long_help = examples::REACH)]
    Reach(AnalyzeReachArgs),
    /// Report tests that are near-copies of each other, and which one
    /// of each set to keep.
    ///
    /// Scores test bodies against each other exactly as `analyze
    /// similarity` does, then runs a greedy dominating-set pass over
    /// the result: the test that makes the most others redundant is the
    /// one to keep, and the rest are its near-copies. Modelled on
    /// similarity-based test-suite minimization (LTM), without the
    /// language-model embeddings or the genetic search — candidate
    /// generation already hands the selection a sparse graph, where
    /// greedy is both cheaper and deterministic. Each group carries a
    /// verdict from how far the bodies agree once each node's recorded
    /// value counts and not just its label: `duplicate` when nothing
    /// the parser can see separates the copies, `parameterize` when
    /// they share a shape but name different things, which is one table
    /// test rather than a deletion. The call graph then subtracts what
    /// it can disprove, in two directions — a pair whose tests reach no
    /// production function in common is not a pair, and a test that is
    /// the sole static caller of something is never offered as
    /// foldable; `--no-reach-guard` skips the graph build and both
    /// checks. A body smaller than `--min-body-nodes` is skipped
    /// outright, because a Rust test that is one unexpanded
    /// `assert_eq!` has the same two-node body as every other such
    /// test. Structural, not behavioural: matching bodies are not
    /// matching assertions, so the report names candidates and never
    /// blesses a deletion. JSON is the default; `--format md` caps the
    /// group list at `--top` (default 20).
    #[command(name = "test-redundancy", after_long_help = examples::TEST_REDUNDANCY)]
    TestRedundancy(AnalyzeTestRedundancyArgs),
    /// Report functions whose body, after stripping a short chain of
    /// trivial adapters, is just a forwarding call to another function.
    ///
    /// Accepts source files or directories, and more than one of
    /// either — several paths are walked into one report. In directory
    /// mode the analyzer walks recursively (respecting `.gitignore` like
    /// ripgrep) and groups findings per file. The parser is chosen from
    /// each file extension (Rust, TypeScript/JavaScript, Python, or Go).
    /// The JSON format is the default machine-readable output and always
    /// carries every finding; `--format md` emits a compact summary
    /// tuned for LLM context, capped at `--top` wrappers (default 20) in
    /// file order, with the remainder counted at the end.
    #[command(after_long_help = examples::WRAPPER)]
    Wrapper(AnalyzeWrapperArgs),
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeCommonArgs {
    /// One or more source files or directories to analyze. Several
    /// paths are walked into a single report, so a finding spanning two
    /// of them (a duplicate, a call edge) is still found — which running
    /// the analyzer once per tree cannot do. Display paths are written
    /// relative to the paths' deepest common ancestor.
    #[arg(required = true, num_args = 1.., value_name = "PATH")]
    pub(in crate::cli) paths: Vec<PathBuf>,
    /// Output format. Defaults to JSON.
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    pub(in crate::cli) format: OutputFormat,
    #[command(flatten)]
    pub(in crate::cli) path_filter: AnalyzePathArgs,
}

impl AnalyzeCommonArgs {
    pub(in crate::cli) fn into_parts(self) -> (AnalyzeRoots, OutputFormat, AnalyzePathArgs) {
        (AnalyzeRoots::new(self.paths), self.format, self.path_filter)
    }
}

/// The single-entry counterpart to [`AnalyzeCommonArgs`], for the
/// graph-rooted analyzers.
///
/// `coupling` and `context-span` grow their module graph outwards from
/// one entry point — a crate root, a TS/JS entry file, a Go module — so
/// "several roots" has no meaning for them: two entry points are two
/// graphs, not a wider one. They keep the single-PATH signature, and the
/// error a non-entry path already produces stays the right answer.
#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeRootArgs {
    /// Path to a source file, Rust crate root, or directory to analyze.
    pub(in crate::cli) path: PathBuf,
    /// Output format. Defaults to JSON.
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    pub(in crate::cli) format: OutputFormat,
    #[command(flatten)]
    pub(in crate::cli) path_filter: AnalyzePathArgs,
}

impl AnalyzeRootArgs {
    pub(in crate::cli) fn into_parts(self) -> (PathBuf, OutputFormat, AnalyzePathArgs) {
        (self.path, self.format, self.path_filter)
    }
}

// Each analyzer's flag group lives with the analyzer, as the same type
// that deserializes its `[profile.<name>.<tool>]` table (see
// `agent_lens::analyze::options`). These structs only bolt the shared
// path/format arguments onto it, so a profile entry can be handed to
// `AnalyzeCommand` without a field-by-field copy.

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeCohesionArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: CohesionOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeComplexityArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: ComplexityOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeContextSpanArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeRootArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: ContextSpanOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeCouplingArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeRootArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: CouplingOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeCommunitiesArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeRootArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: CommunitiesOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeDelegationArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: DelegationOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeGraphQueryArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: GraphQueryOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeHotspotArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: HotspotOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeHubsArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: HubsOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeFootprintArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: FootprintOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeImpactArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: ImpactOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeLayersArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: LayersOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeRiskArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: RiskOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeCoChangeArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: CoChangeOptions,
}

/// `hidden-coupling` scopes the same history window with the same
/// thresholds as `co-change`, so it flattens that analyzer's option
/// group rather than declaring a byte-identical second one.
#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeHiddenCouplingArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: CoChangeOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeChangeEntropyArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: ChangeEntropyOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeSearchArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: SearchOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeSimilarityArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: SimilarityOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeTestRedundancyArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: TestRedundancyOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeReachArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: ReachOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeNarrowableArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: NarrowableOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeWrapperArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: WrapperOptions,
}

#[derive(Debug, Clone, Args, Default)]
pub(in crate::cli) struct AnalyzePathArgs {
    /// Analyze only files that look like tests (`tests/`, `*_test.*`,
    /// `*.test.*`, `test_*`, etc.). For similarity reports, this also
    /// keeps language-level test functions inside non-test files, such
    /// as Rust `#[cfg(test)]` modules.
    #[arg(long, conflicts_with = "exclude_tests")]
    pub(in crate::cli) only_tests: bool,
    /// Exclude files that look like tests. For similarity reports, this
    /// also drops language-level test functions such as Rust
    /// `#[cfg(test)]` modules.
    #[arg(long, conflicts_with = "only_tests")]
    pub(in crate::cli) exclude_tests: bool,
    /// Exclude paths matching this glob. Repeatable. Bare patterns also
    /// match at any depth, so `--exclude generated.rs` matches
    /// `src/generated.rs`. A pattern containing `/` is anchored at the
    /// analyzed path — with several PATHs, at their deepest common
    /// ancestor, the same base display paths use.
    #[arg(long = "exclude", value_name = "GLOB")]
    pub(in crate::cli) exclude: Vec<String>,
}
