# Unsworn architecture

> Describes v1 as built. The edge and serverless hosts (v2, v3) are planned.

## Goals

- **Sealed router.** Users configure Unsworn; they never write or modify Rust in
  it. Everything a project needs is expressed through config, route files and
  handlers.
- **Language-agnostic handlers.** Any language that can read stdin and write
  JSON can serve a route. Popular languages get a faster path through SDKs.
- **Fast by default.** Declarative routes are served entirely in Rust.
  Handler dispatch adds as little overhead as a local IPC hop allows.
- **Runs anywhere.** The same config and handlers work on a VPS, in a
  container, on edge isolates and on serverless platforms.

## Non-goals

- A plugin API for Rust code inside the router.
- A general-purpose web framework or template engine.
- Service-mesh features (mTLS between services, sidecars, traffic shifting
  across clusters).
- Making config a programming language. See the config rule in
  [`config.md`](config.md#the-rule-config-never-computes).

## Layers

```
┌──────────────────────────────────────────────────────────────────┐
│ unsworn-core   pure Rust, no I/O, compiles to native and wasm32    │
│   config parsing and validation · route matching                 │
│   request validation · auth verification · transforms            │
│   policies (CORS, headers) · dispatch decision                   │
├──────────────────────────────────────────────────────────────────┤
│ ingress      hyper (server/container) │ Worker fetch │ fn shim   │
├──────────────────────────────────────────────────────────────────┤
│ dispatch     worker pools (Tier 1/2) │ in-process call │ platform │
├──────────────────────────────────────────────────────────────────┤
│ state        in-memory (single node) │ KV / external store       │
└──────────────────────────────────────────────────────────────────┘
```

### `unsworn-core`

The core takes a request and the loaded config and returns a **decision**:

- `Respond(response)`: a static response, redirect, CORS preflight, cached
  response or validation error.
- `Proxy(upstream, request)`: forward to an HTTP service.
- `Serve(file)`: a static file.
- `Call(handler, validated_request, context)`: run a handler, optionally after
  one or more `before` handlers.

The core never opens sockets, spawns processes, reads files or reads the
clock. Hosts pass in whatever it needs (config text, current time, store
lookups). This keeps it testable and lets it compile to `wasm32`, which is
what makes the edge and serverless targets possible without a rewrite.

Anything stateful, such as rate-limit counters and cache entries, goes
through a small `Store` trait that each host implements.

### Hosts

A host combines an ingress, a dispatcher and a store around the core.

| Host | Ingress | Dispatch | Store |
|---|---|---|---|
| `unsworn` binary (servers, containers) | hyper on tokio, HTTP/1.1 and HTTP/2, TLS | worker process pools over Tier 1 or Tier 2 | in memory, optionally external |
| Edge isolate (v2) | the platform's `fetch` event | direct in-process calls to bundled JS/WASM handlers | platform KV or durable storage |
| Serverless (v3) | the platform's function event, via a generated shim | the handler runs in the same function | external store |

## Request lifecycle (binary host)

1. **Accept.** hyper accepts the connection, terminates TLS and parses HTTP.
2. **Match.** The core matches the method and path against the route table,
   a radix tree built at load time from every route source: `[[route]]`
   entries, the routes directory and the OpenAPI spec (see
   [`config.md`](config.md#route-sources)).
3. **Policies.** CORS, IP rules, body size limit, rate limit.
4. **Authenticate.** JWT/JWKS or API key verification. Claims go into the
   request context.
5. **Validate.** Path params, query, headers and body are checked against the
   route's schemas and converted to typed values (`"42"` becomes `42`).
6. **Cache lookup.** A hit is returned directly.
7. **Before handlers.** Each one may add to the context or reject the request.
8. **Dispatch.** The handler runs in a worker. The response is validated
   against the response schema if one is declared (configurable: off, log, or
   enforce).
9. **Transform and respond.** Header rules apply, the cache is filled and the
   response is sent.
10. **Observe.** Access log, metrics and trace spans for every step.

Declarative routes stop at step 2, 3, 5 or 6, so they never touch a worker.

## Workers

### Runtimes

A **runtime** is a config entry describing how to start workers for some
file extensions. Unsworn has no built-in knowledge of any language.

```toml
[runtimes.go]
extensions = [".go"]
build      = "go build -o .unsworn/go-worker ./routes/..."
command    = ".unsworn/go-worker"
protocol   = "binary"
workers    = 2
```

- Each request carries the route's **handler reference** (`<file>[#<name>]`).
  Unsworn uses only the file part, to pick the runtime; the worker interprets
  the rest.
- **Interpreted languages** (TypeScript, Python) load handler files on demand
  and call the named export.
- **Compiled languages** (Go, Rust) need a `build` step that produces one
  worker binary containing every handler for that runtime, with a registry
  keyed by handler reference. Unsworn runs the build step itself, at startup and on
  file changes in dev mode.

### Supervision

- **Pools.** Each runtime has a pool of N worker processes. A worker can
  handle many requests at once if it declares `max_in_flight` in its `ready`
  message (see [`protocol.md`](protocol.md)).
- **Lazy start.** By default, a runtime's workers start on its first request,
  which keeps startup fast on scale-to-zero platforms. `eager = true` starts
  them at boot.
- **Crashes.** If a worker exits, its in-flight requests get `502` and Unsworn
  restarts it with exponential backoff. Repeated crashes mark the runtime
  unhealthy in `/_unsworn/health` and in metrics.
- **Verification.** When a worker that supports it starts, Unsworn sends a
  `verify` message listing the handler references it will receive, so typos
  in handler names surface at startup instead of on the first request (see
  [`config.md`](config.md#handler-references)).
- **Timeouts.** A request past its timeout gets `504`, and Unsworn sends the
  worker a `cancel` message.
- **Logs.** Worker stderr is captured line by line into Unsworn's structured
  logs, tagged with runtime and worker id.
- **Reload.** On config or handler changes, Unsworn starts new workers, moves
  new requests to them and lets old workers finish their in-flight requests.

## Protocol tiers

| Tier | Transport | Encoding | SDK required | Purpose |
|---|---|---|---|---|
| 1 | stdin/stdout | JSON lines, optional raw body bytes | No | Universal: any language, including future ones |
| 2 | Unix socket (named pipe on Windows) | Length-prefixed binary frames | Yes | Speed: multiplexing, zero-copy bodies, streaming |

Both tiers carry the **same message model**, so the router has one dispatch
pipeline with two codecs. Plain HTTP services are covered by the built-in
`proxy` route type instead of a third tier.

The **portable contract is the handler signature** provided by each language
SDK, not the wire format. The same handler file runs in a Tier 2 worker, in
an edge isolate or in a serverless function. A Tier 1 worker written by hand
without an SDK works only on hosts that can run processes.

## MCP

Unsworn supports remote MCP servers at three levels, described in
[`mcp.md`](mcp.md):

- **v1:** in front of an existing MCP server. This needs no MCP code in
  Unsworn, only general features: OAuth protected-resource metadata, `Origin`
  checks, SSE pass-through and sticky proxying.
- **v2:** MCP tools as handlers. The MCP JSON-RPC handling lives in
  `unsworn-core`, like every other request decision, so it works on every
  host. A tool call becomes a normal `request` to a worker.
- **v3:** routes offered as tools, using each route's existing contract.

By default the MCP endpoint runs without sessions, so it needs no shared
state and works on edge and serverless targets too.

## Deployment targets

- **v1: servers and containers.** The `unsworn` binary plus an official base
  image that projects extend with their runtimes.
- **v2: edge isolates** (`unsworn build --target cloudflare`). The core is
  compiled to WASM inside the Worker. JS/TS handlers are bundled in and called
  directly. Rust handlers compile to WASM. Languages that can't run there need
  a container target instead.
- **v3: serverless functions** (`--target vercel`, `--target lambda`).
  Declarative routes become the platform's routing config (for example
  Vercel's Build Output API). Each handler becomes a native function wrapped
  by a shim that runs the WASM core for validation and auth before calling it.

Features that need shared state (rate limits, caches) need a configured store
on stateless targets. `unsworn check --target <name>` reports this from a
per-target capability table.

## Crates

| Path | Role |
|---|---|
| `crates/unsworn-core` | Pure core: config, route sources, matching, policies, validation, auth checks. No I/O, no tokio. |
| `crates/unsworn-proto` | The message model and both encodings, shared with a future Rust SDK. |
| `crates/unsworn` | The binary: HTTP ingress (hyper), worker pools, proxying, static files, compression, CLI. |
| `sdk/js` | JS/TS SDK for Bun, Node and Deno. |
| `sdk/python` | Python SDK (asyncio). |
| `unsworn-wasm` | Planned (v2): core bindings for edge and serverless hosts. |

## CLI

| Command | What it does |
|---|---|
| `unsworn serve` | Run the router. |
| `unsworn dev` | Serve with file watching, handler rebuilds and verbose errors. |
| `unsworn check [--deep] [--target <t>]` | Validate config and routes and report target capability gaps. `--deep` also starts workers to verify handler names. |
| `unsworn routes [--policies]` | Print the resolved route table and the source of each route. `--policies` adds each route's merged policies and where each value came from. |
| `unsworn openapi` | Emit the OpenAPI document. |
| `unsworn gen <lang>` | Planned: generate typed request and response models for handlers. |
| `unsworn conformance --runtime <r>` | Run the protocol conformance suite against a runtime. |
| `unsworn build --target <t>` | Planned (v2/v3): produce a deployment for an edge or serverless target. |
| `unsworn schema` | Print the config's JSON Schema. |
| `unsworn init` | Write a starter config. |

## Decisions made in v1

- **HTTP:** HTTP/1.1 and HTTP/2, with TLS from certificate files. WebSockets
  are proxied. HTTP/3 and automatic certificates (ACME) come later; until
  then, run behind a platform load balancer, Caddy or Cloudflare for those.
- **Request bodies** are buffered up to `max_body` before reaching a worker.
  Proxied requests stream.
- **Response validation** defaults to `log`.
- **Tier 2 encoding:** length-prefixed frames carrying the same JSON as
  Tier 1, plus raw body bytes (see [`protocol.md`](protocol.md)).

## Open questions

- Windows support for Tier 2: named pipes, or TCP on localhost?
- Request body streaming to workers, for large uploads.
- A shared store (Redis first) for rate limits and caches across instances.
