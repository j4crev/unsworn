// Runs an Unsworn project as a Netlify Function, on the Node host
// (node.ts). `unsworn build --target netlify` writes the entry that uses
// this and bundles it into `.netlify/v1/functions`:
//
//   import { createNetlifyFunction } from "@unswornjs/sdk/netlify";
//   import * as m0 from "../../src/users.ts";
//
//   export default createNetlifyFunction(
//     { config: "unsworn.toml", files: { ... }, modules: { "src/users.ts": m0 } },
//     { wasm: new URL("./unsworn_bg.wasm", import.meta.url), assets: new URL("./assets/", import.meta.url) },
//   );
//
// Every request reaches this one function, which runs the whole project.
// Environment variables are what `{env.NAME}` in the config reads. A
// handler that's an app with `fetch()` gets Netlify's context as its second
// argument.

import { type Bundle, createHandler, type HandlerOptions } from "./node.ts";

export type { Bundle, HandlerOptions };

/** The parts of Netlify's `Context` this uses. */
interface NetlifyContext {
  ip?: string;
  requestId?: string;
  waitUntil?: (promise: Promise<unknown>) => void;
}

/** A Netlify Function that serves the bundled project. */
export function createNetlifyFunction(
  bundle: Bundle,
  options: HandlerOptions = {},
): (request: Request, context?: NetlifyContext) => Promise<Response> {
  const app = createHandler(bundle, options);
  return async (request, context) => {
    const waitUntil = context?.waitUntil ? (promise: Promise<unknown>) => context.waitUntil?.(promise) : undefined;
    const response = await app.fetch(request, {
      ip: context?.ip,
      requestId: context?.requestId,
      waitUntil,
      fetchArgs: [context],
    });
    // Without waitUntil, finish background work before returning, or it
    // may never run.
    if (!waitUntil) await app.idle();
    return response;
  };
}
