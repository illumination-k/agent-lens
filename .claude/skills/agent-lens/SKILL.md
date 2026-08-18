---
name: agent-lens
description: Use when the user asks to analyze this codebase with agent-lens, or asks which analyzer fits a given question (duplication, complexity, hotspots, coupling, cohesion, forwarding wrappers, delegation chains). Routes to the right `agent-lens analyze` subcommand and explains how to read the output. Prefer the more specific skills (find-duplicates, review-pending-changes, find-refactor-targets, audit-architecture) when one of them clearly fits.
---

# agent-lens analyzer dispatcher

`agent-lens` is the project's own CLI. The binary is on `PATH` after `mise install`; if `agent-lens --version` fails, build it with `cargo build -p agent-lens` and use `./target/debug/agent-lens`.

## Pick the analyzer

| Question                                                | Subcommand       | Path argument                                     |
| ------------------------------------------------------- | ---------------- | ------------------------------------------------- |
| Are there near-duplicate functions?                     | `similarity`     | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Are there forwarding-only functions worth inlining?     | `wrapper`        | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| How many hops of forwarding before the real work?       | `delegation`     | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Which classes/`impl` blocks are doing too many things?  | `cohesion`       | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Which functions are landmines to edit?                  | `complexity`     | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Which modules are Fan-In bottlenecks or cyclic?         | `coupling`       | Rust crate / TS/JS entry / Go or Python directory |
| Is this file filed under the right module?              | `communities`    | Rust crate / TS/JS entry / Go or Python directory |
| Which functions call each other in a cycle?             | `cycles`         | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| How many files must I read to understand a module?      | `context-span`   | Rust crate / TS/JS entry / Python / Go            |
| Who calls this function? What does it call?             | `graph-query`    | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Is there a call chain from A to B?                      | `graph-query`    | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| I need the whole call graph as data                     | `function-graph` | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Which functions are hubs I should read/handle first?    | `hubs`           | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| What could my current edit break? Which tests cover it? | `impact`         | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Where does this new function belong vertically?         | `layers`         | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Is it OK for this module to call that one?              | `layers`         | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Which code has no test path guarding it?                | `untested`       | `.rs` / `.ts` / `.js` / `.py` / `.go` file or dir |
| Can anything still reach this code? Is it dead?         | `unreachable`    | `.rs` / `.go` file or dir                         |
| Is this `pub` wider than its callers need?              | `visibility`     | `.rs` / `.go` file or dir                         |
| Where do churn and complexity collide?                  | `hotspot`        | git-tracked file or directory                     |
| How carefully should I treat this edit?                 | `risk`           | git-tracked file or directory                     |
| What else will history make me edit alongside this?     | `co-change`      | git-tracked directory (any file type)             |

`similarity` / `wrapper` / `delegation` / `cohesion` / `complexity` / `function-graph` / `graph-query` / `cycles` / `hubs` / `impact` / `layers` / `untested` / `context-span` work on Rust, TypeScript / JavaScript, Python, and Go. `delegation` is strongest on Rust: only Rust and Go can exempt a module facade, and the per-language forwarding idioms it does not model (Python properties, Go embedded structs) only cost it findings. `visibility` and `unreachable` judge Rust and Go only — TypeScript and Python carry no extracted export status, and both say how many functions they skipped for that reason (`unreachable` treats them as entry points, so nothing they call is reported). `coupling` and `communities` work on Rust crates, TS/JS module graphs, Go modules, and Python package trees; both grow one module graph from one entry point. Read `communities` top-down: if its declared modularity is close to its detected one, the declared boundaries already are the clustering and the misfiled rows below it are noise. For `context-span`, pass `--entry-glob` repeatedly to merge several TS/JS entry trees (Next.js App Router, Remix, Astro, …) in one run. `hotspot`, `risk` and `co-change` require a git working tree. `co-change` has no language matrix at all — it reads `git log` and never parses a file, so it is the only analyzer that sees `.toml`, `.md`, workflow YAML and fixtures.

## Several paths in one run

Every analyzer except `coupling`, `context-span` and `communities` takes more than one PATH, and walks them all into a single report:

```bash
agent-lens analyze similarity packages cli web/src --format md --exclude-tests
```

Reach for this in a monorepo, where the trees you care about are siblings and their only common ancestor is the repo root (which drags in `node_modules`, generated output, and everything else). It is not the same as running the analyzer once per tree: `similarity` clusters across the whole corpus and the call-graph analyzers resolve edges across it, so a duplicate or a call spanning two trees is only visible in one combined run. Display paths are written relative to the paths' deepest common ancestor, so each file keeps the tree it came from in its name.

`coupling`, `context-span` and `communities` grow one module graph out of one entry point, so they keep the single-PATH signature. Use `--entry-glob` for `context-span` when a TS/JS framework has many entries; for `coupling`, pick a representative entry.

## Output format

- Default `stdout` is JSON — pipe into `jq` for ad-hoc filtering.
- Pass `--format md` when feeding the report into another agent's context window.
- Every analyzer that can emit a long markdown report takes `--top` to bound it, `cycles` excepted (a truncated cycle list reads as the whole list). JSON always carries the full result — `--top` is a rendering cap, not a filter.
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

# Cross-tree duplicates: several roots, one corpus
agent-lens analyze similarity crates/lens-py crates/lens-golang --format md

# Crate-wide structure, bounded to the 15 most-coupled modules
agent-lens analyze coupling crates/agent-lens --format md --top 15

# Do the declared module boundaries match the clustering the dependencies form?
agent-lens analyze communities crates/agent-lens --format md --top 15

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

# Inferred layer map: function/module levels, module cycles, skip-level calls
agent-lens analyze layers crates/agent-lens/src --exclude-tests --format md

# Which production functions no test statically reaches
agent-lens analyze untested crates/agent-lens/src --format md

# Which functions nothing can reach any more (leads with the deletable tier)
agent-lens analyze unreachable . --format md

# …plus the leads the tool cannot confirm, when hunting for an abandoned feature
agent-lens analyze unreachable . --tier unknown --format md

# Which `pub` items no caller outside a narrower scope uses
agent-lens analyze visibility . --format md

# How many forwarding hops sit between an entry point and the real work
agent-lens analyze delegation crates/agent-lens/src --format md

# Where is the next refactor likely to pay off?
agent-lens analyze hotspot crates --since=180.days.ago --top 10 --format md

# Which files are both changing often and load-bearing (edit them carefully)?
agent-lens analyze risk crates --since=180.days.ago --top 10 --format md

# What does history say I will have to edit alongside this? Point it at the
# widest path you care about — a pathspec scopes the file sets, not just the
# commits, so cross-tree pairs need a run covering both trees
agent-lens analyze co-change . --min-support 5 --top 15 --format md
```

## Reading the output

- **similarity**: each entry is a pair `(a, b)` with `tsed` in `[0.0, 1.0]`. ≥ 0.95 is essentially a clone; 0.85–0.95 is a near-miss worth refactoring; below 0.85 is filtered out by default. The `--threshold` flag is for tightening or loosening that bar; `--sweep 0.6,0.75,0.85` instead clusters once at the lowest rung and tags each cluster with the highest rung it survives (a coarse dendrogram), separating verbatim clones from structural parallels in one run.
- **wrapper**: a hit means the function body, after stripping `?` / `.into()` / `.unwrap()` / `.await`, is just a forwarding call. Either inline it or document why the indirection exists.
- **delegation**: each row is a chain of functions that only forward, ending at the terminus — the one doing the work, named with its `file:line`, which is the file to open first. Depth is the context tax: a 3-hop chain costs four file opens to answer one question. Trust the hops marked `args forwarded verbatim` most (the language's own wrapper detector agreed); a hop without that mark was classified from body shape alone and can be composing rather than forwarding — a constructor calling a constructor is the usual false positive. `other caller(s) to move` on a hop is the cost of collapsing the chain: those call sites have to be repointed at the terminus. The module roll-up is the lasagna half: a module flagged `layer candidate` is mostly forwarders pointing mostly at one other module, so inlining it is a single mechanical change. One-hop forwards are counted, not listed — that is `analyze wrapper`'s report, and it carries argument-level evidence.
- **cohesion**: `lcom4 == 1` is healthy. `lcom4 >= 2` means the `impl` has disjoint method clusters and is a candidate for splitting.
- **complexity**: cognitive ≥ 15 is a yellow flag, ≥ 25 is a red flag. Maintainability Index < 65 means the function is hard to maintain regardless of what cyclomatic says.
- **coupling**: high `fan_in` ⇒ a hub everything depends on (slow to change safely); high `fan_out` ⇒ a module that is hard to test in isolation; non-empty `cycles` is always a smell. Reports Martin's `instability = Ce/(Ca+Ce)` per module too. The module unit differs by language: for Rust it is the crate's `mod` tree, for TS/JS a source file reachable from the entry, for Go a package (directory) in the module, for Python a `.py` file under the root.
- **cycles**: each entry is a group of 2+ functions that call each other (directly or transitively) over resolved call edges — they must be understood, tested, and changed as one unit. `same_file: true` usually means intentional mutual recursion (parsers, tree walkers) and is ranked below cross-file tangles. `break_suggestions` name the cheapest internal edges (by static call-site count) whose removal breaks the cycle — advisory: check the listed `call_lines` before acting, a cheap edge can still be load-bearing. A high `ambiguous_edge_count_nearby` means the tangle's true extent is uncertain.
- **context-span**: each module's transitive outgoing closure plus the count of distinct source files those modules span. Treat the file count as an "onboarding cost" — a module with span 30 means an agent must open ~30 files to reason about it.
- **function-graph**: nodes are functions with per-node weights (`fan_in`, `fan_out`, complexity, MI, Halstead). Edges are syntactic call sites with a `resolution` (`resolved` / `unresolved` / `ambiguous` / `anonymous`). Resolution is heuristic — high `unresolved_edge_count` mostly means trait dispatch and external calls, not a bug. Prefer `graph-query` for point questions; use the full dump for visualization or offline processing.
- **graph-query**: one canned traversal per run — `--query callers|callees|neighborhood` from `--symbol` (depth 1 by default, `--direction in|out|both` for neighborhood), or `--query path --symbol A --to B` for the shortest call chain with per-hop call lines. Symbols match by `::`-segment suffix (`foo`, `module::foo`, `Owner::method`) or exact node id; on ambiguity the tool lists the candidates instead of guessing — re-run with one of the listed ids. Traversal follows resolved edges only, so results are lower bounds: a row with high `unres`/`ambig` counts has outgoing calls the resolver could not follow (trait dispatch, externals). Output is capped by node count (`--limit`, default 50).
- **hubs**: four ranked lists on the resolved call graph. God functions (outlier fan-out) are refactor candidates; load-bearing functions (outlier fan-in) are a blast-radius signal, not a defect — check their callers before editing them; bottlenecks spike Henry-Kafura `loc × (fan_in × fan_out)²` (size-confounded, read next to `loc`); "misplaced?" entries send most resolved call traffic to one foreign module. Degrees count resolved edges only, so they are lower bounds — the `fallback` share and the resolution-confidence section say how much to trust each number. `PR` is a deterministic PageRank-importance percentile.
- **impact**: one entry per changed function (seeded from the unstaged diff, or `--function` for a pre-edit query). `direct_callers` are verbatim; deeper callers fold to per-depth per-module counts; `reachable_tests` is the verification checklist — run those. `vfi` is the transitive caller count within `--depth` (default 5, cycles count as one hop); `beyond_depth_count` says what the cap hid. Counts follow resolved edges only and are bounds in both directions: `excluded_ambiguous_edge_count` and `unattributed_caller_edge_count` quantify would-be callers the resolver could not attribute. `impact_explosion` flags depth-2 fan-out ≥ 3× depth-1 — a hidden shotgun-surgery signal.
- **layers**: two inferred layerings. `L` is a function level (`1 + max(level of its callees)`, call cycles collapsed to one node), `M` the same computation on the module graph — a module's level need not match its members' levels, since a leaf-heavy module can still hold one high-level caller. Read `entry_points` first, then the level buckets top-down. `module_cycles` is the actionable part: those modules are mutually dependent, so no ordering exists between them, and each listed call site is where the cycle is realised. `skip_calls` are downward calls jumping over an intermediate module level — expected for shared leaf utilities, worth a look when a skipped level owns the same concern. A module with a wide `member_level_span` mixes leaf helpers with orchestration. None of these are errors: callbacks and DI shape the graph the same way. Ignore rows marked `name-fallback` first — the resolver guessed those targets by last segment.
- **untested**: production functions the forward walk from every test function never reaches, grouped by module and ranked by untested LOC — start at the top, it is the largest unguarded body. Read it as "no resolved call path from a test", not "uncovered": integration tests that drive the built binary reach functions with no in-graph test caller, and those are listed here anyway. The listing is an upper bound — a row flagged `may be test-reached` is named by an ambiguous call site leaving test-reached code, so check it before writing a test. `fan_in: 0` means no production caller either, which is a dead-code question rather than a testing one. `--exclude-tests` removes the traversal's starting points and makes the report meaningless; the output says so when it happens.
- **unreachable**: three tiers, and only one of them is permission to delete. `confirmed` = private (Rust) / unexported (Go), no resolved call path from any entry point, and its bare name appears nowhere else in the scanned sources — act on those, ideally per island. `likely` = nothing in the analyzed path uses or names it, but the declaration reaches outside that path; on a library that is its published API, so check the consumers before touching it. `unknown` = a lead: something could reach it in a way the graph does not model, and the row says which (trait or interface dispatch, an annotation, an ambiguous call site, a raw name reference). Markdown shows `confirmed` only unless you pass `--tier`. Read the entry set line first — every verdict is relative to it — and the demotion breakdown next: an empty `confirmed` tier on a Rust workspace usually means the resolver could not follow the calls, not that nothing is dead (rustc's own `dead_code` lint has already taken the easy findings). Islands are the strongest signal: a cluster that only calls itself, with a total LOC and an order that removes callers before their callees. Do not run it with `--exclude-tests` before deleting: that drops the test entry points and makes code used only by tests look dead (the report says so when it happens).
- **visibility**: each row is a `pub` / exported function plus the visibility its resolved callers would still permit, so the row is an edit you can apply and let the compiler check — a wrong one costs a failed build, never lost code. Rows with resolved callers come first in a module section; `verify: no resolved caller in the analyzed tree` means nothing in scope calls it at all, which is a dead-code or external-API question rather than a narrowing one. A row flagged `verify first` is named by a call site the resolver could not attribute (an ambiguous call, or a receiver call on a name like `.clone()`), so confirm that caller before narrowing. A Go row annotated `may satisfy interface …` matches a method of an interface declared in the tree by name and arity — its calls can dispatch through the interface, so the missing caller is expected; those rows list last in their bucket and the annotation can be coincidental (structural match only). Run it at the workspace root: callers outside the analyzed path do not exist for this analyzer, and the report says so when only one crate is in scope. `pub use` re-exports in a crate root are excluded as intended API; a package with both `lib.rs` and `main.rs` is called out, because a `pub(crate)` whose caller sits in the binary will not compile.
- **hotspot**: rows are sorted by `commits × cognitive_max`. The top of the list is where bugs concentrate; refactor budget is best spent there first.
- **risk**: the blast-radius sibling of `hotspot`, and the one place in the tool where **lower is riskier** — `rank_product = churn_rank × centrality_rank`, so rank 1 on both axes gives 1. It answers "how carefully should I treat this edit?", not "what should I fix": a top row means read the callers and run the listed tests first. `churn_rank` and `centrality_rank` show which axis drove the row; a file high on churn but low on centrality is hot-but-leaf and safe to move fast on. `hottest_function` names the member whose PageRank carried the file, and `vfi_max` is how many functions transitively call it. Centrality follows resolved edges only, so modules listed under resolution confidence are ranked lower than they deserve.
- **co-change**: each row is a file pair with `cochanges` (commits touching both), the confidence in **both** directions, and `lift`. Read the two confidences as the closest thing to a direction: `config.rs -> config_schema.rs` at 0.82 and back at 0.86 means neither moves without the other, while 0.22 / 0.93 means one file drags the other along but not the reverse — that is the row that tells an agent what it forgot. Check `lift` before believing a row: near 1 the pair co-occurs exactly as often as two files that busy would by chance, however high its support, and the usual culprit is `Cargo.lock` or another repo-wide file. `last_cochange` dates the pattern, so a strong pair last seen 200 commits ago describes a coupling that has already been broken. It is correlation only — nothing says why two files move together — so a pair with no visible reason to be coupled is a design question, not a fact. Renames are followed; commits over `--max-commit-files` are dropped whole and counted in `skipped_commit_count`, which is where a squash-merge repo's missing history went. A shallow clone warns on stderr: fix it with `git fetch --unshallow` rather than reading the thin report.

## Don't reach for it when

- The user wants human-style lints (style, naming, idioms) — that's clippy / dprint / rustfmt, not agent-lens.
- The file isn't a supported language — agent-lens errors out cleanly, but check the table above first.
- The question is "is this code correct?" — analyzers measure shape, not semantics.
