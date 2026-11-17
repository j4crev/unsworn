# Unsworn architecture

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
│ unsworn-core   pure Rust, no I/O, compiles to native and wasm32  │
│   config parsing and validation · route matching                 │
│   the request pipeline: policies, auth, validation, caching,     │
│   before handlers, actions, MCP · store rules                    │
├──────────────────────────────────────────────────────────────────┤
│ ingress      hyper (server/container) │ Worker fetch │ fn shim   │
├──────────────────────────────────────────────────────────────────┤
│ dispatch     worker pools (Tier 1/2) │ in-process call │ platform │
├──────────────────────────────────────────────────────────────────┤
│ state        in-memory (single node) │ KV / external store       │
└──────────────────────────────────────────────────────────────────┘
```

### `unsworn-core`

The core runs every request from matching to the final headers. A host
builds a `Router` (`unsworn_core::pipeline`) for each loaded project and
calls `Router::handle` with the request, the client's IP and a value that
implements the `Host` trait. `Host` is the I/O the core needs and never does
itself:

- reading a request body, or passing it on unread with a size limit
- calling a handler (or a `before` handler) by a deadline
- proxying to an upstream, and serving a static file
- outgoing requests, for JWKS documents and API-key verifiers
- timeouts, and work that shouldn't delay the response, such as a write to
  a remote store
- the wall clock and a monotonic clock
- the store

Everything else happens in the core, the same way on every host: matching,
CORS, origin and IP rules, authentication (including JWKS refresh and the
verdict cache for remote API keys), rate limits, validation, cache lookups
and fills, `before` handlers, every route action, response validation,
header rules and the MCP endpoint. Bodies the core doesn't need to read,
such as streamed handler replies and proxied responses, pass through it as
the host's own body type.

#### The plan

`unsworn_core::plan` turns a compiled project into its *plan*: for each
route and method, the policies every layer adds up to (worked out by the
same merge a request goes through), its checks and what it does, next to
the auth schemes and server limits. `unsworn plan` writes it as text and
`unsworn diff` compares two of them. A plan is built from a *symbolic*
compile (`compile_symbolic`), which leaves each `{env.NAME}` as written
instead of reading the variable, so it's the same on every machine and
holds no secret. See [`plan.md`](plan.md).

#### Generated types

`unsworn_core::codegen` turns each handler's contract (its routes'
`params`, `query`, `body` and `response` schemas, or a tool's `input` and
`output`) into types for TypeScript, Python, Go or Rust. The schemas go
through one small type model first, so the four languages agree on what a
schema means. `unsworn gen` and `unsworn dev` write the files. See
[`types.md`](types.md).

The core never opens sockets, spawns processes, reads files or reads the
clock. Config files come through a separate `Host` trait for loading
(`unsworn_core::project::Host`). This keeps it testable (its pipeline tests
run against an in-memory host) and lets it compile to `wasm32`, which is
what makes the edge and serverless targets possible without a rewrite. CI
builds it for `wasm32-unknown-unknown`, and its `clippy.toml` rejects clock,
file and socket calls, which compile for `wasm32` but panic there.
`Host`'s futures are `Send` on native targets only, so a JavaScript host can
hold platform values that aren't.

#### Store

Rate-limit counters and cached responses go through two traits in
`unsworn_core::store`, because platforms back them differently:

- `RateLimiter::take` needs consistent increments: a Redis script, or a
  Durable Object on Cloudflare.
- `ResponseCache::get` and `put` suit an eventually consistent cache: Redis,
  the Cache API or KV.

The core owns the rules every backend shares: the token bucket
(`take_token`), the cache entry and its byte encoding, freshness, and what a
failure means (`[store].on_error`). Backends only store and report errors.

### Hosts

A host combines an ingress, a dispatcher and a store around the core.

| Host | Ingress | Dispatch | Store |
|---|---|---|---|
| `unsworn` binary (servers, containers) | hyper on tokio, HTTP/1.1 and HTTP/2, TLS | worker process pools over Tier 1 or Tier 2 | in memory, optionally external |
| Cloudflare Worker (v2) | the Worker's `fetch` event | direct calls to bundled JS/TS handlers in the isolate | a Durable Object per rate-limit bucket, the Cache API |
| Serverless function (v3) | the platform's request or event, in one Node function | direct calls to bundled JS/TS handlers in the process | Redis through a REST API |

## Request lifecycle (binary host)

The binary does steps 1 and 12. Steps 2 to 11 run in the core's `Router`,
which calls back into the binary for I/O.

1. **Accept.** hyper accepts the connection, terminates TLS and parses HTTP.
2. **Match.** The core matches the method and path against the route table,
   a radix tree built at load time from every route source: `[[route]]`
   entries, the routes directory and the OpenAPI spec (see
   [`config.md`](config.md#route-sources)).
3. **Access rules.** CORS preflights are answered, then `origin` and IP
   rules are checked.
4. **Authenticate.** JWT/JWKS or API key verification (against listed keys,
   or by calling a remote verifier). Claims go into the request context.
5. **Rate limit.** A limit keyed on a claim such as `auth.sub` comes after
   authentication. One keyed on the IP or a header is counted before it, and
   a request that fails authentication counts against either kind.
6. **Validate.** Path params, query and headers are checked against the
   route's rules and converted to typed values (`"42"` becomes `42`).
7. **Body.** The body size limit is applied, then the body is read and
   checked against the route's schema, unless the route streams it to the
   handler.
8. **Before handlers.** Each one may add to the context or reject the request.
9. **Cache lookup.** A hit is returned directly. It comes after `before`
   handlers, so one that rejects a caller does so even when a response is
   cached.
10. **Dispatch.** The handler runs in a worker. The response is validated
    against the response schema if one is declared (configurable: off, log, or
    enforce).
11. **Transform and respond.** Header rules apply, the cache is filled and the
    response is sent.
12. **Observe.** The access log and metrics record the request.

A rejected request stops at the step that rejects it, and a cache hit stops
at step 9. Routes with a `respond`, `redirect`, `static` or `proxy` action
are answered at step 10 without a worker.

## Workers

### Runtimes

A **runtime** is a config entry describing how to start workers for some
file extensions. Unsworn has no built-in knowledge of any language.

```toml
[runtimes.go]
extensions = [".go"]
build      = "go build -o .unsworn/go-worker ./routes"
command    = ".unsworn/go-worker"
protocol   = "framed"
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
- **Reload.** On a reload (file changes in `unsworn dev`, `SIGHUP` for
  `unsworn serve`), Unsworn starts new workers, moves new requests to them
  and lets old workers finish their in-flight requests.

## Protocol tiers

| Tier | Transport | Encoding | SDK required | Purpose |
|---|---|---|---|---|
| 1 | stdin/stdout | JSON lines, optional raw body bytes | No | Universal: any language, including future ones |
| 2 | Unix socket | Length-prefixed frames: JSON message plus raw body bytes | Yes | Large bodies, and a worker whose stdout stays its own |

Both tiers carry the **same message model**, so the router has one dispatch
pipeline with two codecs. Neither is the fast one across the board: for
small requests Tier 1 is slightly ahead in every SDK, and from about 64 KB
of body Tier 2 is ahead by 15 to 30% in most of them (see
[benchmarks](benchmarks.md)). Plain HTTP services are covered by the built-in
`proxy` route type instead of a third tier.

The **portable contract is the handler signature** provided by each language
SDK, not the wire format. The same handler file runs in a Tier 2 worker, in
an edge isolate or in a serverless function. A Tier 1 worker written by hand
without an SDK works only on hosts that can run processes.

## MCP

Unsworn supports remote MCP servers at three levels, described in
[`mcp.md`](mcp.md):

- **In front of an existing MCP server.** This needs only general
  features: OAuth protected-resource metadata, `Origin` checks, SSE
  pass-through and sticky proxying. Per-tool policies read each request's
  tool name first.
- **MCP tools as handlers.** The MCP JSON-RPC handling lives in
  `unsworn-core`, like every other request decision, so it works on every
  host. A tool call becomes a normal `request` to a worker.
- **Routes offered as tools,** using each route's existing contract.

Unsworn implements MCP 2026-07-28, which is stateless: no sessions and no
handshake, with every request carrying its own protocol version. The
endpoint needs no shared state, so it works on edge and serverless targets
too.

In the core, `[mcp]` compiles to a `POST`-only route for the endpoint and
one hidden route per tool. Tool routes are never matched by path. They
carry the tool's handler, its input and output schemas, and its policies
merged as `[defaults]`, prefix entries for the endpoint path, `[mcp]`, then
the tool's own. That lets tools reuse the route machinery: per-route rate
limit buckets, validation, `before` handlers and `unsworn routes --policies`.

## Deployment targets

- **v1: servers and containers.** The `unsworn` binary plus an official base
  image that projects extend with their runtimes.
- **v2: edge isolates** (`unsworn build --target cloudflare`). The core runs
  as WebAssembly (`crates/unsworn-wasm`) inside the Worker, and JS/TS
  handlers are bundled in and called directly. The wasm module asks a
  JavaScript *bridge* for its I/O, the way the binary implements `Host`; the
  Cloudflare bridge is `@unswornjs/sdk/cloudflare`. Languages that can't run
  there need a container target instead. See [`cloudflare.md`](cloudflare.md).
- **v3: serverless functions** (`--target vercel`, `--target netlify`,
  `--target lambda`). One Node function runs the whole project: the same
  wasm core, with the Node bridge in `@unswornjs/sdk/node` and a thin entry
  per platform. The platform's routing only sends every path to that
  function, so a route behaves the same on every target. See
  [`serverless.md`](serverless.md).

Features that need shared state (rate limits, caches) need a store on
stateless targets: Cloudflare brings its own, and serverless functions use
Redis through a REST API, from `[store]`. `unsworn check --target <name>` reports what a config needs
from a per-target capability table (`unsworn-core/src/targets.rs`).

## Crates

| Path | Role |
|---|---|
| `crates/unsworn-core` | Pure core: config, route sources, matching, and the request pipeline (policies, auth, validation, caching, MCP) behind the `Host` and store traits. No I/O, no tokio. |
| `crates/unsworn-proto` | The message model and both encodings, shared by the server and the Rust SDK. |
| `crates/unsworn` | The binary: the `Host` for servers (hyper, worker pools, proxying, static files, the memory and Redis stores), compression, metrics, CLI. |
| `crates/unsworn-wasm` | The core as a WebAssembly module for JavaScript hosts, with the `Bridge` interface they implement. Built into `sdk/js/wasm`. |
| `sdk/js` | JS/TS SDK for Bun, Node and Deno, plus the runtimes for built targets: the Cloudflare Worker (`@unswornjs/sdk/cloudflare`), and Node functions (`@unswornjs/sdk/node`, with `vercel`, `netlify` and `lambda` entries). |
| `sdk/python` | Python SDK (asyncio). |
| `sdk/go` | Go SDK, including an `http.Handler` adapter. |
| `sdk/rust` | Rust SDK (`unsworn-sdk`, Tokio). |

## CLI

| Command | What it does |
|---|---|
| `unsworn serve` | Run the router. |
| `unsworn dev` | Serve with file watching, handler rebuilds and verbose errors. |
| `unsworn check [--deep] [--target <t>]` | Validate config and routes and report target capability gaps. `--deep` also starts workers to verify handler names. |
| `unsworn routes [--policies]` | Print the resolved route table and the source of each route. `--policies` adds each route's merged policies and where each value came from. |
| `unsworn plan [--check]` | Write what every route and MCP tool enforces to `unsworn.plan`. `--check` fails when the file is out of date. See [`plan.md`](plan.md). |
| `unsworn diff [<base>] [--fail-on <looser\|any>]` | Compare the rules with those at a git revision or in another directory, marking the changes that let more requests through. |
| `unsworn openapi` | Emit the OpenAPI document. |
| `unsworn gen [<lang>] [--check]` | Write typed request and response models for handlers, from each route's schemas, to the files `[gen]` lists. See [`types.md`](types.md). |
| `unsworn conformance --runtime <r> --handlers <file>` | Run the protocol conformance suite against a runtime. |
| `unsworn build --target <t>` | Bundle the project into a Cloudflare Worker (`cloudflare`) or a serverless function (`vercel`, `netlify`, `lambda`). |
| `unsworn schema` | Print the config's JSON Schema. |
| `unsworn init` | Write a starter config. |

## Decisions made in v1

- **HTTP:** HTTP/1.1 and HTTP/2, with TLS from certificate files. WebSockets
  are proxied. HTTP/3 and automatic certificates (ACME) come later; until
  then, run behind a platform load balancer, Caddy or Cloudflare for those.
- **Request bodies** are buffered up to `max_body` before reaching a worker,
  unless the route sets `stream_body`. Proxied requests stream.
- **Response validation** defaults to `log`.
- **Tier 2 encoding:** length-prefixed frames carrying the same JSON as
  Tier 1, plus raw body bytes (see [`protocol.md`](protocol.md)).
