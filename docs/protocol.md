# Unsworn handler protocol

Unsworn talks to handler workers over one **message model** with two
**encodings**:

| Tier | Transport | Encoding | SDK |
|---|---|---|---|
| 1 | worker stdin/stdout | JSON lines, optional raw body bytes | Not required |
| 2 | Unix socket | Length-prefixed frames: JSON message plus raw body bytes | Required |

Everything in "Message model" applies to both tiers.

A runtime that is an HTTP server of its own speaks neither: Unsworn
forwards it HTTP requests. See [`http-runtime.md`](http-runtime.md).

## Worker startup

Unsworn starts the worker with the runtime's `command` and these environment
variables:

| Variable | Example | Meaning |
|---|---|---|
| `UNSWORN_PROTOCOL` | `1` | Protocol version Unsworn speaks |
| `UNSWORN_VERSION` | `0.4.0` | Unsworn version |
| `UNSWORN_TRANSPORT` | `stdio` or `unix:/run/unsworn/ts-0.sock` | Where to talk |
| `UNSWORN_ROOT` | `/app` | Project root; handler paths are relative to it |
| `UNSWORN_WORKER_ID` | `ts-0` | Worker id, for logs |
| `UNSWORN_DEV` | `1` | Set only under `unsworn dev` and `unsworn check --deep`. SDKs then include stack traces in `error` messages |
| `UNSWORN_RAW_JSON` | `1` | This Unsworn takes a JSON body as raw bytes, marked [`body_json`](#json-as-the-raw-body-raw_json-and-body_json), in both directions |
| `UNSWORN_LOG_LEVEL` | `info` | The most verbose [`log`](#log-worker--unsworn) level Unsworn keeps: `error`, `warn`, `info`, `debug`, `trace` or `off` |
| `UNSWORN_CALLS` | `1` | Set when Unsworn answers [`call`](#calling-a-function-call-and-result) messages |
| `UNSWORN_FETCH` | `1` | Set when Unsworn answers [`fetch`](#asking-for-a-request-to-a-service-fetch-and-fetched) messages |

The worker also inherits Unsworn's own environment, without the variables
the config reads credentials from (see
[`{env.NAME}`](config.md#envname)). On Tier 2, its socket is in a directory
only Unsworn's user can open, so the worker has to run as that user.

**The worker speaks first** by sending `ready`. Unsworn sends nothing until it
arrives, so a worker never has to handle a greeting.

## Message model

Every message has a `type`. Messages about a request carry its `id`, a
string chosen by Unsworn that is unique per worker connection.

### `ready` (worker → Unsworn)

```json
{"type":"ready","protocol":1,"max_in_flight":1,"verify":true,"sdk":"unsworn-python/0.4.0"}
```

- `protocol`: required. Unsworn stops the worker if it can't speak this
  version.
- `max_in_flight`: optional, default `1`. How many requests the worker
  accepts at once. A hand-written loop that handles one request at a time
  leaves this at `1`, and Unsworn starts more processes for concurrency.
- `verify`: optional, default `false`. Whether the worker answers
  [`verify`](#verify-unsworn--worker) requests.
- `stream_body`: optional, default `false`. Whether the worker accepts
  [streamed request bodies](#streamed-request-bodies-chunk-end-and-ack).
  Handlers on `stream_body` routes are reported missing otherwise.
- `calls`: optional, default `false`. Whether the worker's handlers can
  [call functions](#calling-a-function-call-and-result). Unsworn answers
  `call` either way; the conformance suite reads this to decide whether to
  test it.
- `fetch`: optional, default `false`. The same for
  [requests to outbound services](#asking-for-a-request-to-a-service-fetch-and-fetched).
- `raw_json`: optional, default `false`. Whether the worker reads a JSON
  request body from the raw bytes after the message. See
  [JSON as the raw body](#json-as-the-raw-body-raw_json-and-body_json).
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
  - `json`: the JSON body, when the request had one, as the client sent it
  - `body_len` with `"body_json": true`: the JSON body as raw bytes, for a
    worker whose `ready` has `raw_json`
  - `text`: a UTF-8 text body
  - `body_len`: the number of raw body bytes that follow (see each tier)
  - `"stream": true`: the body follows as `chunk` messages, on routes with
    `stream_body` (see [below](#streamed-request-bodies-chunk-end-and-ack))
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
  it. `json` goes to the client as the worker wrote it: the same member
  order, spacing and number spelling.
- `"body_json": true` with `body_len`: the raw body is the JSON to answer
  with, in place of `json`. Only under an Unsworn that sets
  `UNSWORN_RAW_JSON`.

### JSON as the raw body: `raw_json` and `body_json`

A JSON body inside a message is read twice before a handler sees it: by
Unsworn, to be sure the message it writes is JSON, and by the worker, as
part of the message. For a large body that costs several times what the
body takes to send. Sent as the raw bytes after the message, it's read once,
by whoever needs it.

- **Requests.** A worker that sends `"raw_json": true` in `ready` gets a
  JSON body as `body_len` raw bytes with `"body_json": true`, and no `json`
  member. Unsworn has read the body only if the route has a `body` schema
  or `before` handlers, so the bytes may not be JSON at all. The SDKs parse
  them when the handler first asks for the body, and answer `400` with an
  `application/problem+json` body (`"detail": "the body isn't valid JSON:
  ..."`) if they can't. A handler that doesn't read the body isn't told.
- **Responses.** Under an Unsworn that sets `UNSWORN_RAW_JSON=1`, a worker
  may answer with `body_len` raw bytes and `"body_json": true` in place of
  `json`. Unsworn sends the bytes on without reading them, with
  `content-type: application/json` unless a header says otherwise. An older
  Unsworn doesn't know `body_json`, which is why the variable is there to
  check.
- A worker that doesn't send `raw_json` gets `json` inside the message, as
  before, after Unsworn has read it.

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

### Streamed request bodies: `chunk`, `end` and `ack`

After a `request` with `"stream": true`, Unsworn sends the body the same way
a worker streams a response, and the worker acknowledges what it has read:

```json
{"type":"chunk","id":"r-81","body_len":65536}
{"type":"ack","id":"r-81","bytes":65536}
{"type":"end","id":"r-81"}
```

- `ack` (worker → Unsworn) says the handler has read `bytes` more of the
  body. Unsworn keeps at most 256 KiB unacknowledged per request, so a slow
  handler slows the upload instead of the body piling up in memory.
- The handler may respond before reading the whole body. Unsworn then
  stops sending it.
- If the body passes `max_body`, stalls for 30 seconds or the client goes
  away, Unsworn sends `cancel`, and the client gets `413` or `408`.

### Calling a function: `call` and `result`

A handler calls a [function](functions.md), which is another handler,
through Unsworn. The worker sends `call`:

```json
{"type":"call","id":"c-7","request":"r-81","function":"embed","json":{"text":"hello"}}
```

- `id`: chosen by the worker, unique among its calls in flight.
- `request`: the `id` of the request the call is made for, which must be
  in flight on this worker. The function gets that request's context and
  what is left of its time.
- `function`: the function's name.
- `json`: the input. Default `null`.

Unsworn runs the function as a normal `request` to a worker of its
runtime (`route` and `path` are `fn:<name>`, `method` is `POST`, `json` is
the input, `context` is the calling request's plus `caller`, and `headers`
has the calling request's `traceparent` and `tracestate`, if it came with
a valid one), then answers with `result`:

```json
{"type":"result","id":"c-7","status":200,"json":[0.12,0.98]}
{"type":"result","id":"c-8","status":404,"error":"no [[function]] named \"embd\" in the config (it declares: embed)"}
```

- `status`: the function's own, or the one for what went wrong (see
  [`functions.md`](functions.md#calling)). The call succeeded when it's
  below `400`.
- `json`: what the function answered with. For a `422`, the input's
  errors.
- `error`: what went wrong, when Unsworn or the function's worker failed.

When the request a call was made for ends first, Unsworn cancels the call
and sends no `result` for it. A worker should stop waiting for a
request's calls when the request is cancelled or answered.

Unsworn sets `UNSWORN_CALLS=1` for workers when it answers `call`. Under
an older Unsworn the variable isn't set and a `call` would be ignored, so
SDKs fail the call themselves.

### Asking for a request to a service: `fetch` and `fetched`

A handler reaches an [outbound service](outbound.md) through Unsworn,
which adds the service's credential. The worker sends `fetch`, with the
request's body as raw bytes after the message:

```json
{"type":"fetch","id":"f-3","request":"r-81","service":"stripe","method":"POST","path":"/v1/charges?expand=customer","headers":{"content-type":"application/json"},"body_len":15}
```

- `id`: chosen by the worker, unique among its fetches in flight.
- `request`: the `id` of the request the fetch is made for, which must be
  in flight on this worker.
- `service`: the name of an `[outbound.<name>]`.
- `method`: default `GET`.
- `path`: the path and query, which go after the service's URL. Starts
  with `/`.
- `headers`: optional. Unsworn drops the ones that belong to a connection
  and any the service sets itself.

Unsworn answers with `fetched`, and the response's body as raw bytes:

```json
{"type":"fetched","id":"f-3","status":200,"headers":{"content-type":"application/json"},"body_len":482}
{"type":"fetched","id":"f-4","status":403,"error":"[outbound.stripe] doesn't allow DELETE (it allows GET, POST)"}
```

- `status`: the service's status. With `error`, the one for what went
  wrong (see [`outbound.md`](outbound.md#fetching)).
- `headers`: the response's, with lowercase names. A header sent more
  than once has its values joined with `, `.
- `error`: set when the request wasn't made or got no answer.

As with `call`, a fetch ends without a `fetched` when the request it was
made for ends first. Unsworn sets `UNSWORN_FETCH=1` for workers when it
answers `fetch`, and a worker whose handlers can use it says `fetch: true`
in `ready`.

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

### `log` (worker → Unsworn)

```json
{"type":"log","id":"r-81","level":"info","message":"charged card","fields":{"amount":1200}}
```

- `level`: `error`, `warn`, `info`, `debug` or `trace`.
- `id`: optional. Unsworn tags the record with that request's route and its
  HTTP request id (the `x-request-id` it returns), so a handler's logs line
  up with the access log.
- `fields`: optional, kept as structured data.

Workers may skip records more verbose than `UNSWORN_LOG_LEVEL`. When that
variable isn't set, the Unsworn version doesn't accept `log`, so SDKs write
to stderr instead.

Output on stderr (and on stdout, on Tier 2) is logged too. A line that is a
JSON object with `level` and `message` or `msg`, as JSON loggers such as
pino, structlog, zap and Go's slog write, is logged as a structured record,
tied to its request if it has an `id` field naming one in flight.

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

Used by [MCP tools as handlers](mcp.md#level-2-mcp-tools-as-handlers)
and [routes as tools](mcp.md#level-3-routes-as-tools). No new message
types are needed: a tool call is a normal `request`.

- For a `[[mcp.tool]]`: `route` is `mcp:<tool name>`, `method` is `POST`,
  `path` is the MCP endpoint, and `json` holds the tool's arguments, already
  validated against its input schema.
- For a route offered as a tool: the request is the one a REST client would
  send to the route. `method`, `path`, `route`, `params`, `query`,
  `raw_query` and `json` come from the arguments.
- `context.mcp` holds `tool`, `protocol_version` and, when the client sends
  it, `client` (`name` and `version`). MCP has no sessions, so there's no
  session ID.
- A `response` may use one extra body field, `mcp_content`: an array of MCP
  content blocks (text, images, audio, resource links, embedded resources)
  passed to the client as they are. How other responses map to tool results
  is described in [`mcp.md`](mcp.md#how-results-are-mapped).
- An MCP client cancels a call by closing its connection, which becomes a
  `cancel` message like for any other request.

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
    log = {"type": "log", "id": msg["id"], "level": "info", "message": "found user"}
    print(json.dumps(log), flush=True)
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

`unsworn conformance --runtime <name> --handlers <file>` starts a worker
for the runtime and checks that it speaks this protocol correctly: the
`ready` message, `verify`, typed inputs, every body form in both directions,
`before` handlers, streaming, logs, concurrency, errors, timeouts and
crashes.

Each test handler is defined by behavior ("echo the params as JSON"). An SDK
author, or an AI agent, implements them in one file in the target language
and passes it with `--handlers`. The handlers and the checks are listed in
[`conformance.md`](conformance.md).

## Open questions

- WebSockets: proxy-only in v1, or a message type for handler-owned sockets?
- A `progress` message (worker → Unsworn) for long-running work, which
  Unsworn would turn into MCP progress notifications for tool calls.
- A way for an MCP tool handler to ask the user a question (elicitation).
  MCP 2026-07-28 does this without keeping the call open: the tool returns
  an "input required" result, and the client calls the tool again with the
  answers. That likely needs a response field for the input request and a
  `context.mcp` field carrying the answers on the retry, rather than a new
  message type.
