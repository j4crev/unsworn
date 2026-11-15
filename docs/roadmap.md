# Roadmap

What's planned for Unsworn and what's still to be built. For what each
deployment target is meant to look like, see
[`architecture.md`](architecture.md#deployment-targets).

| Version | Runs on | Status |
|---|---|---|
| v1 | Servers and containers | Available (v0.2.0) |
| v2 | Adds edge isolates | Planned |
| v3 | Adds serverless functions | Planned |

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
- **`unsworn-wasm`:** core bindings for edge and serverless hosts.
- **`unsworn build --target cloudflare`:** bundle the core and JS/TS and
  WASM handlers into a Worker, with handlers called in-process.
- **Fill in the target capability table** (`unsworn-core/src/targets.rs`)
  so `unsworn check --target cloudflare` reports everything that won't run
  there, including which store backends work on it.
- **MCP on the edge.** The MCP endpoint needs no shared state, so tools as
  handlers and routes as tools should work once handlers do.

## v3: serverless functions

Vercel, AWS Lambda and Netlify (`--target vercel`, `--target lambda`,
`--target netlify`). Builds on the v2 work:

- Turn declarative routes into the platform's routing config, such as
  Vercel's Build Output API.
- Wrap each handler in a native function with a shim that runs the WASM
  core for validation and auth before calling it.
- Store backends reachable from functions, such as Redis over HTTP.

## Not scheduled yet

- **Automatic TLS certificates (ACME).** Until then, run behind a platform
  load balancer, Caddy or Cloudflare.
- **HTTP/3.**
- **OpenTelemetry** traces and metrics, alongside the Prometheus endpoint.
- **Typed request models for handlers (`unsworn gen <lang>`),** generated
  from the config's params, query, header and body schemas.
- **More MCP features:** resources and prompts, elicitation, progress and
  change notifications, tasks. See [`mcp.md`](mcp.md#later).

## Open questions

Tracked where they apply:

- [Config](config.md#open-questions): several methods in one route file,
  environment-specific overrides.
- [Protocol](protocol.md#open-questions): handler-owned WebSockets,
  progress messages, elicitation.
