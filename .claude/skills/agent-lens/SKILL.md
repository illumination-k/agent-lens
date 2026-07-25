---
name: agent-lens
description: Use when the user asks to analyze this codebase with agent-lens, or asks which analyzer fits a given question (duplication, complexity, hotspots, coupling, cohesion, forwarding wrappers). Routes to the right `agent-lens analyze` subcommand and explains how to read the output. Prefer the more specific skills (find-duplicates, review-pending-changes, find-refactor-targets, audit-architecture) when one of them clearly fits.
---

# agent-lens analyzer dispatcher

`agent-lens` is the project's own CLI. The binary is on `PATH` after `mise install`; if `agent-lens --version` fails, build it with `cargo build -p agent-lens` and use `./target/debug/agent-lens`.

## Pick the analyzer

| Question                                                | Subcommand       | Path argument                                     |
| ------------------------------------------------------- | ---------------- | ------------------------------------------------- |
| Are there near-duplicate functions?                     | `similarity`     | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Are there forwarding-only functions worth inlining?     | `wrapper`        | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Which classes/`impl` blocks are doing too many things?  | `cohesion`       | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Which functions are landmines to edit?                  | `complexity`     | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Which modules are Fan-In bottlenecks or cyclic?         | `coupling`       | Rust crate / TS/JS entry / Go module              |
| Which functions call each other in a cycle?             | `cycles`         | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| How many files must I read to understand a module?      | `context-span`   | Rust crate / TS/JS entry / Python / Go            |
| Who calls this function? What does it call?             | `graph-query`    | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Is there a call chain from A to B?                      | `graph-query`    | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| I need the whole call graph as data                     | `function-graph` | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Which functions are hubs I should read/handle first?    | `hubs`           | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| What could my current edit break? Which tests cover it? | `impact`         | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Where do churn and complexity collide?                  | `hotspot`        | git-tracked file or directory                     |

`similarity` / `wrapper` / `cohesion` / `complexity` / `function-graph` / `graph-query` / `cycles` / `hubs` / `impact` / `context-span` work on Rust, TypeScript / JavaScript, Python, and Go. `coupling` works on Rust crates, TS/JS module graphs, and Go modules. For `context-span`, pass `--entry-glob` repeatedly to merge several TS/JS entry trees (Next.js App Router, Remix, Astro, …) in one run. `hotspot` requires a git working tree.

## Output format

- Default `stdout` is JSON — pipe into `jq` for ad-hoc filtering.
- Pass `--format md` when feeding the report into another agent's context window.
- Diagnostics go to `stderr` via `tracing`. Set `RUST_LOG=debug` for verbose.

## Always prefer `--diff-only` for in-progress edits

`similarity`, `wrapper`, `cohesion`, and `complexity` accept `--diff-only`, which restricts the report to functions or `impl` blocks touching unstaged changes (`git diff -U0`). Use this on a hot file rather than dumping the whole report into context.

## One-shot examples

```bash
# Top-level: what does the analyzer surface look like for a given file?
agent-lens analyze complexity crates/lens-rust/src/lib.rs --format md

# Restricted to the function I'm currently editing
agent-lens analyze similarity crates/lens-rust/src/foo.rs --diff-only --format md

# Cross-file duplicates across a directory tree
agent-lens analyze similarity crates/lens-rust/src --format md

# Crate-wide structure (Rust crate)
agent-lens analyze coupling crates/agent-lens --format md

# Crate-wide structure (TS/JS module graph from an entry file)
agent-lens analyze coupling app/src/index.ts --format md

# How many files must an agent open to reason about each module?
agent-lens analyze context-span crates/agent-lens --format md

# TS/JS frameworks with many entries: merge several trees into one report
agent-lens analyze context-span app \
  --entry-glob 'app/**/page.tsx' --entry-glob 'app/**/route.ts' --format md

# Who calls `Resolver::resolve` (direct callers only)?
agent-lens analyze graph-query crates/agent-lens/src \
  --query callers --symbol 'Resolver::resolve' --format md

# Blast radius: everything that transitively reaches `resolve`
agent-lens analyze graph-query crates/agent-lens/src \
  --query callers --symbol 'Resolver::resolve' --depth 10 --format md

# Shortest call chain from a handler to a sink
agent-lens analyze graph-query crates/agent-lens/src \
  --query path --symbol run_analyze --to write_stdout_line --format md

# Full call graph as data (prefer graph-query for point questions)
agent-lens analyze function-graph crates/lens-rust/src --format md

# God functions, load-bearing utilities, bottlenecks, misplaced functions
agent-lens analyze hubs crates/agent-lens/src --exclude-tests --format md

# Blast radius of my unstaged edits, with the tests that reach them
agent-lens analyze impact crates/agent-lens/src --format md

# Blast radius of a planned edit, before making it
agent-lens analyze impact crates/agent-lens/src \
  --function 'Resolver::resolve' --format md

# Where is the next refactor likely to pay off?
agent-lens analyze hotspot crates --since=180.days.ago --top 10 --format md
```

## Reading the output

- **similarity**: each entry is a pair `(a, b)` with `tsed` in `[0.0, 1.0]`. ≥ 0.95 is essentially a clone; 0.85–0.95 is a near-miss worth refactoring; below 0.85 is filtered out by default. The `--threshold` flag is for tightening or loosening that bar; `--sweep 0.6,0.75,0.85` instead clusters once at the lowest rung and tags each cluster with the highest rung it survives (a coarse dendrogram), separating verbatim clones from structural parallels in one run.
- **wrapper**: a hit means the function body, after stripping `?` / `.into()` / `.unwrap()` / `.await`, is just a forwarding call. Either inline it or document why the indirection exists.
- **cohesion**: `lcom4 == 1` is healthy. `lcom4 >= 2` means the `impl` has disjoint method clusters and is a candidate for splitting.
- **complexity**: cognitive ≥ 15 is a yellow flag, ≥ 25 is a red flag. Maintainability Index < 65 means the function is hard to maintain regardless of what cyclomatic says.
- **coupling**: high `fan_in` ⇒ a hub everything depends on (slow to change safely); high `fan_out` ⇒ a module that is hard to test in isolation; non-empty `cycles` is always a smell. Reports Martin's `instability = Ce/(Ca+Ce)` per module too. The module unit differs by language: for Rust it is the crate's `mod` tree, for TS/JS a source file reachable from the entry, for Go a package (directory) in the module.
- **cycles**: each entry is a group of 2+ functions that call each other (directly or transitively) over resolved call edges — they must be understood, tested, and changed as one unit. `same_file: true` usually means intentional mutual recursion (parsers, tree walkers) and is ranked below cross-file tangles. `break_suggestions` name the cheapest internal edges (by static call-site count) whose removal breaks the cycle — advisory: check the listed `call_lines` before acting, a cheap edge can still be load-bearing. A high `ambiguous_edge_count_nearby` means the tangle's true extent is uncertain.
- **context-span**: each module's transitive outgoing closure plus the count of distinct source files those modules span. Treat the file count as an "onboarding cost" — a module with span 30 means an agent must open ~30 files to reason about it.
- **function-graph**: nodes are functions with per-node weights (`fan_in`, `fan_out`, complexity, MI, Halstead). Edges are syntactic call sites with a `resolution` (`resolved` / `unresolved` / `ambiguous` / `anonymous`). Resolution is heuristic — high `unresolved_edge_count` mostly means trait dispatch and external calls, not a bug. Prefer `graph-query` for point questions; use the full dump for visualization or offline processing.
- **graph-query**: one canned traversal per run — `--query callers|callees|neighborhood` from `--symbol` (depth 1 by default, `--direction in|out|both` for neighborhood), or `--query path --symbol A --to B` for the shortest call chain with per-hop call lines. Symbols match by `::`-segment suffix (`foo`, `module::foo`, `Owner::method`) or exact node id; on ambiguity the tool lists the candidates instead of guessing — re-run with one of the listed ids. Traversal follows resolved edges only, so results are lower bounds: a row with high `unres`/`ambig` counts has outgoing calls the resolver could not follow (trait dispatch, externals). Output is capped by node count (`--limit`, default 50).
- **hubs**: four ranked lists on the resolved call graph. God functions (outlier fan-out) are refactor candidates; load-bearing functions (outlier fan-in) are a blast-radius signal, not a defect — check their callers before editing them; bottlenecks spike Henry-Kafura `loc × (fan_in × fan_out)²` (size-confounded, read next to `loc`); "misplaced?" entries send most resolved call traffic to one foreign module. Degrees count resolved edges only, so they are lower bounds — the `fallback` share and the resolution-confidence section say how much to trust each number. `PR` is a deterministic PageRank-importance percentile.
- **impact**: one entry per changed function (seeded from the unstaged diff, or `--function` for a pre-edit query). `direct_callers` are verbatim; deeper callers fold to per-depth per-module counts; `reachable_tests` is the verification checklist — run those. `vfi` is the transitive caller count within `--depth` (default 5, cycles count as one hop); `beyond_depth_count` says what the cap hid. Counts follow resolved edges only and are bounds in both directions: `excluded_ambiguous_edge_count` and `unattributed_caller_edge_count` quantify would-be callers the resolver could not attribute. `impact_explosion` flags depth-2 fan-out ≥ 3× depth-1 — a hidden shotgun-surgery signal.
- **hotspot**: rows are sorted by `commits × cognitive_max`. The top of the list is where bugs concentrate; refactor budget is best spent there first.

## Don't reach for it when

- The user wants human-style lints (style, naming, idioms) — that's clippy / dprint / rustfmt, not agent-lens.
- The file isn't a supported language — agent-lens errors out cleanly, but check the table above first.
- The question is "is this code correct?" — analyzers measure shape, not semantics.
