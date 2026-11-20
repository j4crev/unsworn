# Unsworn

An API router with allegiance to no language.

That means your endpoint handlers can be written in virtually anything.

Unsworn handles the HTTP side of an API:
routing, validation, auth, CORS, rate limiting, caching, proxying, logging
and metrics. You configure it with a TOML file. Anything that
needs real logic goes to a **handler**, a plain function in whatever language
suits that endpoint. The same handlers can also be offered to AI agents as
MCP tools.

## AI Usage and Security Disclosure

**Latest release built with Fable 5.2 High**

**Last Security Audit on 2026-11-15 by Fable 5.2 Medium, followed by Opus 5.5 High**

## Why Unsworn

### Your API's rules live outside your code

In most frameworks, auth checks, input validation and rate limits are code,
but code can be wrong. One forgotten middleware or an unvalidated field is enough to get you rekt.

With Unsworn, the security-critical part of your API is one file of rules someone can review.
Those rules are declared in `unsworn.toml` and enforced by a
binary that handler code can't change. A handler only runs after its request
has been tested against those rules.

A forgotten rule doesn't pass quietly either. Once a project has an auth
scheme, a route that doesn't say who may call it is an error, so a public
endpoint is always one somebody wrote `auth = "none"` for.

And the rules reach past "is this a valid token". One line keeps every
caller to their own organization's paths, for every route under them:

```toml
[[route]]
path    = "/orgs/{org}/{*rest}"
require = { "params.org" = "auth.org" }
```

A change to the rules can be reviewed too. `unsworn plan` writes what every
route ends up enforcing to a file you commit, and `unsworn diff` says what
a branch changes and marks what it loosens:

```
$ unsworn diff origin/main
~ * /admin/{*rest}
  ! auth               jwt → none  (looser)
```

See [`docs/plan.md`](docs/plan.md).

The rules cover outbound traffic too. Declare a service a handler needs, and
Unsworn makes the request and adds the credential:

```toml
[outbound.stripe]
url     = "https://api.stripe.com"
headers = { Authorization = "Bearer {env.STRIPE_KEY}" }
paths   = ["/v1/charges", "/v1/charges/{id}"]
```

The handler calls `ctx.fetch("stripe", "/v1/charges", ...)` and never
holds the environment variable, so a handler that's tricked into misbehaving has nothing
to leak. See [`docs/outbound.md`](docs/outbound.md).

### Built for MCP servers

Unsworn supports remote MCP servers at three levels:

- **In front of the MCP server you already have.** OAuth token checks with
  the metadata MCP clients expect, origin checks, rate limits and audit
  logs, with no auth code in the server. Per-tool scopes and rate limits
  work for it too.
- **Tools as plain functions.** Unsworn speaks MCP itself (protocol
  version 2026-07-28). Each tool is a handler in
  any language, with no MCP SDK, its arguments validated before it runs,
  with its own scopes and rate limits. Callers only see the tools their
  token allows.
- **Your API, also as tools.** Mark a route, or operations in your OpenAPI
  spec, as tools. One handler with one set of rules serves both your REST
  clients and AI agents.

```toml
[[mcp.tool]]
name        = "create_invoice"
description = "Create a draft invoice for a customer"
input       = "schemas/invoice.json"
handler     = "src/billing.ts#createInvoice"
auth        = { scheme = "oauth", scopes = ["billing:write"] }
rate_limit  = { requests = 10, per = "1m", key = "auth.sub" }

[[route]]
path    = "/users/{id}"
method  = "GET"
handler = "src/users.ts#getUser"
mcp     = { tool = "get_user", description = "Look up a user by id" }
```

See [`docs/mcp.md`](docs/mcp.md).

### Plug 'n Play

Unsworn can sit between your existing app and the internet. Run it behind
your platform's load balancer, Caddy, nginx or Cloudflare, or on its own with a simple setup.
Point a catch-all route at your existing Express, Hono, FastAPI or Go app,
with no code changes, and you can immediately:

- add JWT or API-key auth, rate limits and caching to any path
- validate requests and data against an OpenAPI spec before they reach your app
- serve health checks, redirects and static files without touching the app

Then move routes out of the old app one at a time, at your own pace.

### The right language for each endpoint

Creating an API with one langugage is great, except for the parts that need Python's AI and
data libraries. Those parts usually turn into a separate service with its own
deploy, networking and auth.

With Unsworn, `routes/chat.post.py` sits next to `routes/users.get.ts`. They
share one router, one auth config, one API contract and one deploy. The same
goes for a Go endpoint that needs Go's concurrency, a Rust endpoint, etc.

And they can call each other. A handler declared as a
[function](docs/functions.md) in the config, is called by name from any other handler,
with its input checked on the way and no service in between:

```ts
// let's say "embed" represents a Python function
// but called from your TypeScript handler
const vector = await ctx.call("embed", { text: req.query.q });
```

### Performance behavior and tweaks

- **Declarative endpoints run entirely in Rust.** Static responses,
  redirects, proxies, cached responses and rejected requests never reach a handler.
- **Handlers run in worker pools that grow with the load,** without any
  clustering setup in your code. Each runtime starts with one worker by
  default and adds more as needed, up to one per available CPU.
- **A CPU-heavy endpoint can move to a faster language** without changing
  the API. Replace `get.ts` with `get.rs`, rewrite the handler, and clients
  notice nothing except the speed. In
  [our benchmark](docs/benchmarks.md#a-handler-that-does-work), one search
  handler costs 53 ms of CPU per request in Python, 1.2 ms in TypeScript on
  Bun and 0.82 ms in Rust.
- **Look for latency, and try the rewrite first.**
  [`unsworn top`](docs/top.md) lists each route's latency and its
  handler's CPU on a running server. [`shadow`](docs/shadow.md) sends a
  copy of a route's requests to the new handler and compares its answers
  with the original one. Clients continue being served by the original.

A handler call does cross a process boundary, and that costs about the same
in every language. So a trivial endpoint won't beat the same code running
directly on Bun, for example, and won't get faster by moving to Rust. The
gains come from the points above.

What Unsworn costs per request, for every route type, policy and SDK, is
measured in [docs/benchmarks.md](docs/benchmarks.md).

## A quick look

**Day one:** Unsworn in front of an existing app, with no code changes.

```toml
# unsworn.toml
[server]
listen = "0.0.0.0:8080"

[auth.jwt]
type = "jwt"
url  = "https://auth.example.com/.well-known/jwks.json"

[[route]]
path    = "/health"
auth    = "none"                         # every route says who may call it
respond = { status = 200, json = { ok = true } }

[[route]]
path  = "/{*rest}"
proxy = "http://127.0.0.1:3000"          # your existing app, unchanged
auth  = "jwt"                            # everything in it needs a token...

[[route]]                                # ...except the paths you open
path = "/assets/{*rest}"
auth = "none"

[[route]]                                # add policies to paths inside it
path       = "/api/{*rest}"
rate_limit = { requests = 100, per = "1m", key = "auth.sub" }
```

**Later:** one endpoint moved out of the app, into a TypeScript handler.

```toml
[runtimes.ts]
extensions = [".ts"]
command    = "bun run node_modules/@unswornjs/sdk/src/worker.ts"
protocol   = "framed"

[gen]
ts = "src/unsworn.gen.ts"                # types for handlers, from the routes

[[route]]
path    = "/api/users/{id}"
method  = "GET"
auth    = "jwt"
params  = { id = { type = "integer", min = 1 } }
cache   = { ttl = "30s", key = "auth.sub" }  # one cache entry per user
handler = "src/users.ts#getUser"
```

```ts
// src/users.ts
import type { GetUser } from "./unsworn.gen";   // written by `unsworn gen` from the route above

export const getUser: GetUser = async (req, ctx) => {
  // req.params.id is already validated as an integer >= 1, and typed as a
  // number. ctx.auth holds the verified JWT claims.
  const user = await db.users.find(ctx.auth.sub);
  return { status: 200, json: user };
};
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

All of this can be configured in `unsworn.toml`, with no code:

| Area | Features |
|---|---|
| AI agents (MCP) | OAuth protected-resource metadata, origin checks and streaming pass-through for existing MCP servers, with per-tool policies; MCP tools as handlers, with tool lists filtered by scope; routes and OpenAPI operations as tools |
| Responses | static JSON or text, redirects, static files with SPA fallback, health checks |
| Forwarding | reverse proxy with WebSocket pass-through, load balancing, catch-all to an existing app |
| Security | JWT/JWKS, API keys, `require` rules that tie a path to the caller's claims, CORS, IP allow and deny lists, body size limits, outbound services with credentials handlers never hold |
| Traffic | rate limits, timeouts, response caching |
| Contracts | params, query, header and body validation (JSON Schema), OpenAPI input and output |
| Ops | access logs, Prometheus metrics, per-route cost (`unsworn top`), shadow handlers, request IDs, health checks, config hot reload |

## How it works

- **Routes come from wherever suits you,** and all feed one route table:
  `[[route]]` entries pointing at functions (`src/users.ts#getUser`), a
  nested or flat routes directory (`routes/users.[id].get.ts`), an OpenAPI
  spec, or a catch-all to an existing app.
- **Handlers are plain functions.** Language SDKs give each one a typed
  request and hide everything else. The same handler code runs on a server,
  at the edge and in serverless functions.
- **The contract reaches the code.** [`unsworn gen`](docs/types.md) writes
  each route's params, query and body as types in the handler's language,
  and `unsworn dev` keeps them current. Change a rule and the handlers it
  breaks stop compiling.
- **Any language can join.** The universal protocol is JSON lines over
  stdin/stdout, so a language with no SDK, even one that doesn't exist yet,
  works on day one. Official SDKs also speak a framed protocol over a Unix
  socket, which handles large bodies faster.
- **Custom middleware is just more handlers.** A `before` handler, in any
  language, can add context such as the current tenant, or reject a request.
- **Mistakes surface early.** `unsworn check` validates config and routes
  with precise, fixable errors, and SDK workers confirm at startup that every
  handler name in the config exists. The config has a published JSON Schema
  for editor autocomplete.
- **Rule changes show up in review.** The [plan](docs/plan.md) is every
  route's merged policies in one committed file, and `unsworn diff` lists
  what a change does to them.

Details: [architecture](docs/architecture.md), [config](docs/config.md),
[plan and diff](docs/plan.md), [generated types](docs/types.md),
[functions](docs/functions.md), [outbound services](docs/outbound.md),
[tracing](docs/tracing.md), [`top`](docs/top.md), [`shadow`](docs/shadow.md),
[container image](docs/image.md),
[protocol](docs/protocol.md),
[conformance](docs/conformance.md), [MCP](docs/mcp.md),
[benchmarks](docs/benchmarks.md) and [roadmap](docs/roadmap.md).

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

| Platform type | Examples | How Unsworn runs | Version |
|---|---|---|---|
| **Servers and containers** | Any VPS or bare metal, Docker, Kubernetes, Fly.io, Railway, Render, Google Cloud Run, AWS ECS/Fargate, Cloudflare Containers | The `unsworn` binary plus worker processes. `unsworn build --target image` writes the Dockerfile ([guide](docs/image.md)) | v1 (available) |
| **Edge isolates** | Cloudflare Workers | `unsworn build --target cloudflare`: the core runs as WebAssembly in the Worker, and JS/TS handlers are bundled in and called directly ([guide](docs/cloudflare.md)) | v2 (v0.3.0) |
| **Serverless functions** | Vercel Functions, AWS Lambda, Netlify Functions | `unsworn build --target vercel`, `netlify` or `lambda`: one function runs the whole project, with the core as WebAssembly and JS/TS handlers bundled in and called directly ([guide](docs/serverless.md)) | v3 (v0.3.0) |

Some features need shared state on stateless platforms, such as rate limits
and caches. `unsworn check --target <platform>` reports what a config needs on
that target before you deploy.

## Getting started

Unsworn runs on Linux (x86_64 and arm64). Install the latest release:

```sh
curl -fsSL https://raw.githubusercontent.com/j4crev/unsworn/main/install.sh | sh
```

By default, the installer verifies the release's build attestation if the
GitHub CLI is installed and signed in. Otherwise, it installs without
that check. To require successful verification before installing, set
`UNSWORN_VERIFY=1`. To skip verification, set `UNSWORN_VERIFY=0`.
See [`SECURITY.md`](SECURITY.md).


For containers, `unsworn build --target image` writes a Dockerfile for the
project: each runtime at the version your project names, dependencies
from their lockfiles, compiled workers built in stages of their own (see
[`docs/image.md`](docs/image.md)). Or extend the `ghcr.io/j4crev/unsworn`
base image yourself. Either way it runs as the unprivileged user
`unsworn`, and the project in `/app` stays owned by root, so handler code
can't change the rules it runs under. For the same reason it skips
runtimes' `build` steps at startup: compiled workers are built into the
image. With Rust 1.88 or newer, `cargo install unsworn` works too.

To try unreleased changes, build from `main`:

```sh
cargo install --git https://github.com/j4crev/unsworn unsworn
```

```sh
unsworn init        # writes a starter unsworn.toml
unsworn dev         # serves it, reloads on changes, verifies handler names
```

| Command | What it does |
|---|---|
| `unsworn serve` | Run the router (`--listen`, `--log-format json`, `--no-build` for prebuilt workers); `SIGHUP` reloads |
| `unsworn dev` | Serve with file watching, every runtime started up front, and detailed errors |
| `unsworn check` | Validate config, routes and handler files; `--deep` asks workers whether every handler exists; `--target <t>` checks a deployment target |
| `unsworn routes` | Print the route table and each route's source, including MCP tools; `--policies` shows merged policies and each tool's arguments |
| `unsworn plan` | Write what every route and tool enforces to `unsworn.plan`, to commit with the config; `--check` fails when it's out of date |
| `unsworn diff` | Show what changed in the rules since a git revision (or another directory), marking what got looser; `--fail-on looser` for CI |
| `unsworn top` | Show each route's requests, latency and its handler's share of time and CPU on a running server, costliest first |
| `unsworn gen` | Write typed request models for handlers (TypeScript, Python, Go, Rust) from each route's schemas; `--check` fails when one is out of date |
| `unsworn openapi` | Print the OpenAPI document for what Unsworn serves |
| `unsworn build` | Bundle the project for a deployment target: `--target image` writes a Dockerfile for the server and its workers, `cloudflare` makes a Cloudflare Worker, and `vercel`, `netlify` and `lambda` make a function for each |
| `unsworn conformance` | Test a runtime's workers against the protocol |
| `unsworn schema` | Print the config's JSON Schema (also at [`schema/unsworn.schema.json`](schema/unsworn.schema.json)) |

Handler SDKs:

| Language | Package | Source |
|---|---|---|
| JavaScript and TypeScript (Bun, Node, Deno) | `@unswornjs/sdk` on npm | [`sdk/js`](sdk/js) |
| Python | `unsworn` on PyPI | [`sdk/python`](sdk/python) |
| Go | `github.com/j4crev/unsworn/sdk/go` | [`sdk/go`](sdk/go) |
| Rust | `unsworn-sdk` on crates.io | [`sdk/rust`](sdk/rust) |

[`examples/polyglot`](examples/polyglot) is a TypeScript and Python API in
front of an existing app, and [`examples/raw-python`](examples/raw-python)
is a worker written without an SDK.

## Roadmap

What's left to build for each version, and what isn't scheduled yet, is in
[`docs/roadmap.md`](docs/roadmap.md).

## License

[MIT](LICENSE)
