---
name: find-duplicates
description: Use when the user asks to find duplicated, near-duplicate, copy-pasted, or forwarding-only functions in this codebase — or before adding a new function, to check whether something similar already exists. Wraps `agent-lens analyze similarity` and `agent-lens analyze wrapper`.
---

# Find duplicate and forwarding functions

Two analyzers cover the "is this already written?" question:

- `similarity` — pairs of functions whose normalised AST has TSED ≥ threshold (default `0.85`). Catches type-3 clones (logic-equivalent, names differ). Functions shorter than `--min-lines` (default `5`) are skipped to keep getters and one-liners out of the report.
- `wrapper` — functions whose body is `?` / `.into()` / `.unwrap()` / `.await` chained around a single forwarding call. Either inline or justify.

Both analyzers parse Rust, TypeScript / JavaScript, Python, and Go (parser is selected from the file extension). Both accept either a single file or a directory; in directory mode they walk recursively (respecting `.gitignore` like ripgrep) and group findings per file. `similarity` additionally reports cross-file pairs alongside in-file ones.

## Workflow

### 1. If the user is about to add a function

Run similarity on the file the new function would live in, with the default threshold:

```bash
agent-lens analyze similarity <path> --format md
```

Read the report. If a candidate scores ≥ 0.85, surface it to the user before writing any code: "There's already `foo::bar` at `<path>:42` that does this — fork or extend?"

### 2. If the user is reviewing an in-progress edit

Restrict to the changed functions only — the rest of the file is noise:

```bash
agent-lens analyze similarity <path> --diff-only --format md
agent-lens analyze wrapper    <path> --diff-only --format md
```

### 3. If the user is auditing a whole file or crate

Both analyzers accept a directory, so you don't need to loop manually. `similarity` reports cross-file pairs alongside in-file ones; `wrapper` groups findings per file:

```bash
agent-lens analyze similarity crates/<name>/src --format md
agent-lens analyze wrapper    crates/<name>/src --format md
```

## Tuning the threshold

- `--threshold 0.95` — only true clones. Use this when the report is too noisy.
- `--threshold 0.75` — catches reshuffled logic. Use this on a small file when the user explicitly wants to find loose duplicates.
- Default `0.85` — what the `PostToolUse` hook uses, so it matches what the agent will see during edits.

## Sweeping multiple thresholds

When you don't know the right cut — or want to tell verbatim clones apart from structurally parallel implementations in one pass — sweep a ladder instead of guessing a single `--threshold`:

```bash
agent-lens analyze similarity <path> --format md --sweep 0.6,0.75,0.85
```

This clusters once at the lowest rung (`0.6`) and tags each cluster with the highest rung at which its complete-link structure survives. A cluster tagged `[survives ≥0.85]` is a near-verbatim clone (extract now); `[survives ≥0.6]` is a structural parallel that needs a shared abstraction rather than a literal extraction. `--sweep` conflicts with `--threshold` (it replaces the single cut). Reach for it when the default run reports _nothing_ between two files you suspect are duplicated — the looser pairs only show up at the lower rungs.

## Excluding tests

Table-driven tests dominate similarity reports. If a Rust file is mostly tests, add `--exclude-tests`:

```bash
agent-lens analyze similarity crates/lens-domain/src/apted.rs --exclude-tests
```

This drops `#[test]` / `#[rstest]` / `#[<runner>::test]` free functions and everything inside `#[cfg(test)] mod` blocks.

## What to do with the output

- **TSED ≥ 0.95** — almost certainly a clone. Extract a shared helper, or delete one.
- **TSED 0.85–0.95** — same shape, different specifics. Worth a closer look; sometimes legitimate (e.g. visitor cases that happen to mirror each other), sometimes an extracted parameter away from being one function.
- **`doc_overlap`** (per pair in JSON, present when both functions have a doc comment / docstring; add `--doc-overlap` to roll it up per cluster in `--format md`) — word-level overlap of the two docs. It never affects the score; read it as a tiebreaker: high `doc_overlap` on a high-similarity pair means the _stated intent_ matches too (strong merge candidate, often a copy-paste including the doc), while low `doc_overlap` on a high-similarity pair flags a structural coincidence — two functions that happen to share a shape but do different jobs, which usually should not be merged.

  ```bash
  agent-lens analyze similarity crates/<name>/src --format md --doc-overlap
  ```

  The markdown rollup reads `doc overlap 20–80% (3/3 pairs documented)`: the range across the cluster's pairs, then how many of them had doc text on both sides. `n/a (0/N pairs documented)` means nothing in the cluster is documented — the tiebreaker is unavailable, not zero.
- **wrapper hit, single call site** — inline it.
- **wrapper hit, many call sites** — keep, but verify the indirection is doing real work (lifetime adjustment, trait dispatch, error mapping). If not, the function is a tax.

## Confirming call-site count for a wrapper hit

`wrapper` reports a hit but doesn't tell you _how many_ call sites it has — and "many call sites" vs "one call site" decides between inline and keep. Use `function-graph` to count:

```bash
agent-lens analyze function-graph crates/<name>/src \
  | jq --arg fn "<wrapper-fn-name>" '
      [.edges[] | select(.callee_name == $fn)] | length'
```

If the count is `1`, inline the wrapper. If `2+`, look at the call sites (`.edges[] | select(.callee_name == $fn) | {from, call_lines}`) before deciding. Resolution is heuristic, so treat the count as "at least N", not exact.

## Don't reach for it when

- The "duplication" is structural / architectural (e.g. two services that do the same job) — that's a coupling/coherence question, not a TSED one.
- The file isn't Rust / TypeScript / JavaScript / Python / Go — the analyzer errors out cleanly on unsupported extensions.
