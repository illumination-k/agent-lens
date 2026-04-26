---
name: find-duplicates
description: Use when the user asks to find duplicated, near-duplicate, copy-pasted, or forwarding-only functions in this codebase — or before adding a new function, to check whether something similar already exists. Wraps `agent-lens analyze similarity` and `agent-lens analyze wrapper`.
---

# Find duplicate and forwarding functions

Two analyzers cover the "is this already written?" question:

- `similarity` — pairs of functions whose normalised AST has TSED ≥ threshold (default `0.85`). Catches type-3 clones (logic-equivalent, names differ).
- `wrapper` — functions whose body is `?` / `.into()` / `.unwrap()` / `.await` chained around a single forwarding call. Either inline or justify.

Both analyzers are Rust-only today and operate on a single `.rs` file at a time.

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

Loop over `.rs` files. For a crate:

```bash
find crates/<name>/src -name '*.rs' -print0 | xargs -0 -n1 \
  agent-lens analyze similarity --format md
```

Pipe through `grep -A2 '^- '` if you want a flat list across files.

## Tuning the threshold

- `--threshold 0.95` — only true clones. Use this when the report is too noisy.
- `--threshold 0.75` — catches reshuffled logic. Use this on a small file when the user explicitly wants to find loose duplicates.
- Default `0.85` — what the `PostToolUse` hook uses, so it matches what the agent will see during edits.

## Excluding tests

Table-driven tests dominate similarity reports. If a Rust file is mostly tests, add `--exclude-tests`:

```bash
agent-lens analyze similarity crates/lens-domain/src/apted.rs --exclude-tests
```

This drops `#[test]` / `#[rstest]` / `#[<runner>::test]` free functions and everything inside `#[cfg(test)] mod` blocks.

## What to do with the output

- **TSED ≥ 0.95** — almost certainly a clone. Extract a shared helper, or delete one.
- **TSED 0.85–0.95** — same shape, different specifics. Worth a closer look; sometimes legitimate (e.g. visitor cases that happen to mirror each other), sometimes an extracted parameter away from being one function.
- **wrapper hit, single call site** — inline it.
- **wrapper hit, many call sites** — keep, but verify the indirection is doing real work (lifetime adjustment, trait dispatch, error mapping). If not, the function is a tax.

## Don't reach for it when

- The duplication is across files in different crates — the analyzer is single-file. Run it per file and correlate by hand for now.
- The "duplication" is structural / architectural (e.g. two services that do the same job) — that's a coupling/coherence question, not a TSED one.
