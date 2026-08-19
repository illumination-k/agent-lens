# agent-lens

[![CI Rust](https://github.com/illumination-k/agent-lens/actions/workflows/ci_rust.yml/badge.svg)](https://github.com/illumination-k/agent-lens/actions/workflows/ci_rust.yml)
[![Lint Base](https://github.com/illumination-k/agent-lens/actions/workflows/lint_base.yml/badge.svg)](https://github.com/illumination-k/agent-lens/actions/workflows/lint_base.yml)
[![Lint GHA](https://github.com/illumination-k/agent-lens/actions/workflows/lint_gha.yml/badge.svg)](https://github.com/illumination-k/agent-lens/actions/workflows/lint_gha.yml)
[![Mutation Tests](https://github.com/illumination-k/agent-lens/actions/workflows/mutants.yml/badge.svg)](https://github.com/illumination-k/agent-lens/actions/workflows/mutants.yml)
[![codecov](https://codecov.io/gh/illumination-k/agent-lens/branch/main/graph/badge.svg)](https://codecov.io/gh/illumination-k/agent-lens)

> A sharper view of your codebase, tuned for the agent that's about to edit it.

**Website:** <https://illumination-k.github.io/agent-lens/> — the analyzer
catalogue in short form, plus a live
[function-graph viewer](https://illumination-k.github.io/agent-lens/analyze/)
for `analyze function-graph` JSON.

`agent-lens` is a single-binary Rust CLI that bundles two things coding agents
(Claude Code, Codex, …) need but usually don't get:

1. **Hooks** — handlers that speak each agent's stdin/stdout hook protocol, so
   the agent can be nudged with feedback the moment it finishes editing a file.
2. **Analyzers** — on-demand code analysis that answers questions agents
   actually ask: _which functions duplicate this one?_, _how tangled is this
   module?_, _which `impl` block is doing too many things?_ The output is
   structured for an LLM context window, not for a terminal user.

This is not another lint tool. Lints tell humans how to write nicer code.
`agent-lens` tells an LLM where the dangerous corners of your repo are, so it
can plan around them.

The project is pre-alpha. The API, CLI details, and report schemas are still
allowed to change without a major version bump while the tool settles.

Two commands are the authoritative reference for everything below:

- `agent-lens help --md` — the entire command surface (every subcommand,
  option, and worked example) as one dense Markdown document.
- `agent-lens config schema` — the full `agent-lens.toml` reference.

Both are generated from the code. This README is a narrative over them, not a
substitute for them.

## Why

Coding agents make decisions on partial context. They can read the file
they're editing, but they don't see the near-duplicate function three modules
over, the `impl` block whose methods touch disjoint sets of fields, the module
that's a Fan-In bottleneck, or the function whose Cognitive Complexity is 40
and is a landmine to refactor.

`agent-lens` produces small, structured reports — JSON by default, compact
Markdown on demand — that fit in a context window and surface that information
the moment the agent needs it.

## Install

### One-liner (Linux x86_64 / arm64, glibc or musl; macOS arm64 / x86_64)

```bash
curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh | bash
```

This pulls the matching tarball from the latest stable GitHub Release,
verifies its SHA-256 (verification fails closed; pass `--no-verify` to skip
deliberately), and drops the binary into `$HOME/.local/bin`. Pass
`--dir <path>` for another destination, `AGENT_LENS_TAG=<tag>` to pin a
release, or `--help` for every flag and environment variable.

### Via mise (GitHub backend)

[mise](https://mise.jdx.dev/) installs directly from GitHub Releases — no Rust
toolchain required, version pinned per project:

```bash
mise use -g github:illumination-k/agent-lens          # user-global
mise use github:illumination-k/agent-lens@v0.1.0      # project-local, pinned

# rolling prerelease built from main (mise excludes prereleases by default)
mise use 'github:illumination-k/agent-lens@rolling[prerelease=true]'
```

### Nix (flake)

The repo is a flake; `agent-lens` builds from source, pinned by `flake.lock`:

```bash
nix run github:illumination-k/agent-lens -- --version   # run without installing
nix profile install github:illumination-k/agent-lens    # install into profile
```

Consume it from another flake via `packages.default` or `overlays.default`
(which provides `pkgs.agent-lens`). Linux and macOS, x86_64 and aarch64, are
wired up; only `x86_64-linux` is exercised in CI.

### From source

Requires rustc 1.85+ (the workspace is on `edition = "2024"`):

```bash
cargo install --path crates/agent-lens
```

### Manual download

Pre-built binaries are on the
[GitHub Releases page](https://github.com/illumination-k/agent-lens/releases):
normal releases for version tags, plus a rolling prerelease named `rolling`
built from `main`.

## Quick start

### As an analyzer

Stdin is not used; pass one or more paths and pick an output format. A few
representative invocations:

```bash
# Near-duplicate functions (TSED >= 0.85), as JSON or a compact summary
agent-lens analyze similarity crates/lens-rust/src
agent-lens analyze similarity crates/lens-rust/src --format md --top 10 --min-score 0.9

# Duplicated type definitions, or copy-pasted fragments inside function bodies
agent-lens analyze similarity crates --format md --target types
agent-lens analyze similarity crates --format md --target blocks

# Parallel implementations that have drifted apart (matched by name, scored second)
agent-lens analyze similarity . --format md --paired-by name

# Rank functions by relevance to a query — for when you can describe the thing
# but don't know its name
agent-lens analyze search crates/ --query 'diff range gate' --format md

# Complexity, cohesion (LCOM4), forwarding wrappers, delegation chains
agent-lens analyze complexity src/foo.rs --format md --top 20 --min-score 8
agent-lens analyze cohesion src/foo.rs --format md --min-score 2
agent-lens analyze wrapper src/foo.rs
agent-lens analyze delegation crates/agent-lens --format md

# Module-level structure: coupling, detected vs declared boundaries, layers
agent-lens analyze coupling crates/agent-lens --format md --top 15
agent-lens analyze communities crates/agent-lens --format md
agent-lens analyze layers crates/agent-lens --format md

# Git history: hotspots, blast-radius risk, co-change, change scatter
agent-lens analyze hotspot . --since 90.days.ago --top 20
agent-lens analyze risk . --since 90.days.ago --top 20
agent-lens analyze co-change . --since 180.days.ago --min-support 5
agent-lens analyze hidden-coupling . --min-support 5 --format md

# Scope any report to pending work: --diff-only gates on the unstaged
# working-tree diff, --diff-range on an explicit range
agent-lens analyze similarity src/foo.rs --diff-only
agent-lens analyze complexity src/foo.rs --diff-range main...HEAD

# Path filters work everywhere; several trees form one corpus, so a duplicate
# or call edge spanning them is visible where per-tree runs would miss it
agent-lens analyze similarity packages cli web/src --format md --exclude-tests
agent-lens analyze hotspot . --exclude 'target/**' --exclude '**/generated/**'
```

Conventions that hold across analyzers:

- Analyzer commands share `PATH...`, `--format json|md`, `--only-tests`,
  `--exclude-tests`, and repeatable `--exclude GLOB`. Directory walks follow
  `.gitignore`.
- `--top N` caps the Markdown ranking; JSON always carries the full result.
  `cycles` rejects it — a truncated cycle list reads as the whole list.
- Every analyzer takes multiple `PATH`s except `coupling`, `context-span`, and
  `communities`, which grow one module graph from one entry point.
- `--diff-only` and `--diff-range` conflict (they name different diffs). On
  `impact` the diff is the seed rather than a gate, so only `--diff-range`
  applies.

`search` complements `grep` rather than replacing it: reach for `search` when
you can only describe the thing, and switch to `grep` once you have a name.
Its retrieval unit is a function, so macro bodies, `const` items, and files no
adapter parses are invisible — and scores are absolute, so a list of weak
matches looks like a list of strong ones. Two to four content words beat a
full sentence.

Per-analyzer options (thresholds, sweep, `--since`, graph queries, …) are in
`agent-lens help --md`.

### As a profile runner

For a repeatable multi-analyzer pass, declare a named profile in
`agent-lens.toml` and run it with `agent-lens run <name>`:

```toml
# agent-lens.toml — discovered by walking up from the current directory
[profile.web]
path = "web/" # target handed to every tool; also takes an array
format = "md" # json (default) or md
exclude-tests = true # or only-tests, plus extra `exclude` globs
tools = ["similarity", "complexity", "cohesion"]

# Per-tool overrides mirror the matching CLI flags
[profile.web.similarity]
threshold = 0.9
min-lines = 8
```

```bash
agent-lens run web                       # from the nearest agent-lens.toml
agent-lens run web --config path/to/agent-lens.toml
agent-lens run web --format json         # override the profile's format
```

Keys are kebab-case and match the CLI flags. Unknown keys — a typo, or an
option set on the wrong tool — are rejected at parse time rather than silently
ignored. `agent-lens config schema` prints the full reference: every profile
key and per-tool table, with types, defaults, and a worked example.

### As a baseline

An analyzer report says what is wrong now. A **baseline** says whether it got
worse: `baseline create` reduces a profile's reports to named metrics, and
`baseline compare` re-runs the profile against the stored snapshot — exiting
**2** when a gated metric regressed, distinct from the **1** of a failed run,
so CI can tell "the code got worse" from "the tool broke".

```bash
agent-lens baseline create baseline --out target/agent-lens/baseline.json
agent-lens baseline compare baseline target/agent-lens/baseline.json
agent-lens baseline compare baseline baseline.json --update   # ratchet
```

Each metric gates in its own direction: maxima like `cognitive_max` or
`lcom4_max` may only fall, `maintainability_index_min` may only rise, and
context metrics (surface size like `file_count`, git-accumulating numbers like
`commits_max`) are reported but never gated — a check that fails on every new
commit is not a check. Snapshots are deterministic (nothing reads the clock)
and honest about gaps (an unmeasured metric is omitted, never written as `0`).

`--update` turns one way only: a gated metric takes the better of the two
values — re-running after a regression cannot launder it into the new bar, and
the exit status stays 2. Analyzers a snapshot cannot yet cover are listed
under `skipped` with a reason rather than silently dropped.

### As a Claude Code hook

Wire `agent-lens` into Claude Code at three event points: a one-shot
`SessionStart` summary of the repo's hotspots, a `PreToolUse` heads-up about
complex / low-cohesion code the agent is about to edit, and a `PostToolUse`
follow-up flagging duplicated or forwarding-only functions in the file just
changed.

```bash
agent-lens hook setup                 # project scope: ./.claude/settings.json
agent-lens hook setup --scope user    # user scope: $HOME/.claude/settings.json
agent-lens hook setup --dry-run       # preview the exact block without writing
```

The merge is conservative: existing entries are preserved, missing handlers
are appended, and re-running is a no-op once everything is installed.

### As a Codex hook

Codex's hook protocol differs (every payload carries a `model` slug,
`apply_patch` can touch multiple files at once), so `agent-lens` ships a
separate `codex-hook` command tree with the same handlers:

```bash
agent-lens codex-hook setup                    # user scope: $HOME/.codex/config.toml
agent-lens codex-hook setup --scope project    # <repo-root>/.codex/config.toml
agent-lens codex-hook setup --dry-run          # preview without writing
```

The same conservative merge applies: existing keys and comments are preserved
and re-running is a no-op.

### Command surface

| Command tree | Commands                                                                                                                                                                                                                                                                                                  |
| ------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `hook`       | `setup`, `session-start summary`, `pre-tool-use complexity`, `pre-tool-use cohesion`, `post-tool-use similarity`, `post-tool-use wrapper`                                                                                                                                                                 |
| `codex-hook` | same handlers as `hook`, speaking Codex's protocol                                                                                                                                                                                                                                                        |
| `analyze`    | `search`, `similarity`, `wrapper`, `delegation`, `cohesion`, `complexity`, `coupling`, `communities`, `cycles`, `function-graph`, `graph-query`, `hubs`, `impact`, `layers`, `unreachable`, `untested`, `visibility`, `context-span`, `hotspot`, `risk`, `co-change`, `change-entropy`, `hidden-coupling` |
| `run`        | `run <profile>` — execute every analyzer in a named `agent-lens.toml` profile                                                                                                                                                                                                                             |
| `baseline`   | `create <profile>` — snapshot a profile's metrics; `compare <profile> <SNAPSHOT> [--update]` — gate a fresh run against one                                                                                                                                                                               |
| `skills`     | `list`, `install` — the bundled Claude Code skills                                                                                                                                                                                                                                                        |
| `config`     | `schema` — print the `agent-lens.toml` reference                                                                                                                                                                                                                                                          |
| `help`       | `help [--md]` — print the command reference, optionally as one Markdown document                                                                                                                                                                                                                          |

`agent-lens --help` opens with a question-to-analyzer routing table ("what
breaks if I change this?" → `analyze impact`), and each subcommand's `--help`
ends with worked invocations.

`agent-lens skills install` drops the same skills this repo dogfoods (under
`.claude/skills/`) into a target project — `--scope user` for
`$HOME/.claude/skills`, `--dry-run` to preview, `--force` to overwrite a
conflicting local edit (otherwise conflicts are reported and left untouched).

## What's in the box

### Hook handlers

| Event          | Handler      | What it does                                                                         |
| -------------- | ------------ | ------------------------------------------------------------------------------------ |
| `SessionStart` | `summary`    | Injects a one-shot hotspot + coupling thumbnail into the new session.                |
| `PreToolUse`   | `complexity` | Flags functions in the file about to be edited whose complexity crosses a threshold. |
| `PreToolUse`   | `cohesion`   | Flags cohesion units in the file about to be edited whose LCOM4 is greater than 1.   |
| `PostToolUse`  | `similarity` | Reports near-duplicate function pairs in the file just edited.                       |
| `PostToolUse`  | `wrapper`    | Reports thin forwarding functions in the file just edited.                           |

The `codex-hook` tree ships the same five handlers; the `PreToolUse` /
`PostToolUse` ones run across every file the `apply_patch` touches. Schemas
for the remaining events (`UserPromptSubmit`, `Stop`, `SubagentStop`, Codex's
`PermissionRequest`) live in the `agent-hooks` crate with no handler wired
yet, so a new handler is a domain-logic change rather than a schema change.

Hook handlers are advisory — never a gate on the agent's tool call. A handler
that fails still answers in the agent's response schema (prefixed with
`agent-lens <event> hook failed:`) and exits 0 so the agent parses it; the
full error goes to stderr. The `setup` commands are not hooks and keep the
ordinary CLI contract: errors exit non-zero.

### Analyzers

| Subcommand        | What it surfaces                                                                                                                                                                                                                                          |
| ----------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `search`          | Functions ranked by BM25F relevance to a query over weighted fields (name, path, signature, doc, body), with identifier-aware tokenization and trigram expansion for near-miss terms. `--rank graph` folds in call-graph importance.                      |
| `similarity`      | Near-duplicate pairs (normalised-AST TSED via APTED, complete-link clusters). `--target` picks functions (default), type definitions compared on member shape, or statement blocks inside bodies — the copy-paste whole-definition comparison cannot see. |
| `wrapper`         | Functions whose body is a forwarding call modulo `?`, `.unwrap()`, `.into()`, `.await`, …; Go interface-satisfying wrappers are annotated since deleting them isn't the fix.                                                                              |
| `delegation`      | Chains of forwarding-only functions (`api::save -> service::save -> repo::save`), with the terminus doing the work as the headline and a per-module delegator roll-up. Language-mandated hops are marked.                                                 |
| `cohesion`        | LCOM4 per `impl` block, class, or module unit (connected components of the field-sharing graph).                                                                                                                                                          |
| `complexity`      | Per-function Cyclomatic, Cognitive, Max Nesting Depth, Halstead Volume, and Maintainability Index.                                                                                                                                                        |
| `coupling`        | Module-level Fan-In / Fan-Out, Henry-Kafura IFC, Instability, shared-symbol counts, and cyclic SCCs.                                                                                                                                                      |
| `communities`     | The module clusters the dependency graph actually forms, scored (Newman modularity) against the declared boundaries — naming misfiled members and clusters no declared module owns.                                                                       |
| `function-graph`  | Static function nodes and heuristic caller→callee edges as visualization-ready JSON, with structural and complexity weights per node.                                                                                                                     |
| `cycles`          | Function-level SCCs of the call graph: recursion knots and cross-file tangles, with advisory cheapest-cut break suggestions and call-line evidence.                                                                                                       |
| `hubs`            | Call-graph hub smells: outlier fan-out (god functions), outlier fan-in (blast radius), Henry-Kafura bottlenecks, cross-module pull, with PageRank-importance percentiles.                                                                                 |
| `layers`          | Inferred Lakos levelization: function and module levels, entry points, module cycles, and skip-level calls with call-site evidence.                                                                                                                       |
| `untested`        | Production functions with no resolved call path from any test, grouped by module and ranked by untested LOC.                                                                                                                                              |
| `unreachable`     | Functions no entry point reaches, in confidence tiers (`confirmed` / `likely` / `unknown`); sound in the "confirmed ⇒ really dead" direction only, with deletable islands and a deletion order.                                                           |
| `visibility`      | `pub` / exported functions whose callers all sit inside a narrower scope, with the declaration that would still compile and the caller evidence.                                                                                                          |
| `impact`          | Blast radius of a change: transitive callers of the seeds (working-tree diff or `--function`), plus reachable tests as a verification checklist.                                                                                                          |
| `graph-query`     | One canned call-graph traversal per run: `callers`, `callees`, `neighborhood`, or the shortest `path` between two symbols.                                                                                                                                |
| `context-span`    | Per-module transitive dependency closure: how many files an agent must read to reason about a module.                                                                                                                                                     |
| `hotspot`         | Files ranked by `commits × cognitive_max` over an optional `--since` window — where churn and complexity overlap.                                                                                                                                         |
| `risk`            | Files ranked by the rank product of git churn and call-graph centrality — separating "hot but leaf" from "hot and load-bearing".                                                                                                                          |
| `change-entropy`  | How scattered change activity was per period (Hassan's history complexity), attributed onto files; `--diff-only` scores the pending change against the repo's own commit distribution.                                                                    |
| `co-change`       | File pairs git history says change together: support, per-direction confidence, and lift, with renames followed. Correlation only.                                                                                                                        |
| `hidden-coupling` | The differential between history and the static graph: co-changing pairs with no declared dependency (undeclared contracts), and declared dependencies history never exercised — reported as separate buckets.                                            |

All analyzers default to JSON on stdout; `--format md` emits a compact
Markdown summary tuned to drop straight into an LLM prompt.

## Languages

Analysis is split into a language-neutral core and per-language adapters.
Adding a language means writing one adapter crate and wiring it into the
`SourceLang` match — the metric implementations are shared.

| Language                | Parser                                                        | Adapter crate |
| ----------------------- | ------------------------------------------------------------- | ------------- |
| Rust                    | [`syn`](https://docs.rs/syn)                                  | `lens-rust`   |
| TypeScript / JavaScript | [oxc](https://oxc.rs/) (`oxc_parser`, `oxc_ast`)              | `lens-ts`     |
| Python                  | [`ruff_python_parser`](https://docs.rs/ruff_python_parser)    | `lens-py`     |
| Go                      | [tree-sitter](https://docs.rs/tree-sitter) + `tree-sitter-go` | `lens-golang` |

Supported source extensions: `.rs`; `.ts`, `.tsx`, `.mts`, `.cts`, `.js`,
`.jsx`, `.mjs`, `.cjs`; `.py`; `.go`.

Language coverage per analyzer:

- Most analyzers cover all four language families.
- `visibility` and `unreachable` judge Rust and Go only — the two adapters
  that extract export status. `delegation` runs everywhere but can only apply
  its module-facade exemption where export status exists.
- `co-change` and `change-entropy` are language-agnostic: they read `git log`
  and never parse a file, so `.toml`, `.md`, and CI config are covered too.
- `hotspot`, `risk`, `co-change`, `change-entropy`, and `hidden-coupling`
  require a git working tree.

`coupling`, `context-span`, and `communities` name modules the way the
analyzed language does (`crate::analyze::coupling`, `github.com/x/proj/internal/store`,
`components/Chat`, `util.text`); TS/JS and Python modules are one per file.

In TypeScript / JavaScript, callbacks registered with a recognised test
harness (`describe`, `it`, `test`, hooks, `it.skip`-style chains) are units
named after the callee and its literal title — so a vitest / jest suite is not
an empty module, which matters most to `untested`.

## Workspace layout

```
crates/
├── agent-lens/    # the CLI binary: clap dispatch, hook handlers, analyzers,
│                  # call-graph passes, profile runner, baselines, skills
├── agent-hooks/   # Claude Code & Codex hook protocol schemas + Hook trait
├── lens-domain/   # language-neutral primitives and metric machinery
│                  # (TreeNode, APTED, TSED, LCOM, IFC, Maintainability Index)
├── lens-rust/     # syn-based Rust adapter
├── lens-ts/       # oxc-based TypeScript / JavaScript adapter
├── lens-py/       # ruff_python_parser-based Python adapter
└── lens-golang/   # tree-sitter-based Go adapter
```

Adapters translate a language's AST into the neutral primitives and nothing
else; `agent-lens` owns everything that needs a whole corpus rather than one
AST (the call graph, git-churn joins, report rendering, the CLI itself).

`web/` is a separate pnpm workspace: the TanStack Start site deployed to
GitHub Pages, holding the landing page and the static function-graph viewer.

## Output discipline

- **stdout** is reserved for protocol JSON or analyzer reports.
- **stderr** is for diagnostics, via [`tracing`](https://docs.rs/tracing);
  control verbosity with `RUST_LOG`.
- Direct `println!` / `eprintln!`, `unwrap()`, and `expect()` are clippy
  `deny`, so a renegade `dbg!` can't pollute a hook response.

## Development

All tools are pinned by [mise](https://mise.jdx.dev/):

```bash
mise install      # one-shot setup

mise run fmt      # format everything (cargo fmt, dprint, shfmt, oxfmt)
mise run lint     # clippy, cargo-deny/audit, prek, shell + GHA lints
mise run test     # cargo nextest + doctests + vitest
mise run ci       # the full lint + test pipeline CI runs
mise run bench    # Criterion benchmarks (not in CI)
mise run mutants  # full-workspace cargo-mutants (slow; not in normal CI)
mise run mutants:rust:diff [base]  # diff-scoped mutation tests (what CI runs on a PR)
mise run selftest # run agent-lens over its own sources (dogfooding)
```

A green `mise run ci` means a green PR — it covers every required GitHub
check, and the web tasks install `web/node_modules` themselves.

On NixOS — or anywhere mise's pre-built binaries won't run — `nix develop`
provides the same toolchain from nixpkgs; run the underlying commands directly
(`cargo clippy …`, `cargo nextest run …`, `dprint check`) rather than
`mise run`. `nix build` builds the CLI and `nix flake check` evaluates every
flake output. mise and `ci_rust.yml` remain the source of truth for exact tool
versions.

Testing conventions: prefer [`rstest`](https://docs.rs/rstest) for
parameterized cases and fixtures, property-based tests where regression risk
is high, and diff-scoped mutation testing (`mise run mutants:rust:diff`) for
Rust logic changes. Benchmarked code uses Criterion baselines:

```bash
git stash && mise run bench:rust --save-baseline base && git stash pop
mise run bench:rust --baseline base   # reports % change vs the saved run
```

### Dogfooding

`agent-lens` is its own first user. The repository's `agent-lens.toml`
declares one profile per view of this codebase, and `mise run selftest` builds
the binary and drives every one of them (`mise run selftest <profile>` for
one):

| profile      | what it looks at                                                          |
| ------------ | ------------------------------------------------------------------------- |
| `self`       | the `agent-lens` crate, tests excluded — the product-side refactor audit  |
| `self-reach` | the workspace with tests kept — untested, unreachable, over-exposed code  |
| `self-tests` | the same crate, tests only — copy-pasted fixtures, dead helpers           |
| `lenses`     | the four language front-ends, where a fix applied to one can miss three   |
| `web`        | the TypeScript viewer, the only end-to-end run of the TS front-end        |
| `changes`    | every tool in `--diff-only` mode: a pre-commit review of the working tree |
| `history`    | git history repo-wide: which files change together, and how scattered     |
| `baseline`   | the metric snapshot `baseline create` reduces to numbers                  |

This is not a test — there is nothing to assert on a report an agent reads. It
is the cheapest check for the failure mode unit tests cannot see: an analyzer
that still returns `Ok` while saying something absurd about code the
maintainers know by heart. Run it after touching an analyzer and read the diff
against the previous run, not the report in isolation. Before a commit,
`mise run selftest changes` is the fast one: an empty report means the pending
edit introduced no duplicate, no forwarding-only wrapper, and no complexity
spike.

### CI

`.github/workflows/` runs Rust and TypeScript lint/test, base and GHA lints,
CodeQL, dependency review, Trivy, TruffleHog, SBOM generation, the flake
build, and PR-diff mutation testing. Releases share one reusable workflow
behind two triggers: the rolling prerelease from `main`, and `v*` tags, which
also publish build-provenance attestations.

## Design principles

- **Signal density over decoration.** Reports go to LLMs. Color, ASCII art,
  emoji, and human-only flourishes don't earn their tokens.
- **One binary, many surfaces.** Hooks and analyzers ship together so the
  install + config story stays simple.
- **Schema in one place.** Hook protocol types live in `agent-hooks`, so a
  spec change is a one-crate update.
- **Fail loudly.** Missing required fields error out non-zero. Unknown fields
  are tolerated so upstream additions don't break existing handlers.

## Roadmap

Keep improving the analyzer surfaces that help agents make better edit
decisions, prioritised by _does this change how an agent decides what to do?_
rather than _does it look nice in a dashboard?_ Turning analyzers into checks
is the other half — `baseline create` / `baseline compare --update` cover it
today; which analyzers a snapshot can cover is still open. An MCP server
front-end is a likely next surface, but the CLI is the source of truth.

## License

MIT. See [`LICENSE`](./LICENSE).
