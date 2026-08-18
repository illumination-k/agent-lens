---
name: audit-architecture
description: Use when the user wants to evaluate the structural health of a module or crate — coupling, Fan-In bottlenecks, dependency cycles, layering and level inversions, instability, whether the declared module boundaries match the clustering the dependencies form, or `impl`/class-level cohesion (LCOM4). Wraps `agent-lens analyze coupling`, `layers`, `communities`, `context-span`, and `cohesion`, all of which work on Rust, TypeScript / JavaScript, Python, and Go.
---

# Audit module structure with agent-lens

Five analyzers cover the architecture question:

- `coupling` — module-level metrics: Number of Couplings, Fan-In, Fan-Out, Henry-Kafura IFC `(fan_in × fan_out)²`, Martin's Instability `Ce/(Ca+Ce)`, and the strongly connected components of the dependency graph (cycles). Runs on Rust crates, TS/JS module graphs (the entry file's relative-import closure), Go modules (package-granular), and Python package trees (file-granular, in-tree imports only).
- `context-span` — per-module transitive outgoing closure (the modules and source files an agent must read to reason about the module). Runs on Rust, TS/JS, Python, and Go. For TS/JS frameworks with many implicit entries (Next.js App Router, file-routed Remix / Astro), pass `--entry-glob` repeatedly to merge several entry trees into one report.
- `layers` — inferred Lakos levelization of the call graph: a level per function and per module, the entry-point set, the module cycles that make levelization impossible (with the concrete call sites), and downward calls skipping a level. Needs no crate root or entry file — it walks any directory of Rust, TS/JS, Python, or Go.
- `communities` — the clusters the dependency graph forms, scored against the module boundaries the repository declares. Newman modularity `Q` for both partitions over one graph, plus the members filed in one module but wired into another. Same entry-point rule as `coupling`.
- `cohesion` — per-`impl` (Rust) / per-class (TS, Python, Go) LCOM4: number of connected components in the field-sharing graph. `1` is healthy; `≥ 2` means the unit has disjoint responsibilities.

## Workflow

### 1. Crate-wide / entry-wide coupling

`coupling` takes a Rust crate root (`src/lib.rs` / `src/main.rs`, or a directory containing one), a TypeScript / JavaScript entry file (`.ts` / `.tsx` / `.mts` / `.cts` / `.js` / `.jsx` / `.mjs` / `.cjs`) whose relative imports define the module graph, a Go file / module directory (one containing `go.mod`), or a Python file / package directory:

```bash
# Rust crate
agent-lens analyze coupling crates/agent-lens --format md --top 15

# TS/JS module graph from an entry
agent-lens analyze coupling app/src/index.ts --format md

# Go module (directory containing go.mod)
agent-lens analyze coupling ./cmd/server --format md

# Python package tree
agent-lens analyze coupling ./src/mypkg --format md
```

Look for, in order:

1. **Cycles** (non-empty `cycles` field). Always a smell. The SCC tells you exactly which modules form the cycle — break the weakest edge.
2. **High Fan-In** with high churn (`agent-lens analyze risk <path> --format md` does that cross-reference for you, at file granularity). A hub everyone depends on that keeps changing is a serialization point for the team.
3. **High Fan-Out**. A module that depends on too many others is hard to test in isolation. Often a sign the module is doing orchestration that should be pushed up.
4. **High Instability with high Fan-In**. Martin's diagnostic: stable hubs (low Instability) are good; unstable hubs (high Instability) are fragile.

### 2. Vertical shape (layers)

`coupling` prints one row per module, so `--top` (default 20) is what keeps the report readable on a large package; `--format json` always carries every module.

`coupling` needs a crate root, a single TS/JS entry file, or a Go / Python directory — one entry, never several, since two entries are two graphs rather than a wider one. `layers` does not — it derives the module graph from call edges, so it also answers the layering question for trees `coupling` can't take:

```bash
agent-lens analyze layers crates/agent-lens/src --exclude-tests --format md
```

Read it in this order:

1. **Entry points** — the orientation set. Start the top-down read here.
2. **Module cycles** — the same finding as `coupling`'s `cycles`, but derived from calls rather than imports, and with the exact call sites that realise each cycle. Those call lines are the cut candidates.
3. **Wide member spans** — a module whose members straddle many function levels mixes leaf helpers with orchestration. Pair with `cohesion` on that module.
4. **Skip-level calls** — normal for shared leaf utilities; suspicious when the skipped level owns the same concern.

Nothing here is an error by itself: callbacks, dependency injection, and trait-object dispatch all produce the same shapes, and the call facts cannot tell them apart. Rows marked `name-fallback` had their target guessed by last segment — discount those first.

### 3. Are the boundaries in the right place (communities)

`coupling` and `layers` both take the declared module boundaries as given: one says which modules lean on each other, the other whether the direction is sane. `communities` is the question underneath both — whether the boundary belongs there at all:

```bash
agent-lens analyze communities crates/agent-lens --format md --top 15

# Directories rather than files, for "is this subtree under the right parent?"
agent-lens analyze communities crates/agent-lens --granularity module --format md
```

Read it top-down and stop early when you can:

1. **The two `Q` values.** A declared score close to the detected one means the declared boundaries already are the clustering — the architecture matches reality, and everything below is noise. A wide gap is what makes the listings worth reading.
2. **Misfiled members.** Each row is a move candidate with its evidence: edge weight to the module its community is named after, against edge weight to the module it is filed under. Rank order is that gap, so the top row is the best-evidenced move, not the biggest cluster.
3. **Spanning communities.** A cluster no declared module owns a majority of is a feature smeared across modules — the case for a new module rather than a move.

Two things to discount before acting. Only resolved references become edges, so a module whose imports the extractor cannot resolve looks under-connected to its own neighbours — check the file before believing a row with `→declared 0`. And modularity has a resolution limit: a small genuine cluster can be absorbed into a larger neighbour, which is why every community reports its size.

### 4. Module read-cost (context span)

Pair `coupling` with `context-span` to estimate how much of the crate an agent must hold in context to safely change a given module:

```bash
# Rust crate
agent-lens analyze context-span crates/agent-lens --format md

# TS/JS entry
agent-lens analyze context-span app/src/index.ts --format md

# Python file or directory
agent-lens analyze context-span pkg/foo --format md

# Go file or module directory
agent-lens analyze context-span ./cmd/server --format md
```

For TS/JS frameworks where there is no single entry (Next.js App Router, Remix, Astro), pass `path` as the project root and merge several entry trees with `--entry-glob` (repeatable):

```bash
agent-lens analyze context-span app \
  --entry-glob 'app/**/page.tsx' \
  --entry-glob 'app/**/route.ts' \
  --format md
```

A module with a large `files` count is expensive to onboard onto. If a hub from step 1 also has a wide span, splitting the hub gives an outsized win (smaller change, smaller blast radius).

### 5. Per-`impl` / per-class cohesion

For the worst-offending modules from steps 1-2 — and any `impl` block or class the user is about to extend — run `cohesion`:

```bash
agent-lens analyze cohesion crates/lens-rust/src/coupling.rs --format md
```

`lcom4 == 1` is what you want. `lcom4 == 2` means the `impl` is two `impl`s that share a struct name. `lcom4 ≥ 3` is rare and almost always a refactor target.

For an in-progress edit:

```bash
agent-lens analyze cohesion <path> --diff-only --format md
```

…catches the case "I just added a method that uses none of the fields the others use".

### 6. Cross-reference

The analyzers tell different stories that often line up:

| Coupling signal                  | Cohesion signal              | Diagnosis                                                                                                  |
| -------------------------------- | ---------------------------- | ---------------------------------------------------------------------------------------------------------- |
| Module has high Fan-Out          | LCOM4 = 1 across its `impl`s | God object — split by responsibility, not by struct.                                                       |
| Module has high Fan-In           | One `impl` has LCOM4 ≥ 2     | The hub leaks an internal split — fix cohesion first, then re-measure coupling.                            |
| Cycle between A and B            | —                            | Move the shared abstraction into a third module both depend on.                                            |
| Instability ≈ 1 on a leaf module | —                            | Fine. Leaves are supposed to be unstable.                                                                  |
| Instability ≈ 0 with high churn  | —                            | Stable hub that keeps changing. Either it's miscategorised or the hub abstraction is wrong.                |
| `layers` wide member span        | One `impl` has LCOM4 ≥ 2     | The module holds two layers _and_ two responsibilities — split along the level boundary.                   |
| `communities` misfiled member    | —                            | The file's dependencies put it in another module; move it, or find the abstraction that would let it stay. |
| `communities` spanning community | —                            | No declared module owns the cluster. A new module, not a move.                                             |

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

# Move candidates with at least 5x more weight outside their module than inside
agent-lens analyze communities crates/agent-lens \
  | jq '.misfiled[] | select(.weight_to_suggested >= 5 * (.weight_to_declared + 1))'
```

## Don't reach for it when

- The user wants per-function complexity — that's `complexity`, not `coupling`/`cohesion`.
- The crate / entry tree is a single file — Fan-In / Fan-Out are degenerate, the report will be empty.
- The "module structure" question is across Rust crates — `coupling` is intra-crate. For inter-crate dependency questions, `cargo tree` is the right tool.
- The Python question is about imports that leave the tree — `coupling` only draws edges for imports resolving to a `.py` file under the root, so stdlib and third-party dependencies are invisible.
- The TS/JS project has no single entry file (e.g. a library exporting many barrels, or a Next.js App Router app) — `coupling` requires one entry, so you'll need to pick a representative one. `context-span` supports merging entries via `--entry-glob`, but `coupling` does not.
