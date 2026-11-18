// Runs an Unsworn project as a Cloudflare Worker. The core (WebAssembly)
// matches, authenticates, validates and rate-limits each request, and
// calls your handlers in this isolate.
//
// `unsworn build --target cloudflare` writes the entry that uses this:
//
//   import { createWorker, UnswornRateLimiter } from "@unswornjs/sdk/cloudflare";
//   import * as m0 from "../src/users.ts";
//
//   export default createWorker({
//     config: "unsworn.toml",
//     files: { "unsworn.toml": "...", "src/users.ts": null },
//     modules: { "src/users.ts": m0 },
//   });
//   export { UnswornRateLimiter };
//
// Bindings it uses, which the build declares in wrangler.jsonc:
//   UNSWORN_RATE_LIMITER  Durable Object namespace for `rate_limit`
//   ASSETS                Workers Static Assets, for `static` routes
// Variables and secrets are what `{env.NAME}` in the config reads.
// UNSWORN_DEV=1 puts error details in responses.

import { DurableObject } from "cloudflare:workers";

import { type Bridge, initSync, takeToken, type Unsworn } from "../wasm/unsworn.js";
import wasmModule from "../wasm/unsworn_bg.wasm";
import {
  baseBridge,
  type Bundle,
  errorMessage,
  failure,
  lazyProject,
  pairs,
  toResponse,
  toValue,
  withRequestId,
} from "./host.ts";

export type { Bundle };

interface Env {
  UNSWORN_RATE_LIMITER?: DurableObjectNamespaceLike;
  ASSETS?: { fetch(request: Request): Promise<Response> };
  UNSWORN_DEV?: string;
  [key: string]: unknown;
}

interface ExecutionContextLike {
  waitUntil(promise: Promise<unknown>): void;
}

interface DurableObjectNamespaceLike {
  idFromName(name: string): unknown;
  get(id: unknown): { take(requests: number, perMs: number): Promise<number | null> };
}

// ---------------------------------------------------------------------------
// The bridge: the I/O the core asks for

async function cacheUrl(origin: string, key: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(key));
  const hex = [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
  return `${origin}/__unsworn/cache/${hex}`;
}

/**
 * Where the bridge leaves an upstream's WebSocket when a `proxy` route
 * upgrades. The core only sees the `101`; the Worker hands the socket to
 * the client with it.
 */
interface Upgraded {
  webSocket?: unknown;
}

function bridge(
  bundle: Bundle,
  env: Env,
  ctx: ExecutionContextLike,
  origin: string,
  dev: boolean,
  upgraded: Upgraded,
  project: Unsworn,
): Bridge {
  const cache = () => (globalThis as unknown as { caches: { default: Cache } }).caches.default;
  return {
    ...baseBridge(bundle, dev, () => [env, ctx], project),

    async proxy(request) {
      const res = await fetch(request.url, {
        method: request.method,
        headers: request.headers,
        body: (request.body ?? undefined) as BodyInit | undefined,
        redirect: "manual",
      });
      // With `Upgrade: websocket`, fetch() opens the WebSocket to the
      // upstream. Returning it unaccepted lets the runtime pass messages
      // through without running this isolate's code.
      const webSocket = (res as Response & { webSocket?: unknown }).webSocket;
      if (res.status === 101 && webSocket) upgraded.webSocket = webSocket;
      return toValue(res);
    },

    async asset(path, method, headers) {
      if (!env.ASSETS) throw new Error("static routes need the ASSETS binding; rebuild with unsworn build");
      const res = await env.ASSETS.fetch(new Request(new URL(path, origin), { method, headers }));
      return res.status === 404 ? null : toValue(res);
    },

    async rateLimit(key, requests, perMs) {
      const ns = env.UNSWORN_RATE_LIMITER;
      if (!ns) throw new Error("rate_limit needs the UNSWORN_RATE_LIMITER binding; rebuild with unsworn build");
      return ns.get(ns.idFromName(key)).take(requests, perMs);
    },

    async cacheGet(key) {
      const res = await cache().match(await cacheUrl(origin, key));
      return res ? new Uint8Array(await res.arrayBuffer()) : null;
    },

    async cachePut(key, value, ttlMs) {
      const maxAge = Math.max(1, Math.ceil(ttlMs / 1000));
      const res = new Response(value as BodyInit, { headers: { "cache-control": `max-age=${maxAge}` } });
      await cache().put(await cacheUrl(origin, key), res);
    },

    waitUntil(promise) {
      ctx.waitUntil(promise);
    },
  };
}

// ---------------------------------------------------------------------------
// The Worker

/** A Worker that serves the bundled project. */
export function createWorker(bundle: Bundle) {
  // The first request brings the `env` the project loads with.
  let load: (() => Unsworn) | undefined;
  return {
    async fetch(request: Request, env: Env, ctx: ExecutionContextLike): Promise<Response> {
      const dev = env.UNSWORN_DEV === "1";
      let app: Unsworn;
      try {
        load ??= lazyProject(bundle, () => wasmModule, () => env, dev);
        app = load();
      } catch (err) {
        console.error("unsworn: the project didn't load:", errorMessage(err, false));
        return failure(500, errorMessage(err, false), dev);
      }
      const url = new URL(request.url);
      const requestId =
        request.headers.get("x-request-id")?.slice(0, 128) || request.headers.get("cf-ray") || crypto.randomUUID();
      const upgraded: Upgraded = {};
      const handled = await withRequestId(requestId, () =>
        app.handle(
          {
            method: request.method,
            url: request.url,
            headers: pairs(request.headers),
            body: request.body,
            ip: request.headers.get("cf-connecting-ip") ?? "0.0.0.0",
            requestId,
          },
          bridge(bundle, env, ctx, url.origin, dev, upgraded, app),
        ),
      );
      if (handled.status === 101 && upgraded.webSocket) {
        const init = { status: 101, headers: handled.headers, webSocket: upgraded.webSocket };
        return new Response(null, init as ResponseInit);
      }
      return toResponse(handled, request.method);
    },
  };
}

/**
 * One rate-limit bucket. The Worker addresses one object per bucket key,
 * so every request for a key, anywhere, counts against the same tokens.
 * Runs the core's token bucket, as every Unsworn store does.
 */
export class UnswornRateLimiter extends DurableObject {
  private bucket?: [number, number] | null;

  async take(requests: number, perMs: number): Promise<number | null> {
    initSync({ module: wasmModule });
    const storage = (this as unknown as { ctx: { storage: DurableObjectStorageLike } }).ctx.storage;
    if (this.bucket === undefined) this.bucket = (await storage.get<[number, number]>("bucket")) ?? null;
    const now = Date.now();
    const [tokens, lastMs, waitMs] = takeToken(this.bucket?.[0], this.bucket?.[1], now, requests, perMs);
    this.bucket = [tokens, lastMs];
    void storage.put("bucket", this.bucket);
    // A bucket left alone for `per` is full again, the same as a new one,
    // so its storage can go.
    void storage.setAlarm(now + perMs);
    return waitMs < 0 ? null : waitMs;
  }

  async alarm(): Promise<void> {
    const storage = (this as unknown as { ctx: { storage: DurableObjectStorageLike } }).ctx.storage;
    this.bucket = null;
    await storage.deleteAll();
  }
}

interface DurableObjectStorageLike {
  get<T>(key: string): Promise<T | undefined>;
  put(key: string, value: unknown): Promise<void>;
  setAlarm(time: number): Promise<void>;
  deleteAll(): Promise<void>;
}
