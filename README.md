# Unsworn

An API router with allegiance to no programming language.

That means your handlers can be written in virtually anything. And each handler can be in a different language;
so `src/chat.py` can sit next to `src/users.ts`. The same handlers can also be offered to AI agents as
MCP tools. Hell, call a function in Python from TypeScript if you'd like.

Unsworn also handles the below parts of your app, configured in `unsworn.toml` instead of your code:

| Area | Features |
|---|---|
| Responses | static JSON or text, redirects, static files with SPA fallback, health checks |
| Forwarding | reverse proxy with WebSocket pass-through, basic load balancing, catch-all to an existing app |
| Security | JWT/JWKS, API keys, signed webhooks (Stripe, GitHub, Slack, Standard Webhooks), `require` rules that tie a path to the caller's claims, CORS, IP allow and deny lists, body size limits, outbound services with credentials handlers never hold |
| Traffic | rate limits, timeouts, response caching |
| Contracts | params, query, header and body validation (JSON Schema), OpenAPI input and output |
| Ops | access logs, Prometheus metrics, per-route cost (`unsworn top`), shadow handlers, request IDs, health checks, config hot reload |

## Two ways to serve a route

Unsworn checks every request against your API's rules, then hands
it to your code. A request that fails a rule never reaches your code.

Your code can receive a request in two ways:

**1. As a handler function.**
Unsworn calls it with the validated request and
sends back what it returns.

**2. As an HTTP server.** You have an app built on a
framework such as Hono, Rails, FastAPI or Express. Unsworn forwards
requests to it via HTTP, but you can decide who manages the app.
  - **Unsworn manages it.** Configure the command, and Unsworn starts,
restarts, and shuts it down as needed.
  - **You manage it.** Configure the proxy address to wherever your app lives.

The really cool part is that your project can use both ways, at the same time, by defining them per route.

For example, look at this configuration snippet:

```toml
[[route]]                            # way 1: a function
path    = "/chat"
method  = "POST"
handler = "src/chat.py"              # assumes 'def handler'


[runtimes.bun]                       # way 2: an HTTP server Unsworn manages
command  = "bun run server.ts"
protocol = "http"

[[route]]                            # a route to the server Unsworn manages
path  = "/invoices/{*rest}"
proxy = { runtime = "bun" }


[[route]]                            # way 2: an HTTP server you manage
path  = "/reports/{*rest}"
proxy = "http://127.0.0.1:3000"      # where your own bun instance listens
```

## Why Unsworn

### Your API's rules live outside your code

In most frameworks, auth checks, input validation and rate limits are code,
but code can be wrong. One forgotten middleware or an unvalidated field is enough to get you rekt.

With Unsworn, the security-critical part of your API is declared, not coded: rules in `unsworn.toml`, enforced by a binary that handler code can't change.

And the rules reach past "is this a valid token". For example, one `require` line keeps every
caller scoped to their own organization's paths:

```toml
[[route]]
path    = "/orgs/{org}/{*rest}"
require = { "params.org" = "auth.org" }
# i.e. the {org} in the path must equal the "org" claim in the caller's token
```

You, or your agents, can review changes to the rules using `unsworn plan`. It writes all of them into a file called `unsworn.plan`, next to your config. Keep that file in git, and a pull request that changes the rules shows the change in its diff. `unsworn diff` says what
a branch changes and marks what it loosens:

```
my-api fix/auth-none ❯ unsworn diff origin/main
~ * /admin/{*rest}
  ! auth               jwt → none  (looser)
```

### The right language for each endpoint

Creating an API with one language is great, except for the parts that need Python's AI and
data libraries, or hot paths that need a speed boost. They usually turn into a separate service with their own
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

### Performance

- **Some requests never reach your code.** Unsworn answers static files,
  redirects, fixed `respond` bodies, cached responses and rejected requests
  itself, with no handler call. Proxied routes skip the handler step too:
  Unsworn forwards them straight to your app.
- **Handlers run in worker pools that grow with the load,** without any
  clustering setup in your code.
- **A CPU-heavy endpoint can move to a faster language** without changing
  the API. [`unsworn top`](docs/top.md) finds the costly routes, and
  [`shadow`](docs/shadow.md) tries the rewrite on a copy of live requests
  first.

A handler call does cross a process boundary, so a trivial endpoint won't
beat the same code running directly on Bun, for example. What Unsworn costs per
request, for every route type, policy and SDK, is measured in
[docs/benchmarks.md](docs/benchmarks.md).

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
handler. Everything else still goes to the old app. There's no runtime to
configure for it: a TypeScript, Python, Go or Rust handler runs with its
SDK's worker unless the config says otherwise.

## Documentation

| | |
|---|---|
| Start here | [Installing](docs/install.md), [commands](docs/cli.md), [config reference](docs/config.md) |
| Your code | [Serving a route](docs/serving-routes.md), [HTTP runtimes](docs/http-runtime.md), [generated types](docs/types.md), [functions](docs/functions.md), [outbound services](docs/outbound.md), [MCP](docs/mcp.md) |
| Reviewing and operating | [Plan and diff](docs/plan.md), [`top`](docs/top.md), [`shadow`](docs/shadow.md), [tracing](docs/tracing.md) |
| Deploying | [Container image](docs/image.md), [AWS Lambda](docs/lambda.md), [Cloudflare Workers](docs/cloudflare.md), [Vercel and Netlify](docs/serverless.md) |
| Internals | [Architecture](docs/architecture.md), [worker protocol](docs/protocol.md), [conformance](docs/conformance.md), [benchmarks](docs/benchmarks.md), [roadmap](docs/roadmap.md) |

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

| Where | How | Handlers |
|---|---|---|
| Any VPS or bare metal, Docker, Kubernetes, Fly.io, Railway, Render, Google Cloud Run, AWS ECS/Fargate, Cloudflare Containers | The `unsworn` binary plus worker processes: `unsworn build --target image` ([guide](docs/image.md)) | Any language, in processes of their own |
| AWS Lambda | The same image, with AWS's Lambda Web Adapter: `--target lambda` ([guide](docs/lambda.md)) | Any language, in processes of their own |
| Cloudflare Workers | The core as WebAssembly in the Worker: `--target cloudflare` ([guide](docs/cloudflare.md)) | JavaScript and TypeScript, in the same isolate |
| Vercel Functions, Netlify Functions | One function runs the whole project: `--target vercel` or `netlify` ([guide](docs/serverless.md)) | JavaScript and TypeScript, in the same process |

On the last two rows every request still passes the same rules before its
handler is called. But a handler shares a process with the core there, so
it can read the secrets the rules use, as far as the platform lets it.
`unsworn check --target <platform>` reports what a config needs on a
target before you deploy.

## Getting started

Unsworn runs on Linux (x86_64 and arm64), and on macOS for development.
Install the latest release:

```sh
curl -fsSL https://raw.githubusercontent.com/j4crev/unsworn/main/install.sh | sh
```

[Installing](docs/install.md) has the other ways to get it, what the
installer verifies, what differs on macOS and how to run it in a
container.

```sh
unsworn init        # writes a starter unsworn.toml
unsworn dev         # serves it, reloads on changes, verifies handler names
unsworn check       # validates config, routes and handler files
unsworn plan        # writes every route's rules to unsworn.plan
```

Every command is in [docs/cli.md](docs/cli.md).

Handler SDKs:

| Language | Package | Source |
|---|---|---|
| JavaScript and TypeScript (Bun, Node) | `@unswornjs/sdk` on npm | [`sdk/js`](sdk/js) |
| Python | `unsworn` on PyPI | [`sdk/python`](sdk/python) |
| Go | `github.com/j4crev/unsworn/sdk/go` | [`sdk/go`](sdk/go) |
| Rust | `unsworn-sdk` on crates.io | [`sdk/rust`](sdk/rust) |

Any other language runs as an [HTTP runtime](docs/http-runtime.md), with
its own server and no SDK.

## Roadmap

What's left to build for each version, and what isn't scheduled yet, is in
[`docs/roadmap.md`](docs/roadmap.md).

## License

[MIT](LICENSE)
