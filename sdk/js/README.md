# @unsworn/js

Write [Unsworn](https://github.com/j4crev/unsworn) handlers in JavaScript or
TypeScript. One worker runs on Bun, Node (22.18+, which runs TypeScript
directly) and Deno.

```ts
// src/users.ts
import type { Handler } from "@unsworn/js";

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
command = "bun run node_modules/@unsworn/js/src/worker.ts"
protocol = "binary"

[[route]]
path = "/users/{id}"
method = "GET"
handler = "src/users.ts#getUser"
params = { id = { type = "integer", min = 1 } }
```

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

## Existing apps

A module whose export has a `fetch(Request)` method, such as a Hono or
Elysia app, works as a handler. Use it behind a catch-all route to put
Unsworn in front of the app, then move routes out one at a time:

```toml
[[route]]
path = "/{*rest}"
handler = "src/app.ts"     # export default app
```

## Notes

- `req.signal` is aborted when Unsworn cancels the request.
- On the stdio transport, `console.log` output goes to stderr, which Unsworn
  collects into its logs.
- Status: not yet published to npm (the package name is a placeholder).
