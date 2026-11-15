// Runs an Unsworn project as a Vercel Function, on the Node host
// (node.ts). `unsworn build --target vercel` writes the entry that uses
// this and bundles it into `.vercel/output`:
//
//   import { createVercelFunction } from "@unswornjs/sdk/vercel";
//   import * as m0 from "../../src/users.ts";
//
//   export default createVercelFunction(
//     { config: "unsworn.toml", files: { ... }, modules: { "src/users.ts": m0 } },
//     { wasm: new URL("./unsworn_bg.wasm", import.meta.url), assets: new URL("./assets/", import.meta.url) },
//   );
//
// Every request reaches this one function, which runs the whole project.
// Environment variables are what `{env.NAME}` in the config reads.

import type { IncomingMessage, ServerResponse } from "node:http";

import { type Bundle, createHandler, type HandlerOptions, nodeListener } from "./node.ts";

export type { Bundle, HandlerOptions };

interface RequestContext {
  waitUntil?: (promise: Promise<unknown>) => void;
}

/** Vercel's per-request context, which is where `@vercel/functions` finds `waitUntil` too. */
function requestContext(): RequestContext | undefined {
  const store = (globalThis as Record<symbol, { get?: () => RequestContext | undefined } | undefined>)[
    Symbol.for("@vercel/request-context")
  ];
  return store?.get?.();
}

const one = (value: string | string[] | undefined) => (Array.isArray(value) ? value[0] : value);

/** A Vercel Function, in the Node.js `(req, res)` form, that serves the bundled project. */
export function createVercelFunction(
  bundle: Bundle,
  options: HandlerOptions = {},
): (req: IncomingMessage, res: ServerResponse) => Promise<void> {
  const app = createHandler(bundle, options);
  const listener = nodeListener(app, (req) => {
    const context = requestContext();
    return {
      // Vercel sets both headers itself, replacing whatever the client sent.
      ip: one(req.headers["x-real-ip"]) ?? one(req.headers["x-forwarded-for"])?.split(",")[0].trim(),
      requestId: one(req.headers["x-vercel-id"]),
      waitUntil: context?.waitUntil ? (promise) => context.waitUntil?.(promise) : undefined,
    };
  });
  return async (req, res) => {
    await listener(req, res);
    // Without Vercel's waitUntil, finish background work before returning,
    // or it may never run.
    if (!requestContext()?.waitUntil) await app.idle();
  };
}
