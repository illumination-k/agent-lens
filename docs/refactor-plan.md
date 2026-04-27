# agent-lens self-analysis refactor plan

Findings collected with `agent-lens` itself on the current `crates/` tree
(`hotspot --since=180.days.ago`, `complexity`, `coupling` per crate,
`cohesion`, `similarity --exclude-tests`, `wrapper`). Items are grouped by
expected payoff and ordered so each block can ship as one PR.

## 1. Snapshot

| Metric                               | Value                                                       |
| ------------------------------------ | ----------------------------------------------------------- |
| Hotspot top score                    | 186 (`lens-py/src/cohesion.rs`)                             |
| Top cognitive (single fn)            | 62 (`lens-py LocalNameWalker::walk_stmt`)                   |
| Min Maintainability Index            | 21.4 (same fn)                                              |
| Cycles                               | 1 (`lens-domain::function ↔ lens-domain::lsh`)              |
| LCOM4 ≥ 5 units                      | 9 (8 modules + `EdgeVisitor`/`LocalWalker`/`ModuleRefVisitor`) |
| Similarity clusters (≥ 0.85, no tests) | 52                                                          |
| Wrappers flagged                     | 11                                                          |

## 2. P0 — high-leverage duplication and untangle

### P0-1. Generalise the four `analyze::*` subcommand drivers

`similarity` flagged identical bodies (TSED ≈ 1.0) for these methods across
`agent-lens/src/analyze/{wrapper,cohesion,complexity,similarity}.rs`:

- `Analyzer::analyze` (3 × 100% match)
- `Analyzer::analyze_file` (3 × 100% match)
- `Analyzer::collect_directory` (4 × 86–100%)
- `FileView::from` (3 × 100% match)
- `box_err` (`analyze/similarity.rs:333`) — just `Box::new`, also flagged by `wrapper`

Extract a generic driver in `agent-lens::analyze` (something like
`DirectoryWalker<F>` parameterised over `FileReport` + `run_*`) plus a
`writeln_md!`-style helper. Each subcommand keeps only its language switch
(`run_wrappers` / `run_complexity` / …) and its `format_markdown`.

Effect: ~120 lines deleted, four files drop from LCOM4 = 3–4 toward 1, and
`crate::analyze` (currently `fan_in = 18`) loses one of its drivers' worth of
churn.

### P0-2. Collapse the four `run_*_tool_use` glue functions in `main.rs`

`agent-lens/src/main.rs:539-664` has four functions (`run_pre_tool_use`,
`run_post_tool_use`, `run_codex_pre_tool_use`, `run_codex_post_tool_use`)
that the similarity report scores at 100% pairwise. They differ only in:

1. The `*HookInput` enum and its variant.
2. The handler `match` arms.

A single generic helper

```rust
fn run_hook_event<I, V, O, M>(extract: impl FnOnce(I) -> Result<V, …>, dispatch: M) -> …
```

removes the body duplication while leaving the per-event variants in the
caller. Cuts ~30 LOC and removes the most-touched non-test file's
duplication signal (`main.rs` is currently the #6 hotspot at 23 commits).

### P0-3. Inline the four `prepare_sources` wrappers

`wrapper` flagged identical 3-line forwarders in:

- `agent-lens/src/hooks/pre_tool_use/mod.rs:30-32`
- `agent-lens/src/hooks/post_tool_use/mod.rs:24-26`
- `agent-lens/src/hooks/codex/pre_tool_use/mod.rs:36-38`
- `agent-lens/src/hooks/codex/post_tool_use/mod.rs:28-30`

All four are `fn prepare_sources(input) { prepare_edited_sources(input) }`.
Either inline the call site (preferred — `prepare_edited_sources` is already
free-standing in each module) or have `HookEnvelope` provide a default body
that calls a free function the trait associates. Removes the wrapper
findings and lifts cognitive load on the hooks layer by a hair.

## 3. P1 — break the only dependency cycle

### P1-1. `lens-domain::function ↔ lens-domain::lsh`

`coupling` reports the only cycle in any crate:

```
crate::function (fan_in=3, fan_out=3) ↔ crate::lsh (fan_in=2, fan_out=2)
```

Edges:

- `function.rs:16` — `use crate::lsh::{LshOptions, lsh_candidate_pairs}`
- `lsh.rs:20`     — `use crate::function::FunctionDef`

Two reasonable shapes:

1. **Push `lsh_candidate_pairs` consumption up.** Move the LSH pre-filter
   call out of `function::find_similar_pair_indices` into a new
   `function::similarity` orchestrator (or into `agent-lens` directly),
   leaving `function` ignorant of LSH and `lsh` keeping its dependency on
   `FunctionDef`. Smallest diff.
2. **Move `FunctionDef` into `lens-domain::tree`.** Both `function` and
   `lsh` already depend on `tree`, so the type ends up in a shared root.
   Larger blast radius but yields the cleanest layering.

Option (1) is the one to ship first; (2) is a follow-up if the layering
question comes up again.

## 4. P2 — break up the cohesion walkers

`complexity` says the four worst functions in the workspace are walkers in
the cohesion analyzers:

| Function                                                | cc   | cog | MI   |
| ------------------------------------------------------- | ---: | --: | ---: |
| `lens-py/src/cohesion.rs:LocalNameWalker::walk_stmt`    |   45 |  62 | 21.4 |
| `lens-ts/src/cohesion.rs:LocalNameWalker::walk_stmt`    |   34 |  46 | 29.0 |
| `lens-py/src/cohesion.rs:ModuleRefVisitor::visit_expr`  |   10 |  16 | 45.0 |
| `lens-py/src/cohesion.rs:LocalNameWalker::walk_expr`    |   18 |  14 | 37.0 |

These dominate the hotspot ranking too (`lens-py/src/cohesion.rs` score
186, `lens-ts/src/cohesion.rs` 184). The big switch in `walk_stmt` already
groups by AST node kind; splitting per group (`walk_stmt_loop`,
`walk_stmt_branch`, `walk_stmt_decl`, `walk_stmt_try`) drops cognitive into
the 15–20 band and keeps the same control flow.

`cohesion` also flags `LocalWalker` and `ModuleRefVisitor` traits at LCOM4 = 5;
splitting `walk_stmt` should bring those down naturally because the
disjoint sub-walkers stop sharing the catch-all visitor body.

## 5. P3 — small reuse and fan-in cleanups

### P3-1. Inline the trivial `Self::default` constructors

`wrapper` flags four `T::new()` bodies that are just `Self::default()`:

- `hooks/core/similarity.rs:41` `SimilarityCore::new`
- `analyze/hotspot.rs:64`        `HotspotAnalyzer::new`
- `lens-ts/src/parser.rs:119`    `TypeScriptParser::new`
- `lens-domain/src/lsh.rs:186`   `HashFamily::len` → `self.a.len`
- `lens-domain/src/cohesion.rs:140` `CohesionUnit::lcom4` → `self.components.len`
- `lens-domain/src/complexity.rs:111` `FunctionComplexity::halstead_volume`

For `::new` cases prefer `Default` directly at call sites and delete the
wrappers; for the field-reading ones the wrapper is documentation, so leave
unless they cost a trait boundary.

### P3-2. Centralise `is_test_function*` / `from_path` helpers

`similarity` cluster `(89–100%)` shows:

- `agent-lens/src/analyze/mod.rs:72` `SourceLang::from_path`
- `lens-ts/src/parser.rs:80`         `Dialect::from_path`
- The four `extract_file_path` / `extract_patch_command` shapes in the
  hooks layer.

And cluster `(100%)`:

- `lens-py/src/{attrs,wrapper,parser}.rs::is_test_function`
- `lens-py/src/cohesion.rs::is_test_function_decl`

Pull `is_test_function` into one `lens-py` location (`attrs.rs` already
has the right scope). The extension-based `from_path`/`from_extension`
helpers can stay separate since each carries language-specific dialect
detail, but the `extract_file_path` family in the hooks adapters can lift
into `hooks::core` as a single helper consuming a `ToolName`.

### P3-3. Reduce `crate::analyze` fan-in (Rust)

`coupling` for `agent-lens` shows `crate::analyze` at `fan_in = 18,
fan_out = 0` — i.e. every leaf module pulls helpers from the parent
module. After P0-1 this drops naturally; if it doesn't, split the helpers
(`SourceLang`, `OutputFormat`, `read_source`, `format_optional_f64`,
`resolve_crate_root`) into `analyze::common` so the parent itself stays
empty.

## 6. Out of scope

- The `<module>` LCOM4 hits at 4–7 are mostly reflexive: the cohesion
  analyzers themselves naturally have many free functions sharing few
  module-level statics. Don't treat these as a refactor target unless one
  of them shows up in the hotspot table.
- Test-dominated similarity clusters (the table-driven complexity / cohesion
  tests). These are intentional and `--exclude-tests` filters them when we
  care.
- Cross-crate coupling questions — `coupling` is intra-crate today.

## 7. Validation

After each P-block lands, re-run:

```bash
agent-lens analyze hotspot       crates --since=180.days.ago --top 10 --format md
agent-lens analyze similarity    crates --exclude-tests       --format md
agent-lens analyze wrapper       crates                        --format md
agent-lens analyze coupling      crates/lens-domain            --format md
agent-lens analyze complexity    crates/lens-py/src/cohesion.rs --format md
```

Targets:

- P0-1 + P0-2 + P0-3: similarity clusters drop from 52 → ≤ 40, wrappers
  11 → ≤ 5.
- P1-1: `cycle_count` for `lens-domain` drops from 1 to 0.
- P2:   `cognitive_max` for `lens-py/src/cohesion.rs` drops from 62 to ≤ 25;
  hotspot top score drops below 100.
