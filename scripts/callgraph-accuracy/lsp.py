"""Minimal LSP client shared by the oracles that drive a language server (stdlib only)."""

from __future__ import annotations

import json
import queue
import subprocess
import threading
from collections.abc import Callable
from pathlib import Path
from urllib.parse import unquote, urlparse


class LspClient:
    """Minimal JSON-RPC over stdio: requests block, notifications are queued.

    `answer` replies to server -> client requests (`workspace/configuration`,
    `window/workDoneProgress/create`, ...); the default answers `null`, and a
    `null` per item for `workspace/configuration`.
    """

    def __init__(self, cmd: list[str], cwd: Path, answer: Callable[[dict], object] | None = None):
        self.proc = subprocess.Popen(cmd, cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        self.answer = answer or default_answer
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
                    for box in list(self.responses.values()):
                        box.put({"error": "server exited"})
                    return
                line = line.decode("ascii").strip()
                if not line:
                    break
                key, _, value = line.partition(":")
                headers[key.strip().lower()] = value.strip()
            msg = json.loads(out.read(int(headers["content-length"])))
            if "id" in msg and "method" in msg:
                self._send({"jsonrpc": "2.0", "id": msg["id"], "result": self.answer(msg)})
            elif "id" in msg:
                self.responses.setdefault(msg["id"], queue.Queue()).put(msg)
            else:
                self.notifications.put(msg)

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


def default_answer(msg: dict):
    if msg["method"] == "workspace/configuration":
        return [None for _ in msg["params"]["items"]]
    return None


def uri_to_path(uri: str) -> Path:
    return Path(unquote(urlparse(uri).path))


def locations(result) -> list[tuple[str, int]]:
    """(uri, 0-based selection line) of a definition / implementation response."""
    return [(uri, line) for uri, line, _ in targets(result)]


def targets(result) -> list[tuple[str, int, int]]:
    """(uri, 0-based line, character) of the selection start of a definition / implementation response."""
    if isinstance(result, dict):
        result = [result]
    out = []
    for t in result or []:
        start = (t.get("targetSelectionRange") or t["range"])["start"]
        out.append((t.get("targetUri") or t["uri"], start["line"], start["character"]))
    return out


def position(f: Path, line: int, char: int) -> dict:
    return {"textDocument": {"uri": f.as_uri()}, "position": {"line": line, "character": char}}
