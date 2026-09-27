# Similarity accuracy

`agent-lens analyze similarity` turns a pair of units into one score in
`[0, 1]`, and a threshold turns that score into a finding. This benchmark
measures both halves on labelled clone pairs: how well each `--method` ranks
clones above non-clones (ROC-AUC), and what a given threshold catches and
wrongly flags (recall and false-positive rate).

```sh
mise run similarity-accuracy                         # synthetic suite, offline, seconds
mise run similarity-accuracy synthetic gptclonebench # plus the external reference (network)
mise run similarity-accuracy --methods tsed,lcs      # a subset of methods
```

The task builds the release binary and runs `scripts/similarity-accuracy/run.py`
(set `AGENT_LENS_BIN` to score another binary). Output goes to
`target/similarity-accuracy/<suite>/` and is never committed:

| Path           | Content                                                  |
| -------------- | -------------------------------------------------------- |
| `report.md`    | the tables below, plus the worst-scored pairs            |
| `summary.json` | every metric per language and method                     |
| `scores.json`  | one row per method × pair: label, kind, ids, score       |
| `work/`        | the materialised pair files, to rerun agent-lens by hand |

Like `callgraph-accuracy`, it is a report, not a gate, and sits outside
`ci`. The harness's unit tests (`test:similarity-accuracy`) run under
`ci:rust`.

## How a pair is scored

Each pair is written as two files, `p<i>/a.<ext>` and `p<i>/b.<ext>`, whose
function is renamed to `pair_<i>`, and the whole tree is scored with

```sh
agent-lens analyze similarity <dir> --paired-by qualified --drift-floor 0 --min-lines 1 --method <m>
```

Name-anchored pairing reports the score of every name match regardless of
threshold, so each labelled pair gets exactly its own score, the one a
clustering run would compare against `--threshold`. The name is the same on
both sides and unique to the pair, so it neither leaks the label nor pairs
unrelated units. A pair agent-lens reports nothing for (a file it cannot
parse) scores 0 and is counted in the report.

Paired mode scores at threshold 0, so this measures the scorer, not
candidate generation: LSH and the cheap bound filters that decide which
pairs reach the scorer in a threshold run are bypassed, and the recall they
lose is not measured here. `--idf` (off by default) would make a token score
depend on the rest of the corpus; the harness does not pass it.

## Suites

### `synthetic` (headline)

`scripts/similarity-accuracy/corpus/<lang>/<family>.clone`, for Rust,
TypeScript, Python and Go. Each family is one small function written once per
language (sum of positives, argmax, word count, binary search, key=value
parsing, chunking) and its variants:

| Variant     | Label    | Made by | What changes                                                   |
| ----------- | -------- | ------- | -------------------------------------------------------------- |
| `t1`        | clone    | derived | a comment and a blank line added                               |
| `t2`        | clone    | derived | identifiers renamed per the family's `@@ rename` map           |
| `t3-insert` | clone    | hand    | statements added (a guard, a counter, a log line)              |
| `t3-modify` | clone    | hand    | statements changed or removed                                  |
| `t4`        | clone    | hand    | same behaviour, different implementation (iterator, recursion) |
| `hard`      | negative | hand    | same control-flow shape, different purpose                     |

Positives pair `orig` with each clone variant; `hard` pairs `orig` with the
family's `hard`; `cross` negatives pair every `orig`/`t3-*`/`t4` of one
family with every one of another family in the same language. Type-1 and
Type-2 are derived so they cannot drift from the original.

`hard` is a judgement call, not a clone-benchmark convention: a function
with the same skeleton but a different job is not something to extract a
helper for, so a score above threshold is noise to an agent. It is reported
apart from `cross` (`vs_hard`, `FPR hard`) so the small set does not dilute
the headline AUC.

### `gptclonebench` (reference)

Python stand-alone pairs of [GPTCloneBench](https://github.com/srlabUsask/gptclonebench)
(ICSME 2023) at a pinned commit, fetched into `target/`; the data is
CC BY-NC-ND and is not committed. `--per-kind` pairs (default 500, seed
`--seed`) are sampled per kind:

- `mt3`, `t4`: the validated true semantic clones (moderately Type-3 and
  Type-4), positives;
- `cross`: the first function of one pair against the second of another
  (never the same seed function), negatives;
- `gcb_false`: pairs the validators rejected as _semantic_ clones. Many are
  textual near-copies, so this is a sanity row, not a negative set, and is
  never pooled into the AUC.

A third of the archive is not valid Python 3 (`! =`, Python 2 `print`, tabs
mixed with spaces). agent-lens skips a file it cannot parse, which would
charge the dataset's damage to the tool, so only pairs whose two functions
pass `ast.parse` (as is or with tabs expanded) are sampled.

Read it with the dataset's shape in mind: its positives are semantic clones
that GPT rewrote, so a syntactic method can rank them above `cross` (AUC
around 0.9) and still score most of them below the default 0.85, while
`gcb_false` scores above the positives (`vs_gcb_false` near 0.05). It says
how the methods order rewritten code, not what a threshold run reports on a
real codebase.

## Metrics

| Column          | Meaning                                                                                    |
| --------------- | ------------------------------------------------------------------------------------------ |
| AUC `<type>`    | P(score of a positive of that type > score of a `cross` negative), ties count ½            |
| AUC `all`       | the same over all positive types pooled                                                    |
| AUC `vs_<kind>` | all positives against that negative kind alone                                             |
| best F1 @ thr   | the threshold maximising F1 of all positives against `cross`; a tuning hint, not a default |
| recall `<type>` | share of that type's positives scoring at or above the threshold                           |
| FPR `<kind>`    | share of that negative kind scoring at or above the threshold                              |

AUC says whether a method ranks clones first at all; the threshold table
says what the defaults (`--threshold 0.85`) actually report. Scores from
different methods are not on one scale, so compare methods by AUC and pick a
threshold per method.

## Adding cases

A new family is one `.clone` file per language: `@@ rename a=b ...`, then
`@@ orig`, `@@ t3-insert`, `@@ t3-modify`, `@@ t4`, `@@ hard`, each followed
by a function named `subject` (Go files get a `package` clause prepended).
Keep functions at least three lines, since `t1` inserts after the first. A
bug report about a missed clone or a false positive is best turned into a
family here, so the regression is measured from then on.
