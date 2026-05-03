---
name: audit-architecture
description: Use when the user wants to evaluate the structural health of a module or crate — coupling, Fan-In bottlenecks, dependency cycles, instability, or `impl`/class-level cohesion (LCOM4). Wraps `agent-lens analyze coupling`, `context-span`, and `cohesion`. Works on Rust crates and TypeScript / JavaScript module graphs (with Python supported by `context-span` / `cohesion`).
---

# Audit module structure with agent-lens

Three analyzers cover the architecture question:

- `coupling` — module-level metrics: Number of Couplings, Fan-In, Fan-Out, Henry-Kafura IFC `(fan_in × fan_out)²`, Martin's Instability `Ce/(Ca+Ce)`, and the strongly connected components of the dependency graph (cycles). Runs on Rust crates and on TS/JS module graphs (the entry file's relative-import closure).
- `context-span` — per-module transitive outgoing closure (the modules and source files an agent must read to reason about the module). Runs on Rust, TS/JS, and Python. For TS/JS frameworks with many implicit entries (Next.js App Router, file-routed Remix / Astro), pass `--entry-glob` repeatedly to merge several entry trees into one report.
- `cohesion` — per-`impl` (Rust) / per-class (TS, Python) LCOM4: number of connected components in the field-sharing graph. `1` is healthy; `≥ 2` means the unit has disjoint responsibilities.

## Workflow

### 1. Crate-wide / entry-wide coupling

`coupling` takes either a Rust crate root (`src/lib.rs` / `src/main.rs`, or a directory containing one) or a TypeScript / JavaScript entry file (`.ts` / `.tsx` / `.mts` / `.cts` / `.js` / `.jsx` / `.mjs` / `.cjs`) whose relative imports define the module graph:

```bash
# Rust crate
agent-lens analyze coupling crates/agent-lens --format md

# TS/JS module graph from an entry
agent-lens analyze coupling app/src/index.ts --format md
```

Look for, in order:

1. **Cycles** (non-empty `cycles` field). Always a smell. The SCC tells you exactly which modules form the cycle — break the weakest edge.
2. **High Fan-In** with high churn (cross-reference with `hotspot`). A hub everyone depends on that keeps changing is a serialization point for the team.
3. **High Fan-Out**. A module that depends on too many others is hard to test in isolation. Often a sign the module is doing orchestration that should be pushed up.
4. **High Instability with high Fan-In**. Martin's diagnostic: stable hubs (low Instability) are good; unstable hubs (high Instability) are fragile.

### 2. Module read-cost (context span)

Pair `coupling` with `context-span` to estimate how much of the crate an agent must hold in context to safely change a given module:

```bash
# Rust crate
agent-lens analyze context-span crates/agent-lens --format md

# TS/JS entry
agent-lens analyze context-span app/src/index.ts --format md

# Python file or directory
agent-lens analyze context-span pkg/foo --format md
```

For TS/JS frameworks where there is no single entry (Next.js App Router, Remix, Astro), pass `path` as the project root and merge several entry trees with `--entry-glob` (repeatable):

```bash
agent-lens analyze context-span app \
  --entry-glob 'app/**/page.tsx' \
  --entry-glob 'app/**/route.ts' \
  --format md
```

A module with a large `files` count is expensive to onboard onto. If a hub from step 1 also has a wide span, splitting the hub gives an outsized win (smaller change, smaller blast radius).

### 3. Per-`impl` / per-class cohesion

For the worst-offending modules from step 1 — and any `impl` block or class the user is about to extend — run `cohesion`:

```bash
agent-lens analyze cohesion crates/lens-rust/src/coupling.rs --format md
```

`lcom4 == 1` is what you want. `lcom4 == 2` means the `impl` is two `impl`s that share a struct name. `lcom4 ≥ 3` is rare and almost always a refactor target.

For an in-progress edit:

```bash
agent-lens analyze cohesion <path> --diff-only --format md
```

…catches the case "I just added a method that uses none of the fields the others use".

### 4. Cross-reference

The two analyzers tell different stories that often line up:

| Coupling signal                  | Cohesion signal              | Diagnosis                                                                                   |
| -------------------------------- | ---------------------------- | ------------------------------------------------------------------------------------------- |
| Module has high Fan-Out          | LCOM4 = 1 across its `impl`s | God object — split by responsibility, not by struct.                                        |
| Module has high Fan-In           | One `impl` has LCOM4 ≥ 2     | The hub leaks an internal split — fix cohesion first, then re-measure coupling.             |
| Cycle between A and B            | —                            | Move the shared abstraction into a third module both depend on.                             |
| Instability ≈ 1 on a leaf module | —                            | Fine. Leaves are supposed to be unstable.                                                   |
| Instability ≈ 0 with high churn  | —                            | Stable hub that keeps changing. Either it's miscategorised or the hub abstraction is wrong. |

## Reading the JSON when `--format md` isn't enough

The Markdown summary trims hard. For deeper analysis, drop `--format md` and pipe through `jq`:

```bash
# Top 5 modules by Fan-In
agent-lens analyze coupling crates/agent-lens \
  | jq '.modules | sort_by(-.fan_in) | .[:5]'

# Modules with non-trivial cycles
agent-lens analyze coupling crates/agent-lens \
  | jq '.cycles[] | select(length > 1)'

# Impls with LCOM4 >= 2
agent-lens analyze cohesion <path> | jq '.files[].units[] | select(.lcom4 >= 2)'
```

## Don't reach for it when

- The user wants per-function complexity — that's `complexity`, not `coupling`/`cohesion`.
- The crate / entry tree is a single file — Fan-In / Fan-Out are degenerate, the report will be empty.
- The "module structure" question is across Rust crates — `coupling` is intra-crate. For inter-crate dependency questions, `cargo tree` is the right tool.
- The codebase is Python and the question is about coupling — `coupling` does not parse Python imports. `context-span` and `cohesion` do run on Python.
- The TS/JS project has no single entry file (e.g. a library exporting many barrels, or a Next.js App Router app) — `coupling` requires one entry, so you'll need to pick a representative one. `context-span` supports merging entries via `--entry-glob`, but `coupling` does not.
