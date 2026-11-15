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

import { AsyncLocalStorage } from "node:async_hooks";
import { DurableObject } from "cloudflare:workers";

import {
  type Bridge,
  type CallResult,
  type HttpResponseValue,
  initSync,
  takeToken,
  Unsworn,
} from "../wasm/unsworn.js";
import wasmModule from "../wasm/unsworn_bg.wasm";
import {
  handlerRequest,
  outcome,
  pickHandler,
  type RequestFields,
  splitRef,
  toBytes,
} from "./handler.ts";

/** What `unsworn build` bundles. */
export interface Bundle {
  /** The config file's path, e.g. `unsworn.toml`. */
  config: string;
  /** Project files by path; `null` for files that only need to exist. */
  files: Record<string, string | null>;
  /** Handler modules by project-relative path. */
  modules: Record<string, Record<string, unknown>>;
}

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
// Logging

type Level = "error" | "warn" | "info" | "debug" | "trace";
const currentId = new AsyncLocalStorage<string>();

function logger(id: () => string | undefined) {
  const at = (level: Level) => (message: string, fields?: Record<string, unknown>) => {
    const record = { level, message: String(message), request_id: id(), ...fields };
    const method = level === "trace" ? "debug" : level === "info" ? "log" : level;
    console[method](record);
  };
  return { error: at("error"), warn: at("warn"), info: at("info"), debug: at("debug"), trace: at("trace") };
}

// The `log` export in index.ts finds this, so it works from any module.
(globalThis as Record<symbol, unknown>)[Symbol.for("unsworn.log")] = logger(() => currentId.getStore());

// ---------------------------------------------------------------------------
// Bodies

/** Thrown by a stream from `limitBody` once it passes its limit. */
class BodyTooLarge extends Error {
  constructor(max: number) {
    super(`the request body is larger than ${max} bytes`);
    this.name = "BodyTooLarge";
  }
}

function toStream(source: AsyncIterable<unknown>): ReadableStream<Uint8Array> {
  const it = source[Symbol.asyncIterator]();
  return new ReadableStream<Uint8Array>({
    async pull(controller) {
      for (;;) {
        const { done, value } = await it.next();
        if (done) return controller.close();
        const bytes = toBytes(value);
        if (bytes.length) return controller.enqueue(bytes);
      }
    },
    async cancel(reason) {
      await it.return?.(reason);
    },
  });
}

function concat(chunks: Uint8Array[], len: number): Uint8Array {
  const out = new Uint8Array(len);
  let at = 0;
  for (const c of chunks) {
    out.set(c, at);
    at += c.length;
  }
  return out;
}

/** A stream of `chunks`, then whatever `reader` has left (or its error). */
function replay(chunks: Uint8Array[], reader: ReadableStreamDefaultReader<Uint8Array>, error?: unknown) {
  return new ReadableStream<Uint8Array>({
    async pull(controller) {
      const next = chunks.shift();
      if (next) return controller.enqueue(next);
      if (error !== undefined) return controller.error(error);
      const { done, value } = await reader.read();
      if (done) controller.close();
      else controller.enqueue(value);
    },
    cancel(reason) {
      return reader.cancel(reason);
    },
  });
}

async function readUpTo(
  stream: ReadableStream<Uint8Array>,
  limit: number,
): Promise<{ bytes: Uint8Array } | { body: ReadableStream<Uint8Array> }> {
  const reader = stream.getReader();
  const chunks: Uint8Array[] = [];
  let len = 0;
  for (;;) {
    let result: ReadableStreamReadResult<Uint8Array>;
    try {
      result = await reader.read();
    } catch (err) {
      return { body: replay(chunks, reader, err) };
    }
    if (result.done) return { bytes: concat(chunks, len) };
    chunks.push(result.value);
    len += result.value.length;
    if (len > limit) return { body: replay(chunks, reader) };
  }
}

function pairs(headers: Headers): [string, string][] {
  const out: [string, string][] = [];
  headers.forEach((value, key) => {
    if (key !== "set-cookie") out.push([key, value]);
  });
  for (const c of headers.getSetCookie?.() ?? []) out.push(["set-cookie", c]);
  return out;
}

function toValue(res: Response): HttpResponseValue {
  return { status: res.status, headers: pairs(res.headers), body: res.body };
}

const NULL_BODY = new Set([101, 204, 205, 304]);

// ---------------------------------------------------------------------------
// The bridge: the I/O the core asks for

async function cacheUrl(origin: string, key: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(key));
  const hex = [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, "0")).join("");
  return `${origin}/__unsworn/cache/${hex}`;
}

function errorMessage(err: unknown, dev: boolean): string {
  if (err instanceof Error) return dev && err.stack ? err.stack : err.message;
  return String(err);
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
): Bridge {
  const cache = () => (globalThis as unknown as { caches: { default: Cache } }).caches.default;
  return {
    async call(request: string, body, timeoutMs): Promise<CallResult> {
      const msg = JSON.parse(request) as RequestFields;
      const [file] = splitRef(msg.handler);
      let fn;
      try {
        const mod = bundle.modules[file];
        if (!mod) throw new Error(`${file} isn't in the bundle; rebuild with unsworn build`);
        fn = pickHandler(mod, msg.handler, () => [env, ctx]);
      } catch (err) {
        return { type: "missing", message: errorMessage(err, false) };
      }
      const controller = new AbortController();
      const req = handlerRequest(msg, {
        body: body instanceof Uint8Array ? body : undefined,
        stream: body instanceof ReadableStream ? body : undefined,
        signal: controller.signal,
        log: logger(() => msg.id),
      });
      let timer: ReturnType<typeof setTimeout> | undefined;
      const timeout = new Promise<"timeout">((resolve) => {
        timer = setTimeout(() => resolve("timeout"), Math.max(0, timeoutMs));
      });
      const run = (async () => outcome(await fn(req, msg.context ?? {})))();
      try {
        const out = await Promise.race([run, timeout]);
        if (out === "timeout") {
          controller.abort(new Error("the handler timed out"));
          run.catch(() => {});
          return { type: "timeout" };
        }
        if (out.type === "continue") return out;
        return {
          type: "response",
          response: out.response as unknown as Record<string, unknown>,
          body: out.stream ? toStream(out.stream) : out.body,
        };
      } catch (err) {
        if (err instanceof BodyTooLarge || (err as { cause?: unknown })?.cause instanceof BodyTooLarge) {
          return { type: "body", error: "too_large" };
        }
        console.error(`[${msg.handler}]`, err);
        return { type: "error", message: errorMessage(err, dev) };
      } finally {
        clearTimeout(timer);
      }
    },

    async readBody(body, max) {
      const read = await readUpTo(body, max);
      if ("bytes" in read) return read.bytes;
      await read.body.cancel().catch(() => {});
      return null;
    },

    limitBody(body, max) {
      let seen = 0;
      return body.pipeThrough(
        new TransformStream<Uint8Array, Uint8Array>({
          transform(chunk, controller) {
            seen += chunk.length;
            if (seen > max) controller.error(new BodyTooLarge(max));
            else controller.enqueue(chunk);
          },
        }),
      );
    },

    collect: readUpTo,

    async fetch(request, timeoutMs, limit) {
      let res: Response;
      try {
        res = await fetch(request.url, {
          method: request.method,
          headers: request.headers,
          body: (request.body ?? undefined) as BodyInit | undefined,
          signal: AbortSignal.timeout(Math.max(1, timeoutMs)),
        });
      } catch (err) {
        const name = (err as Error)?.name;
        if (name === "TimeoutError" || name === "AbortError") return { error: "timeout", message: "no answer in time" };
        return { error: "connect", message: errorMessage(err, false) };
      }
      if (!res.body) return { status: res.status, headers: pairs(res.headers), body: null };
      const read = await readUpTo(res.body, limit);
      if (!("bytes" in read)) {
        await read.body.cancel().catch(() => {});
        return { error: "read", message: `the response is larger than ${limit} bytes` };
      }
      return { status: res.status, headers: pairs(res.headers), body: read.bytes };
    },

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

    sleep(ms) {
      return new Promise((resolve) => setTimeout(resolve, ms));
    },
  };
}

// ---------------------------------------------------------------------------
// The Worker

let instance: Unsworn | undefined;
let loadError: string | undefined;

function load(bundle: Bundle, env: Env): Unsworn {
  if (instance) return instance;
  if (loadError) throw new Error(loadError);
  try {
    initSync({ module: wasmModule });
    const vars = Object.fromEntries(Object.entries(env).filter(([, v]) => typeof v === "string"));
    const app = new Unsworn(bundle.config, bundle.files, vars, env.UNSWORN_DEV === "1");
    for (const ref of app.handlers) {
      const [file] = splitRef(ref);
      const mod = bundle.modules[file];
      if (!mod) console.error(`unsworn: handler ${ref}: ${file} isn't in the bundle`);
      else {
        try {
          pickHandler(mod, ref);
        } catch (err) {
          console.error(`unsworn: handler ${ref}: ${errorMessage(err, false)}`);
        }
      }
    }
    instance = app;
    return app;
  } catch (err) {
    loadError = errorMessage(err, false);
    throw err;
  }
}

function failure(status: number, detail: string, dev: boolean): Response {
  const body = { type: "about:blank", title: "Internal Server Error", status, ...(dev ? { detail } : {}) };
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/problem+json" } });
}

/** A Worker that serves the bundled project. */
export function createWorker(bundle: Bundle) {
  return {
    async fetch(request: Request, env: Env, ctx: ExecutionContextLike): Promise<Response> {
      const dev = env.UNSWORN_DEV === "1";
      let app: Unsworn;
      try {
        app = load(bundle, env);
      } catch (err) {
        console.error("unsworn: the project didn't load:", errorMessage(err, false));
        return failure(500, errorMessage(err, false), dev);
      }
      const url = new URL(request.url);
      const requestId =
        request.headers.get("x-request-id")?.slice(0, 128) || request.headers.get("cf-ray") || crypto.randomUUID();
      const upgraded: Upgraded = {};
      const handled = await currentId.run(requestId, () =>
        app.handle(
          {
            method: request.method,
            url: request.url,
            headers: pairs(request.headers),
            body: request.body,
            ip: request.headers.get("cf-connecting-ip") ?? "0.0.0.0",
            requestId,
          },
          bridge(bundle, env, ctx, url.origin, dev, upgraded),
        ),
      );
      if (handled.status === 101 && upgraded.webSocket) {
        const init = { status: 101, headers: handled.headers, webSocket: upgraded.webSocket };
        return new Response(null, init as ResponseInit);
      }
      const body = NULL_BODY.has(handled.status) || request.method === "HEAD" ? null : (handled.body ?? null);
      return new Response(body, { status: handled.status, headers: handled.headers });
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
