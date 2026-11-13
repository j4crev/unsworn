"""Conformance handlers for the Python SDK. See docs/conformance.md."""

import asyncio
import os

from unsworn import body, json, next
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
