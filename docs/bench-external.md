# External complexity benchmark

How fast `agent-lens analyze complexity` is next to the tools a team would
otherwise reach for to get cognitive complexity, on the same code:

| Language   | Reference tool                                                                | Corpora                     |
| ---------- | ----------------------------------------------------------------------------- | --------------------------- |
| TypeScript | ESLint 9.39.5 + eslint-plugin-sonarjs 4.2.2 (SonarSource's own rule, `S3776`) | zod 4.1.12, effect 3.18.4   |
| Go         | gocognit 1.3.0                                                                | cobra 1.8.1, x/tools 0.30.0 |
| Python     | complexipy 8.0.1                                                              | rich 13.9.4, Django 5.1.4   |

```sh
mise run bench-external              # all three languages (network, a few minutes)
mise run bench-external go python    # a subset
```

The task builds the release binary and runs `scripts/bench-external/run.py`
(set `AGENT_LENS_BIN` to time another binary). Output goes to
`target/bench-external/` and is never committed: `report.md` is the table
below, `report.json` its rows, and `<lang>/<corpus>.hyperfine.json` the raw
hyperfine export. Like `callgraph-accuracy`, it is a report, not a gate, and
sits outside `ci`; the harness's unit tests (`test:bench-external`) run under
`ci:rust`.

## Method

- **Same input.** Each corpus is trimmed to the one language both tools read
  before timing: tests (`*.test.ts`, `tests/`, `*_test.go`), Go `testdata/`,
  `.d.ts` files, and every other language's files are deleted. Neither tool
  gets an exclusion flag, so neither is timed walking files the other skips.
- **Timing.** `hyperfine -N --warmup 1 --min-runs 5`, one tool after the
  other on the same machine. complexipy keeps a results cache, so hyperfine's
  `--prepare` deletes it before every run: all numbers are cold.
- **Sanity check.** After timing, each tool's JSON output is parsed and the
  functions it scored above zero are counted (`functions > 0`). Close counts
  say both tools read the same functions; they are not expected to be equal,
  because the tools do not score the same constructs (below).
- **Not equal work.** agent-lens computes cyclomatic complexity, max nesting,
  Halstead volume and the maintainability index in the same pass; the
  reference tools compute cognitive complexity only.

## Results

Measured 2026-10-10 on a 4-vCPU Intel Xeon @ 2.80 GHz cloud container, with
agent-lens built from `main` at 0.9.0. `vs agent-lens` is the reference tool's
mean over agent-lens's: above `1×` the reference tool is slower.

| Language | Corpus  | Files |   Lines | Tool           |           Mean ± σ | vs agent-lens | Functions > 0 |
| -------- | ------- | ----: | ------: | -------------- | -----------------: | ------------: | ------------: |
| ts       | zod     |   108 |  28,273 | agent-lens     |   36.1 ms ± 7.2 ms |          1.0× |           494 |
| ts       | zod     |   108 |  28,273 | eslint+sonarjs |  4,670 ms ± 136 ms |        129.4× |           494 |
| ts       | effect  |   360 | 233,725 | agent-lens     |   139 ms ± 13.8 ms |          1.0× |         2,207 |
| ts       | effect  |   360 | 233,725 | eslint+sonarjs | 21,680 ms ± 957 ms |        155.6× |         2,453 |
| go       | cobra   |    19 |   6,564 | agent-lens     |   51.0 ms ± 9.3 ms |          1.0× |           148 |
| go       | cobra   |    19 |   6,564 | gocognit       |   18.8 ms ± 3.8 ms |          0.4× |           148 |
| go       | x/tools |   565 | 150,773 | agent-lens     | 1,329 ms ± 30.1 ms |          1.0× |         2,731 |
| go       | x/tools |   565 | 150,773 | gocognit       |   362 ms ± 35.3 ms |          0.3× |         2,732 |
| python   | rich    |    78 |  26,607 | agent-lens     |   37.1 ms ± 6.1 ms |          1.0× |           415 |
| python   | rich    |    78 |  26,607 | complexipy     |   71.4 ms ± 5.7 ms |          1.9× |           413 |
| python   | Django  |   879 | 155,128 | agent-lens     |   259 ms ± 12.0 ms |          1.0× |         4,718 |
| python   | Django  |   879 | 155,128 | complexipy     |   371 ms ± 29.8 ms |          1.4× |         4,854 |

## Reading the results

- **TypeScript: ~130–160× faster than ESLint + sonarjs.** oxc parses fast, and
  agent-lens pays none of ESLint's start-up and plugin-dispatch cost. ESLint's
  numbers include Node start-up (a few hundred milliseconds of each run).
- **Python: 1.4–1.9× faster than complexipy**, which is itself native (Rust),
  while doing the extra metrics.
- **Go: 2.7–3.7× slower than gocognit.** gocognit is a thin walk over Go's
  own `go/parser`; agent-lens parses with tree-sitter and walks every token
  for Halstead counts. The run is also single-threaded: `analyze complexity`
  reads and scores files one after another (`collect_per_file`), so 565 files
  cost the sum of their parses. Parallelising the per-file walk and cutting
  the per-token allocations in the Halstead tally are the two levers; both
  apply to every language, Go just has the least headroom to hide them.

The `functions > 0` counts agree to within a few percent, so every tool read
the same code; the gaps that remain follow from what each tool scores:

- **gocognit** agrees exactly on cobra and to within one function on x/tools.
- **eslint+sonarjs** reports nested functions as units of their own (agent-lens
  folds them into their parent) and never charges `||` or `??`, only `&&`
  runs, so the two counts move in opposite directions; on effect it reports
  about 11% more.
- **complexipy** is within 3% on both corpora; the per-function differences
  were not investigated.

The scores themselves are not compared here; this benchmark is about speed.
