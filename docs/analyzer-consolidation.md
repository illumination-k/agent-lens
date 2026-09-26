# Analyzer consolidation plan

`agent-lens analyze` has 29 subcommands. Many of them answer the same question
from a different angle, and they already share most of their code: 18 of them
build the same call graph, 3 of them build the same module graph, and 6 of them
read the same git churn. An agent choosing among 29 names is paying routing cost
that a single analyzer with sections would not charge. This plan merges them
down to 17, with an optional further step to 15.

The project is pre-alpha, so old names are removed rather than kept as
aliases. Each removed name keeps one hidden clap subcommand and one config
parse arm whose only job is to fail with `renamed: use <new form>` — cheap,
and it turns an agent's stale skill or `agent-lens.toml` into an actionable
error instead of "unrecognized subcommand".

## Target surface

| New command   | Absorbs                                                                                            | Why they belong together                                                                     |
| ------------- | -------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| `similarity`  | `test-redundancy`                                                                                  | `test_redundancy.rs` already scores with `similarity`; it adds keep/fold advice over tests   |
| `forwarding`  | `wrapper`, `delegation`                                                                            | one hop with argument evidence vs. the chain those hops stack into; same classification      |
| `reach`       | `unreachable`, `untested`, `test-only`                                                             | reachability from two root sets; see the 2×2 below                                           |
| `narrowable`  | `single-use`, `single-impl`, `parameters`, `visibility`                                            | "declared wider than its uses": one caller, one impl, one value, narrower scope              |
| `graph`       | `function-graph`, `graph-query`                                                                    | no `--query` dumps the graph, `--query` runs a traversal over it                             |
| `layers`      | `cycles`                                                                                           | the function SCCs are exactly what cannot be layered; report them as the layer map's residue |
| `coupling`    | `context-span`                                                                                     | both walk the module graph from one entry; span becomes a column (`transitive`, `files`)     |
| `hotspot`     | `risk`                                                                                             | both rank files by churn × X; `--by complexity\|blast` (risk already imports hotspot)        |
| `co-change`   | `hidden-coupling`                                                                                  | hidden coupling is co-change minus declared edges; becomes a section (`--against-code`)      |
| _(unchanged)_ | `search`, `complexity`, `cohesion`, `communities`, `hubs`, `impact`, `footprint`, `change-entropy` |                                                                                              |

29 → 17. Optional later step (29 → 15): `communities` into `coupling`
(same module graph, same single-PATH signature) and `change-entropy` into
`co-change` (same churn window). Deferred because each has its own options
(`--granularity`, `--period`) and a distinct headline.

### `reach`: one matrix instead of three lists

Each production function is classified by two bits over the resolved call
graph: reached from an entry point, reached from a test.

|                  | reached from tests | not reached from tests |
| ---------------- | ------------------ | ---------------------- |
| from entries     | ok (not listed)    | `untested`             |
| not from entries | `test-only`        | `unreachable` (tiered) |

One graph build, one traversal per root set, and the three current reports
become filters (`--show untested,test-only,unreachable`, default all). The
unreachable confidence tiers (`--tier`) carry over to the bottom-right cell.

### `narrowable`: sections, not a merged score

The four checks stay independent and each keeps its own section and options
(`--max-loc`, `--max-cyclomatic`, `--min-call-sites`). `--check
single-use,single-impl,parameters,visibility` selects sections; default all.
They already share `unreachable`'s root and export logic, so the merged
command builds that once.

## Phases

Order is cheapest-first: phases 1–2 merge analyzers whose code already calls
into each other, so they are mostly CLI, config and output-schema work.

1. **Thin merges** (one PR each, independent):
   `test-redundancy` → `similarity --target tests`;
   `function-graph` + `graph-query` → `graph`;
   `risk` → `hotspot --by blast`;
   `hidden-coupling` → `co-change --against-code`;
   `context-span` → `coupling`.
2. **`reach`** — needs the new JSON shape above; `footprint` and `stop delta`
   consume `unreachable` and must move to the new API in the same PR.
3. **`forwarding`** — the `post-tool-use wrapper` hook and the `stop delta`
   wrapper facts stay; only the analyzer entry point and report change. Hook
   ids are not renamed in this phase (it would break installed settings).
4. **`narrowable`** — largest diff (four modules, ~7.7k lines); lands after
   `reach` so it can build on the shared root set.
5. **`layers` + `cycles`** — last because the layer map's module SCC and the
   function SCC must be reconciled in one report, which is a design question,
   not a move.

## Per-PR checklist

Every analyzer name is enumerated in several places; a merge is done only when
all of them move together:

- `cli/args/analyze.rs`, `cli/analyze.rs` — subcommand, routing table in
  `--help`, worked examples, the hidden `renamed:` stub.
- `config.rs`, `config_schema.rs`, `cli/profile.rs` — profile tool name and
  per-tool table; old names fail with the rename hint.
- `digest.rs`, `baseline.rs` — digest row sources and baseline metric names.
  A renamed metric is a breaking snapshot change; say so in the PR.
- `hooks/` — any handler that calls the old module.
- `agent-lens.toml` — every profile listing the old name.
- `.claude/skills/*` (bundled via `skills.rs`) and `web/src/content.ts`.
- `README.md` — "Command surface", "What's in the box", Quick start.
- `tests/cli_smoke.rs`, benches under `crates/agent-lens/benches/`.

Verification per PR: `mise run ci`, `mise run selftest` diffed against the
previous run (the merged report must contain every finding the old reports
had), `mise run mutants:rust:diff`, and `mise run bench-compare` — a merge that
builds one call graph instead of three should get faster, and the benchmark
should show it.

## Open questions

- Names: `forwarding`, `reach`, `narrowable`, `graph` are proposals.
- Whether hook ids (`post-tool-use:wrapper`) follow the analyzer rename. This
  plan keeps them, since renaming breaks every installed `settings.json`.
- Whether to take the optional step to 15.
