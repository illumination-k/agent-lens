# agent-lens

[![CI Rust](https://github.com/illumination-k/agent-lens/actions/workflows/ci_rust.yml/badge.svg)](https://github.com/illumination-k/agent-lens/actions/workflows/ci_rust.yml)
[![Lint Base](https://github.com/illumination-k/agent-lens/actions/workflows/lint_base.yml/badge.svg)](https://github.com/illumination-k/agent-lens/actions/workflows/lint_base.yml)
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

### One-liner (Linux x86_64, macOS arm64 / x86_64)

```bash
curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh | bash
```

This pulls the matching tarball from the rolling `latest` release, verifies its
SHA-256, and drops the binary into `$HOME/.local/bin`. Override with flags or
environment variables:

```bash
# explicit destination
curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh \
  | bash -s -- --dir /usr/local/bin

# pin a specific release tag (default: latest)
AGENT_LENS_TAG=latest AGENT_LENS_DIR="$HOME/.local/bin" \
  bash <(curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh)
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
mise use github:illumination-k/agent-lens@latest
```

Or add it to `mise.toml` directly:

```toml
[tools]
"github:illumination-k/agent-lens" = "latest"
```

mise auto-detects the right asset for your OS / arch from the
`agent-lens-<target>.tar.gz` artifacts published by `release-latest.yml`.

### From source

Requires a recent Rust toolchain (the workspace is on `edition = "2024"`, so
rustc 1.85+):

```bash
cargo install --path crates/agent-lens
```

### Manual download

Pre-built binaries for `main` are published as a rolling release — grab a
tarball or `.zip` directly from the
[GitHub Releases page](https://github.com/illumination-k/agent-lens/releases)
(`release-latest.yml` keeps it current).

## Quick start

### As an analyzer

Stdin is not used; pass a path and pick an output format.

```bash
# Find near-duplicate functions in a file or directory (TSED >= 0.85)
agent-lens analyze similarity src/foo.rs
agent-lens analyze similarity crates/lens-rust/src

# Same, but emit a compact summary instead of the full JSON
agent-lens analyze similarity src/foo.rs --format md --threshold 0.9

# All analyzers accept path filters: focus tests, drop tests, or exclude globs
agent-lens analyze complexity crates/agent-lens --only-tests
agent-lens analyze similarity crates/lens-rust/src --exclude-tests --min-lines 6
agent-lens analyze hotspot . --exclude 'target/**' --exclude '**/generated/**'

# Analyze only functions touching unstaged diff hunks for this file
agent-lens analyze similarity src/foo.rs --diff-only

# Cohesion (LCOM4) per impl block (Rust) / class (TS, Python)
agent-lens analyze cohesion src/foo.rs

# Cohesion only for impl blocks overlapping `git diff -U0` hunks
agent-lens analyze cohesion src/foo.rs --diff-only

# Cyclomatic / Cognitive / Nesting / Halstead / Maintainability Index
agent-lens analyze complexity src/foo.rs

# Complexity only for functions overlapping `git diff -U0` hunks
agent-lens analyze complexity src/foo.rs --diff-only

# Module-level Fan-In / Fan-Out / Henry-Kafura IFC, Instability,
# and cyclic SCCs for a Rust crate
agent-lens analyze coupling crates/agent-lens

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
| Claude Code | `PreToolUse`   | `cohesion`   | Flags `impl` blocks in the file about to be edited whose LCOM4 is greater than 1 (split-personality).    |
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

| Subcommand     | What it surfaces                                                                                                                                                                                                                  | Languages             |
| -------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------- |
| `similarity`   | Function pairs whose normalised AST has TSED ≥ `--threshold` (default 0.85), via APTED edit distance. Single file or directory; reports cross-file pairs in directory mode.                                                       | Rust, TS / JS, Python |
| `wrapper`      | Functions whose body is a forwarding call to another function modulo a short chain of `?`, `.unwrap()`, `.into()`, `.await`, …                                                                                                    | Rust, TS / JS, Python |
| `cohesion`     | LCOM4 per `impl` block / class (number of connected components in the field-sharing graph).                                                                                                                                       | Rust, TS / JS, Python |
| `complexity`   | Per-function Cyclomatic, Cognitive, Max Nesting Depth, Halstead Volume, and Maintainability Index.                                                                                                                                | Rust, TS / JS, Python |
| `coupling`     | Module-level Number of Couplings, Fan-In, Fan-Out, simplified Henry-Kafura IFC `(fan_in × fan_out)²`, per-pair shared-symbol counts, Robert C. Martin's Instability `Ce/(Ca+Ce)`, and the strongly connected components (cycles). | Rust                  |
| `context-span` | Per-module direct + transitive outgoing dependency closure; counts the distinct source files an agent must read to reason about a module.                                                                                         | Rust                  |
| `hotspot`      | Files ranked by `commits × cognitive_max` over an optional `--since=` window — where churn and complexity overlap, i.e. the bug-prone landmines.                                                                                  | Rust                  |

All analyzers default to JSON on stdout; pass `--format md` for a compact
Markdown summary tuned to drop straight into an LLM prompt.

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

| Language                | Parser                                                     | Adapter crate |
| ----------------------- | ---------------------------------------------------------- | ------------- |
| Rust                    | [`syn`](https://docs.rs/syn)                               | `lens-rust`   |
| TypeScript / JavaScript | [oxc](https://oxc.rs/) (`oxc_parser`, `oxc_ast`)           | `lens-ts`     |
| Python                  | [`ruff_python_parser`](https://docs.rs/ruff_python_parser) | `lens-py`     |

`similarity`, `wrapper`, `cohesion`, and `complexity` are wired through all
three adapter crates. `coupling`, `context-span`, and `hotspot` are Rust-only
today — they reach into `mod` resolution and the git log in ways the other
two adapters don't yet replicate.

## Workspace layout

```
crates/
├── agent-lens/    # the CLI binary (clap-derived dispatch only)
├── agent-hooks/   # Claude Code & Codex hook protocol schemas + Hook trait
├── lens-domain/   # language-neutral primitives: TreeNode, APTED, TSED,
│                  # FunctionDef, CohesionUnit, FunctionComplexity,
│                  # CouplingReport
├── lens-rust/     # syn-based Rust adapter (also: cohesion, coupling, wrapper)
├── lens-ts/       # oxc-based TypeScript / JavaScript adapter
└── lens-py/       # ruff_python_parser-based Python adapter
```

Responsibility split:

- **`lens-domain`** owns the metric definitions and the comparison machinery
  (APTED, TSED, LCOM, IFC, Maintainability Index). It is language-neutral.
- **`lens-{rust,ts,py}`** translate a language's AST into the neutral
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
                  # actionlint, zizmor, ghalint, pinact, shellcheck
mise run test     # cargo nextest run --locked --all-features
mise run ci       # the full lint + test pipeline CI runs
mise run mutants  # cargo-mutants (slow; not in CI by default)
```

When adding or changing tests, prefer `rstest` for parameterized cases and
fixture-style setup. Use property-based tests when regression risk is high,
especially around core logic.

Run diff-scoped mutation testing whenever practical for Rust logic changes.
For example, create a patch with `git diff` and pass it to
`mise exec -- cargo mutants --workspace --no-shuffle --in-diff <patch>`. If the
changed code has Criterion benchmarks, report whether benchmark regression was
checked and what the result was.

CI (`.github/workflows/`) runs Rust lint/test (`ci_rust.yml`), the base
toolchain lints (`lint_base.yml`), CodeQL, dependency review, Trivy,
TruffleHog, SBOM generation, and PR-diff mutation testing
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
complexity, coupling, context span, and hotspots.

New metrics are prioritised by _does this change how an agent decides what to
do?_ rather than _does it look nice in a dashboard?_

An MCP server front-end is a likely next surface, but the CLI is the source
of truth.

## License

MIT. See [`Cargo.toml`](./Cargo.toml).
