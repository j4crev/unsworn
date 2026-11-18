# Functions: handlers that call handlers

The usual reason to put Python next to TypeScript is that one needs
something from the other: the TypeScript endpoint wants an embedding, and
the library that makes one is Python's. A *function* is a handler that
other handlers call by name, whatever language each is written in:

```toml
[[function]]
name    = "embed"
handler = "src/embed.py#embed"
input   = { type = "object", required = ["text"], properties = { text = { type = "string" } } }
output  = { type = "array", items = { type = "number" } }
```

```python
# src/embed.py
from unsworn import json

def embed(req, ctx):
    return json(model.encode(req.json["text"]).tolist())
```

```ts
// src/search.ts
export const search: Search = async (req, ctx) => {
  const vector = await ctx.call("embed", { text: req.query.q });
  return { json: await index.nearest(vector) };
};
```

There is no second service, no URL, no port and no credential between the
two. The call goes over the connection each worker already has to
Unsworn, which runs the function in its own runtime's workers.

## What Unsworn does with a call

- **Checks the input** against the function's `input` schema before the
  function runs, as it checks a request's body. Input that doesn't match
  never reaches the function: the caller gets a `422` with the errors.
- **Passes on the caller's context.** The function's `ctx` has the same
  verified credentials (`ctx.auth`) and whatever `before` handlers added,
  taken from Unsworn's own record of the request, not from the caller. It
  also has `ctx.caller`: `{ function, from, depth }`, where `from` is the
  handler that called.
- **Keeps to the request's time.** A function has its own `timeout`
  (default: `[server].timeout`) and never more time than the request it's
  called for has left. When that request ends, by timeout or because the
  client went away, Unsworn cancels the calls made for it.
- **Ties the logs together.** The function's logs carry the same request
  id as the caller's and the access log line.
- **Keeps it in the request's trace.** When the request came with a
  `traceparent` header, the function gets it, and the `tracestate` with
  it, as `req.headers`. See [`tracing.md`](tracing.md).
- **Checks the output** against `output`, as `response_validation` says:
  `log` (the default) logs a mismatch, `enforce` fails the call with
  `502`, `off` skips it.

A function has no path. Nothing outside Unsworn reaches it, so it needs no
`auth` of its own: only handlers of the same project can call it.

## Calling

| | Call | A failed call |
|---|---|---|
| TypeScript | `await ctx.call("embed", { text })` | throws `CallError` (`status`, `message`, `json`) |
| Python | `ctx.call("embed", {"text": text})`, with `await` in an `async def` handler | raises `CallError` (`status`, `json`) |
| Go | `err := req.Call("embed", input, &out)` | a `*unsworn.CallError` (`Status`, `Message`, `JSON`) |
| Rust | `req.call::<Vec<f32>>("embed", json!({ "text": text })).await?` | `Err(CallError { status, message, json })` |

A call gives back the JSON the function answered with. It fails when the
function answers `400` or above, or when it can't be run:

| Status | Why |
|---|---|
| the function's own (`400`–`599`) | it answered with that status; `json` is its body |
| `404` | the config has no `[[function]]` with that name |
| `422` | the input doesn't match `input`; `json.errors` lists why |
| `500` | the function failed: it threw, or sent `error` |
| `502` | its worker stopped, it answered with something other than JSON, or its output doesn't match `output` under `enforce` |
| `503` | its runtime couldn't start |
| `504` | it ran out of time |
| `508` | calls nested more than 16 deep, as when a function calls itself |

A handler that doesn't catch the error fails with it, so its client gets
a `500` and the reason is logged.

## A function's handler

It's an ordinary handler. The input arrives as the JSON body (`req.json`),
`req.route` is `fn:<name>`, `req.headers` holds only the calling request's
[trace](tracing.md), if it had one, and what it answers with as JSON is
the call's result. A text reply arrives as a JSON string. Bytes and streams aren't
supported: a call fails with `502`.

The same handler can also serve a route or be an MCP tool, if it's listed
there too.

## Types

`unsworn gen` writes each function's `input` and `output` as types in
every language, since any handler may call it (see
[`types.md`](types.md)):

- **TypeScript:** `ctx.call` is typed by name. A wrong input, a wrong use
  of the result or a name that isn't a function is a compile error.
- **Python:** `EmbedInput` and `EmbedOutput`.
- **Go and Rust:** `EmbedInput` and `EmbedOutput`, to pass to `Call` and
  to decode the answer into.

## Config

```toml
[[function]]
name    = "embed"                    # letters, digits, `_`, `-` and `.`
handler = "src/embed.py#embed"
runtime = "py"                       # only when the file's extension doesn't say
input   = "schemas/embed-in.json"    # a JSON Schema file, or an inline table
output  = "schemas/embed-out.json"
timeout = "10s"                      # default: [server].timeout
```

`unsworn plan` lists each function with its handler and schemas, and
`unsworn diff` reports a function whose `input` check was removed as
looser.

## Limits

- **On Cloudflare and serverless targets, a function runs in the same
  process as its caller.** `ctx.call` works there as it does on a server,
  with the same checks, context, time limit and statuses, but only for
  functions in JavaScript or TypeScript, like every handler on those
  targets. `unsworn check --target` reports any others.
- **A worker that takes one request at a time can't call a function in
  its own runtime** unless another worker is free: it's busy with the
  caller. The SDKs' workers take many requests at once, so this is about
  hand-written workers. Unsworn adds a worker when a call has to wait, up
  to the runtime's `workers.max`.
- **JSON in, JSON out.** No raw bodies or streams.
- **No policies of their own** beyond `timeout`: no rate limit, cache or
  `before` handlers. Put those on the routes that call.
