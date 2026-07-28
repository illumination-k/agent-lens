# agent-lens

[![CI Rust](https://github.com/illumination-k/agent-lens/actions/workflows/ci_rust.yml/badge.svg)](https://github.com/illumination-k/agent-lens/actions/workflows/ci_rust.yml)
[![Lint Base](https://github.com/illumination-k/agent-lens/actions/workflows/lint_base.yml/badge.svg)](https://github.com/illumination-k/agent-lens/actions/workflows/lint_base.yml)
[![Lint GHA](https://github.com/illumination-k/agent-lens/actions/workflows/lint_gha.yml/badge.svg)](https://github.com/illumination-k/agent-lens/actions/workflows/lint_gha.yml)
[![Mutation Tests](https://github.com/illumination-k/agent-lens/actions/workflows/mutants.yml/badge.svg)](https://github.com/illumination-k/agent-lens/actions/workflows/mutants.yml)
[![codecov](https://codecov.io/gh/illumination-k/agent-lens/branch/main/graph/badge.svg)](https://codecov.io/gh/illumination-k/agent-lens)

> A sharper view of your codebase, tuned for the agent that's about to edit it.

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

## Why

Coding agents make decisions on partial context. They can read the file
they're editing, but they don't see:

- the near-duplicate function three modules over that they're about to fork,
- the `impl` block whose methods touch disjoint sets of fields and should be
  split,
- the module that's a Fan-In bottleneck and shouldn't grow any more,
- the function whose Cognitive Complexity is 40 and is a landmine to refactor.

`agent-lens` produces small, structured reports — JSON by default, compact
Markdown on demand — that fit in a context window and surface that information
the moment the agent needs it.

The "agent-friendly" stance is enforced in code: `println!`, `eprintln!`,
`unwrap()`, and `expect()` are all `deny`'d via clippy. Stdout is reserved for
protocol payloads and reports; everything else goes to stderr through
`tracing`.

## Install

### One-liner (Linux x86_64 / arm64, glibc or musl; macOS arm64 / x86_64)

```bash
curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh | bash
```

This pulls the matching tarball from the latest stable GitHub Release, verifies
its SHA-256, and drops the binary into `$HOME/.local/bin`. Verification is
mandatory and fails closed: if the release publishes no `.sha256` asset, or the
machine has neither `sha256sum` nor `shasum`, the install aborts instead of
proceeding unverified. Pass `--no-verify` (or `AGENT_LENS_NO_VERIFY=1`) to
install without verification deliberately.

Override with flags or environment variables — run `--help` for the full list:

```bash
# explicit destination
curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh \
  | bash -s -- --dir /usr/local/bin

# pin a specific release tag, or use rolling for the rolling prerelease
AGENT_LENS_TAG=v0.1.0 AGENT_LENS_DIR="$HOME/.local/bin" \
  bash <(curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh)

# list every flag and environment variable
curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh \
  | bash -s -- --help
```

### Via mise (GitHub backend)

[mise](https://mise.jdx.dev/) can install directly from GitHub Releases — no
Rust toolchain required, and the version is pinned per project:

```bash
# user-global
mise use -g github:illumination-k/agent-lens

# project-local (writes mise.toml in the repo root)
mise use github:illumination-k/agent-lens

# pin a specific release tag
mise use github:illumination-k/agent-lens@v0.1.0

# opt into the rolling prerelease built from main
mise use 'github:illumination-k/agent-lens@rolling[prerelease=true]'
```

Or add it to `mise.toml` directly:

```toml
[tools]
"github:illumination-k/agent-lens" = "v0.1.0"
```

To track the rolling prerelease built from `main`, opt into prereleases and pin
`rolling` instead:

```toml
[tools]
"github:illumination-k/agent-lens" = { version = "rolling", prerelease = true }
```

The `prerelease = true` option is required because mise's GitHub backend
excludes GitHub prereleases by default.

mise auto-detects the right asset for your OS / arch from the
`agent-lens-<target>.tar.gz` artifacts published by the release workflow.

### Nix (flake)

The repo is a flake, so no release artifact is involved — `agent-lens` is
built from source and pinned by `flake.lock`:

```bash
# run without installing
nix run github:illumination-k/agent-lens -- --version

# install into your profile
nix profile install github:illumination-k/agent-lens

# pin a tag or commit
nix profile install github:illumination-k/agent-lens/v0.1.0
```

To consume it from another flake, either take `packages.default` directly or
apply `overlays.default` to get `pkgs.agent-lens`:

```nix
{
  inputs.agent-lens.url = "github:illumination-k/agent-lens";

  outputs = { nixpkgs, agent-lens, ... }: {
    # ...
    environment.systemPackages = [ agent-lens.packages.x86_64-linux.default ];
  };
}
```

Linux and macOS, x86_64 and aarch64, are wired up; only `x86_64-linux` is
exercised in CI.

### From source

Requires a recent Rust toolchain (the workspace is on `edition = "2024"`, so
rustc 1.85+):

```bash
cargo install --path crates/agent-lens
```

### Manual download

Pre-built binaries are published for version tags as normal releases. The
current `main` branch is also published as a rolling prerelease named `rolling`.
Grab a tarball or `.zip` directly from the
[GitHub Releases page](https://github.com/illumination-k/agent-lens/releases)
or pin `rolling` when you explicitly want the prerelease build.

## Quick start

### As an analyzer

Stdin is not used; pass a path and pick an output format.

```bash
# Find near-duplicate functions in a file or directory (TSED >= 0.85)
agent-lens analyze similarity src/foo.rs
agent-lens analyze similarity crates/lens-rust/src

# Same, but emit a compact summary instead of the full JSON
agent-lens analyze similarity src/foo.rs --format md --top 10 --min-score 0.9

# Score with token k-gram overlap instead of TSED tree-edit distance:
# faster and more tolerant of reordered code, but less precise
agent-lens analyze similarity crates/lens-rust/src --method token

# Sweep several thresholds in one run: cluster at the lowest rung and tag
# each cluster with the highest rung it survives — verbatim clones vs.
# merely structural parallels, without re-running at 0.85 / 0.75 / 0.6
agent-lens analyze similarity crates/lens-rust/src --format md --sweep 0.6,0.75,0.85

# Roll the doc-comment overlap into the markdown report. Diagnostic only —
# it never feeds the score — but it separates "same stated intent" clones
# from functions that merely share a shape. JSON carries it per pair either way
agent-lens analyze similarity crates/lens-rust/src --format md --doc-overlap

# All analyzers accept path filters: focus tests, drop tests, or exclude globs
agent-lens analyze complexity crates/agent-lens --only-tests --format md --top 20 --min-score 8
agent-lens analyze similarity crates/lens-rust/src --exclude-tests --min-lines 6
agent-lens analyze hotspot . --exclude 'target/**' --exclude '**/generated/**'

# Analyze only functions touching unstaged diff hunks for this file
agent-lens analyze similarity src/foo.rs --diff-only

# Cohesion (LCOM4) per impl block / class / module unit
agent-lens analyze cohesion src/foo.rs --format md --top 20 --min-score 2

# Cohesion only for units overlapping `git diff -U0` hunks
agent-lens analyze cohesion src/foo.rs --diff-only

# Cyclomatic / Cognitive / Nesting / Halstead / Maintainability Index
agent-lens analyze complexity src/foo.rs

# Complexity only for functions overlapping `git diff -U0` hunks
agent-lens analyze complexity src/foo.rs --diff-only

# Module-level Fan-In / Fan-Out / Henry-Kafura IFC, Instability, and
# cyclic SCCs for a Rust crate, TS/JS module graph, Go module, or
# Python package tree
agent-lens analyze coupling crates/agent-lens

# Static function call graph for visualization tooling
agent-lens analyze function-graph crates/agent-lens

# Function-level call cycles (SCC tangles) with advisory cheapest-cut
# suggestions for breaking each one
agent-lens analyze cycles crates/agent-lens

# Per-module transitive dependency closure ("how many files do I need
# to read to understand this module?")
agent-lens analyze context-span crates/agent-lens

# Hotspots: rank files by `commits × cognitive_max` (must be in a git tree)
agent-lens analyze hotspot crates/agent-lens --since 90.days.ago --top 20

# Forwarding wrappers (functions that are just `other(args).into()?` etc.)
agent-lens analyze wrapper src/foo.rs

# Wrapper findings limited to functions overlapping `git diff -U0` hunks
agent-lens analyze wrapper src/foo.rs --diff-only
```

### As a profile runner

For a repeatable multi-analyzer pass, declare a named profile in an
`agent-lens.toml` and run it with `agent-lens run <name>`. A profile bundles a
target path, shared path filters, an ordered list of analyzers, and optional
per-tool overrides; `run` executes each analyzer through the same code path as
`agent-lens analyze` and emits one combined report.

```toml
# agent-lens.toml — discovered by walking up from the current directory
[profile.web]
path = "web/" # target handed to every tool
format = "md" # json (default) or md
exclude = ["tests/**/*.ts"] # extra --exclude globs
exclude-tests = true # or only-tests
tools = ["similarity", "complexity", "cohesion"] # analyzers to run, in order

# Per-tool overrides live in [profile.<name>.<tool>] sub-tables and mirror the
# matching CLI flags. Tables are optional; omitted options use the analyzer's
# CLI default. `coupling`, `cycles`, and `function-graph` take no table —
# they have no extra options.
[profile.web.similarity]
threshold = 0.9
min-lines = 8
method = "tsed" # or "token"
top = 20

[profile.web.complexity]
min-score = 12
top = 20
```

```bash
# Run the `web` profile from the nearest agent-lens.toml
agent-lens run web

# Point at an explicit config file
agent-lens run web --config path/to/agent-lens.toml
```

Keys are kebab-case and match the CLI flags. A relative `path` resolves against
the directory holding `agent-lens.toml`. Unknown keys — a typo like `entrypont`,
or an option set on the wrong tool — are rejected at parse time rather than
silently ignored.

`agent-lens config schema` prints the full `agent-lens.toml` reference as dense
Markdown — every `[profile.<name>]` key and per-tool override table, with types,
defaults, and a worked example — so an agent can author or audit a config
without reading the source.

### Current command surface

The current binary exposes three top-level command trees plus `run`,
`skills`, `config`, and `help`:

| Command tree | Commands                                                                                                                                                                                  |
| ------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `hook`       | `setup`, `session-start summary`, `pre-tool-use complexity`, `pre-tool-use cohesion`, `post-tool-use similarity`, `post-tool-use wrapper`                                                 |
| `codex-hook` | `setup`, `session-start summary`, `pre-tool-use complexity`, `pre-tool-use cohesion`, `post-tool-use similarity`, `post-tool-use wrapper`                                                 |
| `analyze`    | `similarity`, `wrapper`, `cohesion`, `complexity`, `coupling`, `cycles`, `function-graph`, `graph-query`, `hubs`, `impact`, `layers`, `untested`, `visibility`, `context-span`, `hotspot` |
| `run`        | `run <profile>` — execute every analyzer in a named `agent-lens.toml` profile                                                                                                             |
| `skills`     | `list`, `install` — list and install the bundled Claude Code skills                                                                                                                       |
| `config`     | `schema` — print the `agent-lens.toml` schema as agent-friendly Markdown                                                                                                                  |
| `help`       | `help [--md]` — print the command reference, optionally as agent-friendly Markdown                                                                                                        |

`agent-lens --help` opens with a question-to-analyzer routing table
("what breaks if I change this?" → `analyze impact`) plus the output
conventions that hold for every analyzer, and each subcommand's
`--help` ends with worked invocations rather than prose alone.

`agent-lens help --md` prints the entire command surface — a flat
command index, then every subcommand with its description, options, and
examples — as one dense Markdown document, so an agent can read the
whole CLI in a single shot instead of running `--help` on each branch.

`agent-lens skills install` drops the same skills this repo dogfoods
(under `.claude/skills/`) into a target project so a fresh checkout gets
`agent-lens`-aware routing:

```bash
# Project scope: ./.claude/skills (created if missing)
agent-lens skills install

# User scope: $HOME/.claude/skills
agent-lens skills install --scope user

# Preview, then overwrite local edits
agent-lens skills install --dry-run
agent-lens skills install --force
```

Install is conservative and idempotent: a skill that already exists with
different content is reported as a conflict and left untouched unless
`--force` is passed. `agent-lens skills list` summarises what ships in the
binary.

Analyzer commands share `PATH`, `--format json|md`, `--only-tests`,
`--exclude-tests`, and repeatable `--exclude GLOB`. Directory analyzers walk
recursively with `.gitignore` semantics.

Analyzer-specific options today:

| Analyzer         | Extra options                                                                                                                                                   |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `similarity`     | `--threshold FLOAT` (alias: `--min-score`), `--sweep F1,F2,…` (conflicts with `--threshold`), `--min-lines N`, `--method tsed\|token`, `--diff-only`, `--top N` |
| `complexity`     | `--diff-only`, `--top N`, `--min-score N`                                                                                                                       |
| `cohesion`       | `--diff-only`, `--top N`, `--min-score N`                                                                                                                       |
| `wrapper`        | `--diff-only`                                                                                                                                                   |
| `hotspot`        | `--since VALUE`, `--top N`                                                                                                                                      |
| `hubs`           | `--top N`                                                                                                                                                       |
| `impact`         | `--function SYMBOL` (repeatable), `--depth N`, `--top N`                                                                                                        |
| `layers`         | `--top N`                                                                                                                                                       |
| `untested`       | `--top N`                                                                                                                                                       |
| `visibility`     | `--top N`                                                                                                                                                       |
| `graph-query`    | `--query callers\|callees\|neighborhood\|path`, `--symbol SYMBOL`, `--to SYMBOL`, `--depth N`, `--direction in\|out\|both`, `--limit N`                         |
| `coupling`       | shared analyzer options only                                                                                                                                    |
| `cycles`         | shared analyzer options only                                                                                                                                    |
| `function-graph` | shared analyzer options only                                                                                                                                    |
| `context-span`   | shared analyzer options only                                                                                                                                    |

Supported source extensions are `.rs`; `.ts`, `.tsx`, `.mts`, `.cts`, `.js`,
`.jsx`, `.mjs`, `.cjs`; `.py`; and `.go`. `similarity`, `complexity`,
`wrapper`, `cohesion`, `hotspot`, `function-graph`, `cycles`, `graph-query`,
`hubs`, `impact`, `layers`, `untested`, `context-span`, and `coupling`
cover all four language families. `visibility` judges Rust and Go, the two
adapters that extract export status, and counts the rest as skipped.

### As a Claude Code hook

Wire `agent-lens` into Claude Code at three event points: a one-shot
`SessionStart` summary of the repo's hotspots, a `PreToolUse` heads-up about
complex / low-cohesion code the agent is about to edit, and a `PostToolUse`
follow-up that flags duplicated or forwarding-only functions in the file the
agent just changed.

The fastest way is to let `agent-lens` write the `settings.json` block for you:

```bash
# Project scope: ./.claude/settings.json (created if missing)
agent-lens hook setup

# User scope: $HOME/.claude/settings.json
agent-lens hook setup --scope user

# Preview without writing
agent-lens hook setup --dry-run
```

The merge is conservative: existing entries are preserved, and `SessionStart`
/ `PreToolUse` / `PostToolUse` blocks are appended only with the commands
that aren't already wired up. Re-running is a no-op once every handler is
installed.

If you'd rather edit the file by hand, the equivalent block looks like:

```jsonc
// ~/.claude/settings.json (or .claude/settings.json in a project)
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "agent-lens hook session-start summary",
          },
        ],
      },
    ],
    "PreToolUse": [
      {
        "matcher": "Edit|Write|MultiEdit",
        "hooks": [
          {
            "type": "command",
            "command": "agent-lens hook pre-tool-use complexity",
          },
          {
            "type": "command",
            "command": "agent-lens hook pre-tool-use cohesion",
          },
        ],
      },
    ],
    "PostToolUse": [
      {
        "matcher": "Edit|Write|MultiEdit",
        "hooks": [
          {
            "type": "command",
            "command": "agent-lens hook post-tool-use similarity",
          },
          {
            "type": "command",
            "command": "agent-lens hook post-tool-use wrapper",
          },
        ],
      },
    ],
  },
}
```

### As a Codex hook

Codex's hook protocol differs from Claude Code's (every payload carries a
`model` slug, `apply_patch` can touch multiple files at once, etc.).
`agent-lens` ships a separate `codex-hook` command tree so the differences
don't leak into the CLI surface.

The fastest way is to let `agent-lens` write the `config.toml` block for you:

```bash
# User scope: $HOME/.codex/config.toml (Codex's canonical location)
agent-lens codex-hook setup

# Project scope: <repo-root>/.codex/config.toml — the repo root comes from
# `git rev-parse --show-toplevel`, with a fallback to `cwd` outside a git tree
agent-lens codex-hook setup --scope project

# Preview without writing
agent-lens codex-hook setup --dry-run
```

The merge is conservative: existing keys and comments are preserved, and
`[[hooks.SessionStart]]` / `[[hooks.PreToolUse]]` / `[[hooks.PostToolUse]]`
blocks are appended only for handlers that aren't already wired up.
Re-running is a no-op once every handler is installed.

If you'd rather edit the file by hand, the equivalent block looks like:

```toml
# ~/.codex/config.toml
[[hooks.SessionStart]]

[[hooks.SessionStart.hooks]]
type = "command"
command = "agent-lens codex-hook session-start summary"

[[hooks.PreToolUse]]
matcher = "^apply_patch$"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "agent-lens codex-hook pre-tool-use complexity"

[[hooks.PreToolUse.hooks]]
type = "command"
command = "agent-lens codex-hook pre-tool-use cohesion"

[[hooks.PostToolUse]]
matcher = "^apply_patch$"

[[hooks.PostToolUse.hooks]]
type = "command"
command = "agent-lens codex-hook post-tool-use similarity"

[[hooks.PostToolUse.hooks]]
type = "command"
command = "agent-lens codex-hook post-tool-use wrapper"
```

## What's in the box

### Hook handlers

| Agent       | Event          | Handler      | What it does                                                                                             |
| ----------- | -------------- | ------------ | -------------------------------------------------------------------------------------------------------- |
| Claude Code | `SessionStart` | `summary`    | Injects a one-shot hotspot + coupling thumbnail into the new session.                                    |
| Claude Code | `PreToolUse`   | `complexity` | Flags functions in the file about to be edited whose Cyclomatic / Cognitive / Nesting cross a threshold. |
| Claude Code | `PreToolUse`   | `cohesion`   | Flags cohesion units in the file about to be edited whose LCOM4 is greater than 1.                       |
| Claude Code | `PostToolUse`  | `similarity` | Reports near-duplicate function pairs in the file just edited.                                           |
| Claude Code | `PostToolUse`  | `wrapper`    | Reports thin forwarding functions in the file just edited.                                               |
| Codex       | `SessionStart` | `summary`    | Same hotspot + coupling thumbnail at session start.                                                      |
| Codex       | `PreToolUse`   | `complexity` | Same complexity heads-up across every file the upcoming `apply_patch` will touch.                        |
| Codex       | `PreToolUse`   | `cohesion`   | Same LCOM4 heads-up across the touched files.                                                            |
| Codex       | `PostToolUse`  | `similarity` | Reports near-duplicate function pairs across every file the latest `apply_patch` touched.                |
| Codex       | `PostToolUse`  | `wrapper`    | Reports thin forwarding functions across the touched files.                                              |

Schemas for the remaining events (`UserPromptSubmit`, `Stop`, `SubagentStop`,
and Codex's `PermissionRequest`) live in the `agent-hooks` crate, ready for
new handlers to plug into the same plumbing.

### Analyzers

| Subcommand       | What it surfaces                                                                                                                                                                                                                                                                                                                                                | Languages                 |
| ---------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------- |
| `similarity`     | Function pairs whose normalised AST has TSED ≥ `--threshold` (default 0.85), via APTED edit distance. Single file or directory; reports cross-file pairs in directory mode.                                                                                                                                                                                     | Rust, TS / JS, Python, Go |
| `wrapper`        | Functions whose body is a forwarding call to another function modulo a short chain of `?`, `.unwrap()`, `.into()`, `.await`, …                                                                                                                                                                                                                                  | Rust, TS / JS, Python, Go |
| `cohesion`       | LCOM4 per `impl` block, class, or module unit (number of connected components in the field-sharing graph).                                                                                                                                                                                                                                                      | Rust, TS / JS, Python, Go |
| `complexity`     | Per-function Cyclomatic, Cognitive, Max Nesting Depth, Halstead Volume, and Maintainability Index.                                                                                                                                                                                                                                                              | Rust, TS / JS, Python, Go |
| `coupling`       | Module-level Number of Couplings, Fan-In, Fan-Out, simplified Henry-Kafura IFC `(fan_in × fan_out)²`, per-pair shared-symbol counts, Robert C. Martin's Instability `Ce/(Ca+Ce)`, and the strongly connected components (cycles).                                                                                                                               | Rust, TS / JS, Python, Go |
| `function-graph` | Static function nodes and heuristic caller→callee edges as visualization-ready JSON. Node weights include static call counts, fan-in/out, LOC, Cyclomatic / Cognitive / Nesting, Halstead Volume, Maintainability Index, plus runtime placeholders for later trace/profile joins.                                                                               | Rust, TS / JS, Python, Go |
| `cycles`         | Function-level strongly connected components of the call graph (resolved edges only): recursion knots and cross-file tangles that must change as one unit, with members, same-file flag, nearby-ambiguity warning, and advisory cheapest-cut break suggestions with call-line evidence.                                                                         | Rust, TS / JS, Python, Go |
| `hubs`           | Hub smells on the function call graph: outlier fan-out (god functions), outlier fan-in (load-bearing blast-radius signal), Henry-Kafura `loc × (fan_in × fan_out)²` bottlenecks, and cross-module pull (misplaced functions), with prod/test fan-in split and deterministic PageRank-importance percentiles.                                                    | Rust, TS / JS, Python, Go |
| `layers`         | Inferred Lakos levelization of the call graph: a function level per function (`1 + max(level of callees)`, cycles collapsed), a module level per module from the induced module graph, entry points, module cycles and skip-level calls with call-site evidence, and per-module member-level spans as a vertical cohesion smell.                                | Rust, TS / JS, Python, Go |
| `untested`       | Production functions with no resolved call path from any test function: multi-source forward traversal from every test node, grouped by module and ranked by untested LOC, with each row's LOC / fan-in / complexity, plus the upper-bound support (unresolved and ambiguous call sites leaving test-reached code).                                             | Rust, TS / JS, Python, Go |
| `visibility`     | `pub` (Rust) / exported (Go) functions whose resolved callers all sit inside a narrower scope, with the declaration that would still compile (`drop pub`, `pub(super)`, `pub(in …)`, `pub(crate)`, unexport), the caller modules as evidence, and the unattributable call sites that argue against each row. `pub use` re-exports are excluded as intended API. | Rust, Go                  |
| `impact`         | Blast radius of a change: functions transitively calling the seeds (working-tree diff or `--function`) over the SCC condensation, direct callers verbatim, deeper callers folded per depth and module, reachable tests as a verification checklist.                                                                                                             | Rust, TS / JS, Python, Go |
| `graph-query`    | One canned call-graph traversal per run: `callers`, `callees`, `neighborhood`, or the shortest `path` between two symbols, with the call lines of every hop.                                                                                                                                                                                                    | Rust, TS / JS, Python, Go |
| `context-span`   | Per-module direct + transitive outgoing dependency closure; counts the distinct source files an agent must read to reason about a module.                                                                                                                                                                                                                       | Rust, TS / JS, Python, Go |
| `hotspot`        | Files ranked by `commits × cognitive_max` over an optional `--since=` window — where churn and complexity overlap, i.e. the bug-prone landmines.                                                                                                                                                                                                                | Rust, TS / JS, Python, Go |

All analyzers default to JSON on stdout; pass `--format md` for a compact
Markdown summary tuned to drop straight into an LLM prompt.
`coupling` and `context-span` name each module the way the analyzed
language names it. Internally every adapter emits the same
`crate::a::b` shape so the graph algorithms stay language-neutral; the
spelling is applied when the report is rendered:

| Language                | Root module         | Nested module                      |
| ----------------------- | ------------------- | ---------------------------------- |
| Rust                    | `crate`             | `crate::analyze::coupling`         |
| Go (with `go.mod`)      | `github.com/x/proj` | `github.com/x/proj/internal/store` |
| Go (no `go.mod`)        | `.`                 | `internal/store`                   |
| TypeScript / JavaScript | `.`                 | `components/Chat`                  |
| Python                  | `.`                 | `util.text`                        |

TS/JS and Python modules are one-per-file, so their labels are the file's
path relative to the module tree's source root. The same spelling is used
in the `SessionStart` coupling thumbnail.

For `complexity`, `cohesion`, `similarity`, `hotspot`, `hubs`, `impact`,
`layers`, `untested`, and `visibility`, `--top` caps the
Markdown ranking while JSON stays complete. `--min-score` filters the Markdown
ranking for `complexity` (cognitive score) and `cohesion` (LCOM4); for
`similarity` it is an alias of `--threshold`.

### Output discipline

- **stdout** is reserved for protocol JSON or analyzer reports.
- **stderr** is for diagnostics, via [`tracing`](https://docs.rs/tracing).
  Control verbosity with `RUST_LOG` (default `info`).
- Direct `println!` / `eprintln!`, `unwrap()`, and `expect()` are clippy
  `deny` so a renegade `dbg!` can't pollute a hook response.

## Languages

Analysis is split into a language-neutral core and per-language adapters.
Adding a language means writing one adapter crate and wiring it into the
`SourceLang` match — the metric implementations themselves are shared.

| Language                | Parser                                                        | Adapter crate |
| ----------------------- | ------------------------------------------------------------- | ------------- |
| Rust                    | [`syn`](https://docs.rs/syn)                                  | `lens-rust`   |
| TypeScript / JavaScript | [oxc](https://oxc.rs/) (`oxc_parser`, `oxc_ast`)              | `lens-ts`     |
| Python                  | [`ruff_python_parser`](https://docs.rs/ruff_python_parser)    | `lens-py`     |
| Go                      | [tree-sitter](https://docs.rs/tree-sitter) + `tree-sitter-go` | `lens-golang` |

`similarity`, `complexity`, `wrapper`, `cohesion`, `hotspot`,
`function-graph`, `cycles`, `graph-query`, `hubs`, `impact`, `layers`,
`untested`, `context-span`, and `coupling` are wired through the Rust,
TypeScript / JavaScript, Python, and Go adapters. `visibility` is wired
through the Rust and Go adapters only, because TypeScript and Python carry
no extracted export status.
`function-graph` uses a syntactic call-site index rather than type inference
or macro expansion. Its JSON is meant for external visualization: callers can
switch graph layers between structure (`fan_in`/`fan_out`, call counts),
maintainability (`loc`, complexity, MI), and later runtime overlays
(`total_time_ms`, `self_time_ms`, errors).

## Workspace layout

```
crates/
├── agent-lens/    # the CLI binary (clap dispatch + agent-lens.toml profile runner)
├── agent-hooks/   # Claude Code & Codex hook protocol schemas + Hook trait
├── lens-domain/   # language-neutral primitives: TreeNode, APTED, TSED,
│                  # FunctionDef, CohesionUnit, FunctionComplexity,
│                  # CouplingReport
├── lens-rust/     # syn-based Rust adapter (also: cohesion, coupling, wrapper)
├── lens-ts/       # oxc-based TypeScript / JavaScript adapter
├── lens-py/       # ruff_python_parser-based Python adapter
└── lens-golang/   # tree-sitter-based Go adapter
```

Responsibility split:

- **`lens-domain`** owns the metric definitions and the comparison machinery
  (APTED, TSED, LCOM, IFC, Maintainability Index). It is language-neutral.
- **`lens-{rust,ts,py,golang}`** translate a language's AST into the neutral
  primitives and nothing else.
- **`agent-hooks`** defines the stdin/stdout JSON types for both supported
  agents and the `Hook` trait handlers implement.
- **`agent-lens`** is a thin CLI shell over the above three.

## Development

All tools are pinned by [mise](https://mise.jdx.dev/). One install gets you
the Rust toolchain, formatters, linters, security scanners, and mutation
testing.

```bash
mise install      # one-shot setup

mise run fmt      # format everything (cargo fmt, dprint, shfmt)
mise run lint     # clippy, rustfmt --check, cargo-deny, cargo-audit,
                  # dprint/shfmt/shellcheck, actionlint/zizmor/ghalint/pinact
mise run lint:base # base repo lint (dprint, shfmt, shellcheck, pre-commit hooks)
mise run lint:gha  # GitHub Actions lint (actionlint, zizmor, ghalint, pinact)
mise run test     # cargo nextest run --locked --all-features
mise run ci       # the full lint + test pipeline CI runs
mise run mutants  # cargo-mutants (slow; not in CI by default)
mise run mutants:rust:diff [base]  # mutation-test Rust changes vs base
```

On NixOS — or anywhere mise's pre-built tool binaries won't run — `nix develop`
provides the same toolchain from nixpkgs instead:

```bash
nix develop        # rust, cargo-nextest/deny/audit/mutants/llvm-cov, dprint,
                   # shfmt, shellcheck, actionlint, zizmor, node, pnpm, uv

nix build          # build the CLI (runs the workspace tests in the sandbox)
nix flake check    # build + evaluate every flake output
nix fmt            # format the .nix files (nixfmt)
```

The shell ships the tools, not mise, so run the underlying commands directly
(`cargo clippy --all-targets --all-features -- -D warnings`, `cargo nextest run
--locked --all-features`, `dprint check`, …) rather than `mise run`. Versions
track nixpkgs rather than the `mise.*.toml` pins, so mise and `ci_rust.yml`
remain the source of truth for exact tool versions. direnv users can drop
`use flake` into a local (git-ignored) `.envrc`.

When adding or changing tests, prefer `rstest` for parameterized cases and
fixture-style setup. Use property-based tests when regression risk is high,
especially around core logic.

Run diff-scoped mutation testing whenever practical for Rust logic changes.
For example, run `mise run mutants:rust:diff origin/main...HEAD` for a PR-style
diff or omit the argument to compare against `main`. If the changed code has
Criterion benchmarks, report whether benchmark regression was checked and what
the result was.

CI (`.github/workflows/`) runs Rust lint/test (`ci_rust.yml`), the base
toolchain lints (`lint_base.yml`), GitHub Actions lint (`lint_gha.yml`),
CodeQL, dependency review, Trivy, TruffleHog, SBOM generation, the flake build
(`nix.yml` — on nix/lockfile changes plus a weekly run), and PR-diff mutation testing
(`mutants.yml` — full runs are available via `workflow_dispatch`).

## Design principles

- **Signal density over decoration.** Reports go to LLMs. Color, ASCII art,
  emoji, and human-only flourishes don't earn their tokens.
- **One binary, many surfaces.** Hooks and analyzers ship together so the
  install + config story stays simple across direct installs, mise, and hook
  setup commands.
- **Schema in one place.** Hook protocol types live in `agent-hooks` so a
  spec change is a one-crate update.
- **Fail loudly.** Missing required fields error out non-zero. Unknown fields
  are tolerated so upstream additions don't break existing handlers.

## Roadmap

The near-term direction is to keep improving the analyzer surfaces that help
agents make better edit decisions: duplication, wrappers, cohesion,
complexity, coupling, context span, hotspots, and call-graph structure
(hubs, cycles, queries, impact, layers, untested, visibility).

New metrics are prioritised by _does this change how an agent decides what to
do?_ rather than _does it look nice in a dashboard?_

An MCP server front-end is a likely next surface, but the CLI is the source
of truth.

## License

MIT. See [`Cargo.toml`](./Cargo.toml).
