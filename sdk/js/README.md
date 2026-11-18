# @unswornjs/sdk

Write [Unsworn](https://github.com/j4crev/unsworn) handlers in JavaScript or
TypeScript. One worker runs on Bun, Node (22.18+, which runs TypeScript
directly) and Deno.

```ts
// src/users.ts
import type { Handler } from "@unswornjs/sdk";

export const getUser: Handler<{ params: { id: number } }> = async (req, ctx) => {
  // req.params.id is already validated and converted by Unsworn,
  // and ctx.auth holds the verified token claims.
  return { json: { id: req.params.id, name: "Ada" } };
};
```

```toml
# unsworn.toml
[runtimes.ts]
extensions = [".ts", ".js"]
command = "bun run node_modules/@unswornjs/sdk/src/worker.ts"
protocol = "framed"

[[route]]
path = "/users/{id}"
method = "GET"
handler = "src/users.ts#getUser"
params = { id = { type = "integer", min = 1 } }
```

## Types from the config

`Handler<{ params: { id: number } }>` repeats what the config already says.
`unsworn gen` writes that type for you, from the route:

```toml
[gen]
ts = "src/unsworn.gen.ts"
```

```ts
import type { GetUser } from "./unsworn.gen";

export const getUser: GetUser = async (req) => ({ json: { id: req.params.id } });
```

`unsworn dev` keeps the file current. See
[`docs/types.md`](../../docs/types.md).

## Replies

A handler returns an object or a web `Response`:

| Return | Response |
|---|---|
| `{ json, status?, headers? }` | JSON body |
| `{ text, status?, headers? }` | Text body |
| `{ body }` with a string or `Uint8Array` | Raw bytes |
| `{ body }` with a `ReadableStream` or async iterable | Streamed response |
| `new Response(...)` | As given; `text/event-stream` responses are streamed |
| `next({ tenant: "acme" })` | From a `before` handler: continue, adding context |

## MCP tools

A handler listed in `[[mcp.tool]]` is an ordinary handler. Its arguments
arrive as `req.json`, already validated, and `ctx.mcp` has the `tool` name,
`protocol_version` and, when the client sends it, `client`. Unsworn turns
the reply into an MCP tool result (see
[`docs/mcp.md`](../../docs/mcp.md#how-results-are-mapped)):

```ts
import { type Handler, content, json, toolError } from "@unswornjs/sdk";

export const getForecast: Handler<{ json: { city: string } }> = (req, ctx) => {
  const { city } = req.json;
  if (!cities.has(city)) return toolError(`unknown city ${city}`); // the model sees this
  return json({ city, days: forecast(city) });
};

export const chart: Handler = () => content([{ type: "image", data: pngBase64, mimeType: "image/png" }]);
```

| Return | Tool result |
|---|---|
| `json(value)` or `{ json }` | Structured content, plus a text copy |
| `text(value)` or `{ text }` | One text block |
| `content(blocks, json?)` or `{ mcpContent, json? }` | MCP content blocks as given; with `json`, structured content too |
| `toolError(message)` | `isError: true` with `message` |

## Existing apps

A module whose export has a `fetch(Request)` method, such as a Hono or
Elysia app, works as a handler. Use it behind a catch-all route to put
Unsworn in front of the app, then move routes out one at a time:

```toml
[[route]]
path = "/{*rest}"
handler = "src/app.ts"     # export default app
```

## Logging

`req.log.info("charged card", { amount: 1200 })` sends a log record to
Unsworn, tagged with the request. `log`, exported from `@unswornjs/sdk`, does
the same from any module, using the request that is running:

```ts
import { log } from "@unswornjs/sdk";

log.warn("retrying payment", { attempt: 2 });
```

Levels are `error`, `warn`, `info`, `debug` and `trace`.

## Cloudflare Workers

The same handlers run in a Cloudflare Worker, called directly by the
Unsworn core compiled to WebAssembly. `unsworn build --target cloudflare`
generates the Worker, which imports `@unswornjs/sdk/cloudflare`. See
[`docs/cloudflare.md`](../../docs/cloudflare.md).

## Vercel, Netlify and AWS Lambda

They also run in a serverless function. `unsworn build --target vercel`,
`--target netlify` or `--target lambda` bundles the project into one
function that imports `@unswornjs/sdk/vercel`, `/netlify` or `/lambda`, all
built on `@unswornjs/sdk/node`. See
[`docs/serverless.md`](../../docs/serverless.md).

## Notes

- `req.signal` is aborted when Unsworn cancels the request.
- On routes with `stream_body = true`, `req.stream` is the body as a
  `ReadableStream`, and fetch-style apps get it as their request body.
- On the stdio transport, `console.log` output goes to stderr, which Unsworn
  collects into its logs.
