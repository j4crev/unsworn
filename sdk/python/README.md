# unsworn (Python SDK)

Write [Unsworn](https://github.com/j4crev/unsworn) handlers in Python. A
handler is a plain function, sync or async:

```python
from unsworn import json

def get_user(req, ctx):
    # req.params["id"] is already validated and converted by Unsworn,
    # and ctx["auth"] holds the verified token claims.
    return json({"id": req.params["id"], "name": "Ada"})
```

```toml
# unsworn.toml
[[route]]
path = "/users/{id}"
method = "GET"
handler = "src/users.py#get_user"
params = { id = { type = "integer", min = 1 } }
```

That's all the config a Python handler needs: with no `[runtimes.*]` for
`.py`, Unsworn runs `python3 -u -m unsworn.worker` (through `uv run` in a
project with a `uv.lock`). Write a runtime to run it another way.

## Types from the config

`unsworn gen` writes a route's contract as types, and `Request` takes
them:

```toml
[gen]
python = "unsworn_gen.py"
```

```python
from unsworn import json
from unsworn_gen import GetUserRequest

def get_user(req: GetUserRequest, ctx):
    return json({"id": req.params["id"]})   # a type checker knows id is an int
```

`unsworn dev` keeps the file current. See
[`docs/types.md`](../../docs/types.md).

## Calling other handlers

`ctx.call` calls a [function](../../docs/functions.md): a handler declared
as `[[function]]` in the config, in any language.

```python
from unsworn import CallError, json

def search(req, ctx):
    try:
        ranked = ctx.call("rank", {"ids": ids, "user": ctx["auth"]["sub"]})   # a Rust handler
    except CallError as e:
        if e.status == 504:
            ranked = ids   # too slow: answer unranked
        else:
            raise
    return json(ranked)
```

In an `async def` handler, `await ctx.call(...)`. Unsworn checks the input
against the function's schema, and the function gets this request's
context and what is left of its time.

`ctx` is still a dict: `ctx["auth"]`, `ctx.get("tenant")`.

## Reaching other services

`ctx.fetch` sends a request to an [outbound service](../../docs/outbound.md),
declared as `[outbound.<name>]` in the config. Unsworn makes the request
and adds the service's credential, so the handler never holds it:

```python
answer = ctx.fetch("stripe", "/v1/charges", json={"amount": 1200, "currency": "usd"})
if not answer.ok:
    return json({"error": "the charge failed"}, status=502)
charge = answer.json()
```

In an `async def` handler, `await ctx.fetch(...)`. It gives back the
service's answer whatever its status, and raises `FetchError` when the
request isn't allowed or got no answer.

## Replies

| Return | Response |
|---|---|
| `json(value, status=200, headers=None)` | JSON body |
| `text(value, status=200, headers=None)` | Text body |
| `body(bytes_or_iterator, status=200, headers=None)` | Raw bytes, or streamed when given an iterator or async iterator |
| `{"status": 204}` | No body |
| `next({"tenant": "acme"})` | From a `before` handler: continue, adding context |

## MCP tools

A handler listed in `[[mcp.tool]]` is an ordinary handler. Its arguments
arrive as `req.json`, already validated, and `ctx["mcp"]` has the `tool`
name, `protocol_version` and, when the client sends it, `client`. Unsworn
turns the reply into an MCP tool result (see
[`docs/mcp.md`](../../docs/mcp.md#how-results-are-mapped)):

```python
from unsworn import content, json, tool_error

def get_forecast(req, ctx):
    city = req.json["city"]
    if city not in CITIES:
        return tool_error(f"unknown city {city}")  # the model sees this
    return json({"city": city, "days": forecast(city)})

def chart(req, ctx):
    return content([{"type": "image", "data": png_b64, "mimeType": "image/png"}])
```

| Return | Tool result |
|---|---|
| `json(value)` | Structured content, plus a text copy |
| `text(value)` | One text block |
| `content(blocks, json=value)` | MCP content blocks as given; with `json`, structured content too |
| `tool_error(message)` | `isError: true` with `message` |

## The request

`req` has `method`, `path`, `route`, `params`, `query`, `raw_query`,
`headers` (lowercase names), `json`, `text`, `body` (raw bytes), `handler`,
`phase`, and `cancelled` (set when Unsworn cancels the request). `ctx` is a
dict with `auth` (verified claims) and anything `before` handlers added.

On routes with `stream_body = true`, `req.stream` yields the body in chunks
as it arrives: `async for chunk in req.stream`, or `for` in a sync handler.

Sync handlers run on threads, so one that blocks doesn't hold up other
requests. A worker starts with one handler thread, which is the cheapest way
to run handlers that return quickly or compute, since Python runs one thread
at a time. When calls are queued and the threads are waiting on something,
such as a database, it adds a thread for each queued call, up to
`UNSWORN_MAX_IN_FLIGHT` (32 by default, set in the runtime's `env`). That
takes it up to 40 ms the first time, and it retires the extra threads again
after a while. Telling a waiting thread from a busy one needs Linux; on
other systems queued calls get threads whenever none has run out of work.
Handlers that compute in parallel use the runtime's `workers`, one process
each: Unsworn starts with one and adds more while they're busy.
`print()` output goes to stderr, which Unsworn collects into its logs.

## Logging

The worker sends records from Python's `logging` module to Unsworn, tagged
with the request being handled. `extra` values are kept as fields:

```python
import logging

log = logging.getLogger(__name__)

def charge(req, ctx):
    log.info("charged card", extra={"amount": 1200})
```

The worker sets the root logger's level from Unsworn's, so there's nothing to
configure.
