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
from typing import Any, Generic, TypeVar

__all__ = [
    "Request",
    "Context",
    "CallError",
    "Fetched",
    "FetchError",
    "json",
    "text",
    "body",
    "content",
    "tool_error",
    "next",
    "PROTOCOL_VERSION",
]
__version__ = "0.3.2"

PROTOCOL_VERSION = 1


#: The types of a request's path parameters, query parameters and JSON body.
P = TypeVar("P")
Q = TypeVar("Q")
J = TypeVar("J")


@dataclass
class Request(Generic[P, Q, J]):
    """A request, already matched, authenticated and validated by Unsworn.

    ``Request[Params, Query, Body]`` says what a route's contract gives its
    handler. ``unsworn gen python`` writes those types from the config, as
    ``GetUserRequest`` for a handler named ``get_user``. A plain ``Request``
    takes anything.
    """

    method: str
    path: str
    route: str
    handler: str
    phase: str = "handle"
    params: P = field(default_factory=dict)  # type: ignore[assignment]
    query: Q = field(default_factory=dict)  # type: ignore[assignment]
    raw_query: str | None = None
    headers: dict[str, str] = field(default_factory=dict)
    json: J = None  # type: ignore[assignment]
    text: str | None = None
    body: bytes | None = None
    #: The body as it arrives, on routes with ``stream_body = true``: iterate
    #: it with ``async for`` (or ``for`` in a sync handler).
    stream: Any = None
    #: True once Unsworn has cancelled the request (timeout or client gone).
    cancelled: bool = False


_UNSET: Any = object()


class CallError(Exception):
    """A function answered with an error status, or couldn't be called.

    ``status`` is the function's own, or 404 for a name with no
    ``[[function]]``, 422 for input its schema refuses and 504 for a
    timeout. ``json`` is what the function answered with, if anything.
    """

    def __init__(self, status: int, message: str, json: Any = None) -> None:  # noqa: A002
        super().__init__(message)
        self.status = status
        self.json = json


class FetchError(Exception):
    """``ctx.fetch`` couldn't make the request, or got no answer.

    ``status`` is 404 for a name with no ``[outbound.<name>]``, 403 for a
    method or path the config doesn't allow, 502 for a service that couldn't
    be reached and 504 for one that took too long.
    """

    def __init__(self, status: int, message: str) -> None:
        super().__init__(message)
        self.status = status


@dataclass
class Fetched:
    """A service's answer to ``ctx.fetch``."""

    status: int
    #: Response headers, with lowercase names.
    headers: dict[str, str]
    body: bytes

    @property
    def ok(self) -> bool:
        return 200 <= self.status < 300

    @property
    def text(self) -> str:
        return self.body.decode("utf-8", "replace")

    def json(self) -> Any:
        import json as _json

        return _json.loads(self.body)


class Context(dict):
    """A handler's ``ctx``: the verified credentials under ``"auth"`` and
    anything ``before`` handlers added, as a dict, plus :meth:`call`."""

    #: Set by the worker: how to ask Unsworn to run a function.
    _call: Any = None

    def call(self, name: str, input: Any = None) -> Any:  # noqa: A002
        """Calls a ``[[function]]``: a handler in any language, declared in
        the config.

        Unsworn checks ``input`` against the function's schema, runs it with
        this request's context and what is left of its time, and gives back
        the JSON it answered with. In an ``async def`` handler, await it::

            vector = await ctx.call("embed", {"text": text})

        In a plain ``def`` handler it waits and returns the answer::

            vector = ctx.call("embed", {"text": text})

        Raises :class:`CallError` when the function answers 400 or above,
        or can't be called.
        """
        if self._call is None:
            raise CallError(501, "ctx.call works in a handler that Unsworn is running")
        return self._call(name, input)

    #: Set by the worker: how to ask Unsworn for a request to a service.
    _fetch: Any = None

    def fetch(
        self,
        service: str,
        path: str,
        *,
        method: str | None = None,
        headers: dict[str, str] | None = None,
        json: Any = _UNSET,  # noqa: A002
        body: bytes | str | None = None,
    ) -> Any:
        """Sends a request to an outbound service: one the config declares
        under ``[outbound.<name>]``.

        Unsworn makes the request and adds the service's own headers, such
        as its API key, so this handler never holds them. ``path`` goes
        after the service's URL and may carry a query. ``json`` is sent as
        a JSON body; ``method`` defaults to GET, or POST with a body.

        Gives back a :class:`Fetched` whatever the service's status, and
        raises :class:`FetchError` when the request couldn't be made or got
        no answer. Await it in an ``async def`` handler, as with
        :meth:`call`.
        """
        if self._fetch is None:
            raise FetchError(501, "ctx.fetch works in a handler that Unsworn is running")
        return self._fetch(service, path, method, headers, json, body)


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
