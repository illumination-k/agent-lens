# CLAUDE.md

This library is pre-alpha and under active development. The API is not stable and may change without a major version bump, so backwards compatibility is not guaranteed at this stage.

## Purpose

`agent-lens` is a single-binary Rust CLI that gives coding agents such as Claude Code and Codex a **lens for seeing codebases more deeply**.

Unlike ordinary linting, its output is tuned to be useful when placed in an LLM context: keep the signal dense, avoid decorative output, and prefer structured data that an agent can reason over.

The project bundles two related surfaces:

- Hook handlers that integrate with coding-agent workflows and surface focused context at useful moments.
- On-demand analyzers that report codebase shape: duplication, wrappers, cohesion, complexity, coupling, context span, and hotspots.
- A baseline / ratchet mode (`agent-lens baseline save | check`) that snapshots an analyzer's per-item metrics and on later runs fails only on regressions (worsened existing items, plus new items per `--new-item-policy`). Existing debt is grandfathered. Snapshots default to `.agent-lens/baseline/<analyzer>.json` under the git root and are intended to be committed so CI shares the same ratchet as the team. v1 covers the four numeric-metric analyzers: complexity, cohesion, coupling, hotspot.

## Development Process

Run `mise install` first to install the toolchain and project tools.

At the end of a session, run `mise run ci` and make sure it passes. Use the narrower tasks while iterating:

```bash
mise run fmt      # Format
mise run lint     # Lint and policy checks
mise run test     # Tests
mise run ci       # Full required verification
mise run mutants  # Mutation tests; slow and not part of normal ci
```

Also run mutation testing against the current diff whenever practical. It is acceptable for this to be diff-scoped rather than a full mutation run, but do not skip it silently when the change touches Rust logic.

When adding or changing tests, use [`rstest`](https://docs.rs/rstest) as much as practical, especially for parameterized cases and fixture-style setup.

When regression risk is high, especially around core logic, introduce property-based tests.

When a change touches code that has benchmarks, report whether benchmark regression was checked and what the result was.

Keep stdout reserved for protocol data and analyzer results. Send logs and diagnostics to stderr through `tracing`.

Treat analyzer output as agent-facing context, not human-facing decoration. Do not add colors, animations, emoji, or verbose prose to analyzer output unless there is a concrete agent-useful reason.
