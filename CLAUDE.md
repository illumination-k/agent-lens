# Agent guide

`AGENTS.md` symlinks here. Pre-alpha: no backwards-compatibility guarantee.

`agent-lens` is a single-binary Rust CLI giving coding agents a lens on codebase shape: hook handlers, on-demand analyzers, a profile runner over `agent-lens.toml`, baselines, and bundled skills. `agent-lens help --md` and `agent-lens config schema` are generated from code and are the source of truth.

## Output rules

- stdout is for protocol data and analyzer results only; logs go to stderr via `tracing`.
- Analyzer output is LLM context: dense and structured, no colors, emoji, or decorative prose.

## Workflow

- `mise install` first; `mise tasks` lists everything. `mise run ci` must pass before you finish.
- After changing an analyzer, run `mise run selftest <profile>` for the profiles it appears in and diff against the previous run. Findings on our own sources are real: fix them or justify them.
- Before committing, `mise run selftest changes` should be empty.
- When Rust logic changes, run `mise run mutants:rust:diff [base]`; don't skip it silently.
- Tests: prefer `rstest`; add property-based tests where regression risk is high.
- When touching benchmarked code, run `mise run bench-compare [ref]` and report the result. Performance fixes ship with a benchmark reproducing the slow shape (shared corpus writers: `crates/agent-lens/benches/support/`).

## Documentation

A CLI change isn't done until `README.md` matches `agent-lens help --md`: the analyzer lists ("Command surface", "What's in the box"), "Languages" capability claims, and "Quick start" examples. Don't restate per-analyzer options or config keys in the README.
