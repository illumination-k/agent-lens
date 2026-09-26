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
use agent_lens::analyze::parameters::ParametersOptions;
use agent_lens::analyze::risk::RiskOptions;
use agent_lens::analyze::search::SearchOptions;
use agent_lens::analyze::similarity::SimilarityOptions;
use agent_lens::analyze::single_impl::SingleImplOptions;
use agent_lens::analyze::single_use::SingleUseOptions;
use agent_lens::analyze::test_only::TestOnlyOptions;
use agent_lens::analyze::test_redundancy::TestRedundancyOptions;
use agent_lens::analyze::unreachable::UnreachableOptions;
use agent_lens::analyze::untested::UntestedOptions;
use agent_lens::analyze::visibility::VisibilityOptions;
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
    /// Report traits and interfaces with at most one production
    /// implementor.
    ///
    /// Inventories every Rust `trait` and Go `interface` declared in
    /// the analyzed tree and takes an implementor census: `impl Trait
    /// for Type` blocks matched by the trait path's trailing identifier
    /// for Rust, and in-tree types whose method sets cover the
    /// interface by name and parameter count for Go. Declarations with
    /// at most one production implementor are reported — an abstraction
    /// with one user is a candidate for replacement with the concrete
    /// type, not a verdict — with caveats where the indirection is
    /// deliberate or the census weaker: a test implementor (a mock
    /// seam), `dyn` references (dynamic dispatch), public visibility
    /// (implementors outside the tree), a shared declaration name. A
    /// raw-name scan counts references outside the declaration and its
    /// implementors' blocks, which is what removing the abstraction
    /// costs. An implementor-count histogram gives the tree's base
    /// rate. Rust and Go only. JSON is the default; `--format md` caps
    /// each section at `--top` (default 20).
    #[command(name = "single-impl", after_long_help = examples::SINGLE_IMPL)]
    SingleImpl(AnalyzeSingleImplArgs),
    /// Report functions with exactly one resolved production caller as
    /// inline candidates.
    ///
    /// Builds the same heuristic call graph as `analyze function-graph`
    /// and lists the non-test functions exactly one resolved caller
    /// needs, provided the body is small and simple enough to fold into
    /// that caller (`--max-loc`, `--max-cyclomatic`; both absolute and
    /// per-repository on purpose). Each row names the caller and the
    /// call sites, and carries caveats where the single-caller claim or
    /// the edit is weaker: wider-than-private visibility, direct test
    /// callers, several call sites, a cross-module caller, ambiguous or
    /// fallback-resolved inbound edges. Trait/interface methods,
    /// functions with live annotations, and self-recursive functions
    /// are excluded outright. Fan-in counts resolved edges only, so a
    /// hidden caller (a macro body, an unresolved call site) is
    /// possible — a raw-name scan therefore caveats any row whose bare
    /// name is written outside its definition and its known callers,
    /// and the report says what the scan cannot see (files the graph
    /// did not scan). A
    /// calibration section carries the loc and cyclomatic distribution
    /// over every single-caller function, so thresholds can be set from
    /// one run instead of guessed. The parser is chosen from each file
    /// extension (Rust, TypeScript/JavaScript, Python, or Go). JSON is
    /// the default; `--format md` caps the lists at `--top`
    /// (default 20).
    #[command(name = "single-use", after_long_help = examples::SINGLE_USE)]
    SingleUse(AnalyzeSingleUseArgs),
    /// Report constant arguments and dead parameters.
    ///
    /// Builds the same heuristic call graph as `analyze function-graph`
    /// with per-call-site argument shapes, and asks two questions per
    /// declared parameter. Constant argument: does every resolved
    /// production call site pass the same literal (or the same
    /// constant-looking path)? The candidate edit is inlining the value
    /// and dropping the parameter; a Python/TypeScript parameter no
    /// call site ever passes is reported as default-only. Dead
    /// parameter: does the body ever read the name past its
    /// declaration? The check is textual (mentions in strings or
    /// comments count as reads), so it under-reports. "Every call
    /// site" is what the graph could see: sites with spreads,
    /// unmatched keywords, or arity mismatches, ambiguous inbound
    /// edges, raw name references, and public/exported visibility each
    /// demote a row with a caveat. Trait/interface methods and
    /// annotated functions are excluded outright — their signatures
    /// are not theirs to change. A parameter with one call site is
    /// `analyze single-use`'s finding, so `--min-call-sites` defaults
    /// to 2; a calibration section reports how many parameters have
    /// 1 / 2 / 3+ distinct values so the threshold can be set per
    /// repository. The parser is chosen from each file extension
    /// (Rust, TypeScript/JavaScript, Python, or Go). JSON is the
    /// default; `--format md` caps the lists at `--top` (default 20).
    #[command(after_long_help = examples::PARAMETERS)]
    Parameters(AnalyzeParametersArgs),
    /// Report production functions only tests keep alive.
    ///
    /// Builds the same heuristic call graph as `analyze function-graph`
    /// and lists the non-test functions no resolved call path from any
    /// production entry point (`main`, a public/exported declaration, a
    /// live annotation) reaches, while a test does — the code `analyze
    /// unreachable` never reports (tests root its traversal) and
    /// `analyze untested` blesses (tests do reach it). The candidate
    /// edit is moving the function into the language's test scope, or
    /// deleting it together with the tests that exist only to exercise
    /// it. Public/exported declarations whose resolved callers are all
    /// tests are listed as a separate, weaker section — in a library a
    /// consumer outside the analyzed tree cannot be ruled out. Rows
    /// carry caveats where the claim is weaker (crate-restricted
    /// visibility, trait/interface dispatch, ambiguous inbound calls,
    /// the bare name written in a production body), and a raw-name scan
    /// counts unattributed references (imports, attributes, top-level
    /// macros) informationally. Rust and Go only — TypeScript and
    /// Python carry no extracted export status, and the report counts
    /// what it skipped. JSON is the default; `--format md` caps each
    /// section at `--top` (default 20).
    #[command(name = "test-only", after_long_help = examples::TEST_ONLY)]
    TestOnly(AnalyzeTestOnlyArgs),
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
    /// Report production functions with no static call path from any
    /// test function.
    ///
    /// Builds the same heuristic call graph as `analyze function-graph`
    /// and walks forward from every test function over resolved call
    /// edges; the production functions the walk never reaches are the
    /// report, grouped by module and ranked by untested LOC. This is a
    /// structural complement to coverage — no execution, no
    /// instrumentation — and it measures "no resolved call path from a
    /// test function", not "uncovered": integration tests that drive the
    /// built binary reach functions with no in-graph test caller, and
    /// those are listed here anyway. Only resolved edges are traversable,
    /// so the listing is an upper bound; unresolved and ambiguous call
    /// sites leaving test-reached code are counted, and a function an
    /// ambiguous site might reach is flagged on its own row.
    /// `--exclude-tests` removes the traversal's starting points and is
    /// reported as such. The parser is chosen from each file extension
    /// (Rust, TypeScript/JavaScript, Python, or Go); other extensions are
    /// ignored silently. JSON is the default; `--format md` caps the
    /// module listing at `--top` (default 20).
    #[command(after_long_help = examples::UNTESTED)]
    Untested(AnalyzeUntestedArgs),
    /// Report functions no call path from an entry point reaches, in
    /// confidence tiers.
    ///
    /// Builds the same heuristic call graph as `analyze function-graph`,
    /// walks forward from every entry point (`main`, Go `init`, test
    /// functions, `pub` / exported declarations, and anything carrying a
    /// non-inert annotation), and reports what the walk never reaches.
    /// The entry set is emitted with the report, because every verdict
    /// is relative to it. Each candidate then goes through a raw
    /// identifier-reference scan over the scanned sources — a name
    /// written in a macro body, a string, or an expression the parser
    /// did not attribute is a reason to stop trusting the graph — and
    /// lands in one of three tiers: `confirmed` (private/unexported,
    /// unreachable, unreferenced, no caveat: deletable on this evidence
    /// alone), `likely` (nothing in the analyzed path uses it, but the
    /// declaration reaches outside it), `unknown` (a lead, demoted by
    /// trait or interface dispatch, an annotation, an ambiguous call
    /// site, or a raw reference). The direction of soundness is
    /// deliberate: dead code this misses is expected, a `confirmed` row
    /// that is live is a bug. Clusters of unreachable functions that
    /// only call each other are reported as islands with their total LOC
    /// and a deletion order. Export status is extracted for Rust and Go
    /// only; TypeScript and Python functions are treated as entry points
    /// and never judged. `--exclude-tests` removes both the test entry
    /// points and the references test bodies hold, and is reported as
    /// such. JSON is the default and always carries every tier;
    /// `--format md` leads with `confirmed` (`--tier` widens it) and
    /// caps the module listing at `--top` (default 20).
    #[command(after_long_help = examples::UNREACHABLE)]
    Unreachable(AnalyzeUnreachableArgs),
    /// Report `pub` / exported functions no caller outside a narrower
    /// scope uses.
    ///
    /// Builds the same heuristic call graph as `analyze function-graph`
    /// and folds each public function's resolved callers into the
    /// narrowest module containing all of them; when that is narrower
    /// than the declaration, the function is listed with the visibility
    /// its callers would still permit (`drop pub`, `pub(in crate::cli)`,
    /// `pub(in ...)`, `pub(crate)`, or unexporting for Go). Narrowing is
    /// compiler-verified, so a wrong row costs a failed build rather
    /// than lost code. Only resolved edges carry a caller module:
    /// ambiguous and name-matching unresolved call sites from outside
    /// the proposed scope are counted per row as the reason to check it
    /// first. An exported Go method matching a method of an interface
    /// declared in the analyzed tree (same name and parameter count) is
    /// annotated `may satisfy interface ...` and ranked after the
    /// unannotated rows of its bucket: its calls can dispatch through
    /// the interface, so a missing caller is expected rather than
    /// evidence. Callers outside the analyzed path are invisible, so a
    /// single library crate's own API surface looks crate-internal —
    /// the report says so when only one crate is in scope. Export status
    /// is extracted for Rust and Go only; TypeScript and Python
    /// functions are counted as skipped. JSON is the default;
    /// `--format md` caps the module listing at `--top` (default 20).
    #[command(after_long_help = examples::VISIBILITY)]
    Visibility(AnalyzeVisibilityArgs),
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
pub(in crate::cli) struct AnalyzeUnreachableArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: UnreachableOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeUntestedArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: UntestedOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeSingleUseArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: SingleUseOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeParametersArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: ParametersOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeTestRedundancyArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: TestRedundancyOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeSingleImplArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: SingleImplOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeTestOnlyArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: TestOnlyOptions,
}

#[derive(Debug, Clone, Args)]
pub(in crate::cli) struct AnalyzeVisibilityArgs {
    #[command(flatten)]
    pub(in crate::cli) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(in crate::cli) opts: VisibilityOptions,
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
