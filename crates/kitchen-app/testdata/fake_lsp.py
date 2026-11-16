"""A scriptable language server for the kitchen.lsp tests.

Speaks LSP over stdio and behaves as its environment says. Every message it
receives is appended to $FAKE_LOG as one JSON line, so a test can check what
the client sent.

  FAKE_DELAY_MS   delay before answering definition / references (default 0)
  FAKE_ALLOC_MB   memory to allocate and touch at startup (the RSS cap test)
  FAKE_CONFIG     section to ask for with workspace/configuration after `initialized`
  FAKE_REGISTER   1: ask to register didChangeWatchedFiles after `initialized`
"""

import json
import os
import sys
import threading

out_lock = threading.Lock()


def read_message(stream):
    """One framed message (Content-Length header), or None on EOF."""
    length = None
    while True:
        line = stream.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            break
        name, _, value = line.decode().partition(":")
        if name.lower() == "content-length":
            length = int(value)
    body = stream.read(length or 0)
    return json.loads(body) if body else None


def write_message(msg):
    """Frame and write `msg` to stdout; the reply threads share it."""
    msg["jsonrpc"] = "2.0"
    body = json.dumps(msg).encode()
    with out_lock:
        sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body) + body)
        sys.stdout.buffer.flush()


def log(msg):
    """Append `msg` as one JSON line to $FAKE_LOG."""
    path = os.environ.get("FAKE_LOG")
    if path:
        with out_lock, open(path, "a") as f:
            f.write(json.dumps(msg) + "\n")


class Server:
    """One process serves one client; `pending` holds requests a cancel may still stop."""

    def __init__(self):
        self.pending = {}
        self.next_id = 1000

    def answer_later(self, msg):
        cancelled = self.pending[msg["id"]]
        delay = int(os.environ.get("FAKE_DELAY_MS", "0")) / 1000
        if cancelled.wait(delay):
            return
        self.pending.pop(msg["id"], None)
        p = msg["params"]
        pos = p["position"]
        loc = {"uri": p["textDocument"]["uri"], "range": {"start": {"line": pos["line"] + 1, "character": 0}, "end": {"line": pos["line"] + 1, "character": 1}}}
        write_message({"id": msg["id"], "result": [loc]})

    def handle(self, msg):
        log(msg)
        method = msg.get("method")
        if method == "initialize":
            caps = {"positionEncoding": "utf-32", "definitionProvider": True, "referencesProvider": True}
            write_message({"id": msg["id"], "result": {"capabilities": caps}})
        elif method == "initialized":
            if os.environ.get("FAKE_CONFIG"):
                self.next_id += 1
                write_message({"id": self.next_id, "method": "workspace/configuration", "params": {"items": [{"section": os.environ["FAKE_CONFIG"]}]}})
            if os.environ.get("FAKE_REGISTER") == "1":
                self.next_id += 1
                reg = {"id": "watch", "method": "workspace/didChangeWatchedFiles", "registerOptions": {"watchers": [{"globPattern": "**/*.py"}]}}
                write_message({"id": self.next_id, "method": "client/registerCapability", "params": {"registrations": [reg]}})
        elif method in ("textDocument/definition", "textDocument/references"):
            self.pending[msg["id"]] = threading.Event()
            threading.Thread(target=self.answer_later, args=(msg,), daemon=True).start()
        elif method == "$/cancelRequest":
            target = msg["params"]["id"]
            event = self.pending.pop(target, None)
            if event:
                event.set()
                write_message({"id": target, "error": {"code": -32800, "message": "cancelled"}})
        elif method == "shutdown":
            write_message({"id": msg["id"], "result": None})
        elif method == "exit":
            sys.exit(0)


def main():
    hold = None
    if os.environ.get("FAKE_ALLOC_MB"):
        hold = bytearray(int(os.environ["FAKE_ALLOC_MB"]) * 1024 * 1024)
        for i in range(0, len(hold), 4096):
            hold[i] = 1
    server = Server()
    while True:
        msg = read_message(sys.stdin.buffer)
        if msg is None:
            break
        server.handle(msg)
    del hold


if __name__ == "__main__":
    main()
