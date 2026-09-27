# Call-graph accuracy harness

Measures `agent-lens analyze function-graph` against mechanical oracles. The
edge definition, the oracle JSON contract, the metrics and the adjudication
workflow are in [`docs/callgraph-accuracy.md`](../../docs/callgraph-accuracy.md).

```sh
mise run callgraph-accuracy [target ...]   # build the binary, then run.py
mise run test:callgraph-accuracy          # ruff check + scorer, Python (pyright) and Rust oracle tests (in ci:rust)
(cd scripts/callgraph-accuracy/oracle-go && go test ./...)   # Go oracle tests (not in ci)
(cd scripts/callgraph-accuracy/oracle-ts && npm ci && npm test)   # TypeScript oracle tests (not in ci)
```

| File                     | Role                                                                                                                                                                               |
| ------------------------ | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `run.py`                 | entry point: checkout at the pinned SHA, oracle, function-graph, score, combined summary                                                                                           |
| `score.py`               | scorer (stdlib only); also usable alone: `score.py --graph g.json --oracle o.json [--adjudications adjudications.toml --target NAME] [--json out.json] [--disagreements out.json]` |
| `test_score.py`          | unit tests for `score.py`                                                                                                                                                          |
| `lsp.py`                 | minimal LSP client shared by the pyright and Rust oracles                                                                                                                          |
| `targets.toml`           | pinned projects (name, language, oracle, repo, commit)                                                                                                                             |
| `adjudications.toml`     | human verdicts on disagreements, reused on every run                                                                                                                               |
| `oracle-go/`             | Go oracle: `go run . -root <dir> -out <json>` (go/callgraph/vta); `oracle_test.go` runs it on the `testdata/sample` fixture                                                        |
| `oracle_pyright.py`      | Python oracle: `python oracle_pyright.py --root <dir> --out <json> [--python <venv python>]` (pyright over LSP, stdlib only)                                                       |
| `test_oracle_pyright.py` | unit tests for `oracle_pyright.py`; the end-to-end ones skip without `pyright-langserver` (`test:callgraph-accuracy` supplies it)                                                  |
| `oracle_rs.py`           | Rust oracle: `python oracle_rs.py --root <cargo dir> --out <json> [--features a,b]` (rust-analyzer over LSP, stdlib only)                                                          |
| `test_oracle_rs.py`      | unit tests for `oracle_rs.py`; the end-to-end one skips without rust-analyzer                                                                                                      |
| `oracle-ts/`             | TypeScript oracle: `node oracle.mjs --root <dir> --out <json>` (TypeScript compiler API); `oracle.test.mjs` runs it on an inline fixture                                           |

Results go to `target/callgraph-accuracy/` (gitignored): `summary.md`,
`summary.json`, and per target `results/<name>/{report.md,score.json,disagreements.json,graph.json,oracle.json}`.
Set `AGENT_LENS_BIN` to score a binary other than `target/release/agent-lens`.

To adjudicate: open `results/<name>/disagreements.json`, decide each entry,
paste its `toml` snippet into `adjudications.toml` with a `verdict` and a
`note`, and rerun. Only disagreements need a human.
