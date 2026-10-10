# External complexity benchmark harness

Times `agent-lens analyze complexity` against other cognitive-complexity tools
with hyperfine. The corpora, the reference tools and the latest results are in
[`docs/bench-external.md`](../../docs/bench-external.md).

```sh
mise run bench-external [ts] [go] [python]   # build the binary, then run.py
mise run test:bench-external                 # ruff check + unit tests (in ci:rust)
```

| File          | Role                                                                                    |
| ------------- | --------------------------------------------------------------------------------------- |
| `run.py`      | entry point: fetch tools and corpora, trim corpora, time with hyperfine, write a report |
| `test_run.py` | unit tests for the counting, trimming and rendering helpers                             |

Results go to `target/bench-external/` (gitignored): `report.md`,
`report.json`, and per language the tools, corpora and each corpus's
`<name>.hyperfine.json`. A second run reuses what is already downloaded.
Set `AGENT_LENS_BIN` to time a binary other than `target/release/agent-lens`.
