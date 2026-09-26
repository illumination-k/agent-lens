# Analyzer consolidation plan

`agent-lens analyze` has 29 subcommands. Many of them answer the same question
from a different angle, and they already share most of their code: 18 of them
build the same call graph, 3 of them build the same module graph, and 6 of them
read the same git churn. An agent choosing among 29 names is paying routing cost
that a single analyzer with sections would not charge. This plan merges them
down to 17, with an optional further step to 15.

The project is pre-alpha, so old names are removed rather than kept as
aliases. A removed name still fails usefully: `analyze <old>` exits 2 with
`` `analyze <old>` is now `analyze <new> --section <old>` ``, and a profile
listing it fails to load with the analyzer and section that carry it now
(`config::MERGED_TOOLS` is the one table both read). A stale skill or
`agent-lens.toml` then says how to fix itself.

Status: `reach`, `narrowable` and `forwarding` are done (29 → 23). The rest
of this plan is still open.

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

### How a bundle works (`forwarding`, `reach`, `narrowable`)

A merged analyzer is a bundle of sections, and each section is the former
analyzer's report, unchanged. `analyze/composite.rs` runs the selected
sections under one `AnalysisIndexScope`, so the call graph and per-file facts
they share are built once, and stacks the reports:

- JSON: `{schema_version, sections: [keys], <key>: <section report>, …}`.
- Markdown: one `# <Bundle> (sections)` title, each section's headings demoted
  one level, and the repeated resolution-confidence rows folded after the
  first section (the same `ConfidenceDeduper` a profile run uses).
- `--section a,b` (repeatable; `section = [...]` in a profile) selects
  sections; they always render in declaration order. Default is all.
- The digest folds per section, and a drill-down names the section
  (`analyze reach src --section untested`).

Section options keep their spelling on the bundle: `forwarding` takes
`--diff-only` / `--diff-range` for both sections, `reach` takes `--tier`,
`narrowable` takes `--max-loc`, `--max-cyclomatic`, `--min-call-sites`, and
both take `--top`, applied to every section.

### `reach`: one matrix, three sections

Each production function sits in one cell of reached-from-entries ×
reached-from-tests, and each off-diagonal cell is a section:

|                  | reached from tests | not reached from tests |
| ---------------- | ------------------ | ---------------------- |
| from entries     | ok (not listed)    | `untested`             |
| not from entries | `test-only`        | `unreachable` (tiered) |

A true single-traversal matrix report (one row per function with both bits)
is a possible follow-up; the sections were kept byte-identical to the old
reports so the merge changed no finding.

### `narrowable`: sections, not a merged score

`single-use`, `single-impl`, `parameters`, `visibility`: four independent
checks for "declared wider than its uses", each with its own caveats.

## Phases

Order is cheapest-first: phases 1–2 merge analyzers whose code already calls
into each other, so they are mostly CLI, config and output-schema work.

1. **Thin merges** (one PR each, independent):
   `test-redundancy` → `similarity --target tests`;
   `function-graph` + `graph-query` → `graph`;
   `risk` → `hotspot --by blast`;
   `hidden-coupling` → `co-change --against-code`;
   `context-span` → `coupling`.
2. **`reach`** — done. `footprint` and `stop delta` still call the
   `unreachable` module directly; only the CLI surface moved.
3. **`forwarding`** — done. The `post-tool-use wrapper` hook and the
   `stop delta` wrapper facts call the `wrapper` module directly and keep
   their hook ids; only the analyzer surface moved.
4. **`narrowable`** — done, on the same bundle machinery as `reach`.
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

- Names: `graph` is a proposal.
- Whether hook ids (`post-tool-use:wrapper`) follow the analyzer rename. This
  plan keeps them, since renaming breaks every installed `settings.json`.
- Whether to take the optional step to 15.
