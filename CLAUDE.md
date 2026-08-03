# Agent guide

`AGENTS.md` is a symlink to this file, so Claude Code, Codex, and any other
`AGENTS.md` consumer read the same guidance.

This library is pre-alpha and under active development. The API is not stable and may change without a major version bump, so backwards compatibility is not guaranteed at this stage.

## Purpose

`agent-lens` is a single-binary Rust CLI that gives coding agents such as Claude Code and Codex a **lens for seeing codebases more deeply**.

Unlike ordinary linting, its output is tuned to be useful when placed in an LLM context: keep the signal dense, avoid decorative output, and prefer structured data that an agent can reason over.

The project bundles two related surfaces:

- Hook handlers that integrate with coding-agent workflows and surface focused context at useful moments.
- On-demand analyzers that report codebase shape: duplication, wrappers, delegation chains, cohesion, complexity, coupling, context span, hotspots, change risk, and call-graph structure (hubs, cycles, queries, impact, layers, untested, visibility).

## Development Process

Run `mise install` first to install the toolchain and project tools.

At the end of a session, run `mise run ci` and make sure it passes. Use the narrower tasks while iterating:

```bash
mise run fmt      # Format (cargo fmt, dprint, shfmt, oxfmt)
mise run lint     # Lint and policy checks
mise run test     # Tests (nextest, doctests, vitest)
mise run ci       # Full required verification; covers everything CI gates on
mise run bench    # Criterion benchmarks; not part of ci
mise run mutants  # Mutation tests; slow and not part of normal ci
mise run selftest # Run agent-lens over its own sources; not part of ci
```

## Dogfooding

`agent-lens.toml` declares one profile per view of this repository (`self`,
`self-reach`, `self-tests`, `lenses`, `web`, `changes`, `baseline`), and `mise run selftest`
builds the binary and drives every one of them. Run a single profile with
`mise run selftest <profile>`.

After changing an analyzer, run the profiles it appears in and read the diff
against the previous run — not the report in isolation. Unit tests cover
analyzer output on fixtures; this is what catches an analyzer that still returns
`Ok` while saying something absurd about code the maintainers know by heart.
Findings the tool reports about its own sources are real findings: fix them or
say why they are acceptable.

Before committing, `mise run selftest changes` runs every diff-only tool over
the working tree. An empty report means the pending edit introduced no
duplicate, no forwarding-only wrapper, and no complexity spike.

Also run mutation testing against the current diff whenever practical. `mise run
mutants:rust:diff [base]` (base defaults to `main`) is the diff-scoped form and is
what CI runs on a PR; `mise run mutants` is the full-workspace run. It is acceptable
for this to be diff-scoped rather than a full mutation run, but do not skip it
silently when the change touches Rust logic.

When adding or changing tests, use [`rstest`](https://docs.rs/rstest) as much as practical, especially for parameterized cases and fixture-style setup.

When regression risk is high, especially around core logic, introduce property-based tests.

When a change touches code that has benchmarks, report whether benchmark regression was checked and what the result was. The convention is to save a baseline on the unchanged code and compare against it:

```bash
git stash && mise run bench:rust --save-baseline base && git stash pop
mise run bench:rust --baseline base
```

Keep stdout reserved for protocol data and analyzer results. Send logs and diagnostics to stderr through `tracing`.

Treat analyzer output as agent-facing context, not human-facing decoration. Do not add colors, animations, emoji, or verbose prose to analyzer output unless there is a concrete agent-useful reason.
