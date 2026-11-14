# Unsworn

An API router with allegiance to no language.

Declare your API once. Unsworn enforces it in front of handlers written in
any language, by you or your AI agents. Start by putting it in front of the
app you already have.

Unsworn is a single compiled binary that handles the HTTP side of an API:
routing, validation, auth, CORS, rate limiting, caching, proxying, logging
and metrics. You configure it with a TOML file. Anything that
needs real logic goes to a **handler**, a plain function in whatever language
suits that endpoint. The same handlers can also be offered to AI agents as
MCP tools.

> **Status:** v1 in progress. The server, worker pools, both protocol tiers,
> the CLI and the JS/TS, Python, Go and Rust SDKs work and are tested; there's no
> tagged release yet. See [what's in v1](#whats-in-v1).

## Why Unsworn

### Your API's rules live outside your code

In most frameworks, auth checks, input validation and rate limits are code,
and code can be wrong. One forgotten middleware or one unvalidated field is enough to get you rekt.

With Unsworn, the security-critical part of your API is a short file of rules someone can review in a minute.
Those rules are declared in `unsworn.toml` and enforced by a
binary that handler code can't change. A handler only runs after its request
has been authenticated, rate-limited and validated, so it can't forget to do
any of that.

### Built for MCP servers

AI agents reach your systems through MCP servers. Every caller of an MCP
server is a model acting for someone, so that's where rules outside your
code matter most. Unsworn supports remote MCP servers in three steps:

- **v1: In front of the MCP server you already have.** OAuth token checks
  with the metadata MCP clients expect, origin checks, rate limits and audit
  logs, with no auth code in the server.
- **v2: Tools as plain functions.** Unsworn speaks MCP itself. Each tool is
  a handler in any language, with no MCP SDK, its arguments validated before
  it runs, and its own scopes and rate limits.
- **v3: Your API, also as tools.** Mark a route as a tool, and one handler
  with one set of rules serves both your REST clients and AI agents.

```toml
[[mcp.tool]]
name        = "create_invoice"
description = "Create a draft invoice for a customer"
input       = "schemas/invoice.json"
handler     = "src/billing.ts#createInvoice"
auth        = { scheme = "oauth", scopes = ["billing:write"] }
rate_limit  = { requests = 10, per = "1m", key = "auth.sub" }
```

See [`docs/mcp.md`](docs/mcp.md).

### Start in front of the app you already have

Unsworn can sit between your existing app and the internet. Run it behind
your platform's load balancer, Caddy, nginx or Cloudflare, or on its own with a simple setup.
Point a catch-all route at your existing Express, Hono, FastAPI or Go app,
with no code changes, and you can immediately:

- add JWT or API-key auth, rate limits and caching to any path
- validate requests against an OpenAPI spec before they reach your app
- serve health checks, redirects and static files without touching the app

Then move routes out of the old app one at a time, at your own pace. Moving
back out of Unsworn is just as easy, so trying it is low risk.

### The right language for each endpoint

Plenty of apps use one language, except for the parts that need Python's AI and
data libraries. Those parts usually turn into a separate service with its own
deploy, networking and auth.

With Unsworn, `routes/chat.post.py` sits next to `routes/users.get.ts`. They
share one router, one auth config, one API contract and one deploy. The same
goes for a Go endpoint that needs Go's concurrency, or a Rust endpoint that
needs raw speed.

### Speed where it matters

- **Declarative endpoints run entirely in Rust.** Static responses,
  redirects, proxies, cached responses and rejected requests (failed auth,
  invalid input, rate limits) never reach a handler.
- **Handlers run in worker pools across all CPU cores,** without any
  clustering setup in your code.
- **A slow endpoint can move to a faster language** without changing the API.
  Replace `get.ts` with `get.rs`, and clients notice nothing except the
  speed, which Unsworn's metrics will show.

A handler call does cross a process boundary, so a trivial endpoint won't
beat the same code running directly on Bun, for example. The gains come from the points
above. Benchmarks will be published before any speed claims are made.

## A quick look

**Day one:** Unsworn in front of an existing app, with no code changes.

```toml
# unsworn.toml
[server]
listen = "0.0.0.0:8080"

[auth.jwt]
type     = "jwt"
jwks_url = "https://auth.example.com/.well-known/jwks.json"

[[route]]
path    = "/health"
respond = { status = 200, json = { ok = true } }

[[route]]
path  = "/{*rest}"
proxy = "http://127.0.0.1:3000"          # your existing app, unchanged

[[route]]                                # add policies to paths inside it
path       = "/api/{*rest}"
auth       = "jwt"
rate_limit = { requests = 100, per = "1m", key = "auth.sub" }
```

**Later:** one endpoint moved out of the app, into a TypeScript handler.

```toml
[runtimes.ts]
extensions = [".ts"]
command    = "bun run node_modules/@unswornjs/js/src/worker.ts"
protocol   = "binary"

[[route]]
path    = "/api/users/{id}"
method  = "GET"
auth    = "jwt"
params  = { id = { type = "integer", min = 1 } }
cache   = { ttl = "30s", vary = ["Authorization"] }
handler = "src/users.ts#getUser"
```

```ts
// src/users.ts
import type { Handler } from "@unswornjs/js";

export const getUser = (async (req, ctx) => {
  // req.params.id is already validated as an integer >= 1,
  // and ctx.auth holds the verified JWT claims.
  const user = await db.users.find(req.params.id);
  return { status: 200, json: user };
}) satisfies Handler<{ params: { id: number } }>;
```

`/api/users/{id}` is more specific than the catch-all, so it now goes to the
handler. Everything else still goes to the old app.

## Is Unsworn a good fit?

**Probably, if you:**

- are building a remote MCP server, or want AI agents to use your API with
  the same rules as everyone else
- want auth, validation and rate limits enforced outside your application code
- want API features such as auth, validation and contracts in front of an
  existing app, without rewriting it
- need more than one language in one API, such as TypeScript plus Python for
  AI features
- have a few hot endpoints you'd like to move to Rust or Go without splitting
  off a new service

**Probably not, if you** have a single-language app with no performance
problems, are happy with your framework's middleware, and don't need
anything in front of it. A framework like Hono, Elysia or axum on its own is simpler
in that case.

## What's built in

All of this is configured in `unsworn.toml`, with no code:

| Area | Features |
|---|---|
| AI agents (MCP) | OAuth protected-resource metadata, origin checks and streaming pass-through for MCP servers (v1); MCP tools as handlers and per-tool policies (v2); routes as tools (v3) |
| Responses | static JSON or text, redirects, static files with SPA fallback, health checks |
| Forwarding | reverse proxy, load balancing, catch-all to an existing app |
| Security | JWT/JWKS, API keys, CORS, IP allow and deny lists, body size limits |
| Traffic | rate limits, timeouts, response caching |
| Contracts | params, query, header and body validation (JSON Schema), OpenAPI input and output |
| Ops | access logs, Prometheus metrics, request IDs, health checks, config hot reload |

## How it works

- **Routes come from wherever suits you,** and all feed one route table:
  `[[route]]` entries pointing at functions (`src/users.ts#getUser`), a
  nested or flat routes directory (`routes/users.[id].get.ts`), an OpenAPI
  spec, or a catch-all to an existing app.
- **Handlers are plain functions.** Language SDKs give each one a typed
  request and hide everything else. The same handler code is meant to run on
  a server, at the edge and in serverless functions.
- **Any language can join.** The universal protocol is JSON lines over
  stdin/stdout, so a language with no SDK, even one that doesn't exist yet,
  works on day one. Official SDKs use a faster binary protocol.
- **Custom middleware is just more handlers.** A `before` handler, in any
  language, can add context such as the current tenant, or reject a request.
- **Mistakes surface early.** `unsworn check` validates config and routes
  with precise, fixable errors, and SDK workers confirm at startup that every
  handler name in the config exists. The config has a published JSON Schema
  for editor autocomplete.

Details: [architecture](docs/architecture.md), [config](docs/config.md),
[protocol](docs/protocol.md), [conformance](docs/conformance.md) and
[MCP](docs/mcp.md).

## Getting started

Unsworn runs on Linux (x86_64 and arm64). There's no release yet, so build from
source (Rust 1.88 or newer):

```sh
cargo install --git https://github.com/j4crev/unsworn unsworn
```

Once releases exist: `curl -fsSL https://raw.githubusercontent.com/j4crev/unsworn/main/install.sh | sh`,
or the `ghcr.io/j4crev/unsworn` base image for containers.

```sh
unsworn init        # writes a starter unsworn.toml
unsworn dev         # serves it, reloads on changes, verifies handler names
```

| Command | What it does |
|---|---|
| `unsworn serve` | Run the router (`--listen`, `--log-format json`); `SIGHUP` reloads |
| `unsworn dev` | Serve with file watching, every runtime started up front, and detailed errors |
| `unsworn check` | Validate config, routes and handler files; `--deep` asks workers whether every handler exists; `--target <t>` checks a deployment target |
| `unsworn routes` | Print the route table and each route's source; `--policies` shows merged policies |
| `unsworn openapi` | Print the OpenAPI document for what Unsworn serves |
| `unsworn conformance` | Test a runtime's workers against the protocol |
| `unsworn schema` | Print the config's JSON Schema (also at [`schema/unsworn.schema.json`](schema/unsworn.schema.json)) |

Handler SDKs live in [`sdk/js`](sdk/js) (Bun, Node, Deno),
[`sdk/python`](sdk/python), [`sdk/go`](sdk/go) and [`sdk/rust`](sdk/rust). [`examples/`](examples) has a project that
uses both, behind a catch-all.

## What's in v1

| Area | Status |
|---|---|
| Route sources: config, nested and flat route files, OpenAPI, catch-all | Done |
| Policies: defaults, named sets, folder `_policies.toml`, prefix and exact entries | Done |
| Auth (JWT/JWKS, API keys, scopes, OAuth protected-resource metadata), CORS, origin and IP rules | Done |
| Rate limits and response caching (in memory) | Done |
| Validation of params, query, headers, bodies and responses | Done |
| Respond, redirect, static files, reverse proxy (WebSockets, SSE, sticky upstreams), handlers | Done |
| Worker pools: Tier 1 and Tier 2, build steps, verify, cancel, restarts | Done |
| HTTP/1.1 and HTTP/2, TLS, compression, graceful shutdown, reloads | Done |
| JS/TS, Python, Go and Rust SDKs, conformance suite | Done |
| Install script, Docker image, release workflow | Written; runs on the first tagged release |
| Shared stores (Redis), automatic TLS certificates, HTTP/3, OpenTelemetry | Later |
| Typed request models for handlers (`unsworn gen`) | Later |

## Where Unsworn sits

| Tool | What it is | Where custom logic lives |
|---|---|---|
| nginx, Caddy, Envoy | Config-driven proxies and web servers | C or Go modules compiled in, Lua, or WASM filters |
| Kong and similar API gateways | Gateways in front of separate services | Plugins written for the gateway |
| Fermyon Spin | Config-driven WASM app host | WASM components only |
| Express, axum, Hono, Go `net/http` | Frameworks | The app itself, in one language |
| **Unsworn** | An API layer that sits in front of your app and also hosts your handlers | Plain functions in any language |

Unsworn is **not** a service mesh, a full API-management product (developer
portals, billing, key issuance) or a web framework.

## Where Unsworn runs

Unsworn is designed to run on three kinds of platforms. The router core does no
I/O and compiles to WebAssembly, so each target hosts the same core in a
different way. See [`docs/architecture.md`](docs/architecture.md).

| Platform type | Examples | How Unsworn runs | Planned |
|---|---|---|---|
| **Servers and containers** | Any VPS or bare metal, Docker, Kubernetes, Fly.io, Railway, Render, Google Cloud Run, AWS ECS/Fargate, Cloudflare Containers | The `unsworn` binary plus worker processes, from an official base image | v1 |
| **Edge isolates** | Cloudflare Workers and similar V8-isolate platforms | Core compiled to WASM inside the isolate; JS/TS and WASM handlers bundled in and called directly | v2 |
| **Serverless functions** | Vercel Functions, AWS Lambda, Netlify Functions | `unsworn build --target <platform>` turns declarative routes into the platform's own routing config and each handler into a native function with a thin shim | v3 |

Some features need shared state on stateless platforms, such as rate limits
and caches. `unsworn check --target <platform>` reports what a config needs on
that target before you deploy.

## Roadmap

| Version | Runs on | MCP |
|---|---|---|
| v1 | Servers and containers | In front of existing MCP servers |
| v2 | Adds edge isolates | Tools as handlers, and per-tool policies for proxied MCP servers |
| v3 | Adds serverless functions | Routes and OpenAPI operations as tools |

## License

[MIT](LICENSE)
