# Cloudflare Workers

`unsworn build --target cloudflare` turns a project into a Cloudflare
Worker. The core runs inside the Worker as WebAssembly, so routing,
validation, auth, CORS, rate limits, caching and MCP behave as they do on
the server. Your JavaScript and TypeScript handlers are bundled into the
same Worker and called directly, with no worker processes.

## Quick start

In a project with JS or TS handlers:

```sh
npm install @unswornjs/sdk wrangler   # the SDK carries the Worker runtime
unsworn check --target cloudflare     # what won't work there, if anything
unsworn build --target cloudflare
npx wrangler dev                      # run it locally in workerd
npx wrangler deploy
```

The `@unswornjs/sdk` version must match your `unsworn` binary; the build
says so if it doesn't. Add `dist/` and `.wrangler/` to `.gitignore`.

## What the build writes

| Path | What it is |
|---|---|
| `dist/cloudflare/worker.js` | The Worker's entry: the project files the core loads, an `import` per handler module, and the runtime from `@unswornjs/sdk/cloudflare`. |
| `dist/cloudflare/wrangler.json` | Your `wrangler.jsonc`, plus what the Worker needs (below). |
| `dist/cloudflare/assets/` | The directories of your `static` routes. |
| `.wrangler/deploy/config.json` | Points wrangler at the generated config, so `wrangler dev`, `deploy` and `secret` work from the project root. |

Rebuild after changing the config, routes or the set of handler modules.
Wrangler bundles the handler code itself, so `wrangler dev` picks up edits to
handlers without a rebuild. `--out <dir>` writes somewhere else.

The config files are bundled and loaded when the Worker starts, so
`{env.NAME}` is read from the Worker's variables and secrets, not from
your shell at build time. The build lists any it can't see:

```
! {env.JWT_SECRET} isn't set here; it's read at runtime, so set it with `wrangler secret put JWT_SECRET` or as a var
✓ built dist/cloudflare: 12 routes, 3 handler modules, 3 asset files
```

The bundle holds them as JSON, whatever format you wrote them in. That
changes nothing in TOML; in a YAML config, text has to be in quotes where
YAML would also take a bare number. See
[config in a bundle](config.md#in-a-bundle-config-is-json).

## Your `wrangler.jsonc`

Keep your own Worker settings in `wrangler.jsonc` (or `wrangler.json`) in the
project root: its name, routes and custom domains, vars, and bindings for
KV, D1, R2 and the rest. The build copies it into the generated config and
adds:

- `main`: the generated entry.
- `compatibility_date`, if you don't set one, and the `nodejs_compat` flag.
- A Durable Object binding, `UNSWORN_RATE_LIMITER`, with its migration. It's
  always there, so adding or removing `rate_limit` never needs a migration.
- `assets`, when the project has `static` routes.

The generated config lives in `dist/cloudflare`, so relative paths in your
`wrangler.jsonc`, such as a D1 `migrations_dir`, are read from there. A
`wrangler.toml` isn't read; convert it to `wrangler.jsonc`.

## Handlers

Handlers have the same shape as on the server (see
[`sdk/js`](../sdk/js/README.md)): `(req, ctx)` returning `{ json }`,
`{ text }`, `{ body }`, a `Response`, or `next(...)` from a `before`
handler. Only JavaScript and TypeScript handlers run in a Worker; `unsworn
check --target cloudflare` reports any others.

`ctx.call` reaches a [function](functions.md) and `ctx.fetch` an
[outbound service](outbound.md), with the checks the server makes. A
route's [`shadow`](shadow.md) handler gets its copy after the response.

Read bindings and variables through `cloudflare:workers`:

```ts
import { env } from "cloudflare:workers";

export async function getUser(req) {
  const row = await env.DB.prepare("select * from users where id = ?").bind(req.params.id).first();
  return row ? { json: row } : { status: 404, json: { error: "not found" } };
}
```

An exported app with a `fetch()` method, such as Hono, gets the Worker's
`env` and `ctx` as usual, so `c.env` works.

Node.js APIs are available through `nodejs_compat`. There's no file system
and no child processes.

## Stores

On Cloudflare, the target picks the store and `[store]` is ignored:

- **Rate limits** use a Durable Object per bucket key. Every request for a
  key counts against the same tokens wherever it lands, so limits are exact.
  The cost is a call to wherever that object lives, which can add latency
  for clients far from it. A bucket's storage is deleted once it would be
  full again.
- **`cache`** uses the Cache API. Each Cloudflare location keeps its own
  copy, so a response cached in one city is a miss in another. The Cache
  API only works on a custom domain, not on `*.workers.dev`, where every
  lookup misses.

`[store].on_error` still applies: if the Durable Object can't be reached
within a second, `"open"` lets the request through and `"closed"` answers
`503`. `backend = "redis"` is an error on this target.

## Differences from the server

- **TLS, HTTP versions and compression** are Cloudflare's job. `[server.tls]`
  is ignored.
- **The client IP** comes from `CF-Connecting-IP`; `trusted_proxies` is
  ignored.
  If the platform doesn't give one, a route with an `ip` policy answers
  `403`: an unknown client can't be shown to be allowed.
- **Request ids** come from an incoming `X-Request-Id`, else the `CF-Ray`
  header.
- **There's no `/_unsworn/health`, metrics endpoint or admin listener.** Use
  Workers Logs and analytics.
- **Logs**, from the core and from `log` and `req.log`, go to the console,
  which Workers Logs collects. Handler logs are structured records with the
  request id.
- **Error details:** set the var `UNSWORN_DEV` to `1` to put error details in
  responses, as `unsworn dev` does.
- **Static files** are served from Workers Static Assets. Every request
  reaches the Worker first, so a `static` route's auth, CORS and rate limits
  apply to its files.
- **Proxy routes** forward with `fetch()`, WebSocket upgrades included. Auth
  and rate limits run before the upgrade. After it, Cloudflare passes
  messages straight through without running the Worker's code, and the
  connection counts as one request. Cloudflare's runtime updates and your
  deploys close open WebSockets, so clients should reconnect.
- **Timeouts:** `timeout` works as on the server, but Cloudflare's own
  CPU-time and duration limits apply too.

## Size

The core adds about 1.2 MB, gzipped, to the Worker. Cloudflare allows 3 MB
compressed on the free plan and 10 MB on paid plans, handlers and their
dependencies included.

## How it works

`crates/unsworn-wasm` compiles the core to WebAssembly. It exports an
`Unsworn` class that loads the bundled project and handles each request with
the same `Router` as the binary, asking a JavaScript *bridge* for I/O:
calling a handler, reading a stream, the store, assets, `fetch()`.
`sdk/js/src/cloudflare.ts` is the Cloudflare bridge (on the parts every JS
host shares, in `sdk/js/src/host.ts`) and the
`UnswornRateLimiter` Durable Object. The [serverless targets](serverless.md)
use the same wasm with a bridge for Node. See
[`architecture.md`](architecture.md).
