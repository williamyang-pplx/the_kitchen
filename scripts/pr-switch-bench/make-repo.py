#!/usr/bin/env python3
"""Build the PR-switch bench's repos in DIR:

  DIR/origin   a big repo (FILES files) with a stack of PRS PRs, each changing
               CHANGED files a lot: branches stack-1..N (stack-k on stack-k-1,
               stack-1 on main), refs/pull/<100+k>/head, as on GitHub.
  DIR/agi      a blob:none partial clone of it (like a devbox's agi), with
               PACKS extra one-object promisor packs, as a devbox's on-demand
               downloads and fetches leave them, in a multi-pack index.

Prints the stack as JSON: [{"n", "branch", "base", "head", "base_oid"}].

  make-repo.py DIR [--files 60000] [--prs 8] [--changed 400] [--packs 50000]
"""
import argparse
import hashlib
import json
import os
import random
import struct
import subprocess
import zlib


def run(*args, **kw):
    return subprocess.run(args, check=True, capture_output=True, text=True, **kw).stdout


def text(seed, lines):
    r = random.Random(seed)
    words = ["alpha", "beta", "gamma", "delta", "retry", "judge", "client", "token", "batch", "harness"]
    return "".join(f"def f_{seed}_{i}({r.choice(words)}, {r.choice(words)}):\n    return {r.randint(0, 10**6)}\n" for i in range(lines))


def make_origin(path, files, prs, changed):
    os.makedirs(path)
    run("git", "-c", "init.defaultBranch=main", "init", "-q", path)
    for k, v in [("uploadpack.allowFilter", "true"), ("uploadpack.allowAnySHA1InWant", "true")]:
        run("git", "-C", path, "config", k, v)
    paths = [f"pkg{i // 100:03d}/mod_{i:05d}.py" for i in range(files)]
    out = []

    def blob(data):
        b = data.encode()
        out.append(b"blob\ndata %d\n" % len(b) + b + b"\n")

    def commit(ref, msg, parent, edits):
        out.append(f"commit {ref}\ncommitter Bench <b@b> 1700000000 +0000\ndata {len(msg)}\n{msg}\n".encode())
        if parent:
            out.append(f"from {parent}\n".encode())
        for p, data in edits:
            b = data.encode()
            out.append(f"M 100644 inline {p}\ndata {len(b)}\n".encode() + b + b"\n")
        out.append(b"\n")

    commit("refs/heads/main", "base", None, [(p, text(i, 30)) for i, p in enumerate(paths)])
    rng = random.Random(1)
    parent = "refs/heads/main"
    for k in range(1, prs + 1):
        edits = [(p, text(10**7 + k * 10**5 + i, 40)) for i, p in enumerate(rng.sample(paths, changed))]
        edits += [(f"new{k}/file_{j}.py", text(k * 1000 + j, 60)) for j in range(changed // 20)]
        ref = f"refs/heads/stack-{k}"
        commit(ref, f"PR {100 + k}", parent, edits)
        parent = ref
    subprocess.run(["git", "-C", path, "fast-import", "--quiet"], input=b"".join(out), check=True)
    stack = []
    for k in range(1, prs + 1):
        head = run("git", "-C", path, "rev-parse", f"stack-{k}").strip()
        base = "main" if k == 1 else f"stack-{k - 1}"
        run("git", "-C", path, "update-ref", f"refs/pull/{100 + k}/head", head)
        stack.append({"n": 100 + k, "branch": f"stack-{k}", "base": base, "head": head, "base_oid": run("git", "-C", path, "rev-parse", base).strip()})
    return stack


def write_pack(pack_dir, data):
    """A one-blob pack + v2 idx + .promisor marker, written directly (git
    commands slow down with every pack, so making 50k with git takes hours)."""
    raw = data
    oid = hashlib.sha1(b"blob %d\0" % len(raw) + raw).digest()
    size = len(raw)
    hdr = bytearray([(3 << 4) | (size & 0x0F)])
    size >>= 4
    while size:
        hdr[-1] |= 0x80
        hdr.append(size & 0x7F)
        size >>= 7
    entry = bytes(hdr) + zlib.compress(raw)
    body = b"PACK" + struct.pack(">II", 2, 1) + entry
    trailer = hashlib.sha1(body).digest()
    fanout = b"".join(struct.pack(">I", 1 if i >= oid[0] else 0) for i in range(256))
    idx = b"\xfftOc" + struct.pack(">I", 2) + fanout + oid + struct.pack(">I", zlib.crc32(entry)) + struct.pack(">I", 12) + trailer
    idx += hashlib.sha1(idx).digest()
    name = os.path.join(pack_dir, "pack-" + trailer.hex())
    with open(name + ".pack", "wb") as f:
        f.write(body + trailer)
    with open(name + ".idx", "wb") as f:
        f.write(idx)
    open(name + ".promisor", "w").close()


def main():
    a = argparse.ArgumentParser()
    a.add_argument("dir")
    a.add_argument("--files", type=int, default=60000)
    a.add_argument("--prs", type=int, default=8)
    a.add_argument("--changed", type=int, default=400)
    a.add_argument("--packs", type=int, default=50000)
    o = a.parse_args()
    o.dir = os.path.abspath(o.dir)
    origin, clone = os.path.join(o.dir, "origin"), os.path.join(o.dir, "agi")
    stack = make_origin(origin, o.files, o.prs, o.changed)
    run("git", "clone", "-q", "--filter=blob:none", f"file://{origin}", clone)
    # As on a devbox: git's own maintenance off (else its auto gc repacks
    # once there are 50 packs, mid-run), the untracked cache on.
    for k, v in [("gc.auto", "0"), ("maintenance.auto", "false"), ("core.untrackedCache", "true")]:
        run("git", "-C", clone, "config", k, v)
    pack_dir = os.path.join(clone, ".git", "objects", "pack")
    for i in range(o.packs):
        write_pack(pack_dir, b"download %d\n" % i)
    # Indexed, as the kitchen keeps a devbox's agi (without the index every
    # git command reads every pack's index: 22 s each at 100k packs).
    run("git", "-C", clone, "multi-pack-index", "write")
    print(json.dumps(stack))


if __name__ == "__main__":
    main()
