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
    protocol = "framed"
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any

__all__ = ["Request", "json", "text", "body", "content", "tool_error", "next", "PROTOCOL_VERSION"]
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
    #: The body as it arrives, on routes with ``stream_body = true``: iterate
    #: it with ``async for`` (or ``for`` in a sync handler).
    stream: Any = None
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


_UNSET: Any = object()


def content(blocks: list[dict[str, Any]], json: Any = _UNSET) -> dict:  # noqa: A002
    """An MCP tool result made of content blocks, such as images.

    Blocks are MCP content blocks: ``{"type": "text", "text": ...}``,
    ``{"type": "image", "data": <base64>, "mimeType": ...}`` and so on. With
    ``json`` too, the agent also gets it as structured content. In a tool
    handler, ``ctx["mcp"]`` holds the tool name, protocol version and client.
    """
    reply: dict[str, Any] = {"status": 200, "mcp_content": blocks}
    if json is not _UNSET:
        reply["json"] = json
    return reply


def tool_error(message: str, status: int = 400) -> dict:
    """A tool error the model sees and can react to, such as "city not found".

    Unsworn turns it into an MCP result with ``isError: true``. As an HTTP
    reply, it's a 400 (or ``status``) with ``{"error": message}``.
    """
    return {"status": status, "json": {"error": message}}


def next(context: dict[str, Any] | None = None) -> dict:  # noqa: A001 - mirrors the JS SDK
    """From a ``before`` handler: let the request continue, adding context."""
    return {"continue": context or {}}
