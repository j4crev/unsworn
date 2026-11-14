# Unsworn handler protocol

> Protocol version **1**, implemented in `crates/unsworn-proto` and both SDKs.

Unsworn talks to handler workers over one **message model** with two
**encodings**:

| Tier | Transport | Encoding | SDK |
|---|---|---|---|
| 1 | worker stdin/stdout | JSON lines, optional raw body bytes | Not required |
| 2 | Unix socket | Length-prefixed binary frames | Required |

Everything in "Message model" applies to both tiers.

## Worker startup

Unsworn starts the worker with the runtime's `command` and these environment
variables:

| Variable | Example | Meaning |
|---|---|---|
| `UNSWORN_PROTOCOL` | `1` | Protocol version Unsworn speaks |
| `UNSWORN_VERSION` | `0.1.0` | Unsworn version |
| `UNSWORN_TRANSPORT` | `stdio` or `unix:/run/unsworn/ts-0.sock` | Where to talk |
| `UNSWORN_ROOT` | `/app` | Project root; handler paths are relative to it |
| `UNSWORN_WORKER_ID` | `ts-0` | Worker id, for logs |

**The worker speaks first** by sending `ready`. Unsworn sends nothing until it
arrives, so a worker never has to handle a greeting.

## Message model

Every message has a `type`. Messages about a request carry its `id`, a
string chosen by Unsworn that is unique per worker connection.

### `ready` (worker → Unsworn)

```json
{"type":"ready","protocol":1,"max_in_flight":1,"verify":true,"sdk":"unsworn-python/0.1.0"}
```

- `protocol`: required. Unsworn stops the worker if it can't speak this
  version.
- `max_in_flight`: optional, default `1`. How many requests the worker
  accepts at once. A hand-written loop that handles one request at a time
  leaves this at `1`, and Unsworn starts more processes for concurrency.
- `verify`: optional, default `false`. Whether the worker answers
  [`verify`](#verify-unsworn--worker) requests.
- `sdk`: optional, for logs and diagnostics.

### `request` (Unsworn → worker)

```json
{
  "type": "request",
  "id": "r-81",
  "phase": "handle",
  "handler": "src/users.ts#getUser",
  "route": "/users/{id}",
  "method": "GET",
  "path": "/users/42",
  "params": {"id": 42},
  "query": {"limit": 20},
  "headers": {"accept": "application/json"},
  "context": {"auth": {"sub": "u_123"}, "tenant": "acme"},
  "json": null,
  "deadline_ms": 4975
}
```

- `handler`: the route's handler reference, exactly as written in config or
  derived from a route file or OpenAPI spec, in the form `<file>[#<name>]`.
  Unsworn treats it as opaque; the worker decides what it means (load a file,
  call an export, look up a registered function). See
  [handler references](config.md#handler-references).
- `phase`: `"handle"` for the route's handler, `"before"` for a before
  handler.
- `params`, `query`, `headers`: already validated and converted to their
  declared types. Header names are lowercase.
- `context`: auth claims plus anything added by before handlers. For MCP
  tool calls it also has `mcp` (see [MCP tool calls](#mcp-tool-calls)).
- Body, at most one of:
  - `json`: the parsed JSON body, when the request had a JSON body
  - `text`: a UTF-8 text body
  - `body_len`: the number of raw body bytes that follow (see each tier)
- `deadline_ms`: time left before Unsworn times the request out.
- `raw_query`: the query string as sent, without the `?`. Useful for handlers
  that wrap an existing app with its own query parsing.

### `response` (worker → Unsworn)

```json
{"type":"response","id":"r-81","status":200,"headers":{"cache-control":"max-age=30"},"json":{"id":42,"name":"Ada"}}
```

- `status`: required.
- `headers`: optional.
- Body, at most one of `json`, `text`, `body_len`, or `"stream": true`.
  Unsworn sets `content-type` for `json` and `text` unless a header overrides
  it.

### `continue` (worker → Unsworn, `before` phase only)

```json
{"type":"continue","id":"r-81","context":{"tenant":"acme","plan":"pro"}}
```

Merges `context` into the request context and moves to the next step. A
`before` handler rejects a request by sending a normal `response` instead.

### Streaming responses: `chunk` and `end` (worker → Unsworn)

After a `response` with `"stream": true`:

```json
{"type":"chunk","id":"r-81","text":"data: hello\n\n"}
{"type":"chunk","id":"r-81","body_len":4096}
{"type":"end","id":"r-81"}
```

Used for server-sent events, large downloads and generated output.

### `cancel` (Unsworn → worker)

```json
{"type":"cancel","id":"r-81","reason":"timeout"}
```

The client disconnected or the deadline passed. The worker should stop work
on that request. Anything it sends afterwards for that `id` is discarded.
Workers that can't cancel may ignore this message.

### `verify` (Unsworn → worker)

Sent only to workers whose `ready` message had `"verify": true`, after
`ready` and before any `request`. It lists every handler reference this
worker's runtime will be sent:

```json
{"type":"verify","id":"v-1","handlers":["src/users.ts#getUser","src/users.ts#getUsr","src/billing.ts"]}
```

### `verified` (worker → Unsworn)

```json
{"type":"verified","id":"v-1","missing":[{"handler":"src/users.ts#getUsr","reason":"no export \"getUsr\""}]}
```

- `missing`: the references the worker can't resolve, each with a readable
  `reason`. An empty list means every handler was found.
- The worker should check without running handler code beyond what loading
  a module requires.

Unsworn reports missing handlers as `unsworn check` errors. What happens
next depends on the command; see
[handler references](config.md#handler-references).

### `error` (worker → Unsworn)

```json
{"type":"error","id":"r-81","message":"db connection refused"}
```

The handler failed. Unsworn returns `500` to the client and logs the message.
Without `id`, the error concerns the whole worker; Unsworn logs it and
restarts the worker.

### Failure handling

| Situation | Client gets | Unsworn does |
|---|---|---|
| Worker sends `error` | `500` | Logs the message |
| Worker exits with requests in flight | `502` | Restarts it with backoff |
| Deadline passes | `504` | Sends `cancel` |
| Invalid message | `502` for that request if identifiable | Logs it, keeps the worker |
| Wrong protocol version in `ready` | n/a | Stops the worker, marks the runtime unhealthy |

Error bodies sent to clients never include worker messages unless
`unsworn dev` is running.

## MCP tool calls

Planned for v2, with [MCP tools as handlers](mcp.md#level-2-mcp-tools-as-handlers).
No new message types are needed: a tool call is a normal `request`.

- `route` is `mcp:<tool name>`, `method` is `POST` and `path` is the MCP
  endpoint.
- `json` holds the tool's arguments, already validated against its input
  schema.
- `context.mcp` holds `tool`, `session` (when sessions are on) and `client`
  (`name` and `version`).
- A `response` may use one extra body field, `mcp_content`: an array of MCP
  content blocks (text, images, audio, embedded resources) passed to the
  client as they are. How other responses map to tool results is described in
  [`mcp.md`](mcp.md#how-results-are-mapped).
- An MCP cancellation from the client becomes a `cancel` message.

## Tier 1: JSON lines over stdio

- Unsworn writes messages to the worker's **stdin**. The worker writes messages
  to **stdout**.
- Each message is one line of UTF-8 JSON ending in `\n`. JSON strings escape
  newlines, so a message never spans lines.
- **stdout is reserved for protocol messages.** Logs go to **stderr**, which
  Unsworn captures into its own logs. If a stdout line isn't a valid message,
  Unsworn logs it as a warning and keeps going instead of failing the worker.
- **Raw bodies.** If a message has `body_len: N`, exactly `N` raw bytes follow
  immediately after its newline, then the next message starts. This carries
  binary data without base64.
- Workers flush stdout after each message.

### A complete worker with no SDK

```python
import json, sys

print(json.dumps({"type": "ready", "protocol": 1}), flush=True)

for line in sys.stdin:
    msg = json.loads(line)
    if msg["type"] != "request":
        continue  # e.g. cancel: safe to ignore in a sequential worker
    user = {"id": msg["params"]["id"], "name": "Ada"}
    reply = {"type": "response", "id": msg["id"], "status": 200, "json": user}
    print(json.dumps(reply), flush=True)
```

This worker never receives `body_len` messages if its routes don't accept
raw bodies. Handling them needs reading `N` bytes from stdin's underlying
binary buffer, which is the kind of detail an SDK takes care of.

## Tier 2: length-prefixed frames over a Unix socket

- Unsworn creates a Unix socket and passes its path in `UNSWORN_TRANSPORT`
  as `unix:<path>`. The worker connects to it, then sends `ready`. One
  connection per worker; requests are multiplexed by `id`.
- stdin is closed. stdout and stderr are both collected into Unsworn's logs,
  so stray output can't break the protocol.
- Every message is one frame:

  ```
  ┌────────────────┬───────────────┬──────────────┬─────────────────┐
  │ frame_len: u32 │ json_len: u32 │ JSON message │ raw body bytes  │
  └────────────────┴───────────────┴──────────────┴─────────────────┘
  ```

  Both lengths are big-endian. `frame_len` counts everything after itself
  (`4 + json_len + body length`), and the message's `body_len` equals the
  number of raw body bytes.
- The JSON message is exactly the Tier 1 message. Only the framing differs,
  so a Tier 1 implementation becomes Tier 2 by swapping a few dozen lines.

Compared with Tier 1, Tier 2 needs no newline scanning, can't be corrupted by
stray output, and keeps the worker's stdout free for logs. The JSON payload
was chosen over Protocol Buffers or FlatBuffers so that an SDK needs nothing
beyond a JSON parser. A future protocol version could negotiate a binary
payload encoding in `ready` if benchmarks show JSON parsing matters.

## Shutting down

To stop a worker, Unsworn closes its input (stdin on Tier 1, the socket on
Tier 2) after in-flight requests finish. The worker should exit when its
input closes. Workers that don't exit within two seconds are killed.

## Versioning

- The protocol version is an integer. Additive changes (new optional fields,
  new message types a worker may ignore) keep the version. Anything else
  increments it.
- Workers must ignore unknown fields and unknown message types they aren't
  required to handle.
- An Unsworn release supports the current protocol version and the one before
  it.

## Conformance suite

`unsworn conformance --runtime <name>` starts the runtime's workers against a
built-in set of test handlers and checks:

- the `ready` handshake and version negotiation
- params, query, headers and context arrive with the right types
- `json`, `text` and raw `body_len` bodies in both directions
- `before` handlers: `continue` with context, and rejection
- concurrency up to the declared `max_in_flight`, with out-of-order responses
- `verify`, if the worker declares support: found and missing handlers are
  reported correctly
- streaming with `chunk` and `end`
- `error` messages, crashes mid-request and `cancel` on timeout
- stdout noise on Tier 1 is logged without breaking the worker

Each test handler is defined by behavior ("echo the params as JSON"), so the
suite ships a short spec that an SDK author, or an AI agent, implements in
the target language before running it.

## Open questions

- Request body streaming to workers (`request` with `"stream": true` followed
  by `chunk` messages), in v1 or later?
- Should workers be able to send log records through the protocol, with
  levels and fields, in addition to stderr?
- WebSockets: proxy-only in v1, or a message type for handler-owned sockets?
- A `progress` message (worker → Unsworn) for long-running work, which
  Unsworn would turn into MCP progress notifications for tool calls.
- Messages for requests from an MCP server to the client mid-call (sampling,
  elicitation), so a handler can ask a question and wait for the answer.
