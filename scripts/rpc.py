#!/usr/bin/env python3
"""Tiny test client: rpc.py <method> [json-params]  (uses KITCHEN_STATE_DIR)."""
import json, os, socket, sys
d = os.environ.get("KITCHEN_STATE_DIR", os.path.expanduser("~/.kitchen"))
s = socket.socket(socket.AF_UNIX); s.connect(os.path.join(d, "kitchen.sock"))
req = {"id": 1, "method": sys.argv[1], "params": json.loads(sys.argv[2]) if len(sys.argv) > 2 else {}}
s.sendall((json.dumps(req) + "\n").encode())
buf = b""
while not buf.endswith(b"\n"):
    chunk = s.recv(65536)
    if not chunk: break
    buf += chunk
r = json.loads(buf)
print(json.dumps(r.get("result") if r.get("ok") else r, indent=1))
sys.exit(0 if r.get("ok") else 1)
