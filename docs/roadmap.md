# Roadmap

What's planned for Unsworn and what's still to be built. For what each
deployment target is meant to look like, see
[`architecture.md`](architecture.md#deployment-targets).

| Version | Runs on | Status |
|---|---|---|
| v1 | Servers and containers | Available (v0.3.0) |
| v2 | Adds edge isolates | Available (v0.3.0): Cloudflare Workers |
| v3 | Adds serverless functions | Available (v0.3.0): Vercel and Netlify as Node functions; AWS Lambda as the server in a container |

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
  Not planned for now; see item 5 under
  [Beyond the front door](#beyond-the-front-door).
- **More isolate platforms,** such as Fastly, each with its own bridge.

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
  functions in Node, called the way each platform calls them. Vercel has
  been deployed to and checked; Netlify hasn't. Lambda's image has been
  built and run under Lambda's constraints, and not on Lambda.
- **More than one function per project,** for routes that need their own
  memory, duration or permissions: named groups, each a function running
  the same router with some of the handlers. Only if someone needs it.
- **Other stores,** such as DynamoDB on Lambda.
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
     [`types.md`](types.md).
   - **A typed client for callers,** generated from the same schemas, so
     the code that calls an API is checked against it as its handlers are.
   - **A smaller core for edge and serverless builds.** The idea was a
     compiled plan the core runs, so those builds ship the plan instead
     of the config files and the code that reads them. Measured first,
     by taking pieces out of the WebAssembly core (gzipped):

     | | Size |
     |---|--:|
     | The core as it is | 1,457 KiB |
     | Without the TOML and YAML parsers | 1,218 KiB |
     | Without the config types and compiler either | 1,076 KiB at best |
     | As it is, without the `jsonschema` crate | 847 KiB |
     | Without the compiler or `jsonschema` | 482 KiB |

     Not planned for now. The core is under half of what Cloudflare's
     free plan allows (3 MB compressed), so none of this is worth its
     cost until someone is near a platform's size limit. In three parts:

     - **Config as JSON in the bundle,** with the two parsers left out
       of the core for these targets. Built, and not merged
       ([#77](https://github.com/j4crev/unsworn/pull/77)). The core was a
       sixth smaller and a project loaded about a fifth sooner in Node
       (13 to 10.5 ms for 5 routes, 48 to 37 ms for 300). Against that,
       the server and these targets would read config in two ways, the
       binary and the SDK would have to be released together, and a
       YAML config could pass `unsworn check` and fail `unsworn build`:
       YAML takes an unquoted number as text where JSON doesn't.
     - **A compiled plan the core runs,** in place of the config and the
       compiler. About another 140 KiB at best, for a second way to
       build a project that has to match the first for every setting.
       `unsworn.plan` stays text for people.
     - **A validator format of Unsworn's own,** in place of the
       `jsonschema` crate. That crate is about 600 KiB of the core, and
       replacing it is the only way to stop compiling each route's
       schema when a request first needs it (about 5 ms a route). It
       means implementing JSON Schema and giving the same errors on
       every target.
2. **A cheaper call into a handler.**
   - ~~**Worker pools that start small.**~~ Done: a runtime starts one
     worker and adds more when calls wait or its workers are busy and
     there's idle CPU; see [`workers`](config.md#how-many-workers).
   - **A worker connection per server thread,** so a call doesn't cross
     threads on its way to a worker and back. To be tried on its own
     first: the last attempt at one runtime per core made handler calls
     slower, because worker I/O stayed on another thread. A shared-memory
     ring in place of the socket is a further step and less certain:
     kernel sends are about a third of a call's cost, and Node would need
     a native addon to use one.
3. **Calls between handlers and to other services.**
   - ~~**`ctx.call` from one handler to another.**~~ Done: a
     `[[function]]` is a handler any other handler calls by name, in any
     language, over the worker's own connection. See
     [`functions.md`](functions.md).
   - ~~**Declared outbound services.**~~ Done: `ctx.fetch` reaches a
     service under `[outbound.<name>]`, and Unsworn adds its credential,
     so the handler never holds it. See [`outbound.md`](outbound.md).
     Still open: streamed bodies.
   - ~~**Functions, outbound services and shadows on edge and serverless
     targets.**~~ Done: the checks for `ctx.call` and `ctx.fetch` moved
     into the core, which runs them on every target, and a `shadow` gets
     its copy everywhere. What differs there: handlers share a process
     with the core, so a service's key isn't kept from them, and a
     shadow's differences are logged but not counted.
   - ~~**`ctx.call` and `ctx.fetch` for HTTP runtimes.**~~ Done: a
     server run as an [HTTP runtime](http-runtime.md) calls functions and
     outbound services with plain HTTP requests to a local endpoint, on a
     socket or a port, sending back the token its request came with. It
     calls only what its runtime's `calls` names. See
     [`http-runtime.md`](http-runtime.md#calling-functions-and-outbound-services).
   - **Limits on what a worker can reach.** An outbound service keeps the
     credential from a handler, but nothing stops the handler opening its
     own connections or reading a file Unsworn's user can read (a TLS
     key, a `public_key` file), and a worker has whatever memory and CPU
     the machine gives it. To add, for each runtime: the hosts it may
     connect to and the files it may read (Landlock, a network namespace
     or a user of its own) and cgroup limits. The platform has to do this
     for now.
4. ~~**Choosing a language by measurement.**~~ Done: `unsworn top` shows
   each route's latency and its handler's share of time and CPU on a
   running server ([`top.md`](top.md)), and a route's `shadow` handler
   gets a copy of its requests so a rewrite's answers can be compared
   before it takes over ([`shadow.md`](shadow.md)).
5. **WebAssembly handlers** called in the server's process, and from the
   core on edge targets. Measured first, with a prototype
   ([`bench/wasm-prototype`](../bench/wasm-prototype)): a call into a
   WebAssembly handler adds 0.7 µs, against about 20 µs for a call to a
   worker, but code that computes runs half as fast as it does natively.
   So it would suit handlers that do little, and not the one that's in
   Rust for speed. It also needs wasmtime, a dependency of about 170
   packages. Not built, and not planned for now: the trade isn't worth
   it until people ask for it. The prototype stays, so the numbers can be
   taken again.
6. **One deployment.**
   - ~~**A container image target.**~~ Done: `unsworn build --target
     image` writes a Dockerfile for the project, with each runtime at the
     version the project names. See [`image.md`](image.md).
   - ~~**The binary on AWS Lambda,** for handlers in any language there,
     without the WebAssembly core.~~ Done: `unsworn build --target lambda`
     writes the server's image with AWS's Lambda Web Adapter in it, which
     replaces the Node function that target used to build. See
     [`lambda.md`](lambda.md).
7. **OpenTelemetry traces.** A request's `traceparent` is already carried
   through `ctx.call` and `ctx.fetch` ([`tracing.md`](tracing.md)), but
   Unsworn records no spans of its own. The design and each phase are in
   [`telemetry-design.md`](telemetry-design.md).
   - **Phase 0: config and checks.** `[telemetry]` names an `[outbound]`
     service to send spans to, checked when the config is compiled and
     listed in `unsworn plan`.
   - **Phase 1: a span for every sampled request, exported** over
     OTLP/HTTP by the server, once the size of the dependencies has been
     measured.
   - **Phase 2: spans for the server's I/O:** handler calls and the wait
     for a worker, upstreams, verifiers, functions and services.
   - **Phase 3: the pipeline's steps,** such as auth, rate limits,
     validation and the cache.
   - **Phase 4: a handler's own span as the parent of `ctx.call`,** in
     every SDK.
   - **Phase 5, later:** metrics and logs over OTLP, and the edge and
     serverless targets.

## Not scheduled yet

- **Automatic TLS certificates (ACME).** Until then, run behind a platform
  load balancer, Caddy or Cloudflare.
- **HTTP/3.**
- **More MCP features:** resources and prompts, elicitation, progress and
  change notifications, tasks. See [`mcp.md`](mcp.md#later).

## Open questions

Tracked where they apply:

- [Config](config.md#open-questions): several methods in one route file,
  environment-specific overrides.
- [Protocol](protocol.md#open-questions): handler-owned WebSockets,
  progress messages, elicitation.

How Unsworn is described used to be one more: on a server, handlers run
in other processes and can't change the rules, and on Cloudflare, Vercel
and Netlify they share an isolate or a process with the core. The README
now says which is which, under "Where Unsworn runs".
