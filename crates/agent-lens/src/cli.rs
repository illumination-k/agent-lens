//! `agent-lens` CLI parsing and command dispatch.
//!
//! Each hook handler is a clap subcommand, so `agent-lens hook
//! post-tool-use similarity` and `agent-lens codex-hook pre-tool-use
//! complexity` are parsed statically instead of routed by runtime name
//! strings. Analyzers live under `agent-lens analyze ...` and write their
//! report to stdout. Stdout is otherwise reserved for the hook's JSON
//! response; diagnostics go to stderr via `tracing`.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::io::{self, Read, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use agent_hooks::Hook;
use agent_hooks::claude_code::ClaudeCodeHookInput;
use agent_hooks::codex::CodexHookInput;
use agent_lens::analyze::{
    CohesionAnalyzer, ComplexityAnalyzer, ContextSpanAnalyzer, CouplingAnalyzer, CyclesAnalyzer,
    DEFAULT_SIMILARITY_MIN_LINES, DEFAULT_SIMILARITY_THRESHOLD, FunctionGraphAnalyzer,
    FunctionSelection, GraphDirection, GraphQueryAnalyzer, GraphQueryKind, HotspotAnalyzer,
    HubsAnalyzer, ImpactAnalyzer, LayersAnalyzer, OutputFormat, SimilarityAnalyzer,
    SimilarityMethod, UntestedAnalyzer, VisibilityAnalyzer, WrapperAnalyzer,
};
use agent_lens::config::{self, ConfigError};
use agent_lens::hooks::codex::post_tool_use::{
    SimilarityHook as CodexSimilarityHook, WrapperHook as CodexWrapperHook,
};
use agent_lens::hooks::codex::pre_tool_use::{
    CohesionHook as CodexPreCohesionHook, ComplexityHook as CodexPreComplexityHook,
};
use agent_lens::hooks::codex::session_start::SummaryHook as CodexSessionStartSummaryHook;
use agent_lens::hooks::codex::setup::{self as codex_setup, SetupSummary as CodexSetupSummary};
use agent_lens::hooks::post_tool_use::{SimilarityHook, WrapperHook};
use agent_lens::hooks::pre_tool_use::{CohesionHook, ComplexityHook};
use agent_lens::hooks::session_start::SummaryHook as SessionStartSummaryHook;
use agent_lens::hooks::setup::{self, SettingsScope, SetupSummary};
use agent_lens::{config_schema, help_md, skills};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

mod examples;

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
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
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
struct HelpArgs {
    /// Emit the full command reference as Markdown tuned for agent context.
    #[arg(long)]
    md: bool,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
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
enum SkillsCommand {
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
struct SkillsInstallArgs {
    /// Where to install the skills. `project` writes to
    /// `<cwd>/.claude/skills`; `user` writes to `$HOME/.claude/skills`.
    #[arg(long, value_enum, default_value_t = SkillsScopeArg::Project)]
    scope: SkillsScopeArg,
    /// Show what would be written without touching disk.
    #[arg(long)]
    dry_run: bool,
    /// Overwrite skills that already exist on disk with different content.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SkillsScopeArg {
    Project,
    User,
}

impl From<SkillsScopeArg> for skills::SkillsScope {
    fn from(value: SkillsScopeArg) -> Self {
        match value {
            SkillsScopeArg::Project => Self::Project,
            SkillsScopeArg::User => Self::User,
        }
    }
}

#[derive(Debug, Args)]
struct RunArgs {
    /// Name of the `[profile.<name>]` table to run.
    profile: String,
    /// Path to an explicit `agent-lens.toml`. Defaults to the nearest
    /// one found by walking up from the current directory.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum HookCommand {
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
struct SetupArgs {
    /// Where to install the hooks. `project` writes to
    /// `<cwd>/.claude/settings.json`; `user` writes to
    /// `$HOME/.claude/settings.json`.
    #[arg(long, value_enum, default_value_t = SetupScope::Project)]
    scope: SetupScope,
    /// Show the resulting JSON without touching disk.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum SetupScope {
    Project,
    User,
}

impl From<SetupScope> for SettingsScope {
    fn from(value: SetupScope) -> Self {
        match value {
            SetupScope::Project => Self::Project,
            SetupScope::User => Self::User,
        }
    }
}

#[derive(Debug, Subcommand)]
enum SessionStartCommand {
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
enum PreToolUseCommand {
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
enum PostToolUseCommand {
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
enum CodexHookCommand {
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
struct CodexSetupArgs {
    /// Where to install the hooks. `user` writes to
    /// `$HOME/.codex/config.toml` (Codex's canonical location);
    /// `project` writes to `<repo-root>/.codex/config.toml`, where
    /// `repo-root` comes from `git rev-parse --show-toplevel` and
    /// falls back to the current directory outside a git tree.
    #[arg(long, value_enum, default_value_t = CodexSetupScope::User)]
    scope: CodexSetupScope,
    /// Show the resulting TOML without touching disk.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CodexSetupScope {
    Project,
    User,
}

impl From<CodexSetupScope> for codex_setup::ConfigScope {
    fn from(value: CodexSetupScope) -> Self {
        match value {
            CodexSetupScope::Project => Self::Project,
            CodexSetupScope::User => Self::User,
        }
    }
}

#[derive(Debug, Subcommand)]
enum CodexPostToolUseCommand {
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
enum CodexPreToolUseCommand {
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
enum CodexSessionStartCommand {
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
enum AnalyzeCommand {
    /// Report LCOM4 cohesion units (`impl` blocks, classes, or module
    /// units).
    ///
    /// Accepts either a single source file or a directory; in directory
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
    /// Accepts either a single source file or a directory; in directory
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
    /// module graph.
    #[command(after_long_help = examples::COUPLING)]
    Coupling(AnalyzeCommonArgs),
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
    /// Report clusters of near-duplicate functions.
    ///
    /// Accepts either a single source file or a directory; in directory
    /// mode the analyzer walks recursively (respecting `.gitignore` like
    /// ripgrep) and reports cross-file clusters alongside in-file ones.
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
    /// first. Callers outside the analyzed path are invisible, so a
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
    /// Accepts either a single source file or a directory; in directory
    /// mode the analyzer walks recursively (respecting `.gitignore` like
    /// ripgrep) and groups findings per file. The parser is chosen from
    /// each file extension (Rust, TypeScript/JavaScript, Python, or Go).
    /// The JSON format is the default machine-readable output;
    /// `--format md` emits a compact summary tuned for LLM context.
    #[command(after_long_help = examples::WRAPPER)]
    Wrapper(AnalyzeWrapperArgs),
}

#[derive(Debug, Clone, Args)]
struct AnalyzeCommonArgs {
    /// Path to a source file, Rust crate root, or directory to analyze.
    path: PathBuf,
    /// Output format. Defaults to JSON.
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    format: OutputFormat,
    #[command(flatten)]
    path_filter: AnalyzePathArgs,
}

impl AnalyzeCommonArgs {
    fn into_parts(self) -> (PathBuf, OutputFormat, AnalyzePathArgs) {
        (self.path, self.format, self.path_filter)
    }
}

#[derive(Debug, Clone, Args, Default)]
struct AnalyzeDiffArgs {
    /// Restrict the report to units touching unstaged changed lines in
    /// `git diff -U0`.
    #[arg(long)]
    diff_only: bool,
}

#[derive(Debug, Clone, Args, Default)]
struct AnalyzeRankingArgs {
    /// Cap the markdown ranking to the top-N entries. JSON output
    /// always carries the full list.
    #[arg(long)]
    top: Option<usize>,
}

#[derive(Debug, Clone, Args)]
struct AnalyzeCohesionArgs {
    #[command(flatten)]
    common: AnalyzeCommonArgs,
    #[command(flatten)]
    diff: AnalyzeDiffArgs,
    #[command(flatten)]
    ranking: AnalyzeRankingArgs,
    /// Minimum LCOM4 score included in the markdown ranking. The
    /// markdown default is 2, which hides cohesive LCOM4=1 units;
    /// pass `--min-score 1` to include them.
    #[arg(long)]
    min_score: Option<usize>,
}

#[derive(Debug, Clone, Args)]
struct AnalyzeComplexityArgs {
    #[command(flatten)]
    common: AnalyzeCommonArgs,
    #[command(flatten)]
    diff: AnalyzeDiffArgs,
    #[command(flatten)]
    ranking: AnalyzeRankingArgs,
    /// Minimum cognitive complexity score included in the markdown
    /// ranking. JSON output always carries the full list.
    #[arg(long)]
    min_score: Option<u32>,
}

#[derive(Debug, Clone, Args)]
struct AnalyzeHubsArgs {
    #[command(flatten)]
    common: AnalyzeCommonArgs,
    #[command(flatten)]
    ranking: AnalyzeRankingArgs,
}

#[derive(Debug, Clone, Args)]
struct AnalyzeGraphQueryArgs {
    #[command(flatten)]
    common: AnalyzeCommonArgs,
    /// Traversal verb to run.
    #[arg(long, value_enum)]
    query: GraphQueryKind,
    /// Function to start from: a `::`-segment suffix of its qualified
    /// name (e.g. `foo`, `module::foo`, `Owner::method`) or an exact
    /// node id (`file:name:line`, as listed on ambiguity).
    #[arg(long)]
    symbol: String,
    /// Destination symbol for `--query path` (same matching rules as
    /// `--symbol`).
    #[arg(long)]
    to: Option<String>,
    /// Traversal depth cap in call hops. Defaults to 1 for
    /// callers/callees/neighborhood; for `path` it caps the search
    /// (default unbounded).
    #[arg(long)]
    depth: Option<usize>,
    /// Traversal direction for `--query neighborhood` (default both).
    #[arg(long, value_enum)]
    direction: Option<GraphDirection>,
    /// Cap the result set by node count (default 50).
    #[arg(long)]
    limit: Option<usize>,
}

#[derive(Debug, Clone, Args)]
struct AnalyzeImpactArgs {
    #[command(flatten)]
    common: AnalyzeCommonArgs,
    #[command(flatten)]
    ranking: AnalyzeRankingArgs,
    /// Seed the query from this function instead of the working-tree
    /// diff: a `::`-segment suffix of its qualified name (e.g. `foo`,
    /// `module::foo`, `Owner::method`) or an exact node id
    /// (`file:name:line`, as listed on ambiguity). Repeatable.
    #[arg(long = "function", value_name = "SYMBOL")]
    function: Vec<String>,
    /// Reverse-traversal depth cap in call hops (cycles count as one).
    /// Callers beyond the cap are counted, not listed.
    #[arg(long)]
    depth: Option<usize>,
}

#[derive(Debug, Clone, Args)]
struct AnalyzeLayersArgs {
    #[command(flatten)]
    common: AnalyzeCommonArgs,
    #[command(flatten)]
    ranking: AnalyzeRankingArgs,
}

#[derive(Debug, Clone, Args)]
struct AnalyzeUntestedArgs {
    #[command(flatten)]
    common: AnalyzeCommonArgs,
    #[command(flatten)]
    ranking: AnalyzeRankingArgs,
}

#[derive(Debug, Clone, Args)]
struct AnalyzeVisibilityArgs {
    #[command(flatten)]
    common: AnalyzeCommonArgs,
    #[command(flatten)]
    ranking: AnalyzeRankingArgs,
}

#[derive(Debug, Clone, Args)]
struct AnalyzeHotspotArgs {
    #[command(flatten)]
    common: AnalyzeCommonArgs,
    #[command(flatten)]
    ranking: AnalyzeRankingArgs,
    /// Restrict churn to commits in this `--since=` window. Accepts
    /// anything git's approxidate parser does (e.g. `90.days.ago`,
    /// `2024-01-01`).
    #[arg(long)]
    since: Option<String>,
}

#[derive(Debug, Clone, Args)]
struct AnalyzeSimilarityArgs {
    #[command(flatten)]
    common: AnalyzeCommonArgs,
    #[command(flatten)]
    diff: AnalyzeDiffArgs,
    #[command(flatten)]
    ranking: AnalyzeRankingArgs,
    /// Similarity threshold in [0.0, 1.0]. Pairs scoring at or above
    /// this value are eligible for clustering, and the same threshold
    /// is the complete-link cut so every pair inside a reported cluster
    /// stays at or above it. Defaults to the same cutoff used by the
    /// PostToolUse `similarity` hook.
    #[arg(long, visible_alias = "min-score", default_value_t = DEFAULT_SIMILARITY_THRESHOLD)]
    threshold: f64,
    /// Multi-threshold sweep: a comma-separated ascending ladder of
    /// thresholds, e.g. `--sweep 0.6,0.75,0.85`. Pairs are scored and
    /// clustered once at the lowest rung, and every reported cluster is
    /// annotated with the highest rung at which its complete-link structure
    /// survives intact — a coarse dendrogram in one run that separates
    /// verbatim clones from merely structural parallels. Supersedes
    /// `--threshold`, which it conflicts with.
    #[arg(long, value_delimiter = ',', conflicts_with = "threshold")]
    sweep: Vec<f64>,
    /// Minimum source line count for a function to be considered.
    /// Functions shorter than this are dropped before pairwise
    /// comparison; keeps trivial getters / one-liners out of the
    /// report.
    #[arg(long, default_value_t = DEFAULT_SIMILARITY_MIN_LINES)]
    min_lines: usize,
    /// Body-scoring algorithm. `tsed` (default) uses APTED tree-edit
    /// distance over the body AST. `token` compares preorder token
    /// k-gram multisets — faster and more tolerant of reordered code,
    /// but less precise. Scores from the two methods are not directly
    /// comparable.
    #[arg(long, value_enum, default_value_t = SimilarityMethod::Tsed)]
    method: SimilarityMethod,
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
    doc_overlap: bool,
}

#[derive(Debug, Clone, Args)]
struct AnalyzeWrapperArgs {
    #[command(flatten)]
    common: AnalyzeCommonArgs,
    #[command(flatten)]
    diff: AnalyzeDiffArgs,
}

#[derive(Debug, Clone, Args)]
struct AnalyzeContextSpanArgs {
    #[command(flatten)]
    common: AnalyzeCommonArgs,
    /// Treat `path` as a project root and merge the TS/JS module trees
    /// rooted at every file matching this gitignore-aware glob.
    /// Repeatable: pass `--entry-glob 'app/**/page.tsx' --entry-glob
    /// 'app/**/route.ts'` to cover Next.js App Router entries in one
    /// invocation. Patterns are evaluated relative to `path`.
    #[arg(long = "entry-glob", value_name = "GLOB")]
    entry_glob: Vec<String>,
}

#[derive(Debug, Clone, Args, Default)]
struct AnalyzePathArgs {
    /// Analyze only files that look like tests (`tests/`, `*_test.*`,
    /// `*.test.*`, `test_*`, etc.). For similarity reports, this also
    /// keeps language-level test functions inside non-test files, such
    /// as Rust `#[cfg(test)]` modules.
    #[arg(long, conflicts_with = "exclude_tests")]
    only_tests: bool,
    /// Exclude files that look like tests. For similarity reports, this
    /// also drops language-level test functions such as Rust
    /// `#[cfg(test)]` modules.
    #[arg(long, conflicts_with = "only_tests")]
    exclude_tests: bool,
    /// Exclude paths matching this glob. Repeatable. Bare patterns also
    /// match at any depth, so `--exclude generated.rs` matches
    /// `src/generated.rs`.
    #[arg(long = "exclude", value_name = "GLOB")]
    exclude: Vec<String>,
}

pub fn main() -> ExitCode {
    init_tracing();
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!(error = %err, "agent-lens failed");
            ExitCode::from(1)
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    // Ignore the init result — a second call would only happen in tests
    // and would silently re-use the first subscriber.
    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(filter)
        .try_init();
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Hook(HookCommand::SessionStart(sub)) => run_session_start(sub),
        Command::Hook(HookCommand::PreToolUse(sub)) => run_pre_tool_use(sub),
        Command::Hook(HookCommand::PostToolUse(sub)) => run_post_tool_use(sub),
        Command::Hook(HookCommand::Setup(args)) => run_hook_setup(args),
        Command::CodexHook(CodexHookCommand::SessionStart(sub)) => run_codex_session_start(sub),
        Command::CodexHook(CodexHookCommand::PreToolUse(sub)) => run_codex_pre_tool_use(sub),
        Command::CodexHook(CodexHookCommand::PostToolUse(sub)) => run_codex_post_tool_use(sub),
        Command::CodexHook(CodexHookCommand::Setup(args)) => run_codex_hook_setup(args),
        Command::Analyze(sub) => run_analyze(sub),
        Command::Run(args) => run_profile(args),
        Command::Skills(sub) => run_skills(sub),
        Command::Config(sub) => run_config(sub),
        Command::Help(args) => run_help(args),
    }
}

/// Emit the `agent-lens.toml` schema reference on stdout.
fn run_config(cmd: ConfigCommand) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        ConfigCommand::Schema => write_stdout_line(&config_schema::render()),
    }
}

/// Print the command reference. `--md` renders the agent-friendly
/// Markdown document; otherwise we defer to clap's own long help so
/// `agent-lens help` matches `agent-lens --help`.
fn run_help(args: HelpArgs) -> Result<(), Box<dyn std::error::Error>> {
    let mut command = Cli::command();
    if args.md {
        let report = help_md::render(&command);
        write_stdout_line(&report)
    } else {
        write_stdout_line(&command.render_long_help().to_string())
    }
}

fn run_skills(cmd: SkillsCommand) -> Result<(), Box<dyn std::error::Error>> {
    match cmd {
        SkillsCommand::List => write_stdout_line(&skills::render_list()),
        SkillsCommand::Install(args) => run_skills_install(args),
    }
}

/// Diff the bundled skills against the chosen scope and install the
/// missing (or, with `--force`, the changed) ones. Conflicts are logged
/// and reflected in the JSON summary so the agent can decide whether to
/// re-run with `--force`.
fn run_skills_install(args: SkillsInstallArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let root = skills::resolve_root(args.scope.into(), &cwd)?;
    let plan = skills::plan(root, args.force)?;

    for conflict in plan.conflicts() {
        warn!(
            skill = conflict.name,
            path = %conflict.path.display(),
            "skill already exists with different content; re-run with --force to overwrite",
        );
    }

    let wrote = if args.dry_run {
        info!(root = %plan.root.display(), "dry-run: leaving skills untouched");
        false
    } else if plan.changed() {
        skills::apply(&plan)?;
        info!(root = %plan.root.display(), "installed skills");
        true
    } else {
        info!(root = %plan.root.display(), "skills already installed; nothing to do");
        false
    };

    write_stdout_json(&plan.summary(wrote))
}

fn run_analyze(cmd: AnalyzeCommand) -> Result<(), Box<dyn std::error::Error>> {
    write_stdout_line(&cmd.run()?)
}

/// Run every analyzer in a named `agent-lens.toml` profile and emit one
/// combined report.
///
/// Each analyzer is driven through the same [`AnalyzeCommand`] the
/// `analyze` subcommand builds, so a profile run and the equivalent
/// hand-typed commands produce identical per-tool output.
fn run_profile(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let config_path = match args.config {
        Some(path) => path,
        None => {
            let cwd = std::env::current_dir()?;
            config::discover(&cwd).ok_or(ConfigError::NotFound { start: cwd })?
        }
    };
    let config = config::load(&config_path)?;
    let profile = config.profile(&args.profile)?;
    let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
    let target = profile.resolved_path(config_dir);
    // Checked once here rather than per analyzer: every tool in the
    // profile would otherwise fail the same way, and only this layer
    // knows the path came from a config and what it was resolved
    // against.
    if !target.exists() {
        return Err(ConfigError::ProfilePathNotFound {
            name: args.profile,
            path: profile.path.clone(),
            resolved: target,
        }
        .into());
    }
    let format = profile.format.unwrap_or(OutputFormat::Json);

    for tool in unused_tool_option_tables(profile) {
        warn!(
            profile = %args.profile,
            tool = tool.as_str(),
            "options table set for a tool not listed in `tools`; ignored",
        );
    }

    let mut seen = std::collections::HashSet::new();
    let mut sections: Vec<(config::ToolName, String)> = Vec::new();
    for &tool in &profile.tools {
        if !seen.insert(tool) {
            info!(
                profile = %args.profile,
                tool = tool.as_str(),
                "tool listed more than once in profile; running it only once",
            );
            continue;
        }
        let report = build_analyze_command(tool, profile, &target, format)?.run()?;
        sections.push((tool, report));
    }

    write_stdout_line(&render_profile_report(&args.profile, format, &sections)?)
}

/// Tool-option tables (`[profile.<name>.<tool>]`) set for a tool the
/// profile's `tools` list never runs — their options would otherwise be
/// silently ignored, so `run` warns about each one.
fn unused_tool_option_tables(profile: &config::Profile) -> Vec<config::ToolName> {
    [
        (profile.similarity.is_some(), config::ToolName::Similarity),
        (profile.complexity.is_some(), config::ToolName::Complexity),
        (profile.cohesion.is_some(), config::ToolName::Cohesion),
        (profile.hotspot.is_some(), config::ToolName::Hotspot),
        (profile.hubs.is_some(), config::ToolName::Hubs),
        (profile.impact.is_some(), config::ToolName::Impact),
        (profile.layers.is_some(), config::ToolName::Layers),
        (profile.graph_query.is_some(), config::ToolName::GraphQuery),
        (
            profile.context_span.is_some(),
            config::ToolName::ContextSpan,
        ),
        (profile.untested.is_some(), config::ToolName::Untested),
        (profile.visibility.is_some(), config::ToolName::Visibility),
        (profile.wrapper.is_some(), config::ToolName::Wrapper),
    ]
    .into_iter()
    .filter(|&(present, tool)| present && !profile.tools.contains(&tool))
    .map(|(_, tool)| tool)
    .collect()
}

/// Translate one profile tool entry into the [`AnalyzeCommand`] the
/// `analyze` subcommand would build for the same options. Per-tool tables
/// that are absent fall back to the analyzer's CLI defaults; `graph-query`
/// has no defaults to fall back to (its `query` and `symbol` are
/// required), so a missing table is an error — [`config::load`] already
/// rejects such profiles, this is the seam-level guard.
fn build_analyze_command(
    tool: config::ToolName,
    profile: &config::Profile,
    target: &Path,
    format: OutputFormat,
) -> Result<AnalyzeCommand, ConfigError> {
    let common = AnalyzeCommonArgs {
        path: target.to_path_buf(),
        format,
        path_filter: AnalyzePathArgs {
            only_tests: profile.only_tests,
            exclude_tests: profile.exclude_tests,
            exclude: profile.exclude.clone(),
        },
    };
    Ok(match tool {
        config::ToolName::Cohesion => {
            let opts = profile.cohesion.clone().unwrap_or_default();
            AnalyzeCommand::Cohesion(AnalyzeCohesionArgs {
                common,
                diff: AnalyzeDiffArgs {
                    diff_only: opts.diff_only,
                },
                ranking: AnalyzeRankingArgs { top: opts.top },
                min_score: opts.min_score,
            })
        }
        config::ToolName::Complexity => {
            let opts = profile.complexity.clone().unwrap_or_default();
            AnalyzeCommand::Complexity(AnalyzeComplexityArgs {
                common,
                diff: AnalyzeDiffArgs {
                    diff_only: opts.diff_only,
                },
                ranking: AnalyzeRankingArgs { top: opts.top },
                min_score: opts.min_score,
            })
        }
        config::ToolName::Coupling => AnalyzeCommand::Coupling(common),
        config::ToolName::Cycles => AnalyzeCommand::Cycles(common),
        config::ToolName::FunctionGraph => AnalyzeCommand::FunctionGraph(common),
        config::ToolName::GraphQuery => {
            let opts = profile
                .graph_query
                .clone()
                .ok_or(ConfigError::MissingToolOptions {
                    tool: config::ToolName::GraphQuery.as_str(),
                })?;
            AnalyzeCommand::GraphQuery(AnalyzeGraphQueryArgs {
                common,
                query: opts.query,
                symbol: opts.symbol,
                to: opts.to,
                depth: opts.depth,
                direction: opts.direction,
                limit: opts.limit,
            })
        }
        config::ToolName::ContextSpan => {
            let opts = profile.context_span.clone().unwrap_or_default();
            AnalyzeCommand::ContextSpan(AnalyzeContextSpanArgs {
                common,
                entry_glob: opts.entry_glob,
            })
        }
        config::ToolName::Hotspot => {
            let opts = profile.hotspot.clone().unwrap_or_default();
            AnalyzeCommand::Hotspot(AnalyzeHotspotArgs {
                common,
                ranking: AnalyzeRankingArgs { top: opts.top },
                since: opts.since,
            })
        }
        config::ToolName::Hubs => {
            let opts = profile.hubs.clone().unwrap_or_default();
            AnalyzeCommand::Hubs(AnalyzeHubsArgs {
                common,
                ranking: AnalyzeRankingArgs { top: opts.top },
            })
        }
        config::ToolName::Impact => {
            let opts = profile.impact.clone().unwrap_or_default();
            AnalyzeCommand::Impact(AnalyzeImpactArgs {
                common,
                ranking: AnalyzeRankingArgs { top: opts.top },
                function: opts.function,
                depth: opts.depth,
            })
        }
        config::ToolName::Layers => {
            let opts = profile.layers.clone().unwrap_or_default();
            AnalyzeCommand::Layers(AnalyzeLayersArgs {
                common,
                ranking: AnalyzeRankingArgs { top: opts.top },
            })
        }
        config::ToolName::Similarity => {
            let opts = profile.similarity.clone().unwrap_or_default();
            AnalyzeCommand::Similarity(AnalyzeSimilarityArgs {
                common,
                diff: AnalyzeDiffArgs {
                    diff_only: opts.diff_only,
                },
                ranking: AnalyzeRankingArgs { top: opts.top },
                threshold: opts.threshold.unwrap_or(DEFAULT_SIMILARITY_THRESHOLD),
                sweep: opts.sweep.unwrap_or_default(),
                min_lines: opts.min_lines.unwrap_or(DEFAULT_SIMILARITY_MIN_LINES),
                method: opts.method.unwrap_or_default(),
                doc_overlap: opts.doc_overlap,
            })
        }
        config::ToolName::Untested => {
            let opts = profile.untested.clone().unwrap_or_default();
            AnalyzeCommand::Untested(AnalyzeUntestedArgs {
                common,
                ranking: AnalyzeRankingArgs { top: opts.top },
            })
        }
        config::ToolName::Visibility => {
            let opts = profile.visibility.clone().unwrap_or_default();
            AnalyzeCommand::Visibility(AnalyzeVisibilityArgs {
                common,
                ranking: AnalyzeRankingArgs { top: opts.top },
            })
        }
        config::ToolName::Wrapper => {
            let opts = profile.wrapper.clone().unwrap_or_default();
            AnalyzeCommand::Wrapper(AnalyzeWrapperArgs {
                common,
                diff: AnalyzeDiffArgs {
                    diff_only: opts.diff_only,
                },
            })
        }
    })
}

/// Render the per-tool reports as one document: stacked `## <tool>`
/// sections for markdown, or a `{profile, results}` object for JSON where
/// each analyzer's JSON output is nested under its tool name.
fn render_profile_report(
    profile: &str,
    format: OutputFormat,
    sections: &[(config::ToolName, String)],
) -> Result<String, Box<dyn std::error::Error>> {
    Ok(match format {
        OutputFormat::Md => {
            let mut out = String::new();
            for (tool, report) in sections {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("## ");
                out.push_str(tool.as_str());
                out.push_str("\n\n");
                out.push_str(report.trim_end_matches('\n'));
                out.push('\n');
            }
            out
        }
        OutputFormat::Json => {
            let mut results = Vec::with_capacity(sections.len());
            for (tool, report) in sections {
                let report: serde_json::Value = serde_json::from_str(report)?;
                results.push(serde_json::json!({ "tool": tool.as_str(), "report": report }));
            }
            serde_json::to_string(&serde_json::json!({
                "profile": profile,
                "results": results,
            }))?
        }
    })
}

trait WithAnalyzePathArgs: Sized {
    fn with_analyze_path_args(self, args: AnalyzePathArgs) -> Self;
}

macro_rules! impl_with_analyze_path_args {
    ($($analyzer:ty),+ $(,)?) => {
        $(
            impl WithAnalyzePathArgs for $analyzer {
                fn with_analyze_path_args(self, args: AnalyzePathArgs) -> Self {
                    self.with_only_tests(args.only_tests)
                        .with_exclude_tests(args.exclude_tests)
                        .with_exclude_patterns(args.exclude)
                }
            }
        )+
    };
}

impl_with_analyze_path_args!(
    CohesionAnalyzer,
    ComplexityAnalyzer,
    CouplingAnalyzer,
    CyclesAnalyzer,
    FunctionGraphAnalyzer,
    GraphQueryAnalyzer,
    ContextSpanAnalyzer,
    HotspotAnalyzer,
    HubsAnalyzer,
    ImpactAnalyzer,
    LayersAnalyzer,
    UntestedAnalyzer,
    VisibilityAnalyzer,
    WrapperAnalyzer,
);

// Similarity needs the same `(only_tests, exclude_tests)` args at two
// granularities: the path-level filter (skip whole files) plus a
// function-level [`FunctionSelection`] (drop `#[test]` fns inside
// non-test files). Wire both from the same args here so the analyzer
// itself never has to read the bools back out of the path filter.
impl WithAnalyzePathArgs for SimilarityAnalyzer {
    fn with_analyze_path_args(self, args: AnalyzePathArgs) -> Self {
        let selection = FunctionSelection::from_args(args.only_tests, args.exclude_tests);
        self.with_only_tests(args.only_tests)
            .with_exclude_tests(args.exclude_tests)
            .with_exclude_patterns(args.exclude)
            .with_function_selection(selection)
    }
}

impl AnalyzeCommand {
    /// Pick the right analyzer for this CLI variant and produce its
    /// report. Shared CLI concepts are flattened into the command args
    /// structs above; each arm only applies analyzer-specific options.
    fn run(self) -> Result<String, Box<dyn std::error::Error>> {
        Ok(match self {
            Self::Cohesion(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                CohesionAnalyzer::new()
                    .with_diff_only(args.diff.diff_only)
                    .with_top(args.ranking.top)
                    .with_min_score(args.min_score)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Complexity(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                ComplexityAnalyzer::new()
                    .with_diff_only(args.diff.diff_only)
                    .with_top(args.ranking.top)
                    .with_min_score(args.min_score)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Coupling(args) => {
                let (path, format, path_filter) = args.into_parts();
                CouplingAnalyzer::new()
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Cycles(args) => {
                let (path, format, path_filter) = args.into_parts();
                CyclesAnalyzer::new()
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::FunctionGraph(args) => {
                let (path, format, path_filter) = args.into_parts();
                FunctionGraphAnalyzer::new()
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::GraphQuery(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                GraphQueryAnalyzer::new(args.query, args.symbol)
                    .with_to(args.to)
                    .with_depth(args.depth)
                    .with_direction(args.direction)
                    .with_limit(args.limit)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::ContextSpan(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                ContextSpanAnalyzer::new()
                    .with_analyze_path_args(path_filter)
                    .with_entry_globs(args.entry_glob)
                    .analyze(&path, format)?
            }
            Self::Hubs(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                HubsAnalyzer::new()
                    .with_top(args.ranking.top)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Impact(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                ImpactAnalyzer::new()
                    .with_functions(args.function)
                    .with_depth(args.depth)
                    .with_top(args.ranking.top)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Layers(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                LayersAnalyzer::new()
                    .with_top(args.ranking.top)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Hotspot(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                HotspotAnalyzer::new()
                    .with_top(args.ranking.top)
                    .with_since_opt(args.since)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Similarity(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                let sweep = (!args.sweep.is_empty()).then_some(args.sweep);
                SimilarityAnalyzer::new()
                    .with_threshold(args.threshold)
                    .with_sweep(sweep)
                    .with_diff_only(args.diff.diff_only)
                    .with_min_lines(args.min_lines)
                    .with_method(args.method)
                    .with_doc_overlap(args.doc_overlap)
                    .with_top(args.ranking.top)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Untested(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                UntestedAnalyzer::new()
                    .with_top(args.ranking.top)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Visibility(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                VisibilityAnalyzer::new()
                    .with_top(args.ranking.top)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Wrapper(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                WrapperAnalyzer::new()
                    .with_diff_only(args.diff.diff_only)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
        })
    }
}

fn write_stdout_line(report: &str) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    stdout.write_all(report.as_bytes())?;
    if !report.ends_with('\n') {
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

fn run_session_start(cmd: SessionStartCommand) -> Result<(), Box<dyn std::error::Error>> {
    let ClaudeCodeHookInput::SessionStart(input) = read_stdin_json::<ClaudeCodeHookInput>()? else {
        return Err("expected a SessionStart hook payload on stdin".into());
    };
    let output = match cmd {
        SessionStartCommand::Summary => SessionStartSummaryHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

fn run_pre_tool_use(cmd: PreToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    let ClaudeCodeHookInput::PreToolUse(input) = read_stdin_json::<ClaudeCodeHookInput>()? else {
        return Err("expected a PreToolUse hook payload on stdin".into());
    };
    let output = match cmd {
        PreToolUseCommand::Complexity => ComplexityHook::new().handle(input)?,
        PreToolUseCommand::Cohesion => CohesionHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

fn run_post_tool_use(cmd: PostToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    let ClaudeCodeHookInput::PostToolUse(input) = read_stdin_json::<ClaudeCodeHookInput>()? else {
        return Err("expected a PostToolUse hook payload on stdin".into());
    };
    let output = match cmd {
        PostToolUseCommand::Similarity => SimilarityHook::new().handle(input)?,
        PostToolUseCommand::Wrapper => WrapperHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

fn run_hook_setup(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let path = setup::resolve_path(args.scope.into(), &cwd)?;
    let plan = setup::plan(path)?;
    let wrote = apply_setup_plan(
        args.dry_run,
        plan.changed(),
        SetupApplyContext {
            path: &plan.path,
            added_commands: plan.added_commands.len(),
            dry_run_message: "dry-run: leaving settings.json untouched",
            wrote_message: "wrote settings.json",
            unchanged_message: "settings.json already configured; nothing to do",
        },
        || setup::apply(&plan).map_err(Into::into),
    )?;
    write_stdout_json(&SetupSummary {
        path: &plan.path,
        wrote,
        added_commands: &plan.added_commands,
        settings: &plan.after,
    })
}

fn run_codex_hook_setup(args: CodexSetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = std::env::current_dir()?;
    let project_root = git_top_level(&cwd).unwrap_or(cwd);
    let path = codex_setup::resolve_path(args.scope.into(), &project_root)?;
    let plan = codex_setup::plan(path)?;
    let wrote = apply_setup_plan(
        args.dry_run,
        plan.changed(),
        SetupApplyContext {
            path: &plan.path,
            added_commands: plan.added_commands.len(),
            dry_run_message: "dry-run: leaving config.toml untouched",
            wrote_message: "wrote config.toml",
            unchanged_message: "config.toml already configured; nothing to do",
        },
        || codex_setup::apply(&plan).map_err(Into::into),
    )?;
    write_stdout_json(&CodexSetupSummary {
        path: &plan.path,
        wrote,
        added_commands: &plan.added_commands,
        config: &plan.after,
    })
}

fn apply_setup_plan(
    dry_run: bool,
    changed: bool,
    context: SetupApplyContext<'_>,
    apply: impl FnOnce() -> Result<(), Box<dyn std::error::Error>>,
) -> Result<bool, Box<dyn std::error::Error>> {
    if dry_run {
        info!(path = %context.path.display(), "{}", context.dry_run_message);
        return Ok(false);
    }

    if !changed {
        info!(path = %context.path.display(), "{}", context.unchanged_message);
        return Ok(false);
    }

    apply()?;
    info!(
        path = %context.path.display(),
        added = context.added_commands,
        "{}",
        context.wrote_message,
    );
    Ok(true)
}

struct SetupApplyContext<'a> {
    path: &'a Path,
    added_commands: usize,
    dry_run_message: &'static str,
    wrote_message: &'static str,
    unchanged_message: &'static str,
}

fn run_codex_pre_tool_use(cmd: CodexPreToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    let CodexHookInput::PreToolUse(input) = read_stdin_json::<CodexHookInput>()? else {
        return Err("expected a Codex PreToolUse hook payload on stdin".into());
    };
    let output = match cmd {
        CodexPreToolUseCommand::Complexity => CodexPreComplexityHook::new().handle(input)?,
        CodexPreToolUseCommand::Cohesion => CodexPreCohesionHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

fn run_codex_post_tool_use(cmd: CodexPostToolUseCommand) -> Result<(), Box<dyn std::error::Error>> {
    let CodexHookInput::PostToolUse(input) = read_stdin_json::<CodexHookInput>()? else {
        return Err("expected a Codex PostToolUse hook payload on stdin".into());
    };
    let output = match cmd {
        CodexPostToolUseCommand::Similarity => CodexSimilarityHook::new().handle(input)?,
        CodexPostToolUseCommand::Wrapper => CodexWrapperHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

fn run_codex_session_start(
    cmd: CodexSessionStartCommand,
) -> Result<(), Box<dyn std::error::Error>> {
    let CodexHookInput::SessionStart(input) = read_stdin_json::<CodexHookInput>()? else {
        return Err("expected a Codex SessionStart hook payload on stdin".into());
    };
    let output = match cmd {
        CodexSessionStartCommand::Summary => CodexSessionStartSummaryHook::new().handle(input)?,
    };
    write_stdout_json(&output)
}

/// Resolve the enclosing git repository's top-level directory, or
/// `None` when `cwd` is not inside a git tree (or `git` isn't on
/// `PATH`). Used to anchor `--scope project` so the hook lands at the
/// repo root no matter which subdirectory the user invoked from.
fn git_top_level(cwd: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim_end_matches(['\n', '\r']);
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
}

fn read_stdin_json<T: serde::de::DeserializeOwned>() -> Result<T, Box<dyn std::error::Error>> {
    let mut buf = String::new();
    io::stdin().read_to_string(&mut buf)?;
    Ok(serde_json::from_str(&buf)?)
}

fn write_stdout_json<T: serde::Serialize>(value: &T) -> Result<(), Box<dyn std::error::Error>> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_lens::test_support::write_file;
    use clap::CommandFactory;
    use rstest::rstest;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    /// `WithAnalyzePathArgs for SimilarityAnalyzer` is the only special
    /// case in the trait family — it derives a [`FunctionSelection`] in
    /// addition to the path-level filter so test-function filtering
    /// stays in lock-step with path-level filtering. Drive the trait
    /// impl end-to-end on a fixture with one `#[test]` and one
    /// production function and assert each path-args combination
    /// surfaces the right corpus.
    #[test]
    fn similarity_with_analyze_path_args_threads_function_selection() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
fn production(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}

#[cfg(test)]
mod tests {
    fn alpha() -> i32 {
        let a = 1;
        let b = 2;
        let c = 3;
        let d = 4;
        a + b + c + d
    }
}
"#,
        );

        let run = |args: AnalyzePathArgs| {
            let json = SimilarityAnalyzer::new()
                .with_threshold(0.5)
                .with_analyze_path_args(args)
                .analyze(&file, OutputFormat::Json)
                .unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            parsed["function_count"].as_u64().unwrap()
        };

        assert_eq!(run(AnalyzePathArgs::default()), 2, "All keeps both");
        assert_eq!(
            run(AnalyzePathArgs {
                only_tests: true,
                ..AnalyzePathArgs::default()
            }),
            1,
            "OnlyTests drops the production fn"
        );
        assert_eq!(
            run(AnalyzePathArgs {
                exclude_tests: true,
                ..AnalyzePathArgs::default()
            }),
            1,
            "ExcludeTests drops the test fn"
        );
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
        assert!(matches!(args.scope, SetupScope::Project));
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
        assert!(matches!(args.scope, SetupScope::User));
        assert!(args.dry_run);
    }

    #[test]
    fn parses_codex_hook_setup_defaults_to_user_scope() {
        let cli = Cli::try_parse_from(["agent-lens", "codex-hook", "setup"]).expect("clean parse");
        let Command::CodexHook(CodexHookCommand::Setup(args)) = cli.command else {
            panic!("expected codex-hook setup");
        };
        assert!(matches!(args.scope, CodexSetupScope::User));
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
        assert_eq!(args.common.path, PathBuf::from("src/lib.rs"));
        assert_eq!(args.common.format, OutputFormat::Md);
        assert!(args.diff.diff_only);
        assert!(args.common.path_filter.exclude_tests);
        assert_eq!(args.common.path_filter.exclude, ["generated/**"]);
        assert!((args.threshold - 0.85).abs() < f64::EPSILON);
        assert_eq!(args.min_lines, 8);
        assert_eq!(args.ranking.top, Some(3));
        // `--method` is omitted above, so it defaults to TSED.
        assert_eq!(args.method, SimilarityMethod::Tsed);
        // `--doc-overlap` is omitted above; the markdown rollup is opt-in.
        assert!(!args.doc_overlap);
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
        assert!(args.doc_overlap);
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
        assert_eq!(args.method, SimilarityMethod::Token);
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
        assert_eq!(args.sweep, vec![0.6, 0.75, 0.85]);
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
        assert!((args.threshold - 0.91).abs() < f64::EPSILON);
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
        assert_eq!(args.ranking.top, Some(12));
        assert_eq!(args.min_score, Some(8));
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
        assert_eq!(args.ranking.top, Some(7));
        assert_eq!(args.min_score, Some(2));
    }

    #[test]
    fn analyze_command_run_executes_analyzer_with_markdown_options() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
fn quiet() {}
fn branchy(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }
fn dispatch(n: i32) -> i32 {
    match n { 0 => 0, 1 => 1, 2 => 2, _ => 3 }
}
"#,
        );
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "complexity",
            file.to_str().unwrap(),
            "--format",
            "md",
            "--top",
            "1",
            "--min-score",
            "2",
        ])
        .expect("clean parse");
        let Command::Analyze(cmd) = cli.command else {
            panic!("expected analyze command");
        };
        let out = cmd.run().unwrap();
        assert!(out.contains("Top 1 by complexity"), "got: {out}");
        assert!(out.contains("`branchy`"), "got: {out}");
        assert!(!out.contains("`dispatch`"), "got: {out}");
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
        assert_eq!(args.since.as_deref(), Some("90.days.ago"));
        assert_eq!(args.ranking.top, Some(5));
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
        assert_eq!(args.query, GraphQueryKind::Path);
        assert_eq!(args.symbol, "handler");
        assert_eq!(args.to.as_deref(), Some("db_write"));
        assert_eq!(args.depth, Some(4));
        assert_eq!(args.direction, None);
        assert_eq!(args.limit, Some(10));
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
        assert_eq!(args.query, GraphQueryKind::Neighborhood);
        assert_eq!(args.direction, Some(GraphDirection::In));
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
        assert_eq!(args.function, ["Resolver::resolve", "helper"]);
        assert_eq!(args.depth, Some(3));
        assert_eq!(args.ranking.top, Some(5));
        assert_eq!(args.common.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_impact_without_flags_as_diff_mode() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "impact", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Impact(args)) = cli.command else {
            panic!("expected analyze impact");
        };
        assert!(args.function.is_empty());
        assert_eq!(args.depth, None);
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
        assert_eq!(args.common.path, PathBuf::from("crates"));
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.ranking.top, Some(10));
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
        assert_eq!(args.common.path, PathBuf::from("crates"));
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.ranking.top, Some(8));
        assert!(args.common.path_filter.exclude_tests);
    }

    #[test]
    fn parses_analyze_layers_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "layers", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Layers(args)) = cli.command else {
            panic!("expected analyze layers");
        };
        assert_eq!(args.common.path, PathBuf::from("."));
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.ranking.top, None);
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
        assert_eq!(args.common.path, PathBuf::from("crates"));
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.ranking.top, Some(30));
        assert_eq!(args.common.path_filter.exclude, ["benches/**"]);
    }

    #[test]
    fn parses_analyze_untested_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "untested", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Untested(args)) = cli.command else {
            panic!("expected analyze untested");
        };
        assert_eq!(args.common.path, PathBuf::from("."));
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.ranking.top, None);
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
        assert_eq!(args.common.path, PathBuf::from("crates"));
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.ranking.top, Some(30));
        assert_eq!(args.common.path_filter.exclude, ["benches/**"]);
    }

    #[test]
    fn parses_analyze_visibility_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "visibility", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Visibility(args)) = cli.command else {
            panic!("expected analyze visibility");
        };
        assert_eq!(args.common.path, PathBuf::from("."));
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.ranking.top, None);
    }

    #[test]
    fn parses_analyze_coupling_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "coupling", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Coupling(args)) = cli.command else {
            panic!("expected analyze coupling");
        };
        assert_eq!(args.path, PathBuf::from("."));
        assert_eq!(args.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_cycles_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "cycles", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Cycles(args)) = cli.command else {
            panic!("expected analyze cycles");
        };
        assert_eq!(args.path, PathBuf::from("."));
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
        assert_eq!(args.path, PathBuf::from("src"));
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
        assert_eq!(args.path, PathBuf::from("."));
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
        assert_eq!(args.path, PathBuf::from("src/lib.rs"));
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
        assert!(args.entry_glob.is_empty());
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
            args.entry_glob,
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

    #[test]
    fn setup_scope_into_settings_scope_round_trip() {
        let project: SettingsScope = SetupScope::Project.into();
        let user: SettingsScope = SetupScope::User.into();
        assert!(matches!(project, SettingsScope::Project));
        assert!(matches!(user, SettingsScope::User));
    }

    #[test]
    fn codex_setup_scope_into_config_scope_round_trip() {
        let project: codex_setup::ConfigScope = CodexSetupScope::Project.into();
        let user: codex_setup::ConfigScope = CodexSetupScope::User.into();
        assert!(matches!(project, codex_setup::ConfigScope::Project));
        assert!(matches!(user, codex_setup::ConfigScope::User));
    }

    #[test]
    fn apply_setup_plan_reports_and_runs_only_when_changed() {
        let path = Path::new("settings.json");
        let context = || SetupApplyContext {
            path,
            added_commands: 1,
            dry_run_message: "dry run",
            wrote_message: "wrote",
            unchanged_message: "unchanged",
        };

        let dry_run_applied = std::cell::Cell::new(false);
        let wrote = apply_setup_plan(true, true, context(), || {
            dry_run_applied.set(true);
            Ok(())
        })
        .unwrap();
        assert!(!wrote);
        assert!(!dry_run_applied.get());

        let unchanged_applied = std::cell::Cell::new(false);
        let wrote = apply_setup_plan(false, false, context(), || {
            unchanged_applied.set(true);
            Ok(())
        })
        .unwrap();
        assert!(!wrote);
        assert!(!unchanged_applied.get());

        let changed_applied = std::cell::Cell::new(false);
        let wrote = apply_setup_plan(false, true, context(), || {
            changed_applied.set(true);
            Ok(())
        })
        .unwrap();
        assert!(wrote);
        assert!(changed_applied.get());
    }

    #[test]
    fn git_top_level_returns_none_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        // tempdir() returns a fresh path; nothing inside it is git-tracked.
        assert!(git_top_level(dir.path()).is_none());
    }

    #[test]
    fn git_top_level_finds_repo_root_from_subdirectory() {
        let dir = tempfile::tempdir().unwrap();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        let nested = dir.path().join("nested/inner");
        std::fs::create_dir_all(&nested).unwrap();
        let resolved = git_top_level(&nested).expect("inside the new repo");
        // Resolve symlinks on both sides — macOS tempdirs live under
        // /private/var/... while git emits /var/..., so a literal
        // comparison is fragile.
        let canonical_dir = std::fs::canonicalize(dir.path()).unwrap();
        let canonical_resolved = std::fs::canonicalize(&resolved).unwrap();
        assert_eq!(canonical_resolved, canonical_dir);
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
        assert_eq!(args.profile, "web");
        assert_eq!(args.config, Some(PathBuf::from("cfg/agent-lens.toml")));
    }

    #[test]
    fn parses_run_without_config_flag() {
        let cli = Cli::try_parse_from(["agent-lens", "run", "backend"]).expect("clean parse");
        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.profile, "backend");
        assert_eq!(args.config, None);
    }

    #[test]
    fn run_requires_a_profile_name() {
        let err = Cli::try_parse_from(["agent-lens", "run"]).expect_err("missing profile");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn build_analyze_command_maps_similarity_options() {
        let profile: config::Profile = toml::from_str(
            "path = \"web\"\ntools = [\"similarity\"]\n\n[similarity]\nthreshold = 0.7\nmin-lines = 9\ntop = 4\nmethod = \"token\"\ndoc-overlap = true\ndiff-only = true\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Similarity,
            &profile,
            Path::new("/repo/web"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Similarity(args) = cmd else {
            panic!("expected analyze similarity");
        };
        assert_eq!(args.common.path, PathBuf::from("/repo/web"));
        assert_eq!(args.common.format, OutputFormat::Md);
        assert!((args.threshold - 0.7).abs() < f64::EPSILON);
        assert_eq!(args.min_lines, 9);
        assert_eq!(args.ranking.top, Some(4));
        assert_eq!(args.method, SimilarityMethod::Token);
        assert!(args.doc_overlap);
        assert!(args.diff.diff_only);
    }

    #[test]
    fn build_analyze_command_uses_similarity_defaults_without_table() {
        let profile: config::Profile =
            toml::from_str("path = \"web\"\ntools = [\"similarity\"]\n").unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Similarity,
            &profile,
            Path::new("web"),
            OutputFormat::Json,
        )
        .unwrap();
        let AnalyzeCommand::Similarity(args) = cmd else {
            panic!("expected analyze similarity");
        };
        assert!((args.threshold - DEFAULT_SIMILARITY_THRESHOLD).abs() < f64::EPSILON);
        assert_eq!(args.min_lines, DEFAULT_SIMILARITY_MIN_LINES);
        assert_eq!(args.ranking.top, None);
        assert_eq!(args.method, SimilarityMethod::Tsed);
        assert!(!args.doc_overlap);
        assert!(!args.diff.diff_only);
    }

    #[test]
    fn build_analyze_command_maps_hubs_options() {
        let profile: config::Profile =
            toml::from_str("path = \"crates\"\ntools = [\"hubs\"]\n\n[hubs]\ntop = 7\n").unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Hubs,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Hubs(args) = cmd else {
            panic!("expected analyze hubs");
        };
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.ranking.top, Some(7));
    }

    #[test]
    fn build_analyze_command_maps_layers_options() {
        let profile: config::Profile =
            toml::from_str("path = \"crates\"\ntools = [\"layers\"]\n\n[layers]\ntop = 9\n")
                .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Layers,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Layers(args) = cmd else {
            panic!("expected analyze layers");
        };
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.ranking.top, Some(9));
    }

    #[test]
    fn build_analyze_command_maps_visibility_options() {
        let profile: config::Profile = toml::from_str(
            "path = \"crates\"\ntools = [\"visibility\"]\n\n[visibility]\ntop = 9\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Visibility,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Visibility(args) = cmd else {
            panic!("expected analyze visibility");
        };
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.ranking.top, Some(9));
    }

    #[test]
    fn build_analyze_command_maps_untested_options() {
        let profile: config::Profile =
            toml::from_str("path = \"crates\"\ntools = [\"untested\"]\n\n[untested]\ntop = 11\n")
                .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Untested,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Untested(args) = cmd else {
            panic!("expected analyze untested");
        };
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.ranking.top, Some(11));
    }

    #[test]
    fn build_analyze_command_maps_impact_options() {
        let profile: config::Profile = toml::from_str(
            "path = \"crates\"\ntools = [\"impact\"]\n\n\
             [impact]\nfunction = [\"Resolver::resolve\"]\ndepth = 3\ntop = 5\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Impact,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Impact(args) = cmd else {
            panic!("expected analyze impact");
        };
        assert_eq!(args.function, ["Resolver::resolve"]);
        assert_eq!(args.depth, Some(3));
        assert_eq!(args.ranking.top, Some(5));
        assert_eq!(args.common.format, OutputFormat::Md);
    }

    #[test]
    fn build_analyze_command_defaults_impact_to_diff_mode() {
        let profile: config::Profile =
            toml::from_str("path = \"crates\"\ntools = [\"impact\"]\n").unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Impact,
            &profile,
            Path::new("crates"),
            OutputFormat::Json,
        )
        .unwrap();
        let AnalyzeCommand::Impact(args) = cmd else {
            panic!("expected analyze impact");
        };
        assert!(args.function.is_empty());
        assert_eq!(args.depth, None);
    }

    #[test]
    fn build_analyze_command_maps_graph_query_options() {
        let profile: config::Profile = toml::from_str(
            "path = \"crates\"\ntools = [\"graph-query\"]\n\n\
             [graph-query]\nquery = \"path\"\nsymbol = \"handler\"\nto = \"db_write\"\n\
             depth = 3\nlimit = 10\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::GraphQuery,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::GraphQuery(args) = cmd else {
            panic!("expected analyze graph-query");
        };
        assert_eq!(args.query, GraphQueryKind::Path);
        assert_eq!(args.symbol, "handler");
        assert_eq!(args.to.as_deref(), Some("db_write"));
        assert_eq!(args.depth, Some(3));
        assert_eq!(args.direction, None);
        assert_eq!(args.limit, Some(10));
        assert_eq!(args.common.format, OutputFormat::Md);
    }

    #[test]
    fn build_analyze_command_rejects_graph_query_without_table() {
        // `toml::from_str` skips `Config::validate`, so the seam-level
        // guard in `build_analyze_command` is what stands here.
        let profile: config::Profile =
            toml::from_str("path = \"crates\"\ntools = [\"graph-query\"]\n").unwrap();
        let err = build_analyze_command(
            config::ToolName::GraphQuery,
            &profile,
            Path::new("crates"),
            OutputFormat::Json,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::MissingToolOptions {
                    tool: "graph-query"
                }
            ),
            "got: {err:?}",
        );
    }

    #[test]
    fn build_analyze_command_propagates_profile_path_filters() {
        let profile: config::Profile = toml::from_str(
            "path = \"web\"\nexclude = [\"gen/**\"]\nexclude-tests = true\ntools = [\"coupling\"]\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Coupling,
            &profile,
            Path::new("web"),
            OutputFormat::Json,
        )
        .unwrap();
        let AnalyzeCommand::Coupling(args) = cmd else {
            panic!("expected analyze coupling");
        };
        assert_eq!(args.path_filter.exclude, ["gen/**"]);
        assert!(args.path_filter.exclude_tests);
        assert!(!args.path_filter.only_tests);
    }

    #[test]
    fn render_profile_report_md_stacks_tool_sections() {
        let sections = vec![
            (config::ToolName::Complexity, "complexity body\n".to_owned()),
            (config::ToolName::Wrapper, "wrapper body".to_owned()),
        ];
        let out = render_profile_report("audit", OutputFormat::Md, &sections).unwrap();
        // No leading newline, and a single blank line between sections.
        assert_eq!(
            out,
            "## complexity\n\ncomplexity body\n\n## wrapper\n\nwrapper body\n",
        );
    }

    #[test]
    fn unused_tool_option_tables_flags_tables_off_the_tools_list() {
        let profile: config::Profile = toml::from_str(
            "path = \"web\"\ntools = [\"similarity\"]\n\n[similarity]\nthreshold = 0.9\n\n[complexity]\nmin-score = 3\n\n[wrapper]\ndiff-only = true\n",
        )
        .unwrap();
        // similarity is listed in `tools`, so only complexity and wrapper
        // are flagged — in the fixed iteration order.
        assert_eq!(
            unused_tool_option_tables(&profile),
            [config::ToolName::Complexity, config::ToolName::Wrapper],
        );
    }

    #[test]
    fn unused_tool_option_tables_empty_when_every_table_is_listed() {
        let profile: config::Profile = toml::from_str(
            "path = \"web\"\ntools = [\"similarity\"]\n\n[similarity]\nthreshold = 0.9\n",
        )
        .unwrap();
        assert!(unused_tool_option_tables(&profile).is_empty());
    }

    #[test]
    fn render_profile_report_json_nests_each_tool_report() {
        let sections = vec![(config::ToolName::Complexity, "{\"k\":1}".to_owned())];
        let out = render_profile_report("audit", OutputFormat::Json, &sections).unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["profile"], "audit");
        assert_eq!(value["results"][0]["tool"], "complexity");
        assert_eq!(value["results"][0]["report"]["k"], 1);
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
        assert!(matches!(args.scope, SkillsScopeArg::Project));
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
        assert!(matches!(args.scope, SkillsScopeArg::User));
        assert!(args.dry_run);
        assert!(args.force);
    }

    #[test]
    fn skills_scope_arg_into_scope_round_trip() {
        let project: skills::SkillsScope = SkillsScopeArg::Project.into();
        let user: skills::SkillsScope = SkillsScopeArg::User.into();
        assert!(matches!(project, skills::SkillsScope::Project));
        assert!(matches!(user, skills::SkillsScope::User));
    }

    #[test]
    fn help_md_render_covers_the_whole_command_surface() {
        // The `help --md` document must reach the deepest analyzer leaves,
        // not just the top-level command trees.
        let md = help_md::render(&Cli::command());
        assert!(md.starts_with("# agent-lens\n"), "got: {md}");
        assert!(md.contains("## `agent-lens analyze`"), "got: {md}");
        assert!(
            md.contains("### `agent-lens analyze similarity`"),
            "got: {md}",
        );
        assert!(md.contains("## `agent-lens skills`"), "got: {md}");
        assert!(md.contains("## `agent-lens config`"), "got: {md}");
        assert!(md.contains("### `agent-lens config schema`"), "got: {md}");
        // Analyzer-specific options surface in the table.
        assert!(md.contains("`--threshold <THRESHOLD>`"), "got: {md}");
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
