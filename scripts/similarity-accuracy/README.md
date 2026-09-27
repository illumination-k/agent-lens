# Similarity accuracy harness

Scores `agent-lens analyze similarity` against labelled clone pairs. The
suites, the labels and the meaning of every metric are in
[`docs/similarity-accuracy.md`](../../docs/similarity-accuracy.md).

```sh
mise run similarity-accuracy [synthetic] [gptclonebench]   # build the binary, then run.py
mise run test:similarity-accuracy                          # ruff check + unit tests (in ci:rust)
```

| File               | Role                                                                                         |
| ------------------ | -------------------------------------------------------------------------------------------- |
| `run.py`           | entry point: build pairs, materialise them, score each `--method`, write the report          |
| `corpus.py`        | parse `corpus/<lang>/*.clone`, derive Type-1/Type-2 variants, build the labelled pairs       |
| `corpus/`          | the synthetic families, one `.clone` file per family per language                            |
| `gptclonebench.py` | fetch the pinned GPTCloneBench commit, split its Python pair files, sample the reference set |
| `score.py`         | ROC-AUC, recall / FPR per threshold, best-F1 threshold (stdlib only)                         |
| `test_harness.py`  | unit tests for the four modules above                                                        |

Results go to `target/similarity-accuracy/<suite>/` (gitignored):
`report.md`, `summary.json`, `scores.json` and the materialised `work/` tree.
Set `AGENT_LENS_BIN` to score a binary other than `target/release/agent-lens`.
