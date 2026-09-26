# Call-graph accuracy harness

Measures `agent-lens analyze function-graph` against mechanical oracles. The
edge definition, the oracle JSON contract, the metrics and the adjudication
workflow are in [`docs/callgraph-accuracy.md`](../../docs/callgraph-accuracy.md).

```sh
mise run callgraph-accuracy [target ...]   # build the binary, then run.py
mise run test:callgraph-accuracy          # ruff check + scorer and Python oracle tests (in ci:rust)
(cd scripts/callgraph-accuracy/oracle-go && go test ./...)   # Go oracle tests (not in ci)
```

| File                 | Role                                                                                                                                                                               |
| -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `run.py`             | entry point: checkout at the pinned SHA, oracle, function-graph, score, combined summary                                                                                           |
| `score.py`           | scorer (stdlib only); also usable alone: `score.py --graph g.json --oracle o.json [--adjudications adjudications.toml --target NAME] [--json out.json] [--disagreements out.json]` |
| `test_score.py`      | unit tests for `score.py`                                                                                                                                                          |
| `test_oracle_py.py`  | unit tests for `oracle_py.py` (need pytest; run via `test:callgraph-accuracy`)                                                                                                     |
| `targets.toml`       | pinned projects (name, language, oracle, repo, commit, pytest args)                                                                                                                |
| `adjudications.toml` | human verdicts on disagreements, reused on every run                                                                                                                               |
| `oracle-go/`         | Go oracle: `go run . -root <dir> -out <json>` (go/callgraph/vta); `oracle_test.go` runs it on the `testdata/sample` fixture                                                        |
| `oracle_py.py`       | Python oracle: `python oracle_py.py --root <dir> --out <json> -- <pytest args>` (sys.setprofile)                                                                                   |

Results go to `target/callgraph-accuracy/` (gitignored): `summary.md`,
`summary.json`, and per target `results/<name>/{report.md,score.json,disagreements.json,graph.json,oracle.json}`.
Set `AGENT_LENS_BIN` to score a binary other than `target/release/agent-lens`.

To adjudicate: open `results/<name>/disagreements.json`, decide each entry,
paste its `toml` snippet into `adjudications.toml` with a `verdict` and a
`note`, and rerun. Only disagreements need a human.
