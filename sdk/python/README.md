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
protocol = "binary"

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

## The request

`req` has `method`, `path`, `route`, `params`, `query`, `raw_query`,
`headers` (lowercase names), `json`, `text`, `body` (raw bytes), `handler`,
`phase`, and `cancelled` (set when Unsworn cancels the request). `ctx` is a
dict with `auth` (verified claims) and anything `before` handlers added.

Sync handlers run in a thread pool, so they don't block other requests.
`print()` output goes to stderr, which Unsworn collects into its logs.

Status: not yet published to PyPI. Until then, set `PYTHONPATH` to this
directory in the runtime's `env`.
