//! The clap surface: every argument struct and subcommand enum the
//! `agent-lens` CLI parses.

use std::path::PathBuf;

use agent_lens::analyze::cohesion::CohesionOptions;
use agent_lens::analyze::complexity::ComplexityOptions;
use agent_lens::analyze::context_span::ContextSpanOptions;
use agent_lens::analyze::coupling::CouplingOptions;
use agent_lens::analyze::delegation::DelegationOptions;
use agent_lens::analyze::graph_query::GraphQueryOptions;
use agent_lens::analyze::hotspot::HotspotOptions;
use agent_lens::analyze::hubs::HubsOptions;
use agent_lens::analyze::impact::ImpactOptions;
use agent_lens::analyze::layers::LayersOptions;
use agent_lens::analyze::risk::RiskOptions;
use agent_lens::analyze::similarity::SimilarityOptions;
use agent_lens::analyze::unreachable::UnreachableOptions;
use agent_lens::analyze::untested::UntestedOptions;
use agent_lens::analyze::visibility::VisibilityOptions;
use agent_lens::analyze::wrapper::WrapperOptions;
use agent_lens::analyze::{AnalyzeRoots, OutputFormat};
use agent_lens::hooks::setup_engine::SetupScope;
use agent_lens::skills;
use clap::{Args, Parser, Subcommand};

use super::examples;

#[derive(Debug, Parser)]
#[command(
    name = "agent-lens",
    about = "Hook handlers and analyzers that give coding agents a sharper view of the codebase.",
    after_long_help = examples::ROOT,
    version,
    propagate_version = true,
    // We ship our own `help` subcommand (with `--md`), so turn off clap's
    // auto-generated one to avoid a name clash. `--help` flags are
    // untouched and still work everywhere.
    disable_help_subcommand = true
)]
pub(super) struct Cli {
    #[command(subcommand)]
    pub(super) command: Command,
}

#[derive(Debug, Subcommand)]
pub(super) enum Command {
    /// Run a handler for one of Claude Code's hook events.
    #[command(subcommand)]
    Hook(HookCommand),
    /// Run a handler for one of Codex's hook events.
    #[command(subcommand)]
    CodexHook(CodexHookCommand),
    /// Run an on-demand analyzer that emits LLM-friendly context.
    #[command(subcommand, after_long_help = examples::ANALYZE)]
    Analyze(AnalyzeCommand),
    /// Run every analyzer in a named `agent-lens.toml` profile.
    ///
    /// A profile bundles a target path, shared path filters, an ordered
    /// list of analyzers, and optional per-tool overrides. The config is
    /// discovered by walking up from the current directory (or pointed
    /// at with `--config`); each analyzer runs through the same path as
    /// `agent-lens analyze`, and the per-tool reports are emitted as one
    /// combined document.
    #[command(after_long_help = examples::RUN)]
    Run(RunArgs),
    /// Snapshot a profile's analyzers as a compact set of metrics.
    ///
    /// A baseline is what turns an analyzer into a check: comparing a
    /// later run against a stored snapshot separates "this change made
    /// things worse" from "this file was already like that", so a
    /// repository can adopt a threshold without first paying off its
    /// existing debt.
    #[command(subcommand, after_long_help = examples::BASELINE)]
    Baseline(BaselineCommand),
    /// List or install the Claude Code skills bundled with this binary.
    ///
    /// The skills teach a coding agent which analyzer fits a given
    /// question, so installing them into a project's `.claude/skills`
    /// (or `$HOME/.claude/skills`) is how a fresh checkout gets
    /// `agent-lens`-aware routing.
    #[command(subcommand, after_long_help = examples::SKILLS)]
    Skills(SkillsCommand),
    /// Inspect the `agent-lens.toml` configuration format.
    #[command(subcommand, after_long_help = examples::CONFIG)]
    Config(ConfigCommand),
    /// Print the command reference, optionally as agent-friendly Markdown.
    ///
    /// Without flags this prints the same long help clap renders for
    /// `--help`. With `--md` it emits a dense Markdown document covering
    /// every subcommand, its description, and its options in one place —
    /// tuned for dropping into an LLM context.
    #[command(after_long_help = examples::HELP)]
    Help(HelpArgs),
}

#[derive(Debug, Args)]
pub(super) struct HelpArgs {
    /// Emit the full command reference as Markdown tuned for agent context.
    #[arg(long)]
    pub(super) md: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum ConfigCommand {
    /// Print the `agent-lens.toml` schema as agent-friendly Markdown.
    ///
    /// Lists the `[profile.<name>]` keys and every per-tool override
    /// table — their types, defaults, and meaning — plus a worked
    /// example. The format lives only in the config structs, so this is
    /// the canonical reference for writing or auditing an
    /// `agent-lens.toml` without reading the source.
    Schema,
}

#[derive(Debug, Subcommand)]
pub(super) enum SkillsCommand {
    /// List the bundled skills and what each one is for.
    List,
    /// Install the bundled skills into a `.claude/skills` directory.
    ///
    /// Conservative by default: a skill that already exists with
    /// different content is reported as a conflict and left untouched.
    /// Re-running once installed is a no-op; pass `--force` to overwrite
    /// local edits.
    Install(SkillsInstallArgs),
}

#[derive(Debug, Args)]
pub(super) struct SkillsInstallArgs {
    /// Where to install the bundled skills. `project` is the current
    /// directory.
    #[arg(long, value_enum, default_value_t = skills::SkillsScope::Project)]
    pub(super) scope: skills::SkillsScope,
    /// Show what would be written without touching disk.
    #[arg(long)]
    pub(super) dry_run: bool,
    /// Overwrite skills that already exist on disk with different content.
    #[arg(long)]
    pub(super) force: bool,
}

/// How a command names the profile it works on. Shared by every
/// profile-driven command so `run` and `baseline create` cannot drift on
/// what "which profile, from which config" means.
#[derive(Debug, Args)]
pub(super) struct ProfileSelectorArgs {
    /// Name of the `[profile.<name>]` table to run.
    pub(super) profile: String,
    /// Path to an explicit `agent-lens.toml`. Defaults to the nearest
    /// one found by walking up from the current directory.
    #[arg(long, value_name = "PATH")]
    pub(super) config: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(super) struct RunArgs {
    #[command(flatten)]
    pub(super) selector: ProfileSelectorArgs,
    /// Override the profile's `format` for this run. The profile picks
    /// the format its readers usually want; this is for the run that
    /// wants the other one — piping a normally-markdown profile into
    /// `jq`, or reading a JSON profile by eye — without editing the
    /// config.
    #[arg(long, value_enum, value_name = "FORMAT")]
    pub(super) format: Option<OutputFormat>,
}

#[derive(Debug, Subcommand)]
pub(super) enum BaselineCommand {
    /// Snapshot a profile's metrics as a JSON document.
    ///
    /// Every analyzer in the profile runs as JSON — whatever the
    /// profile's `format` says, since that key shapes the report a
    /// human or agent reads while a snapshot is built from structured
    /// fields — and each report is reduced to a handful of named
    /// numbers. Analyzers with no baseline summary yet are listed under
    /// `skipped` instead of being silently dropped.
    ///
    /// The document is deterministic: the same tree at the same commit
    /// snapshots byte-identically, with no wall-clock timestamp to make
    /// a regeneration look like a change.
    Create(BaselineCreateArgs),
    /// Compare a fresh run against a stored snapshot, and fail on a
    /// regression.
    ///
    /// The profile runs exactly as `baseline create` runs it, and each
    /// metric is judged by its own direction: extremes and totals are
    /// worse when they rise, `maintainability_index_min` is worse when
    /// it falls, and the figures that only size the measured surface
    /// (file/function/unit/module counts, `loc_total`, `edge_count`) or
    /// that track git history (`commits_max`, `score_max`) are reported
    /// when they move but never gate — a growing codebase and an extra
    /// commit are not regressions.
    ///
    /// Exits 0 when nothing gated moved the wrong way and 2 when
    /// something did, which is distinct from the 1 a failure to run
    /// exits with. `--update` turns the snapshot into a ratchet:
    /// improvements are written back, regressions keep the stored value,
    /// so the bar only ever tightens.
    Compare(BaselineCompareArgs),
}

#[derive(Debug, Args)]
pub(super) struct BaselineCreateArgs {
    #[command(flatten)]
    pub(super) selector: ProfileSelectorArgs,
    /// Write the snapshot here instead of stdout, creating the
    /// directory if needed.
    #[arg(long, value_name = "PATH")]
    pub(super) out: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(super) struct BaselineCompareArgs {
    #[command(flatten)]
    pub(super) selector: ProfileSelectorArgs,
    /// The stored snapshot to compare this run against — the file
    /// `baseline create --out` wrote. `--update` rewrites this same path.
    #[arg(value_name = "SNAPSHOT")]
    pub(super) snapshot: PathBuf,
    /// Output format. Defaults to JSON, which carries every metric;
    /// `md` leads with what moved.
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    pub(super) format: OutputFormat,
    /// Tighten the snapshot in place: metrics this run improved are
    /// written back, metrics it regressed keep their stored value, and
    /// the exit status is unchanged. The bar only ever moves down.
    #[arg(long)]
    pub(super) update: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum HookCommand {
    /// Handle a `SessionStart` event.
    #[command(subcommand)]
    SessionStart(SessionStartCommand),
    /// Handle a `PreToolUse` event.
    #[command(subcommand)]
    PreToolUse(PreToolUseCommand),
    /// Handle a `PostToolUse` event.
    #[command(subcommand)]
    PostToolUse(PostToolUseCommand),
    /// Wire `agent-lens`'s hook handlers into a Claude Code
    /// `settings.json`.
    ///
    /// The merge is conservative: existing entries are preserved, and a
    /// new block is appended only with the commands that aren't already
    /// wired up. Re-running the command is a no-op once every handler
    /// is installed.
    #[command(after_long_help = examples::HOOK_SETUP)]
    Setup(SetupArgs),
}

#[derive(Debug, Args)]
pub(super) struct SetupArgs {
    /// Where to install the hooks: `project` writes
    /// `<cwd>/.claude/settings.json`, `user` writes
    /// `$HOME/.claude/settings.json`.
    #[arg(long, value_enum, default_value_t = SetupScope::Project)]
    pub(super) scope: SetupScope,
    /// Show the resulting JSON without touching disk.
    #[arg(long)]
    pub(super) dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum SessionStartCommand {
    /// Inject a one-shot summary of the project's hotspots and
    /// coupling thumbnail into the new Claude Code session.
    ///
    /// Runs once per session against `cwd`. Pieces that don't apply
    /// (cwd outside a git working tree, or not anchored at a Rust
    /// crate) are silently omitted; if neither applies, the hook
    /// returns a no-op and Claude Code starts unchanged.
    Summary,
}

#[derive(Debug, Subcommand)]
pub(super) enum PreToolUseCommand {
    /// Report functions whose pre-edit complexity (cyclomatic /
    /// cognitive / nesting) crosses a non-trivial threshold in the
    /// file the agent is about to edit.
    ///
    /// The parser is chosen from the file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently. `Write` against a brand-new path
    /// is a silent no-op (no current state to read).
    Complexity,
    /// Report cohesion units (`impl` blocks, classes, or module units)
    /// whose pre-edit LCOM4 is above 1 in the file the agent is about
    /// to edit.
    ///
    /// The parser is chosen from the file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Cohesion,
}

#[derive(Debug, Subcommand)]
pub(super) enum PostToolUseCommand {
    /// Report clusters of similar functions in the file that was just edited.
    ///
    /// The parser is chosen from the file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Similarity,
    /// Report functions whose body, after stripping a short chain of
    /// trivial adapters, is just a forwarding call to another function.
    ///
    /// The parser is chosen from the file extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Wrapper,
}

#[derive(Debug, Subcommand)]
pub(super) enum CodexHookCommand {
    /// Handle a Codex `SessionStart` event.
    #[command(subcommand)]
    SessionStart(CodexSessionStartCommand),
    /// Handle a Codex `PreToolUse` event.
    #[command(subcommand)]
    PreToolUse(CodexPreToolUseCommand),
    /// Handle a Codex `PostToolUse` event.
    #[command(subcommand)]
    PostToolUse(CodexPostToolUseCommand),
    /// Wire `agent-lens`'s Codex hook handlers into a Codex
    /// `config.toml`.
    ///
    /// The merge is conservative: existing keys and comments are
    /// preserved, and `[[hooks.SessionStart]]`, `[[hooks.PreToolUse]]`,
    /// and `[[hooks.PostToolUse]]` blocks are appended only for handlers
    /// that aren't already wired up. Re-running the
    /// command is a no-op once every handler is installed.
    #[command(after_long_help = examples::CODEX_HOOK_SETUP)]
    Setup(CodexSetupArgs),
}

#[derive(Debug, Args)]
pub(super) struct CodexSetupArgs {
    /// Where to install the hooks: `project` writes
    /// `<repo-root>/.codex/config.toml` — the nearest ancestor holding a
    /// `.git` entry, falling back to the current directory outside a git
    /// tree — and `user` writes `$HOME/.codex/config.toml`.
    #[arg(long, value_enum, default_value_t = SetupScope::User)]
    pub(super) scope: SetupScope,
    /// Show the resulting TOML without touching disk.
    #[arg(long)]
    pub(super) dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum CodexPostToolUseCommand {
    /// Report clusters of similar functions across every file Codex's
    /// `apply_patch` just touched.
    ///
    /// The parser is chosen from each file's extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Similarity,
    /// Report functions whose body, after stripping a short chain of
    /// trivial adapters, is just a forwarding call to another function.
    ///
    /// Runs against every file Codex's `apply_patch` just touched. The
    /// parser is chosen from each file's extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Wrapper,
}

#[derive(Debug, Subcommand)]
pub(super) enum CodexPreToolUseCommand {
    /// Report functions whose pre-patch complexity crosses a
    /// non-trivial threshold across every file Codex's `apply_patch`
    /// is about to update.
    ///
    /// `*** Add File:` entries are skipped (no current state on disk);
    /// only `*** Update File:` paths are inspected.
    /// The parser is chosen from each updated file's extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Complexity,
    /// Report cohesion units (`impl` blocks, classes, or module units)
    /// whose pre-patch LCOM4 is above 1 across every file Codex's
    /// `apply_patch` is about to update.
    ///
    /// `*** Add File:` entries are skipped (no current state on disk);
    /// only `*** Update File:` paths are inspected.
    /// The parser is chosen from each updated file's extension (Rust,
    /// TypeScript/JavaScript, Python, or Go). Files with an unsupported
    /// extension are ignored silently.
    Cohesion,
}

#[derive(Debug, Subcommand)]
pub(super) enum CodexSessionStartCommand {
    /// Inject a one-shot summary of the project's hotspots and
    /// coupling thumbnail into the new Codex session.
    ///
    /// Runs once per session against `cwd`. Pieces that don't apply
    /// (cwd outside a git working tree, or not anchored at a Rust
    /// crate) are silently omitted; if neither applies, the hook
    /// returns a no-op and Codex starts unchanged.
    Summary,
}

#[derive(Debug, Subcommand)]
pub(super) enum AnalyzeCommand {
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
    /// `.cjs`) whose relative imports define the module graph, a
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
    /// its callers would still permit (`drop pub`, `pub(super)`,
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
pub(super) struct AnalyzeCommonArgs {
    /// One or more source files or directories to analyze. Several
    /// paths are walked into a single report, so a finding spanning two
    /// of them (a duplicate, a call edge) is still found — which running
    /// the analyzer once per tree cannot do. Display paths are written
    /// relative to the paths' deepest common ancestor.
    #[arg(required = true, num_args = 1.., value_name = "PATH")]
    pub(super) paths: Vec<PathBuf>,
    /// Output format. Defaults to JSON.
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    pub(super) format: OutputFormat,
    #[command(flatten)]
    pub(super) path_filter: AnalyzePathArgs,
}

impl AnalyzeCommonArgs {
    pub(super) fn into_parts(self) -> (AnalyzeRoots, OutputFormat, AnalyzePathArgs) {
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
pub(super) struct AnalyzeRootArgs {
    /// Path to a source file, Rust crate root, or directory to analyze.
    pub(super) path: PathBuf,
    /// Output format. Defaults to JSON.
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    pub(super) format: OutputFormat,
    #[command(flatten)]
    pub(super) path_filter: AnalyzePathArgs,
}

impl AnalyzeRootArgs {
    pub(super) fn into_parts(self) -> (PathBuf, OutputFormat, AnalyzePathArgs) {
        (self.path, self.format, self.path_filter)
    }
}

// Each analyzer's flag group lives with the analyzer, as the same type
// that deserializes its `[profile.<name>.<tool>]` table (see
// `agent_lens::analyze::options`). These structs only bolt the shared
// path/format arguments onto it, so a profile entry can be handed to
// `AnalyzeCommand` without a field-by-field copy.

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeCohesionArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: CohesionOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeComplexityArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: ComplexityOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeContextSpanArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeRootArgs,
    #[command(flatten)]
    pub(super) opts: ContextSpanOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeCouplingArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeRootArgs,
    #[command(flatten)]
    pub(super) opts: CouplingOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeDelegationArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: DelegationOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeGraphQueryArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: GraphQueryOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeHotspotArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: HotspotOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeHubsArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: HubsOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeImpactArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: ImpactOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeLayersArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: LayersOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeRiskArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: RiskOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeSimilarityArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: SimilarityOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeUnreachableArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: UnreachableOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeUntestedArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: UntestedOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeVisibilityArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: VisibilityOptions,
}

#[derive(Debug, Clone, Args)]
pub(super) struct AnalyzeWrapperArgs {
    #[command(flatten)]
    pub(super) common: AnalyzeCommonArgs,
    #[command(flatten)]
    pub(super) opts: WrapperOptions,
}

#[derive(Debug, Clone, Args, Default)]
pub(super) struct AnalyzePathArgs {
    /// Analyze only files that look like tests (`tests/`, `*_test.*`,
    /// `*.test.*`, `test_*`, etc.). For similarity reports, this also
    /// keeps language-level test functions inside non-test files, such
    /// as Rust `#[cfg(test)]` modules.
    #[arg(long, conflicts_with = "exclude_tests")]
    pub(super) only_tests: bool,
    /// Exclude files that look like tests. For similarity reports, this
    /// also drops language-level test functions such as Rust
    /// `#[cfg(test)]` modules.
    #[arg(long, conflicts_with = "only_tests")]
    pub(super) exclude_tests: bool,
    /// Exclude paths matching this glob. Repeatable. Bare patterns also
    /// match at any depth, so `--exclude generated.rs` matches
    /// `src/generated.rs`. A pattern containing `/` is anchored at the
    /// analyzed path — with several PATHs, at their deepest common
    /// ancestor, the same base display paths use.
    #[arg(long = "exclude", value_name = "GLOB")]
    pub(super) exclude: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_lens::analyze::{
        DEFAULT_SIMILARITY_DRIFT_FLOOR, GraphDirection, GraphQueryKind, PairKey, SimilarityMethod,
        UnreachableTier,
    };
    use clap::CommandFactory;
    use rstest::rstest;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    fn help_for(args: &[&str]) -> String {
        let mut argv = args.to_vec();
        argv.push("--help");
        let err = Cli::try_parse_from(argv).expect_err("help exits before parsing");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        err.to_string()
    }

    #[test]
    fn cohesion_hook_help_describes_all_supported_unit_kinds() {
        let help = help_for(&["agent-lens", "hook", "pre-tool-use", "cohesion"]);
        assert!(help.contains("cohesion units"), "got: {help}");
        assert!(help.contains("classes"), "got: {help}");
        assert!(help.contains("module units"), "got: {help}");
        assert!(help.contains("Python, or Go"), "got: {help}");
    }

    #[test]
    fn similarity_help_does_not_mention_retired_tool_name() {
        let help = help_for(&["agent-lens", "analyze", "similarity"]);
        assert!(!help.contains("similarity-ts"), "got: {help}");
        assert!(help.contains("keeps trivial getters"), "got: {help}");
    }

    #[test]
    fn context_span_help_lists_non_rust_entry_shapes() {
        let help = help_for(&["agent-lens", "analyze", "context-span"]);
        assert!(
            help.contains("TypeScript/JavaScript entry file"),
            "got: {help}"
        );
        assert!(help.contains("Python file/directory"), "got: {help}");
        assert!(help.contains("Go file or module directory"), "got: {help}");
        assert!(help.contains("--entry-glob"), "got: {help}");
    }

    #[rstest]
    #[case::hook_session_start_summary(
        &["agent-lens", "hook", "session-start", "summary"],
        |c: &Command| matches!(c, Command::Hook(HookCommand::SessionStart(SessionStartCommand::Summary))),
    )]
    #[case::hook_pre_tool_use_complexity(
        &["agent-lens", "hook", "pre-tool-use", "complexity"],
        |c: &Command| matches!(c, Command::Hook(HookCommand::PreToolUse(PreToolUseCommand::Complexity))),
    )]
    #[case::hook_pre_tool_use_cohesion(
        &["agent-lens", "hook", "pre-tool-use", "cohesion"],
        |c: &Command| matches!(c, Command::Hook(HookCommand::PreToolUse(PreToolUseCommand::Cohesion))),
    )]
    #[case::hook_post_tool_use_similarity(
        &["agent-lens", "hook", "post-tool-use", "similarity"],
        |c: &Command| matches!(c, Command::Hook(HookCommand::PostToolUse(PostToolUseCommand::Similarity))),
    )]
    #[case::hook_post_tool_use_wrapper(
        &["agent-lens", "hook", "post-tool-use", "wrapper"],
        |c: &Command| matches!(c, Command::Hook(HookCommand::PostToolUse(PostToolUseCommand::Wrapper))),
    )]
    #[case::codex_hook_post_tool_use_similarity(
        &["agent-lens", "codex-hook", "post-tool-use", "similarity"],
        |c: &Command| matches!(
            c,
            Command::CodexHook(CodexHookCommand::PostToolUse(CodexPostToolUseCommand::Similarity)),
        ),
    )]
    #[case::codex_hook_pre_tool_use_complexity(
        &["agent-lens", "codex-hook", "pre-tool-use", "complexity"],
        |c: &Command| matches!(
            c,
            Command::CodexHook(CodexHookCommand::PreToolUse(CodexPreToolUseCommand::Complexity)),
        ),
    )]
    #[case::codex_hook_pre_tool_use_cohesion(
        &["agent-lens", "codex-hook", "pre-tool-use", "cohesion"],
        |c: &Command| matches!(
            c,
            Command::CodexHook(CodexHookCommand::PreToolUse(CodexPreToolUseCommand::Cohesion)),
        ),
    )]
    #[case::codex_hook_session_start_summary(
        &["agent-lens", "codex-hook", "session-start", "summary"],
        |c: &Command| matches!(
            c,
            Command::CodexHook(CodexHookCommand::SessionStart(CodexSessionStartCommand::Summary)),
        ),
    )]
    fn parses_hook_subcommand(#[case] argv: &[&str], #[case] expected: fn(&Command) -> bool) {
        let cli = Cli::try_parse_from(argv).expect("clean parse");
        assert!(
            expected(&cli.command),
            "unexpected command: {:?}",
            cli.command
        );
    }

    #[test]
    fn parses_hook_setup_with_default_scope() {
        let cli = Cli::try_parse_from(["agent-lens", "hook", "setup"]).expect("clean parse");
        let Command::Hook(HookCommand::Setup(args)) = cli.command else {
            panic!("expected hook setup");
        };
        assert_eq!(args.scope, SetupScope::Project);
        assert!(!args.dry_run);
    }

    #[test]
    fn parses_hook_setup_with_user_scope_and_dry_run() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "hook",
            "setup",
            "--scope",
            "user",
            "--dry-run",
        ])
        .expect("clean parse");
        let Command::Hook(HookCommand::Setup(args)) = cli.command else {
            panic!("expected hook setup");
        };
        assert_eq!(args.scope, SetupScope::User);
        assert!(args.dry_run);
    }

    #[test]
    fn parses_codex_hook_setup_defaults_to_user_scope() {
        let cli = Cli::try_parse_from(["agent-lens", "codex-hook", "setup"]).expect("clean parse");
        let Command::CodexHook(CodexHookCommand::Setup(args)) = cli.command else {
            panic!("expected codex-hook setup");
        };
        assert_eq!(args.scope, SetupScope::User);
        assert!(!args.dry_run);
    }

    #[test]
    fn parses_analyze_similarity_with_threshold() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--threshold",
            "0.85",
            "--format",
            "md",
            "--diff-only",
            "--exclude-tests",
            "--exclude",
            "generated/**",
            "--min-lines",
            "8",
            "--top",
            "3",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert_eq!(args.common.paths, [PathBuf::from("src/lib.rs")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert!(args.opts.diff_only);
        assert!(args.common.path_filter.exclude_tests);
        assert_eq!(args.common.path_filter.exclude, ["generated/**"]);
        assert!((args.opts.threshold - 0.85).abs() < f64::EPSILON);
        assert_eq!(args.opts.min_lines, Some(8));
        assert_eq!(args.opts.top, Some(3));
        // `--method` is omitted above, so it defaults to TSED.
        assert_eq!(args.opts.method, SimilarityMethod::Tsed);
        // `--doc-overlap` is omitted above; the markdown rollup is opt-in.
        assert!(!args.opts.doc_overlap);
    }

    #[test]
    fn parses_analyze_similarity_doc_overlap() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--doc-overlap",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert!(args.opts.doc_overlap);
    }

    #[test]
    fn parses_analyze_similarity_method() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--method",
            "token",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert_eq!(args.opts.method, SimilarityMethod::Token);
    }

    #[test]
    fn parses_analyze_similarity_sweep_ladder() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--sweep",
            "0.6,0.75,0.85",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert_eq!(args.opts.sweep, vec![0.6, 0.75, 0.85]);
    }

    #[test]
    fn analyze_similarity_rejects_sweep_with_threshold() {
        let err = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--sweep",
            "0.6,0.85",
            "--threshold",
            "0.7",
        ])
        .expect_err("--sweep and --threshold are mutually exclusive");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    /// `--paired-by` takes either spelling of the tight key — the issue
    /// that asked for the mode named it `name`, the value enum calls it
    /// `qualified` — plus the loose `method` key.
    #[rstest]
    #[case::qualified("qualified", PairKey::Qualified)]
    #[case::name_alias("name", PairKey::Qualified)]
    #[case::method("method", PairKey::Method)]
    fn parses_analyze_similarity_paired_by(#[case] value: &str, #[case] expected: PairKey) {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--paired-by",
            value,
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert_eq!(args.opts.paired_by, Some(expected));
        assert!((args.opts.drift_floor - DEFAULT_SIMILARITY_DRIFT_FLOOR).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_analyze_similarity_drift_floor() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--paired-by",
            "method",
            "--drift-floor",
            "0",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert_eq!(args.opts.drift_floor, 0.0);
    }

    /// Without `--paired-by` there is nothing for a floor to filter, so
    /// a lone `--drift-floor` is a mistake worth reporting rather than
    /// silently ignoring.
    #[test]
    fn analyze_similarity_rejects_drift_floor_without_paired_by() {
        let err = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--drift-floor",
            "0.5",
        ])
        .expect_err("--drift-floor requires --paired-by");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    /// Sweeping annotates clusters; pairing does not cluster at all.
    #[test]
    fn analyze_similarity_rejects_paired_by_with_sweep() {
        let err = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--sweep",
            "0.6,0.85",
            "--paired-by",
            "name",
        ])
        .expect_err("--sweep and --paired-by are mutually exclusive");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parses_analyze_similarity_min_score_alias() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--min-score",
            "0.91",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert!((args.opts.threshold - 0.91).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_analyze_complexity_with_top_and_min_score() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "complexity",
            "src/lib.rs",
            "--top",
            "12",
            "--min-score",
            "8",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Complexity(args)) = cli.command else {
            panic!("expected analyze complexity");
        };
        assert_eq!(args.opts.top, Some(12));
        assert_eq!(args.opts.min_score, Some(8));
    }

    #[test]
    fn parses_analyze_cohesion_with_top_and_min_score() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "cohesion",
            "src/lib.rs",
            "--top",
            "7",
            "--min-score",
            "2",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Cohesion(args)) = cli.command else {
            panic!("expected analyze cohesion");
        };
        assert_eq!(args.opts.top, Some(7));
        assert_eq!(args.opts.min_score, Some(2));
    }

    #[test]
    fn parses_analyze_hotspot_with_since_and_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "hotspot",
            ".",
            "--since",
            "90.days.ago",
            "--top",
            "5",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Hotspot(args)) = cli.command else {
            panic!("expected analyze hotspot");
        };
        assert_eq!(args.opts.since.as_deref(), Some("90.days.ago"));
        assert_eq!(args.opts.top, Some(5));
        assert_eq!(args.common.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_risk_with_since_and_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "risk",
            ".",
            "--since",
            "90.days.ago",
            "--top",
            "5",
            "--exclude-tests",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Risk(args)) = cli.command else {
            panic!("expected analyze risk");
        };
        assert_eq!(args.opts.since.as_deref(), Some("90.days.ago"));
        assert_eq!(args.opts.top, Some(5));
        assert!(args.common.path_filter.exclude_tests);
        assert_eq!(args.common.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_graph_query_with_flags() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "graph-query",
            ".",
            "--query",
            "path",
            "--symbol",
            "handler",
            "--to",
            "db_write",
            "--depth",
            "4",
            "--limit",
            "10",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::GraphQuery(args)) = cli.command else {
            panic!("expected analyze graph-query");
        };
        assert_eq!(args.opts.query, GraphQueryKind::Path);
        assert_eq!(args.opts.symbol, "handler");
        assert_eq!(args.opts.to.as_deref(), Some("db_write"));
        assert_eq!(args.opts.depth, Some(4));
        assert_eq!(args.opts.direction, None);
        assert_eq!(args.opts.limit, Some(10));
        assert_eq!(args.common.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_graph_query_direction() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "graph-query",
            ".",
            "--query",
            "neighborhood",
            "--symbol",
            "resolve",
            "--direction",
            "in",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::GraphQuery(args)) = cli.command else {
            panic!("expected analyze graph-query");
        };
        assert_eq!(args.opts.query, GraphQueryKind::Neighborhood);
        assert_eq!(args.opts.direction, Some(GraphDirection::In));
    }

    #[test]
    fn analyze_graph_query_requires_query_and_symbol() {
        let err = Cli::try_parse_from(["agent-lens", "analyze", "graph-query", "."])
            .expect_err("missing required flags");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parses_analyze_impact_with_flags() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "impact",
            ".",
            "--function",
            "Resolver::resolve",
            "--function",
            "helper",
            "--depth",
            "3",
            "--top",
            "5",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Impact(args)) = cli.command else {
            panic!("expected analyze impact");
        };
        assert_eq!(args.opts.function, ["Resolver::resolve", "helper"]);
        assert_eq!(args.opts.depth, Some(3));
        assert_eq!(args.opts.top, Some(5));
        assert_eq!(args.common.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_impact_without_flags_as_diff_mode() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "impact", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Impact(args)) = cli.command else {
            panic!("expected analyze impact");
        };
        assert!(args.opts.function.is_empty());
        assert_eq!(args.opts.depth, None);
    }

    #[test]
    fn parses_analyze_hubs_with_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "hubs",
            "crates",
            "--top",
            "10",
            "--format",
            "md",
            "--exclude-tests",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Hubs(args)) = cli.command else {
            panic!("expected analyze hubs");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(10));
        assert!(args.common.path_filter.exclude_tests);
    }

    #[test]
    fn parses_analyze_layers_with_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "layers",
            "crates",
            "--top",
            "8",
            "--format",
            "md",
            "--exclude-tests",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Layers(args)) = cli.command else {
            panic!("expected analyze layers");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(8));
        assert!(args.common.path_filter.exclude_tests);
    }

    #[test]
    fn parses_analyze_layers_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "layers", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Layers(args)) = cli.command else {
            panic!("expected analyze layers");
        };
        assert_eq!(args.common.paths, [PathBuf::from(".")]);
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.opts.top, None);
    }

    #[test]
    fn parses_analyze_unreachable_with_tier_and_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "unreachable",
            "crates",
            "--tier",
            "unknown",
            "--top",
            "12",
            "--format",
            "md",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Unreachable(args)) = cli.command else {
            panic!("expected analyze unreachable");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.tier, Some(UnreachableTier::Unknown));
        assert_eq!(args.opts.top, Some(12));
    }

    #[test]
    fn parses_analyze_unreachable_defaults_to_json_and_no_tier() {
        let cli = Cli::try_parse_from(["agent-lens", "analyze", "unreachable", "."])
            .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Unreachable(args)) = cli.command else {
            panic!("expected analyze unreachable");
        };
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.opts.tier, None);
        assert_eq!(args.opts.top, None);
    }

    #[test]
    fn parses_analyze_untested_with_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "untested",
            "crates",
            "--top",
            "30",
            "--format",
            "md",
            "--exclude",
            "benches/**",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Untested(args)) = cli.command else {
            panic!("expected analyze untested");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(30));
        assert_eq!(args.common.path_filter.exclude, ["benches/**"]);
    }

    #[test]
    fn parses_analyze_untested_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "untested", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Untested(args)) = cli.command else {
            panic!("expected analyze untested");
        };
        assert_eq!(args.common.paths, [PathBuf::from(".")]);
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.opts.top, None);
    }

    #[test]
    fn parses_analyze_visibility_with_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "visibility",
            "crates",
            "--top",
            "30",
            "--format",
            "md",
            "--exclude",
            "benches/**",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Visibility(args)) = cli.command else {
            panic!("expected analyze visibility");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(30));
        assert_eq!(args.common.path_filter.exclude, ["benches/**"]);
    }

    #[test]
    fn parses_analyze_visibility_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "visibility", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Visibility(args)) = cli.command else {
            panic!("expected analyze visibility");
        };
        assert_eq!(args.common.paths, [PathBuf::from(".")]);
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.opts.top, None);
    }

    #[test]
    fn parses_analyze_delegation_with_top_and_diff_only() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "delegation",
            "crates",
            "--format",
            "md",
            "--top",
            "30",
            "--diff-only",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Delegation(args)) = cli.command else {
            panic!("expected analyze delegation");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(30));
        assert!(args.opts.diff_only);
    }

    #[test]
    fn parses_analyze_delegation_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "delegation", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Delegation(args)) = cli.command else {
            panic!("expected analyze delegation");
        };
        assert_eq!(args.common.paths, [PathBuf::from(".")]);
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.opts.top, None);
        assert!(!args.opts.diff_only);
    }

    /// The monorepo case the multi-PATH signature exists for: several
    /// trees in one invocation, with the flags still parsed as flags.
    #[test]
    fn parses_several_analyze_paths() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "packages",
            "cli",
            "web/src",
            "--format",
            "md",
            "--exclude-tests",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert_eq!(
            args.common.paths,
            [
                PathBuf::from("packages"),
                PathBuf::from("cli"),
                PathBuf::from("web/src"),
            ],
        );
        assert_eq!(args.common.format, OutputFormat::Md);
        assert!(args.common.path_filter.exclude_tests);
    }

    /// A repeatable option before the paths must not swallow them.
    #[test]
    fn a_repeatable_option_does_not_absorb_the_trailing_paths() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "wrapper",
            "--exclude",
            "generated/**",
            "packages",
            "cli",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Wrapper(args)) = cli.command else {
            panic!("expected analyze wrapper");
        };
        assert_eq!(args.common.path_filter.exclude, ["generated/**"]);
        assert_eq!(
            args.common.paths,
            [PathBuf::from("packages"), PathBuf::from("cli")],
        );
    }

    /// Every analyzer needs somewhere to look: an omitted PATH is a
    /// parse error, not an implicit `.`.
    #[test]
    fn analyze_requires_at_least_one_path() {
        let err = Cli::try_parse_from(["agent-lens", "analyze", "similarity", "--format", "md"])
            .expect_err("PATH is required");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    /// The graph-rooted analyzers grow one module graph from one entry,
    /// so a second path is a mistake worth reporting rather than a
    /// silently ignored argument.
    #[rstest]
    #[case::coupling("coupling")]
    #[case::context_span("context-span")]
    fn graph_rooted_analyzers_take_exactly_one_path(#[case] tool: &str) {
        let err = Cli::try_parse_from(["agent-lens", "analyze", tool, "packages", "cli"])
            .expect_err("a second path is not an entry point");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn parses_analyze_coupling_with_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "coupling",
            "src/lib.rs",
            "--format",
            "md",
            "--top",
            "15",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Coupling(args)) = cli.command else {
            panic!("expected analyze coupling");
        };
        assert_eq!(args.common.path, PathBuf::from("src/lib.rs"));
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(15));
    }

    #[test]
    fn parses_analyze_wrapper_with_top() {
        let cli = Cli::try_parse_from(["agent-lens", "analyze", "wrapper", "src", "--top", "7"])
            .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Wrapper(args)) = cli.command else {
            panic!("expected analyze wrapper");
        };
        assert_eq!(args.opts.top, Some(7));
        assert!(!args.opts.diff_only);
    }

    #[test]
    fn parses_analyze_coupling_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "coupling", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Coupling(args)) = cli.command else {
            panic!("expected analyze coupling");
        };
        assert_eq!(args.common.path, PathBuf::from("."));
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.opts.top, None);
    }

    #[test]
    fn parses_analyze_cycles_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "cycles", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Cycles(args)) = cli.command else {
            panic!("expected analyze cycles");
        };
        assert_eq!(args.paths, [PathBuf::from(".")]);
        assert_eq!(args.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_cycles_with_md_format_and_exclude_tests() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "cycles",
            "src",
            "--format",
            "md",
            "--exclude-tests",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Cycles(args)) = cli.command else {
            panic!("expected analyze cycles");
        };
        assert_eq!(args.paths, [PathBuf::from("src")]);
        assert_eq!(args.format, OutputFormat::Md);
        assert!(args.path_filter.exclude_tests);
    }

    #[test]
    fn parses_analyze_function_graph_default_format_is_json() {
        let cli = Cli::try_parse_from(["agent-lens", "analyze", "function-graph", "."])
            .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::FunctionGraph(args)) = cli.command else {
            panic!("expected analyze function-graph");
        };
        assert_eq!(args.paths, [PathBuf::from(".")]);
        assert_eq!(args.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_function_graph_with_md_format() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "function-graph",
            "src/lib.rs",
            "--format",
            "md",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::FunctionGraph(args)) = cli.command else {
            panic!("expected analyze function-graph");
        };
        assert_eq!(args.paths, [PathBuf::from("src/lib.rs")]);
        assert_eq!(args.format, OutputFormat::Md);
    }

    #[test]
    fn parses_analyze_context_span_with_md_format() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "context-span",
            "src/lib.rs",
            "--format",
            "md",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::ContextSpan(args)) = cli.command else {
            panic!("expected analyze context-span");
        };
        assert_eq!(args.common.path, PathBuf::from("src/lib.rs"));
        assert_eq!(args.common.format, OutputFormat::Md);
        assert!(args.opts.entry_glob.is_empty());
    }

    #[test]
    fn parses_analyze_context_span_with_entry_globs() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "context-span",
            "web",
            "--entry-glob",
            "app/**/page.tsx",
            "--entry-glob",
            "app/**/route.ts",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::ContextSpan(args)) = cli.command else {
            panic!("expected analyze context-span");
        };
        assert_eq!(args.common.path, PathBuf::from("web"));
        assert_eq!(
            args.opts.entry_glob,
            vec!["app/**/page.tsx".to_owned(), "app/**/route.ts".to_owned()]
        );
    }

    #[test]
    fn analyze_command_requires_a_subcommand() {
        let err = Cli::try_parse_from(["agent-lens", "analyze"]).expect_err("missing subcommand");
        // clap reports this as DisplayHelpOnMissingArgumentOrSubcommand
        // because the parent command has no default behaviour without a
        // subcommand.
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
        );
    }

    #[test]
    fn analyze_cohesion_requires_path() {
        let err =
            Cli::try_parse_from(["agent-lens", "analyze", "cohesion"]).expect_err("missing path");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument,);
    }

    #[test]
    fn invalid_format_value_is_rejected() {
        let err = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "cohesion",
            "src/lib.rs",
            "--format",
            "yaml",
        ])
        .expect_err("yaml is not a known format");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn invalid_setup_scope_is_rejected() {
        let err = Cli::try_parse_from(["agent-lens", "hook", "setup", "--scope", "global"])
            .expect_err("global is not a known scope");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        let err = Cli::try_parse_from(["agent-lens", "lint"]).expect_err("no lint subcommand");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn unknown_post_tool_use_handler_is_rejected() {
        let err = Cli::try_parse_from(["agent-lens", "hook", "post-tool-use", "complexity"])
            .expect_err("complexity is not a hook handler");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn version_flag_short_circuits_parsing() {
        let err = Cli::try_parse_from(["agent-lens", "--version"]).expect_err("version exits");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    /// The three `--scope` flags used to parse into CLI-local enums that
    /// were converted to the domain ones; clap now parses the domain
    /// enums directly, so the accepted spellings come from their variant
    /// names. Pin both spellings on all three commands: renaming a
    /// variant is now a CLI-visible change.
    #[rstest]
    #[case::hook_project(&["agent-lens", "hook", "setup", "--scope", "project"])]
    #[case::hook_user(&["agent-lens", "hook", "setup", "--scope", "user"])]
    #[case::codex_project(&["agent-lens", "codex-hook", "setup", "--scope", "project"])]
    #[case::codex_user(&["agent-lens", "codex-hook", "setup", "--scope", "user"])]
    #[case::skills_project(&["agent-lens", "skills", "install", "--scope", "project"])]
    #[case::skills_user(&["agent-lens", "skills", "install", "--scope", "user"])]
    fn scope_flags_accept_project_and_user(#[case] argv: &[&str]) {
        Cli::try_parse_from(argv).expect("clean parse");
    }

    #[test]
    fn scope_flags_reject_an_unknown_value() {
        assert!(
            Cli::try_parse_from(["agent-lens", "hook", "setup", "--scope", "global"]).is_err(),
            "an unknown --scope value must not parse",
        );
    }

    #[test]
    fn parses_run_with_profile_and_config() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "run",
            "web",
            "--config",
            "cfg/agent-lens.toml",
        ])
        .expect("clean parse");
        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.selector.profile, "web");
        assert_eq!(
            args.selector.config,
            Some(PathBuf::from("cfg/agent-lens.toml")),
        );
    }

    #[test]
    fn parses_run_without_config_flag() {
        let cli = Cli::try_parse_from(["agent-lens", "run", "backend"]).expect("clean parse");
        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.selector.profile, "backend");
        assert_eq!(args.selector.config, None);
    }

    #[test]
    fn run_requires_a_profile_name() {
        let err = Cli::try_parse_from(["agent-lens", "run"]).expect_err("missing profile");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parses_baseline_create_with_profile_config_and_out() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "baseline",
            "create",
            "web",
            "--config",
            "cfg/agent-lens.toml",
            "--out",
            "target/baseline.json",
        ])
        .expect("clean parse");
        let Command::Baseline(BaselineCommand::Create(args)) = cli.command else {
            panic!("expected baseline create command");
        };
        assert_eq!(args.selector.profile, "web");
        assert_eq!(
            args.selector.config,
            Some(PathBuf::from("cfg/agent-lens.toml")),
        );
        assert_eq!(args.out, Some(PathBuf::from("target/baseline.json")));
    }

    #[test]
    fn baseline_create_defaults_to_stdout_and_a_discovered_config() {
        let cli =
            Cli::try_parse_from(["agent-lens", "baseline", "create", "web"]).expect("clean parse");
        let Command::Baseline(BaselineCommand::Create(args)) = cli.command else {
            panic!("expected baseline create command");
        };
        assert_eq!(args.selector.config, None);
        assert_eq!(args.out, None);
    }

    #[test]
    fn baseline_create_requires_a_profile_name() {
        let err =
            Cli::try_parse_from(["agent-lens", "baseline", "create"]).expect_err("missing profile");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parses_help_with_and_without_md() {
        let cli = Cli::try_parse_from(["agent-lens", "help", "--md"]).expect("clean parse");
        let Command::Help(args) = cli.command else {
            panic!("expected help");
        };
        assert!(args.md);

        let cli = Cli::try_parse_from(["agent-lens", "help"]).expect("clean parse");
        let Command::Help(args) = cli.command else {
            panic!("expected help");
        };
        assert!(!args.md);
    }

    #[test]
    fn parses_skills_list() {
        let cli = Cli::try_parse_from(["agent-lens", "skills", "list"]).expect("clean parse");
        assert!(matches!(cli.command, Command::Skills(SkillsCommand::List)));
    }

    #[test]
    fn parses_skills_install_with_default_scope() {
        let cli = Cli::try_parse_from(["agent-lens", "skills", "install"]).expect("clean parse");
        let Command::Skills(SkillsCommand::Install(args)) = cli.command else {
            panic!("expected skills install");
        };
        assert!(matches!(args.scope, skills::SkillsScope::Project));
        assert!(!args.dry_run);
        assert!(!args.force);
    }

    #[test]
    fn parses_skills_install_user_scope_with_flags() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "skills",
            "install",
            "--scope",
            "user",
            "--dry-run",
            "--force",
        ])
        .expect("clean parse");
        let Command::Skills(SkillsCommand::Install(args)) = cli.command else {
            panic!("expected skills install");
        };
        assert!(matches!(args.scope, skills::SkillsScope::User));
        assert!(args.dry_run);
        assert!(args.force);
    }

    /// The routing table is hand-written, so it can drift the moment a new
    /// analyzer lands. Pin it to the actual subcommand list in both
    /// directions: every analyzer is routable, and the table never names
    /// one that no longer exists.
    #[test]
    fn routing_table_names_exactly_the_analyze_subcommands() {
        let command = Cli::command();
        let analyze = command
            .get_subcommands()
            .find(|sub| sub.get_name() == "analyze")
            .expect("analyze subcommand");

        // Routing rows are `<question>  analyze <name>`, indented to read
        // as a code block; the surrounding prose has no such row.
        let mut routed: Vec<&str> = examples::ANALYZE
            .lines()
            .filter(|line| line.starts_with("    "))
            .filter_map(|line| line.rsplit_once(" analyze "))
            .map(|(_, name)| name.trim())
            .collect();
        routed.sort_unstable();

        let mut declared: Vec<&str> = analyze
            .get_subcommands()
            .map(|sub| sub.get_name())
            .collect();
        declared.sort_unstable();

        assert_eq!(routed, declared);
    }

    /// Each analyzer's help ends with a worked invocation of *that*
    /// analyzer — a copy-pasted example block would otherwise go unnoticed.
    #[test]
    fn every_analyze_subcommand_has_its_own_example_block() {
        let command = Cli::command();
        let analyze = command
            .get_subcommands()
            .find(|sub| sub.get_name() == "analyze")
            .expect("analyze subcommand");

        for sub in analyze.get_subcommands() {
            let epilogue = sub
                .get_after_long_help()
                .map(ToString::to_string)
                .unwrap_or_default();
            let invocation = format!("    agent-lens analyze {} ", sub.get_name());
            assert!(
                epilogue.contains(&invocation),
                "`analyze {}` help is missing an example of itself: {epilogue}",
                sub.get_name(),
            );
        }
    }

    #[test]
    fn parses_config_schema_subcommand() {
        let cli = Cli::try_parse_from(["agent-lens", "config", "schema"]).expect("clean parse");
        assert!(
            matches!(cli.command, Command::Config(ConfigCommand::Schema)),
            "unexpected command: {:?}",
            cli.command,
        );
    }
}
