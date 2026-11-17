# Roadmap

What's planned for Unsworn and what's still to be built. For what each
deployment target is meant to look like, see
[`architecture.md`](architecture.md#deployment-targets).

| Version | Runs on | Status |
|---|---|---|
| v1 | Servers and containers | Available (v0.3.0) |
| v2 | Adds edge isolates | Available (v0.3.0): Cloudflare Workers |
| v3 | Adds serverless functions | Available (v0.3.0): Vercel, Netlify and AWS Lambda |

## v2: edge isolates

Cloudflare Workers first (`unsworn build --target cloudflare`). Work needed:

- ~~**Build the core for `wasm32`.**~~ Done: `unsworn-core` and
  `unsworn-proto` build for `wasm32-unknown-unknown`, with `getrandom`'s
  JavaScript backend picked for that target. A CI job keeps it that way, and
  the core's `clippy.toml` rejects clock, file, process and socket calls,
  which compile for `wasm32` but panic there.
- ~~**Move the request pipeline into the core.**~~ Done:
  `unsworn_core::pipeline` runs every request, MCP included, behind a
  `Host` trait for the I/O. The binary is one host, and the core's tests
  run against an in-memory one. A Worker host implements the same trait.
- ~~**A store interface hosts implement.**~~ Done: `unsworn_core::store`
  defines a `RateLimiter` and a `ResponseCache` trait, so a host can back
  them differently, such as Durable Objects for rate-limit counters (which
  need consistent increments) and the Cache API or KV for cached responses.
  The core owns the token bucket, the cache entry format and what
  `on_error` means. The binary's memory and Redis stores implement both.
- ~~**`unsworn-wasm`.**~~ Done: the core as a WebAssembly module that asks
  a JavaScript bridge for I/O, so every JS platform reuses it. About 1.4 MB
  gzipped; CI keeps it under 1.6 MB.
- ~~**`unsworn build --target cloudflare`.**~~ Done: bundles the project
  files and JS/TS handlers into a Worker, with handlers called in the
  isolate, a Durable Object per rate-limit bucket, the Cache API for `cache`
  and Workers Static Assets for `static` routes. See
  [`cloudflare.md`](cloudflare.md).
- ~~**Fill in the target capability table.**~~ Done:
  `unsworn check --target cloudflare` reports handlers that can't run in a
  Worker, Redis as the store, and server settings Cloudflare ignores.
- ~~**MCP on the edge.**~~ Done: tools run as handlers in the Worker; the
  Worker tests cover it.
- ~~**WebSocket upgrades through `proxy` routes** in a Worker.~~ Done: the
  Worker returns the upstream's WebSocket, so Cloudflare passes messages
  through as the server does.

Still open for the edge:

- **WASM handlers** called directly, rather than through a JS wrapper.
- **More isolate platforms,** such as Deno Deploy and Fastly, each with its
  own bridge.

## v3: serverless functions

Vercel, Netlify and AWS Lambda (`unsworn build --target vercel`,
`--target netlify`, `--target lambda`). See [`serverless.md`](serverless.md).
The first plan was a function per handler, with declarative routes turned
into each platform's routing config. That would have meant re-expressing
matching, CORS and header rules per platform, and MCP tools need every
handler in one place anyway, so each target is one function that runs the
whole project, as a Worker does. Work needed:

- ~~**Build schemas on first use.**~~ Done: the wasm core compiles a
  route's validation schemas when a request first needs them. Compiling
  them all at load took 2 to 4 ms per route, on every cold start.
- ~~**A Node host.**~~ Done: `@unswornjs/sdk/node` is the bridge for Node,
  sharing handler calls, bodies and logging with the Cloudflare one.
- ~~**A store functions can reach.**~~ Done: `[store]` takes the URL and
  token of a Redis REST API (Upstash's protocol). Rate limits run the same
  script on the same keys as the server's Redis store.
- ~~**`unsworn build --target vercel`, `netlify` and `lambda`.**~~ Done:
  each bundles the project and its handlers with esbuild into the
  platform's own layout.
- ~~**Fill in the target capability table.**~~ Done: `unsworn check
  --target` reports a `rate_limit` or `cache` without a shared store, a
  `redis://` URL, and handlers that aren't JS or TS.

Still open for serverless:

- **Running on the platforms themselves.** The tests run the built
  functions in Node, called the way each platform calls them.
- **More than one function per project,** for routes that need their own
  memory, duration or permissions: named groups, each a function running
  the same router with some of the handlers. Only if someone needs it.
- **Other stores,** such as DynamoDB on Lambda, and Redis over TCP for
  functions inside a VPC.
- **Handlers in other languages.** The wasm core talks to a JavaScript
  bridge, so a Python or Go function would need another interface to it.
  Until then, those run on the server in a container.

## Beyond the front door

Unsworn's rules cover a request on its way in. The next stretch of work
takes them further: into review, into handler code, between handlers and
on the way out. In the order planned:

1. **The plan.** One artifact that holds what a project enforces.
   - ~~**`unsworn plan` and `unsworn diff`.**~~ Done: every route's merged
     rules in a file to commit, and a comparison of two versions of a
     project that marks what got looser. See [`plan.md`](plan.md).
   - ~~**Typed request models (`unsworn gen`).**~~ Done: types for
     TypeScript, Python, Go and Rust handlers from each route's params,
     query, body and response schemas, kept current by `unsworn dev`. See
     [`types.md`](types.md). Still open: a typed client for callers.
   - **A compiled plan the core runs,** so edge and serverless builds
     ship the plan instead of the config files and the parsers for them.
2. **A cheaper call into a handler.**
   - ~~**Worker pools that start small.**~~ Done: a runtime starts one
     worker and adds more when calls wait or its workers are busy and
     there's idle CPU; see [`workers`](config.md#how-many-workers).
   - **A worker connection per server thread,** so a call doesn't cross
     threads on its way to a worker and back. To be tried on its own
     first: the last attempt at one runtime per core made handler calls
     slower, because worker I/O stayed on another thread.
3. **Calls between handlers and to other services.**
   - ~~**`ctx.call` from one handler to another.**~~ Done: a
     `[[function]]` is a handler any other handler calls by name, in any
     language, over the worker's own connection. See
     [`functions.md`](functions.md). Servers and containers only so far.
   - ~~**Declared outbound services.**~~ Done: `ctx.fetch` reaches a
     service under `[outbound.<name>]`, and Unsworn adds its credential,
     so the handler never holds it. See [`outbound.md`](outbound.md).
     Still open: keeping a handler from opening its own connections
     (Landlock, a network namespace), which the platform has to do for
     now, and streamed bodies.
4. ~~**Choosing a language by measurement.**~~ Done: `unsworn top` shows
   each route's latency and its handler's share of time and CPU on a
   running server ([`top.md`](top.md)), and a route's `shadow` handler
   gets a copy of its requests so a rewrite's answers can be compared
   before it takes over ([`shadow.md`](shadow.md)).
5. **WebAssembly handlers** called in the server's process, and from the
   core on edge targets.
6. **A container image target** (`unsworn build --target image`), and the
   binary as an AWS Lambda custom runtime.

## Not scheduled yet

- **Automatic TLS certificates (ACME).** Until then, run behind a platform
  load balancer, Caddy or Cloudflare.
- **HTTP/3.**
- **OpenTelemetry** traces and metrics, alongside the Prometheus endpoint.
- **More MCP features:** resources and prompts, elicitation, progress and
  change notifications, tasks. See [`mcp.md`](mcp.md#later).

## Open questions

Tracked where they apply:

- [Config](config.md#open-questions): several methods in one route file,
  environment-specific overrides.
- [Protocol](protocol.md#open-questions): handler-owned WebSockets,
  progress messages, elicitation.
