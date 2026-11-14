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
[runtimes.python]
extensions = [".py"]
command = "python3 -u -m unsworn.worker"
protocol = "framed"

[[route]]
path = "/users/{id}"
method = "GET"
handler = "src/users.py#get_user"
params = { id = { type = "integer", min = 1 } }
```

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

Sync handlers run in a thread pool, so they don't block other requests.
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
