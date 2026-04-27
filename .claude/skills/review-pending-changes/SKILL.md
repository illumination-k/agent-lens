---
name: review-pending-changes
description: Use when the user wants to audit pending git changes (unstaged edits, work-in-progress) for quality issues before committing — duplicated functions, thin wrappers, complexity creep, weakened cohesion. Composes the `--diff-only` modes of `agent-lens analyze similarity / wrapper / cohesion / complexity`.
---

# Review pending changes with agent-lens

Goal: surface only the noise that the current `git diff` introduced, not the whole file's history. Every analyzer used here scopes to functions or `impl` blocks that overlap unstaged hunks (`git diff -U0`).

## When to run

- Before the user asks to commit or push.
- After the user finishes a multi-file edit and asks "did I break anything?"
- As a sanity pass after the agent itself made a large edit.

The PostToolUse hook already runs `similarity` + `wrapper` on every Edit/Write, so don't re-run those for a single just-edited file — those reports are already in context. Reach for this skill when the change is broader than one file or when the user explicitly wants a sweep.

## Workflow

### 1. Find the touched source files

```bash
git diff --name-only --diff-filter=AM | grep -E '\.(rs|ts|tsx|js|jsx|py)$'
```

All four diff-only analyzers (`similarity`, `wrapper`, `cohesion`, `complexity`) accept Rust, TypeScript / JavaScript, and Python — no need to fan out by extension.

### 2. Run the diff-scoped analyzers per file

For each touched source file:

```bash
agent-lens analyze similarity <path> --diff-only --format md
agent-lens analyze wrapper    <path> --diff-only --format md
agent-lens analyze cohesion   <path> --diff-only --format md
agent-lens analyze complexity <path> --diff-only --format md
```

If a report is empty, skip it silently — empty diff-only output is the success case.

### 3. Crate-level coupling (no `--diff-only`)

`coupling` doesn't have a diff mode; it's a whole-crate metric. Only re-run it if the diff added or removed `mod` declarations, `pub use` re-exports, or moved files between modules:

```bash
git diff --name-only | grep -q -E '(lib|main|mod)\.rs|src/.*\.rs' && \
  agent-lens analyze coupling crates/<name> --format md
```

### 4. Aggregate and decide

For each finding, classify:

- **Block-on-commit**: new clone (TSED ≥ 0.95), new function with cognitive ≥ 25, new `impl` whose LCOM4 jumped from 1 to ≥ 2, new dependency cycle in the coupling cycles list.
- **Worth a callout**: cognitive 15–25, MI < 65, new wrapper with one call site, fan-out increase that pushes a module past the rest of the crate.
- **Noise**: TSED < 0.85 once `--exclude-tests` is on; minor cognitive deltas on already-complex functions.

Surface block-on-commit findings to the user before they commit. Mention worth-a-callout findings once, then move on.

## Combining with `--exclude-tests`

If the diff is in a file dominated by `#[cfg(test)] mod tests`, similarity will fire on the table-driven test cases. Add `--exclude-tests` to silence that:

```bash
agent-lens analyze similarity <path> --diff-only --exclude-tests --format md
```

## Don't reach for it when

- The user is mid-edit and hasn't paused — the PostToolUse hook is already running similarity + wrapper after every save. Adding more analyzers here would be redundant noise.
- The change is documentation-only or config-only — none of these analyzers will have anything useful to say.
- The diff is empty — `--diff-only` reports will all be empty by definition.
