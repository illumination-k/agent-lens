---
name: audit-architecture
description: Use when the user wants to evaluate the structural health of a Rust crate — module coupling, Fan-In bottlenecks, dependency cycles, instability, or `impl`-level cohesion (LCOM4). Wraps `agent-lens analyze coupling` and `agent-lens analyze cohesion`.
---

# Audit module structure with agent-lens

Two analyzers cover the architecture question:

- `coupling` — module-level metrics for a Rust crate: Number of Couplings, Fan-In, Fan-Out, Henry-Kafura IFC `(fan_in × fan_out)²`, Martin's Instability `Ce/(Ca+Ce)`, and the strongly connected components of the dependency graph (cycles).
- `cohesion` — per-`impl` LCOM4: number of connected components in the field-sharing graph. `1` is healthy; `≥ 2` means the `impl` has disjoint responsibilities.

Both are Rust-only.

## Workflow

### 1. Crate-wide coupling

`coupling` takes either a `.rs` crate root (`src/lib.rs` / `src/main.rs`) or a directory containing one:

```bash
agent-lens analyze coupling crates/agent-lens --format md
```

Look for, in order:

1. **Cycles** (non-empty `cycles` field). Always a smell. The SCC tells you exactly which modules form the cycle — break the weakest edge.
2. **High Fan-In** with high churn (cross-reference with `hotspot`). A hub everyone depends on that keeps changing is a serialization point for the team.
3. **High Fan-Out**. A module that depends on too many others is hard to test in isolation. Often a sign the module is doing orchestration that should be pushed up.
4. **High Instability with high Fan-In**. Martin's diagnostic: stable hubs (low Instability) are good; unstable hubs (high Instability) are fragile.

### 2. Per-`impl` cohesion

For the worst-offending modules from step 1 — and any `impl` block the user is about to extend — run `cohesion`:

```bash
agent-lens analyze cohesion crates/lens-rust/src/coupling.rs --format md
```

`lcom4 == 1` is what you want. `lcom4 == 2` means the `impl` is two `impl`s that share a struct name. `lcom4 ≥ 3` is rare and almost always a refactor target.

For an in-progress edit:

```bash
agent-lens analyze cohesion <path> --diff-only --format md
```

…catches the case "I just added a method that uses none of the fields the others use".

### 3. Cross-reference

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
agent-lens analyze cohesion <path> | jq '.units[] | select(.lcom4 >= 2)'
```

## Don't reach for it when

- The user wants per-function complexity — that's `complexity`, not `coupling`/`cohesion`.
- The crate is a single file — Fan-In / Fan-Out are degenerate, the report will be empty.
- The "module structure" question is across crates — coupling is intra-crate today. For inter-crate dependency questions, `cargo tree` is the right tool.
- The codebase isn't Rust — neither analyzer supports TS/JS/Python yet.
