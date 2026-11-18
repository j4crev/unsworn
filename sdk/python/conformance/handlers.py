"""Conformance handlers for the Python SDK. See docs/conformance.md."""

import asyncio
import logging
import os

from unsworn import CallError, body, content as content_reply, json, next
from unsworn import text as text_reply


def echo(req, ctx):
    return json(
        {
            "method": req.method,
            "params": req.params,
            "query": req.query,
            "context": ctx,
            "json": req.json,
            "header": req.headers.get("x-test"),
        }
    )


def text(req, ctx):
    return text_reply("hello", status=201, headers={"x-conformance": "yes"})


def binary(req, ctx):
    return body(req.body or b"")


def status(req, ctx):
    return {"status": int(req.query["code"])}


def stream(req, ctx):
    return body(iter(["a", "b", "c"]))


def fail(req, ctx):
    raise RuntimeError("boom")


async def slow(req, ctx):
    await asyncio.sleep(0.5)
    return text_reply("done")


def crash(req, ctx):
    os._exit(3)


def allow(req, ctx):
    return next({"checked": True})


def deny(req, ctx):
    return json({"error": "denied"}, status=403)


def noisy(req, ctx):
    print("this goes to stderr, not the protocol stream")
    return text_reply("ok")


def log(req, ctx):
    logging.getLogger().info("conformance log", extra={"n": 1})
    return {"status": 200}


def upload(req, ctx):
    return body(b"".join(req.stream))


def content(req, ctx):
    return content_reply([{"type": "text", "text": "hi"}], json={"ok": True})


def relay(req, ctx):
    try:
        return json({"answer": ctx.call(req.query["fn"], req.json)})
    except CallError as e:
        return json({"failed": e.status})


def mcp(req, ctx):
    call = ctx.get("mcp") or {}
    return json(
        {
            "tool": call.get("tool"),
            "protocol_version": call.get("protocol_version"),
            "client": (call.get("client") or {}).get("name"),
        }
    )
