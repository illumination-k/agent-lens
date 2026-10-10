#!/usr/bin/env python3
"""Time `agent-lens analyze complexity` against other cognitive-complexity tools.

`run.py [language ...]` (default: ts go python). For each language, fetch the
pinned corpora and the pinned reference tool, trim every corpus to the one
language both tools read (tests, testdata, and other languages' files are
deleted, so neither side needs an exclusion flag), then time both commands
with hyperfine and count the functions each one scored above zero as a check
that they read the same code.

| language | reference tool                                   | corpora                    |
| -------- | ------------------------------------------------ | -------------------------- |
| ts       | ESLint + eslint-plugin-sonarjs (SonarSource's own) | zod, effect (npm sources)  |
| go       | gocognit                                         | cobra, golang.org/x/tools  |
| python   | complexipy                                       | rich, Django (sdists)      |

Everything is written under target/bench-external/ (gitignored); the report is
printed to stdout and saved as target/bench-external/report.md. The agent-lens
binary is not built here: `mise run bench-external` builds it first. Override
its path with AGENT_LENS_BIN. Needs hyperfine, and node + npm (ts), go (go),
or uv + pip (python) on PATH. See docs/bench-external.md.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
OUT = REPO / "target" / "bench-external"

ESLINT = ["eslint@9.39.5", "eslint-plugin-sonarjs@4.2.2", "typescript-eslint@8.71.1", "typescript@6.0.3"]
GOCOGNIT = "github.com/uudashr/gocognit/cmd/gocognit@v1.3.0"
COMPLEXIPY = "complexipy==8.0.1"

# name, package spec, sub-directory holding the sources
TS_CORPORA = [("zod", "zod@4.1.12", "src"), ("effect", "effect@3.18.4", "src")]
GO_CORPORA = [("cobra", "github.com/spf13/cobra@v1.8.1", "."), ("x-tools", "golang.org/x/tools@v0.30.0", ".")]
PY_CORPORA = [("rich", "rich==13.9.4", "rich"), ("django", "Django==5.1.4", "django")]

ESLINT_CONFIG = """\
import tseslint from "typescript-eslint";
import sonarjs from "eslint-plugin-sonarjs";
export default [
  {
    files: ["**/*.ts"],
    languageOptions: { parser: tseslint.parser },
    plugins: { sonarjs },
    rules: { "sonarjs/cognitive-complexity": ["error", 0] },
  },
];
"""


def log(msg: str) -> None:
    print(f"==> {msg}", file=sys.stderr, flush=True)


def run(cmd: list, **kw) -> subprocess.CompletedProcess:
    log(" ".join(map(str, cmd)))
    return subprocess.run([str(c) for c in cmd], check=True, **kw)


def require(tool: str) -> str:
    path = shutil.which(tool)
    if path is None:
        sys.exit(f"{tool} not found on PATH (run through `mise run bench-external`)")
    return path


def keep_only(root: Path, suffix: str, drop_dirs: set[str], drop_name) -> None:
    """Delete every file under `root` but `suffix` sources, plus whole
    directories named in `drop_dirs` and files `drop_name` accepts, so both
    tools see exactly the same tree."""
    for d in sorted((p for p in root.rglob("*") if p.is_dir() and p.name in drop_dirs), reverse=True):
        if d.exists():
            shutil.rmtree(d)
    for f in root.rglob("*"):
        if f.is_file() and (not f.name.endswith(suffix) or drop_name(f.name)):
            f.unlink()


def source_stats(root: Path, suffix: str) -> tuple[int, int]:
    files = [f for f in root.rglob(f"*{suffix}") if f.is_file()]
    lines = sum(f.read_bytes().count(b"\n") for f in files)
    return len(files), lines


# --- counting functions scored above zero -------------------------------------


def count_agent_lens(report: dict) -> int:
    return sum(1 for f in report["files"] for fn in f["functions"] if fn["cognitive"] > 0)


def count_eslint(report: list) -> int:
    # Threshold 0: the rule reports every function scoring at least 1.
    return sum(
        1 for f in report for m in f["messages"] if m.get("ruleId") == "sonarjs/cognitive-complexity"
    )


def count_gocognit(report: list) -> int:
    # gocognit lists only functions scoring at least 1.
    return len(report)


def count_complexipy(report: list) -> int:
    return sum(1 for fn in report if fn["complexity"] > 0)


# --- per-language setup -------------------------------------------------------


def setup_ts(work: Path) -> tuple[list[tuple[str, Path]], dict]:
    require("npm")
    tools = work / "tools"
    if not (tools / "node_modules" / ".bin" / "eslint").exists():
        tools.mkdir(parents=True, exist_ok=True)
        (tools / "package.json").write_text('{"private": true}\n')
        run(["npm", "install", "--silent", "--no-audit", "--no-fund", *ESLINT], cwd=tools)
    (tools / "eslint.config.mjs").write_text(ESLINT_CONFIG)
    corpora = []
    for name, spec, sub in TS_CORPORA:
        # ESLint only lints files under its working directory, so the
        # corpora live inside `tools`.
        root = tools / "corpus" / name
        if not root.exists():
            pack = work / "pack"
            pack.mkdir(parents=True, exist_ok=True)
            tgz = run(["npm", "pack", "--silent", spec], cwd=pack, capture_output=True, text=True).stdout.split()[-1]
            with tarfile.open(pack / tgz) as tar:
                tar.extractall(pack / name, filter="data")
            shutil.move(pack / name / "package" / sub, root)
            keep_only(
                root,
                ".ts",
                {"tests", "test", "__tests__"},
                lambda n: n.endswith(".d.ts") or ".test." in n or ".spec." in n,
            )
        corpora.append((name, root))
    eslint = tools / "node_modules" / ".bin" / "eslint"

    def command(root: Path) -> list:
        return [eslint, "-f", "json", root.relative_to(tools)]

    return corpora, {
        "tool": "eslint+sonarjs",
        "command": command,
        "cwd": tools,
        "scored": lambda root: count_eslint(capture_json(command(root), tools)),
    }

def setup_go(work: Path) -> tuple[list[tuple[str, Path]], dict]:
    require("go")
    env = os.environ | {"GOPATH": str(work / "gopath"), "GOBIN": str(work / "bin"), "GOFLAGS": "-modcacherw"}
    gocognit = work / "bin" / "gocognit"
    if not gocognit.exists():
        run(["go", "install", GOCOGNIT], env=env)
    corpora = []
    for name, spec, sub in GO_CORPORA:
        root = work / "corpus" / name
        if not root.exists():
            info = run(["go", "mod", "download", "-json", spec], env=env, capture_output=True, text=True)
            shutil.copytree(Path(json.loads(info.stdout)["Dir"]) / sub, root)
            for p in root.rglob("*"):
                p.chmod(p.stat().st_mode | 0o200)
            keep_only(root, ".go", {"testdata"}, lambda n: n.endswith("_test.go"))
        corpora.append((name, root))
    return corpora, {
        "tool": "gocognit",
        "command": lambda root: [gocognit, root],
        "cwd": work,
        "scored": lambda root: count_gocognit(capture_json([gocognit, "-json", root], work)),
    }


def setup_python(work: Path) -> tuple[list[tuple[str, Path]], dict]:
    require("uv")
    venv = work / "venv"
    complexipy = venv / "bin" / "complexipy"
    if not complexipy.exists():
        run(["uv", "venv", "--quiet", venv])
        run(["uv", "pip", "install", "--quiet", "--python", venv / "bin" / "python", COMPLEXIPY])
    corpora = []
    for name, spec, sub in PY_CORPORA:
        root = work / "corpus" / name
        if not root.exists():
            dl = work / "sdist" / name
            run([sys.executable, "-m", "pip", "download", "--quiet", "--no-deps", "--no-binary", ":all:", spec, "-d", dl])
            (tgz,) = dl.glob("*.tar.gz")
            with tarfile.open(tgz) as tar:
                tar.extractall(dl, filter="data")
            (src,) = (p for p in dl.iterdir() if p.is_dir())
            shutil.move(src / sub, root)
            keep_only(root, ".py", set(), lambda n: False)
        corpora.append((name, root))
    cache = work / "complexipy-cache"

    def command(root: Path, output: Path = Path("/dev/null")) -> list:
        return [complexipy, root, "--quiet", "--cache-dir", cache, "--output-format", "json", "--output", output]

    def scored(root: Path) -> int:
        shutil.rmtree(cache, ignore_errors=True)
        out = work / f"{root.name}.complexipy.json"
        out.unlink(missing_ok=True)
        # Exits 1 when any function is over its default limit of 15.
        subprocess.run([str(a) for a in command(root, out)], capture_output=True)
        return count_complexipy(json.loads(out.read_text()))

    return corpora, {
        "tool": "complexipy",
        "command": command,
        # complexipy caches results; `prepare` clears it so every run is cold.
        "prepare": f"rm -rf {cache}",
        "cwd": work,
        "scored": scored,
    }


LANGUAGES = {"ts": (".ts", setup_ts), "go": (".go", setup_go), "python": (".py", setup_python)}


# --- timing and report --------------------------------------------------------


def hyperfine(commands: list[tuple[str, list]], prepare: str | None, cwd: Path, out: Path) -> dict:
    # ESLint and gocognit exit non-zero whenever they report a function, so a
    # non-zero status is not a failure here; the scored-function counts taken
    # afterwards are the check that every tool actually read the corpus.
    cmd = ["hyperfine", "-N", "--ignore-failure", "--warmup", "1", "--min-runs", "5", "--export-json", out, "--style", "basic"]
    if prepare:
        cmd += ["--prepare", prepare]
    for name, argv in commands:
        cmd += ["--command-name", name, " ".join(f"'{a}'" for a in map(str, argv))]
    run(cmd, cwd=cwd, stdout=sys.stderr)
    return json.loads(out.read_text())


def capture_json(argv: list, cwd: Path):
    # ESLint and gocognit exit non-zero when they report anything, so the
    # exit status is not checked; an empty or broken stdout fails the parse.
    proc = subprocess.run([str(a) for a in argv], cwd=cwd, capture_output=True, text=True)
    return json.loads(proc.stdout)


def render(rows: list[dict]) -> str:
    lines = [
        "| language | corpus | files | lines | tool | mean ± σ | vs agent-lens | functions > 0 |",
        "| --- | --- | ---: | ---: | --- | ---: | ---: | ---: |",
    ]
    for r in rows:
        ratio = "1.0×" if r["tool"] == "agent-lens" else f"{r['mean'] / r['baseline']:.1f}×"
        lines.append(
            f"| {r['language']} | {r['corpus']} | {r['files']} | {r['lines']} | {r['tool']} "
            f"| {fmt_ms(r['mean'])} ± {fmt_ms(r['stddev'])} | {ratio} | {r['scored']} |"
        )
    return "\n".join(lines) + "\n"


def fmt_ms(seconds: float) -> str:
    return f"{seconds * 1000:,.0f} ms" if seconds >= 0.1 else f"{seconds * 1000:.1f} ms"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("languages", nargs="*", help=f"any of {', '.join(LANGUAGES)} (default: all)")
    args = ap.parse_args()
    languages = args.languages or list(LANGUAGES)
    unknown = set(languages) - set(LANGUAGES)
    if unknown:
        sys.exit(f"unknown language(s): {', '.join(sorted(unknown))}")

    binary = Path(os.environ.get("AGENT_LENS_BIN", REPO / "target" / "release" / "agent-lens"))
    if not binary.is_file():
        sys.exit(f"agent-lens binary not found at {binary}; build it with `cargo build --release -p agent-lens`")
    require("hyperfine")

    rows = []
    for lang in languages:
        suffix, setup = LANGUAGES[lang]
        work = OUT / lang
        work.mkdir(parents=True, exist_ok=True)
        corpora, ref = setup(work)
        for name, root in corpora:
            files, lines = source_stats(root, suffix)
            lens_cmd = [binary.resolve(), "analyze", "complexity", root]
            ref_cmd = ref["command"](root)
            result = hyperfine(
                [("agent-lens", lens_cmd), (ref["tool"], ref_cmd)],
                ref.get("prepare"),
                ref["cwd"],
                work / f"{name}.hyperfine.json",
            )
            scored = {
                "agent-lens": count_agent_lens(capture_json(lens_cmd, ref["cwd"])),
                ref["tool"]: ref["scored"](root),
            }
            means = {r["command"]: r for r in result["results"]}
            baseline = means["agent-lens"]["mean"]
            for tool in ("agent-lens", ref["tool"]):
                rows.append(
                    {
                        "language": lang,
                        "corpus": name,
                        "files": files,
                        "lines": lines,
                        "tool": tool,
                        "mean": means[tool]["mean"],
                        "stddev": means[tool]["stddev"] or 0.0,
                        "baseline": baseline,
                        "scored": scored[tool],
                    }
                )

    report = render(rows)
    (OUT / "report.md").write_text(report)
    (OUT / "report.json").write_text(json.dumps(rows, indent=2) + "\n")
    sys.stdout.write(report)
    return 0


if __name__ == "__main__":
    sys.exit(main())
