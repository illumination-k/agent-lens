#!/usr/bin/env python3
"""Rust type-checker oracle for the call-graph accuracy benchmark (issue #579).

Drives rust-analyzer over LSP and records its call hierarchy between
functions defined under the analysed root, in the oracle contract of
docs/callgraph-accuracy.md. Standard library only.

    oracle_rs.py --root <cargo project dir> --out <file.json> [--rust-analyzer PATH] [--features a,b]

For every `fn` rust-analyzer's document symbols list (free functions,
methods in `impl` and `trait` blocks, nested fns), the oracle asks for
`textDocument/prepareCallHierarchy` at its name and then
`callHierarchy/outgoingCalls`. rust-analyzer resolves each call with the
type checker (method receivers, trait impls, `use` paths), so its static
edges are near exact.

Edge conventions:
  - caller: the function whose outgoing calls list the call; the scorer
    re-attributes the call to the innermost agent-lens node containing
    `call_line`, so a call in a closure or a nested fn lands where the
    edge definition puts it.
  - calls inside a macro invocation (`assert_eq!(f(x), 1)`, `vec![g()]`,
    `format!("{}", h())`): rust-analyzer's call hierarchy does not descend
    into macro arguments, so every `name(` / `.name(` / `name::<..>(` token
    in a macro's token tree is looked up with `textDocument/definition`,
    which does resolve through the expansion; it is an edge when the
    definition is a function under root. The caller is the innermost fn
    whose range contains the token.
  - call_line: the line of the callee's name at the call site (`foo` in
    `a.foo(x)`, the last path segment in `m::foo(x)`): the end of the
    call hierarchy's range, which spans the whole path.
  - def lines: the line of the function's name, which is the `fn` line
    (attributes and doc comments above it are not part of it).
  - callee: a function under root. Tuple-struct and enum-variant
    constructors, std / dependency functions and functions outside root
    are dropped and counted on stderr.
  - dispatch "dynamic" when the callee is declared in a `trait` block: a
    call rust-analyzer can only pin to the trait method (a `dyn` receiver, a
    generic `T: Trait`, or a default method). Such a call also gets a
    "dynamic" edge to every implementation of that method under root
    (`textDocument/implementation` on the trait method), the Rust analogue
    of a CHA candidate set. "static" otherwise (a free fn, an associated fn,
    an inherent or trait-impl method on a concrete type). Calls through a
    closure or fn pointer are not in rust-analyzer's call hierarchy, so there
    are no such dynamic edges.
  - trait methods, std traits included (`Version::from_str(s)`,
    `T::default()`): the call hierarchy names the trait method, so the call
    site is looked up with `textDocument/definition`, which gives the impl
    when the receiver type is known.
  - analyzed_files: every `.rs` file under root with at least one function
    rust-analyzer resolved (a file outside every crate's module tree has none);
    unanalyzed_functions: every fn it could not resolve (cfg-inactive under
    the enabled features: all unless --features names some), whose agent-lens
    edges the scorer leaves out of precision.

Logs go to stderr.
"""

from __future__ import annotations

import argparse
import json
import os
import queue
import subprocess
import sys
import threading
import time
from collections import Counter
from pathlib import Path
from urllib.parse import unquote, urlparse

SKIP_DIRS = {"target", ".git", "node_modules", "vendor"}
# LSP SymbolKind values.
FUNCTION_KINDS = {6, 12}  # Method, Function
TRAIT_KIND = 11  # Interface: rust-analyzer's kind for a `trait`
INDEXING_TIMEOUT_S = 1800


def log(msg: str) -> None:
    print(f"oracle_rs: {msg}", file=sys.stderr, flush=True)


# ---------------------------------------------------------------- LSP client


class LspClient:
    """Minimal JSON-RPC over stdio: requests block, notifications are queued."""

    def __init__(self, cmd: list[str], cwd: Path):
        self.proc = subprocess.Popen(cmd, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.next_id = 0
        self.responses: dict[int, queue.Queue] = {}
        self.notifications: queue.Queue = queue.Queue()
        self.lock = threading.Lock()
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self) -> None:
        out = self.proc.stdout
        while True:
            headers = {}
            while True:
                line = out.readline()
                if not line:
                    self.notifications.put(None)
                    return
                line = line.decode("ascii").strip()
                if not line:
                    break
                key, _, value = line.partition(":")
                headers[key.strip().lower()] = value.strip()
            msg = json.loads(out.read(int(headers["content-length"])))
            if "id" in msg and "method" in msg:
                # A server -> client request (workDoneProgress/create, configuration, ...).
                self._send({"jsonrpc": "2.0", "id": msg["id"], "result": self._answer(msg)})
            elif "id" in msg:
                self.responses.setdefault(msg["id"], queue.Queue()).put(msg)
            else:
                self.notifications.put(msg)

    @staticmethod
    def _answer(msg: dict):
        if msg["method"] == "workspace/configuration":
            return [None for _ in msg["params"]["items"]]
        return None

    def _send(self, msg: dict) -> None:
        body = json.dumps(msg).encode()
        with self.lock:
            self.proc.stdin.write(f"Content-Length: {len(body)}\r\n\r\n".encode() + body)
            self.proc.stdin.flush()

    def request(self, method: str, params) -> dict:
        with self.lock:
            self.next_id += 1
            rid = self.next_id
        box = self.responses.setdefault(rid, queue.Queue())
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        msg = box.get()
        del self.responses[rid]
        if "error" in msg:
            raise RuntimeError(f"{method}: {msg['error']}")
        return msg.get("result")

    def notify(self, method: str, params) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def wait_quiescent(self, timeout: float) -> None:
        """Block until rust-analyzer reports it finished loading and indexing."""
        deadline = time.monotonic() + timeout
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                raise TimeoutError("rust-analyzer did not become quiescent")
            msg = self.notifications.get(timeout=left)
            if msg is None:
                raise RuntimeError("rust-analyzer exited")
            if msg.get("method") == "experimental/serverStatus":
                status = msg["params"]
                if status.get("quiescent"):
                    if status.get("health") == "error":
                        raise RuntimeError(f"rust-analyzer: {status.get('message', '')}")
                    if status.get("health") != "ok":
                        log(f"server status {status.get('health')}: {status.get('message', '')}")
                    return

    def close(self) -> None:
        try:
            self.request("shutdown", None)
            self.notify("exit", None)
            self.proc.wait(timeout=30)
        except Exception:  # noqa: BLE001 - best effort teardown
            self.proc.kill()
            self.proc.wait()
        for pipe in (self.proc.stdin, self.proc.stdout):
            try:
                pipe.close()
            except OSError:
                pass


# ---------------------------------------------------------------- oracle


def rust_files(root: Path) -> list[Path]:
    out = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP_DIRS and not d.startswith("."))
        out += [Path(dirpath) / f for f in sorted(filenames) if f.endswith(".rs")]
    return out


def uri_to_path(uri: str) -> Path:
    return Path(unquote(urlparse(uri).path))


def function_symbols(symbols: list[dict], in_trait: bool = False):
    """Yield (selection line, selection character, range start line, range end line, in_trait) of every fn.

    Lines are 0-based, depth first.
    """
    for s in symbols or []:
        if s.get("kind") in FUNCTION_KINDS:
            sel, rng = s["selectionRange"]["start"], s["range"]
            yield sel["line"], sel["character"], rng["start"]["line"], rng["end"]["line"], in_trait
        yield from function_symbols(s.get("children"), s.get("kind") == TRAIT_KIND)


# ---------------------------------------------------------------- macro call tokens


def tokens(src: str):
    """Yield (kind, text, offset) of a Rust source, skipping comments and whitespace.

    kind is "ident", "lit" (string, char, number) or "punct" (one character).
    Only as exact as finding call tokens inside macro invocations needs.
    """
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        if c.isspace():
            i += 1
        elif src.startswith("//", i):
            j = src.find("\n", i)
            i = n if j < 0 else j
        elif src.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if src.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif src.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    i += 1
        elif c in "rbc" and _raw_string_at(src, i):
            start = i
            while src[i] != "r":
                i += 1
            i += 1
            hashes = 0
            while src[i] == "#":
                hashes, i = hashes + 1, i + 1
            end = src.find('"' + "#" * hashes, i + 1)
            i = n if end < 0 else end + 1 + hashes
            yield "lit", src[start:i], start
        elif c == '"' or (c in "bc" and src.startswith('"', i + 1)):
            start = i
            i = src.index('"', i) + 1
            while i < n and src[i] != '"':
                i += 2 if src[i] == "\\" else 1
            i += 1
            yield "lit", src[start:i], start
        elif c == "'" or (c == "b" and src.startswith("'", i + 1)):
            start = i
            i = src.index("'", i) + 1
            if i < n and src[i] == "\\":
                end = src.find("'", i + 2)
                i = n if end < 0 else end + 1
            elif i + 1 < n and src[i + 1] == "'":
                i += 2
            else:
                # A lifetime or label: 'a
                while i < n and (src[i].isalnum() or src[i] == "_"):
                    i += 1
            yield "lit", src[start:i], start
        elif c.isalpha() or c == "_":
            start = i
            while i < n and (src[i].isalnum() or src[i] == "_"):
                i += 1
            yield "ident", src[start:i], start
        elif c.isdigit():
            start = i
            while i < n and (src[i].isalnum() or src[i] in "_."):
                if src[i] == "." and not (i + 1 < n and src[i + 1].isdigit()):
                    break
                i += 1
            yield "lit", src[start:i], start
        else:
            yield "punct", c, i
            i += 1


def _raw_string_at(src: str, i: int) -> bool:
    j = i
    if src[j] in "bc":
        j += 1
    if j >= len(src) or src[j] != "r":
        return False
    j += 1
    while j < len(src) and src[j] == "#":
        j += 1
    return j < len(src) and src[j] == '"' and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_"))


OPEN, CLOSE = "([{", ")]}"


def macro_call_tokens(src: str) -> list[int]:
    """Return the offsets of call-shaped identifiers inside macro invocations.

    A macro invocation is `name!` followed by a delimited token tree (not
    `macro_rules!`, whose body is a pattern, not code). Inside it, an
    identifier is call-shaped when the next token is `(`, or `::<` opens a
    turbofish that closes right before `(`, and the identifier is not the
    name of a nested macro or of a `fn` being declared.
    """
    toks = list(tokens(src))
    out: list[int] = []
    i = 0
    while i + 2 < len(toks):
        kind, text, _ = toks[i]
        if kind == "ident" and toks[i + 1][1] == "!" and toks[i + 2][1] in OPEN and text != "macro_rules":
            end = _group_end(toks, i + 2)
            for j in range(i + 3, end):
                if toks[j][0] == "ident" and _is_call(toks, j, end) and toks[j - 1][1] != "fn":
                    out.append(toks[j][2])
            i = end
        i += 1
    return sorted(set(out))


def _group_end(toks, open_idx: int) -> int:
    depth = 0
    for j in range(open_idx, len(toks)):
        t = toks[j]
        if t[0] == "punct" and t[1] in OPEN:
            depth += 1
        elif t[0] == "punct" and t[1] in CLOSE:
            depth -= 1
            if depth == 0:
                return j
    return len(toks)


def _is_call(toks, j: int, end: int) -> bool:
    k = j + 1
    if k < end and toks[k][1] == ":" and k + 2 < end and toks[k + 1][1] == ":" and toks[k + 2][1] == "<":
        depth, k = 0, k + 2
        while k < end:
            if toks[k][1] == "<":
                depth += 1
            elif toks[k][1] == ">":
                depth -= 1
                if depth == 0:
                    break
            k += 1
        k += 1
    return k < end and toks[k][0] == "punct" and toks[k][1] == "("


def line_col(src: str, offset: int) -> tuple[int, int]:
    """0-based line and UTF-16 column of a string offset, as LSP positions count."""
    line = src.count("\n", 0, offset)
    start = src.rfind("\n", 0, offset) + 1
    return line, len(src[start:offset].encode("utf-16-le")) // 2


def _locations(result) -> list[tuple[str, int]]:
    """(uri, 0-based selection line) of a definition / implementation response."""
    if isinstance(result, dict):
        result = [result]
    return [
        (t.get("targetUri") or t["uri"], (t.get("targetSelectionRange") or t["range"])["start"]["line"])
        for t in result or []
    ]


def _position(f: Path, line: int, char: int) -> dict:
    return {"textDocument": {"uri": f.as_uri()}, "position": {"line": line, "character": char}}


class RustOracle:
    """One rust-analyzer session over one cargo project; `run` returns the oracle document."""

    def __init__(self, root: Path, client: LspClient):
        self.root = root
        self.client = client
        self.stats: Counter = Counter()
        self.edges: set[tuple] = set()
        self.files = rust_files(root)
        self.rel = {f: f.relative_to(root).as_posix() for f in self.files}
        # Every fn as (file, 0-based name line, name column); the (file,
        # 1-based def line) of every fn and of those declared in a trait block;
        # per file, the (start, end, name line) of every fn's range.
        self.functions: list[tuple[Path, int, int]] = []
        self.positions: dict[tuple[str, int], tuple[Path, int, int]] = {}
        self.trait_fns: set[tuple[str, int]] = set()
        self.ranges: dict[Path, list[tuple[int, int, int]]] = {}
        self.impls: dict[tuple[str, int], list[tuple[str, int]]] = {}

    def run(self) -> dict:
        self.collect_functions()
        analyzed, unanalyzed = set(), []
        for f, line, char in self.functions:
            if self.outgoing_calls(f, line, char):
                analyzed.add(self.rel[f])
            else:
                unanalyzed.append({"file": self.rel[f], "def_line": line + 1})
        for f in self.files:
            self.macro_calls(f)
        keys = ["caller_file", "caller_def_line", "call_line", "callee_file", "callee_def_line", "dispatch"]
        return {
            "oracle": "rust-analyzer",
            "language": "rust",
            "kind": "type-checker",
            "root": str(self.root),
            "edges": [dict(zip(keys, e)) for e in sorted(self.edges)],
            "analyzed_files": sorted(analyzed),
            "unanalyzed_functions": unanalyzed,
        }

    def collect_functions(self) -> None:
        for f in self.files:
            symbols = self.client.request("textDocument/documentSymbol", {"textDocument": {"uri": f.as_uri()}})
            for line, char, start, end, in_trait in function_symbols(symbols):
                key = (self.rel[f], line + 1)
                self.functions.append((f, line, char))
                self.positions[key] = (f, line, char)
                self.ranges.setdefault(f, []).append((start, end, line))
                if in_trait:
                    self.trait_fns.add(key)
        log(f"{len(self.functions)} functions in {len(self.files)} files")

    def function_at(self, uri: str, line0: int) -> tuple[str, int] | None:
        """The (file, def line) of the fn under root named at a location, else None (counted)."""
        try:
            file = uri_to_path(uri).resolve().relative_to(self.root).as_posix()
        except ValueError:
            return None
        key = (file, line0 + 1)
        return key if key in self.positions else None

    def outgoing_calls(self, f: Path, line: int, char: int) -> bool:
        """Record a fn's call-hierarchy edges; False when rust-analyzer cannot resolve the fn."""
        items = self.client.request("textDocument/prepareCallHierarchy", _position(f, line, char))
        if not items:
            self.stats["unresolved_function"] += 1
            return False
        for call in self.client.request("callHierarchy/outgoingCalls", {"item": items[0]}) or []:
            to = self.function_at(call["to"]["uri"], call["to"]["selectionRange"]["start"]["line"])
            for r in call["fromRanges"]:
                # The range covers the whole callee path (`Version::from_str`);
                # its last character is inside the name that is called.
                site = (r["end"]["line"], max(r["end"]["character"] - 1, 0))
                if to is not None and to not in self.trait_fns:
                    self.add(f, line, site[0], to)
                else:
                    # A trait method (a std trait's included): the definition
                    # at the call site is the impl when the receiver type is known.
                    self.resolve_site(f, line, site)
        return True

    def macro_calls(self, f: Path) -> None:
        src = f.read_text(errors="replace")
        for offset in macro_call_tokens(src):
            line, col = line_col(src, offset)
            enclosing = [r for r in self.ranges.get(f, []) if r[0] <= line <= r[1]]
            if not enclosing:
                self.stats["macro_call_outside_fn"] += 1
                continue
            caller_line = min(enclosing, key=lambda r: r[1] - r[0])[2]
            if self.resolve_site(f, caller_line, (line, col)):
                self.stats["macro_call_edges"] += 1

    def resolve_site(self, f: Path, caller_line: int, site: tuple[int, int]) -> bool:
        """Record the edge `textDocument/definition` gives at a call site; False when none."""
        found = False
        for uri, line0 in _locations(self.client.request("textDocument/definition", _position(f, *site))):
            to = self.function_at(uri, line0)
            if to is None:
                self.stats["callee_outside_root_or_not_fn"] += 1
                continue
            self.add(f, caller_line, site[0], to)
            found = True
        return found

    def add(self, f: Path, caller_line: int, call_line: int, to: tuple[str, int]) -> None:
        """Record caller -> to; a trait method also gets its implementations, all dynamic."""
        caller = (self.rel[f], caller_line + 1, call_line + 1)
        if to not in self.trait_fns:
            self.edges.add((*caller, *to, "static"))
            return
        self.edges.add((*caller, *to, "dynamic"))
        for impl in self.implementations(to):
            self.edges.add((*caller, *impl, "dynamic"))

    def implementations(self, trait_fn: tuple[str, int]) -> list[tuple[str, int]]:
        if trait_fn not in self.impls:
            found = self.client.request("textDocument/implementation", _position(*self.positions[trait_fn]))
            out = [self.function_at(uri, line0) for uri, line0 in _locations(found)]
            self.impls[trait_fn] = [to for to in out if to is not None and to != trait_fn]
        return self.impls[trait_fn]


def initialize(client: LspClient, root: Path, features: list[str] | None) -> None:
    client.request(
        "initialize",
        {
            "processId": os.getpid(),
            "rootUri": root.as_uri(),
            "workspaceFolders": [{"uri": root.as_uri(), "name": root.name}],
            "capabilities": {
                "textDocument": {
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": True},
                    "callHierarchy": {},
                    "definition": {"linkSupport": True},
                    "implementation": {"linkSupport": True},
                },
                "experimental": {"serverStatusNotification": True},
            },
            # All features by default, so that as little code as possible is
            # cfg-inactive (agent-lens reads every fn whatever its cfg); a
            # crate with mutually exclusive features names a set instead.
            # Build scripts and proc macros so that generated code expands.
            "initializationOptions": {
                "cargo": {"features": "all" if features is None else features, "buildScripts": {"enable": True}},
                "procMacro": {"enable": True},
                "checkOnSave": False,
                "cachePriming": {"enable": True},
            },
        },
    )
    client.notify("initialized", {})
    log("waiting for rust-analyzer to load the workspace")
    client.wait_quiescent(INDEXING_TIMEOUT_S)


def run(root: Path, ra: str, features: list[str] | None = None) -> tuple[dict, Counter]:
    root = root.resolve()
    client = LspClient([ra], cwd=root)
    try:
        initialize(client, root, features)
        oracle = RustOracle(root, client)
        return oracle.run(), oracle.stats
    finally:
        client.close()


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--root", required=True, type=Path)
    ap.add_argument("--out", type=Path)
    ap.add_argument("--rust-analyzer", default=os.environ.get("RUST_ANALYZER", "rust-analyzer"))
    ap.add_argument("--features", help="comma-separated cargo features (default: all features)")
    args = ap.parse_args()
    features = None if args.features is None else [f for f in args.features.split(",") if f]
    doc, stats = run(args.root, args.rust_analyzer, features)
    text = json.dumps(doc, indent=1) + "\n"
    if args.out:
        args.out.write_text(text)
    else:
        sys.stdout.write(text)
    from_macros = stats.pop("macro_call_edges", 0)
    dropped = ", ".join(f"{n} {k}" for k, n in sorted(stats.items())) or "nothing"
    log(f"{len(doc['edges'])} edges ({from_macros} call sites inside macros) from {len(doc['analyzed_files'])} files; dropped: {dropped}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
