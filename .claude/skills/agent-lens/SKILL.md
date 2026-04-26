---
name: agent-lens
description: Use when the user asks to analyze this codebase with agent-lens, or asks which analyzer fits a given question (duplication, complexity, hotspots, coupling, cohesion, forwarding wrappers). Routes to the right `agent-lens analyze` subcommand and explains how to read the output. Prefer the more specific skills (find-duplicates, review-pending-changes, find-refactor-targets, audit-architecture) when one of them clearly fits.
---

# agent-lens analyzer dispatcher

`agent-lens` is the project's own CLI. The binary is on `PATH` after `mise install`; if `agent-lens --version` fails, build it with `cargo build -p agent-lens` and use `./target/debug/agent-lens`.

## Pick the analyzer

| Question | Subcommand | Path argument |
|---|---|---|
| Are there near-duplicate functions in this file? | `similarity` | a `.rs` file |
| Are there forwarding-only functions worth inlining? | `wrapper` | a `.rs` file |
| Which `impl` blocks are doing too many things? | `cohesion` | a `.rs` file |
| Which functions are landmines to edit? | `complexity` | `.rs` / `.ts` / `.js` / `.py` file |
| Which modules are Fan-In bottlenecks or cyclic? | `coupling` | crate root or directory |
| Where do churn and complexity collide? | `hotspot` | git-tracked file or directory |

Only `complexity` runs on TS/JS/Python today; the rest are Rust-only.

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

# Crate-wide structure
agent-lens analyze coupling crates/agent-lens --format md

# Where is the next refactor likely to pay off?
agent-lens analyze hotspot crates --since=180.days.ago --top 10 --format md
```

## Reading the output

- **similarity**: each entry is a pair `(a, b)` with `tsed` in `[0.0, 1.0]`. ≥ 0.95 is essentially a clone; 0.85–0.95 is a near-miss worth refactoring; below 0.85 is filtered out by default. The `--threshold` flag is for tightening or loosening that bar.
- **wrapper**: a hit means the function body, after stripping `?` / `.into()` / `.unwrap()` / `.await`, is just a forwarding call. Either inline it or document why the indirection exists.
- **cohesion**: `lcom4 == 1` is healthy. `lcom4 >= 2` means the `impl` has disjoint method clusters and is a candidate for splitting.
- **complexity**: cognitive ≥ 15 is a yellow flag, ≥ 25 is a red flag. Maintainability Index < 65 means the function is hard to maintain regardless of what cyclomatic says.
- **coupling**: high `fan_in` ⇒ a hub everything depends on (slow to change safely); high `fan_out` ⇒ a module that is hard to test in isolation; non-empty `cycles` is always a smell.
- **hotspot**: rows are sorted by `commits × cognitive_max`. The top of the list is where bugs concentrate; refactor budget is best spent there first.

## Don't reach for it when

- The user wants human-style lints (style, naming, idioms) — that's clippy / dprint / rustfmt, not agent-lens.
- The file isn't a supported language — agent-lens errors out cleanly, but check the table above first.
- The question is "is this code correct?" — analyzers measure shape, not semantics.
