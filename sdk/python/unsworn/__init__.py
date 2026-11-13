"""Write Unsworn handlers in Python.

A handler is a plain function, sync or async, that takes a request and a
context and returns a reply::

    from unsworn import json

    def get_user(req, ctx):
        return json({"id": req.params["id"], "name": "Ada"})

Run handlers with the worker, from ``unsworn.toml``::

    [runtimes.python]
    extensions = [".py"]
    command = "python3 -u -m unsworn.worker"
    protocol = "binary"
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

__all__ = ["Request", "json", "text", "body", "next", "PROTOCOL_VERSION"]
__version__ = "0.1.0"

PROTOCOL_VERSION = 1


@dataclass
class Request:
    """A request, already matched, authenticated and validated by Unsworn."""

    method: str
    path: str
    route: str
    handler: str
    phase: str = "handle"
    params: dict[str, Any] = field(default_factory=dict)
    query: dict[str, Any] = field(default_factory=dict)
    raw_query: str | None = None
    headers: dict[str, str] = field(default_factory=dict)
    json: Any = None
    text: str | None = None
    body: bytes | None = None
    #: True once Unsworn has cancelled the request (timeout or client gone).
    cancelled: bool = False


def json(value: Any, status: int = 200, headers: dict[str, str] | None = None) -> dict:
    """A JSON reply."""
    reply: dict[str, Any] = {"status": status, "json": value}
    if headers:
        reply["headers"] = headers
    return reply


def text(value: str, status: int = 200, headers: dict[str, str] | None = None) -> dict:
    """A text reply."""
    reply: dict[str, Any] = {"status": status, "text": value}
    if headers:
        reply["headers"] = headers
    return reply


def body(value: Any, status: int = 200, headers: dict[str, str] | None = None) -> dict:
    """A raw reply: bytes, or an iterator/async iterator to stream."""
    reply: dict[str, Any] = {"status": status, "body": value}
    if headers:
        reply["headers"] = headers
    return reply


def next(context: dict[str, Any] | None = None) -> dict:  # noqa: A001 - mirrors the JS SDK
    """From a ``before`` handler: let the request continue, adding context."""
    return {"continue": context or {}}
