"""A Tier 1 worker written by hand, with no SDK.

It handles one request at a time, which is the simplest possible worker.
Unsworn starts several of these processes for concurrency. It implements
the conformance handlers from docs/conformance.md, so it doubles as a test:

    unsworn conformance -c tests/conformance.toml --runtime raw-python \
        --handlers examples/raw-python/worker.py
"""

import json
import os
import sys
import time

out = sys.stdout.buffer


def send(message, raw=b""):
    if raw:
        message = {**message, "body_len": len(raw)}
    out.write(json.dumps(message).encode() + b"\n" + raw)
    out.flush()


def reply(rid, status=200, **fields):
    send({"type": "response", "id": rid, "status": status, **fields})


send({"type": "ready", "protocol": 1})

stdin = sys.stdin.buffer
while True:
    line = stdin.readline()
    if not line:
        break
    msg = json.loads(line)
    raw = stdin.read(msg["body_len"]) if msg.get("body_len") else b""
    if msg["type"] != "request":
        continue  # e.g. cancel: safe to ignore for a sequential worker
    rid = msg["id"]
    name = msg["handler"].partition("#")[2]
    if name == "echo":
        reply(rid, json={
            "method": msg["method"],
            "params": msg.get("params", {}),
            "query": msg.get("query", {}),
            "context": msg.get("context", {}),
            "json": msg.get("json"),
            "header": msg.get("headers", {}).get("x-test"),
        })
    elif name == "text":
        reply(rid, 201, text="hello", headers={"x-conformance": "yes"})
    elif name == "binary":
        send({"type": "response", "id": rid, "status": 200}, raw)
    elif name == "status":
        reply(rid, int(msg["query"]["code"]))
    elif name == "stream":
        reply(rid, stream=True)
        for chunk in "abc":
            send({"type": "chunk", "id": rid, "text": chunk})
        send({"type": "end", "id": rid})
    elif name == "fail":
        send({"type": "error", "id": rid, "message": "boom"})
    elif name == "slow":
        time.sleep(0.5)
        reply(rid, text="done")
    elif name == "crash":
        os._exit(3)
    elif name == "allow":
        send({"type": "continue", "id": rid, "context": {"checked": True}})
    elif name == "deny":
        reply(rid, 403)
    elif name == "noisy":
        print("not a protocol message", flush=True)
        reply(rid, text="ok")
    else:
        send({"type": "error", "id": rid, "message": f"unknown handler {name}"})
