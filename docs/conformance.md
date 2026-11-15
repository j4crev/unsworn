# Conformance suite

`unsworn conformance --runtime <name> --handlers <file>` checks that a
runtime's workers speak the [protocol](protocol.md) correctly. It starts
one worker for the runtime and sends it requests for a set of test
handlers, which you implement in one file in the runtime's language.

```
$ unsworn conformance --runtime ts --handlers sdk/js/conformance/handlers.ts
conformance: [runtimes.ts] (Framed)
  ✓ worker starts and sends `ready`
  ✓ verify reports missing handlers
  ✓ params, query, headers, context and JSON body arrive with their types
  ...
20 passed, 0 failed
```

Implementations in this repository:

| Runtime | Handlers file |
|---|---|
| JS/TS SDK (Bun, Node, Deno) | [`sdk/js/conformance/handlers.ts`](../sdk/js/conformance/handlers.ts) |
| Python SDK | [`sdk/python/conformance/handlers.py`](../sdk/python/conformance/handlers.py) |
| Go SDK | [`sdk/go/conformance/handlers.go`](../sdk/go/conformance/handlers.go) |
| Rust SDK | [`sdk/rust/examples/conformance.rs`](../sdk/rust/examples/conformance.rs) |
| Python, no SDK (Tier 1 by hand) | [`examples/raw-python/worker.py`](../examples/raw-python/worker.py) |

## The handlers

Each handler is called by name (`<file>#<name>`). Every request arrives with
`method: "POST"` and `route`/`path` set to `/conformance`.

| Name | Phase | Must |
|---|---|---|
| `echo` | handle | Respond `200` with a JSON body: `{ "method", "params", "query", "context", "json", "header" }`, where `header` is the value of the request's `x-test` header and the rest are copied from the request. |
| `text` | handle | Respond `201` with the text body `hello` and the header `x-conformance: yes`. |
| `binary` | handle | Respond `200` with the request's raw body bytes as the raw response body. |
| `status` | handle | Respond with the status in `query.code` (a number) and no body. |
| `stream` | handle | Stream the body `a`, `b`, `c` as three chunks, then end. |
| `fail` | handle | Fail with an `error` message whose text contains `boom` (in an SDK: throw an error). |
| `slow` | handle | Wait 500 ms, then respond `200`. |
| `crash` | handle | Exit the worker process immediately, without responding. |
| `allow` | before | Send `continue` with the context `{ "checked": true }`. |
| `deny` | before | Respond `403`. |
| `noisy` | handle | Write a line that isn't a protocol message to stdout (Tier 1) or to the log, then respond `200`. |
| `log` | handle | Log `conformance log` at `info` with the field `n: 1`, through the SDK's logging integration, then respond `200`. |
| `upload` | handle | Read the streamed request body and respond `200` with it as the raw response body. Only called when the worker's `ready` has `stream_body: true`. |
| `content` | handle | Respond `200` with the JSON body `{ "ok": true }` and `mcp_content` set to `[{ "type": "text", "text": "hi" }]` (in an SDK: its MCP content helper). |
| `mcp` | handle | Respond `200` with the JSON body `{ "tool", "protocol_version", "client" }`, read from `context.mcp`, where `client` is the client's `name`. In an SDK with a typed accessor for `context.mcp`, use it. |

`does_not_exist` is listed in the `verify` request but must not exist, so
the worker reports it as missing. Workers that don't support `verify` skip
that check.

## What is checked

- the worker starts and sends `ready` with protocol `1`
- `verify` reports `does_not_exist`, and only it, as missing
- params, query, headers, context and the JSON body arrive with their types
- text responses with a status and headers
- raw binary bodies in both directions
- status codes
- streamed responses (`chunk` and `end`)
- `error` messages
- `before` handlers continuing with context, and rejecting
- stray stdout output doesn't break the worker
- a `log` record arrives with its level, fields and request `id`
- a 1 MiB streamed request body arrives intact, paced by `ack`, if the
  worker declares `stream_body`
- `mcp_content` reaches Unsworn next to a JSON body
- MCP call details arrive in `context.mcp`
- several requests in flight at once, when `max_in_flight` is above 1
- a timed-out request is cancelled, and the worker keeps answering
- a crash fails only the request in flight, and the worker is restarted
