#!/usr/bin/env python3
"""Run the call-graph accuracy benchmark: `run.py [target ...]` (default: all).

For each target in targets.toml: check out the pinned commit, run its oracle,
run `agent-lens analyze function-graph`, and score the graph against the
oracle with score.py. Everything is written under target/callgraph-accuracy/
(gitignored); the combined per-language table is printed to stdout.

The agent-lens binary is not built here: `mise run callgraph-accuracy` builds
it first. Override its path with AGENT_LENS_BIN. Needs git, and go (Go
targets), uv (Python targets), rust-analyzer (Rust targets) or node and npm
(TypeScript targets) on PATH. See docs/callgraph-accuracy.md.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tomllib
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
OUT = REPO / "target" / "callgraph-accuracy"

sys.path.insert(0, str(HERE))
import score  # noqa: E402


def log(msg: str) -> None:
    print(f"==> {msg}", file=sys.stderr, flush=True)


def run(cmd: list, **kw) -> None:
    log(" ".join(map(str, cmd)))
    subprocess.run([str(c) for c in cmd], check=True, **kw)


def require(tool: str) -> str:
    path = shutil.which(tool)
    if path is None:
        sys.exit(f"{tool} not found on PATH (run through `mise run callgraph-accuracy`)")
    return path


def checkout(t: dict) -> Path:
    dest = OUT / "repos" / t["name"]
    if (dest / ".git").is_dir():
        head = subprocess.run(["git", "-C", dest, "rev-parse", "HEAD"], capture_output=True, text=True)
        if head.stdout.strip() == t["commit"]:
            return dest
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    run(["git", "init", "-q", dest])
    run(["git", "-C", dest, "fetch", "-q", "--depth", "1", t["repo"], t["commit"]])
    run(["git", "-C", dest, "checkout", "-q", "FETCH_HEAD"])
    return dest


def go_oracle(t: dict, root: Path, out: Path) -> None:
    run([require("go"), "run", ".", "-root", root, "-out", out], cwd=HERE / "oracle-go")


def python_oracle(t: dict, checkout_dir: Path, root: Path, out: Path) -> None:
    uv = require("uv")
    venv = OUT / "venvs" / t["name"]
    stamp = venv / ".callgraph-accuracy-commit"
    if not stamp.is_file() or stamp.read_text() != t["commit"]:
        shutil.rmtree(venv, ignore_errors=True)
        run([uv, "venv", "-q", "--python", t.get("python", "3.11"), venv])
        env = os.environ | {"VIRTUAL_ENV": str(venv)}
        run([uv, "pip", "install", "-q", "-e", checkout_dir, *t.get("install", ["pytest"])], env=env)
        stamp.write_text(t["commit"])
    python = venv / "bin" / "python"
    run([python, HERE / "oracle_py.py", "--root", root, "--out", out, "--", *t.get("pytest", [])], cwd=root)


def rust_oracle(t: dict, root: Path, out: Path) -> None:
    ra = os.environ.get("RUST_ANALYZER") or require("rust-analyzer")
    features = ["--features", ",".join(t["features"])] if "features" in t else []
    run([sys.executable, HERE / "oracle_rs.py", "--root", root, "--out", out, "--rust-analyzer", ra, *features])


def ts_oracle(t: dict, root: Path, out: Path) -> None:
    oracle_dir = HERE / "oracle-ts"
    if not (oracle_dir / "node_modules" / "typescript").is_dir():
        run([require("npm"), "ci", "--no-audit", "--no-fund", "--silent"], cwd=oracle_dir)
    run([require("node"), oracle_dir / "oracle.mjs", "--root", root, "--out", out])


def run_target(t: dict, binary: Path, adjudications: Path) -> dict:
    checkout_dir = checkout(t)
    root = checkout_dir / t.get("subdir", "")
    res = OUT / "results" / t["name"]
    res.mkdir(parents=True, exist_ok=True)
    oracle_json, graph_json = res / "oracle.json", res / "graph.json"

    if t["oracle"] == "go-vta":
        go_oracle(t, root, oracle_json)
    elif t["oracle"] == "python-setprofile":
        python_oracle(t, checkout_dir, root, oracle_json)
    elif t["oracle"] == "rust-analyzer":
        rust_oracle(t, root, oracle_json)
    elif t["oracle"] == "typescript-checker":
        ts_oracle(t, root, oracle_json)
    else:
        sys.exit(f"{t['name']}: unknown oracle {t['oracle']!r}")

    log(f"{binary} analyze function-graph {root}")
    with graph_json.open("w") as f:
        subprocess.run([str(binary), "analyze", "function-graph", str(root), "--format", "json"], check=True, stdout=f)

    graph = json.loads(graph_json.read_text())
    oracle = json.loads(oracle_json.read_text())
    result = score.score(graph, oracle, score.load_adjudications(adjudications, t["name"]), source_root=root)
    disagreements = result.pop("disagreements")
    for d in disagreements:
        d["toml"] = score.toml_snippet(t["name"], d)
    result |= {"target": t["name"], "commit": t["commit"]}
    (res / "score.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    (res / "disagreements.json").write_text(json.dumps(disagreements, indent=2) + "\n")
    (res / "report.md").write_text(score.render(result, t["name"]))
    return result


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("targets", nargs="*", help="target names from targets.toml (default: all)")
    ap.add_argument("--targets-file", type=Path, default=HERE / "targets.toml")
    ap.add_argument("--adjudications", type=Path, default=HERE / "adjudications.toml")
    args = ap.parse_args()

    targets = tomllib.loads(args.targets_file.read_text())["target"]
    if args.targets:
        unknown = set(args.targets) - {t["name"] for t in targets}
        if unknown:
            sys.exit(f"unknown target(s): {', '.join(sorted(unknown))}")
        targets = [t for t in targets if t["name"] in args.targets]

    binary = Path(os.environ.get("AGENT_LENS_BIN", REPO / "target" / "release" / "agent-lens"))
    if not binary.is_file():
        sys.exit(f"agent-lens binary not found at {binary}; build it with `cargo build --release -p agent-lens`")
    require("git")

    results = [run_target(t, binary, args.adjudications) for t in targets]

    by_language: dict[str, list[dict]] = defaultdict(list)
    for r in results:
        by_language[f"{r['language']} ({r['oracle']})"].append(r)
    combined = {lang: score.merge(rs) | {"targets": [r["target"] for r in rs]} for lang, rs in by_language.items()}

    sections = ["# Call-graph accuracy", "", "Per-target reports and disagreements: target/callgraph-accuracy/results/<target>/", ""]
    sections += [score.render(c, f"{lang}: {', '.join(c['targets'])}") for lang, c in combined.items()]
    summary = "\n".join(sections)
    (OUT / "summary.md").write_text(summary)
    (OUT / "summary.json").write_text(json.dumps({"languages": combined, "targets": results}, indent=2, sort_keys=True) + "\n")
    sys.stdout.write(summary)
    return 0


if __name__ == "__main__":
    sys.exit(main())
