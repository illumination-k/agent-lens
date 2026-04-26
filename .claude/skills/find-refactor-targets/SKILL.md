---
name: find-refactor-targets
description: Use when the user wants to know where to refactor first, where bugs concentrate, which files are landmines, or which functions are too complex to safely change. Combines `agent-lens analyze hotspot` (churn × complexity ranking) with per-file `complexity` drill-downs.
---

# Find refactor targets with agent-lens

Goal: produce a short, ordered list of files and functions where a refactor is most likely to pay off, instead of letting the user guess.

The driver is **Hotspot** — `commits × cognitive_max`. A file with a high cognitive complexity that nobody touches isn't urgent. A file that everyone touches *and* is complex is where bugs accumulate.

## Workflow

### 1. Get the hotspot ranking

Start at the workspace level, scoped to the recent past:

```bash
agent-lens analyze hotspot crates --since=180.days.ago --top 15 --format md
```

Tune `--since`:
- `90.days.ago` for "what's been hot this quarter"
- `1.year.ago` for stable repos
- omit for full history (defaults to all commits)

The path must lie inside a git working tree. For a single crate:

```bash
agent-lens analyze hotspot crates/agent-lens --since=180.days.ago --format md
```

### 2. Drill into the top entries

For each top-ranked file, get per-function complexity:

```bash
agent-lens analyze complexity <hotspot-path> --format md
```

Read the report and pick the worst offender(s) — usually one or two functions account for the file's `cognitive_max`.

### 3. (Rust only) Check the surrounding `impl`

If the worst function lives in an `impl` block, see whether the block itself is incoherent:

```bash
agent-lens analyze cohesion <hotspot-path> --format md
```

`lcom4 ≥ 2` plus high cognitive in the same `impl` means: the methods are doing unrelated jobs *and* one of them is a landmine. Splitting the `impl` is a high-leverage move.

### 4. Verify the file isn't an architectural bottleneck

If the hotspot is a module that lots of other modules import from, refactoring needs more care. Run coupling on the crate:

```bash
agent-lens analyze coupling crates/<name> --format md
```

A high `fan_in` on the hotspot module means changes ripple. Stage the refactor: extract first, then change the implementation.

## Reading the metrics

| Signal | Threshold | What it means |
|---|---|---|
| Hotspot rank | top 5 | Where to spend refactor budget first |
| Cognitive | ≥ 25 | Hard to hold in your head; bug magnet |
| Cognitive | 15–24 | Yellow flag; consider splitting |
| Maintainability Index | < 65 | Hard to maintain regardless of cyclomatic |
| LCOM4 | ≥ 2 | `impl` has disjoint responsibilities |
| Cyclomatic alone | — | Don't anchor on this; cognitive is the more useful number |

## Output format for the user

When summarising back to the user, lead with the action, not the number:

> `crates/lens-rust/src/coupling.rs` — touched 47× in 6 months, `cognitive_max = 38` in `Coupling::collect`. Splitting out edge collection from the report builder would cap the per-function cognitive at ~15 each.

Don't dump the raw JSON. The skill is for the user; the analyzer's `--format md` already trims to the essentials.

## Don't reach for it when

- The repo isn't a git working tree — `hotspot` errors out. Use `complexity` directly on a path instead.
- The user already knows which file to refactor — skip hotspot, go straight to `complexity` + `cohesion` on that file.
- There are < 50 commits of history — hotspot ranking is unstable on small repos. Eyeball complexity directly.
