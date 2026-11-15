// Runs an Unsworn project in Node: the host under the serverless targets
// (vercel.ts, netlify.ts, lambda.ts). The core (WebAssembly) matches,
// authenticates, validates and rate-limits each request, and calls your
// handlers in this process.
//
//   const app = createHandler(bundle, { assets: "/var/task/assets" });
//   const response = await app.fetch(request, { ip: "203.0.113.7" });
//
// `[store]` says where rate limits and cached responses live: Redis behind
// a REST API (see store.ts), or this process's memory. `{env.NAME}` in the
// config reads the process's environment. UNSWORN_DEV=1 puts error details
// in responses.

import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { createReadStream, readFileSync } from "node:fs";
import { stat } from "node:fs/promises";
import type { IncomingMessage, ServerResponse } from "node:http";
import { extname, resolve, sep } from "node:path";
import { Readable } from "node:stream";
import { fileURLToPath } from "node:url";

import type { Bridge, HeaderPairs, HttpResponseValue, Unsworn } from "../wasm/unsworn.js";
import {
  baseBridge,
  type Bundle,
  errorMessage,
  failure,
  jsonLogs,
  loadProject,
  pairs,
  toResponse,
  toValue,
  withRequestId,
} from "./host.ts";
import { type Store, storeFor } from "./store.ts";

export type { Bundle };

export interface HandlerOptions {
  /** The core's WebAssembly: its bytes, a module, or the file's location. Read from the SDK when not given. */
  wasm?: BufferSource | WebAssembly.Module | URL;
  /** The directory holding each `static` route's directory, at its project path. */
  assets?: string | URL;
  /** What `{env.NAME}` reads. `process.env` when not given. */
  env?: Record<string, string | undefined>;
}

/** What the platform knows about a request. */
export interface RequestInfo {
  /** The client's IP address, from the platform (never from a header a client could set). */
  ip?: string;
  /** Used when the request has no `X-Request-Id`. */
  requestId?: string;
  /** Keeps the function alive for work that outlasts the response. */
  waitUntil?: (promise: Promise<unknown>) => void;
  /** Extra arguments for the `fetch` of a handler that's an app, such as the platform's context. */
  fetchArgs?: unknown[];
}

export interface Handler {
  fetch(request: Request, info?: RequestInfo): Promise<Response>;
  /**
   * Resolves once work that outlasts responses (a cache write, say) is
   * done, for platforms that freeze the process when the handler returns.
   * Only tracks requests that came without `waitUntil`.
   */
  idle(): Promise<void>;
}

// ---------------------------------------------------------------------------
// Static files

const TYPES: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".htm": "text/html; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".json": "application/json",
  ".map": "application/json",
  ".webmanifest": "application/manifest+json",
  ".txt": "text/plain; charset=utf-8",
  ".md": "text/markdown; charset=utf-8",
  ".xml": "application/xml",
  ".csv": "text/csv; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".jpg": "image/jpeg",
  ".jpeg": "image/jpeg",
  ".gif": "image/gif",
  ".webp": "image/webp",
  ".avif": "image/avif",
  ".ico": "image/x-icon",
  ".woff": "font/woff",
  ".woff2": "font/woff2",
  ".ttf": "font/ttf",
  ".otf": "font/otf",
  ".mp3": "audio/mpeg",
  ".mp4": "video/mp4",
  ".webm": "video/webm",
  ".pdf": "application/pdf",
  ".wasm": "application/wasm",
  ".zip": "application/zip",
};

/** One `bytes=` range of a file of `size` bytes: `null` to send it whole, `"bad"` for `416`. */
function byteRange(header: string | undefined, size: number): [number, number] | null | "bad" {
  const m = header?.match(/^bytes=(\d*)-(\d*)$/);
  if (!m || (m[1] === "" && m[2] === "")) return null;
  let start: number;
  let end: number;
  if (m[1] === "") {
    const last = Number(m[2]);
    if (last === 0) return "bad";
    start = Math.max(0, size - last);
    end = size - 1;
  } else {
    start = Number(m[1]);
    end = m[2] === "" ? size - 1 : Math.min(Number(m[2]), size - 1);
  }
  return start > end || start >= size ? "bad" : [start, end];
}

/** A file below `root` by URL path, as a response, or `null` when there's none. */
async function serveFile(
  root: string,
  path: string,
  method: string,
  headers: HeaderPairs,
): Promise<HttpResponseValue | null> {
  let file: string;
  try {
    file = resolve(root, `.${sep}${decodeURIComponent(path)}`);
  } catch {
    return null;
  }
  if (file !== root && !file.startsWith(root + sep)) return null;
  const info = await stat(file).catch(() => null);
  if (!info?.isFile()) return null;

  const header = (name: string) => headers.find(([k]) => k.toLowerCase() === name)?.[1];
  // HTTP dates have whole seconds.
  const modified = Math.floor(info.mtimeMs / 1000) * 1000;
  const out: HeaderPairs = [
    ["content-type", TYPES[extname(file).toLowerCase()] ?? "application/octet-stream"],
    ["last-modified", new Date(modified).toUTCString()],
    ["accept-ranges", "bytes"],
  ];
  const since = Date.parse(header("if-modified-since") ?? "");
  if (!Number.isNaN(since) && modified <= since) return { status: 304, headers: out, body: null };

  const range = byteRange(header("range"), info.size);
  if (range === "bad") {
    return { status: 416, headers: [...out, ["content-range", `bytes */${info.size}`]], body: null };
  }
  const [start, end] = range ?? [0, info.size - 1];
  out.push(["content-length", String(range ? end - start + 1 : info.size)]);
  if (range) out.push(["content-range", `bytes ${start}-${end}/${info.size}`]);
  const body =
    method === "HEAD" || info.size === 0
      ? null
      : (Readable.toWeb(createReadStream(file, { start, end })) as unknown as ReadableStream<Uint8Array>);
  return { status: range ? 206 : 200, headers: out, body };
}

// ---------------------------------------------------------------------------
// The bridge: the I/O the core asks for

function problem(status: number, title: string, detail: string): HttpResponseValue {
  const body = JSON.stringify({ type: "about:blank", title, status, detail });
  return {
    status,
    headers: [["content-type", "application/problem+json"]],
    body: new TextEncoder().encode(body),
  };
}

function bridge(
  bundle: Bundle,
  store: () => Store,
  assets: string | undefined,
  dev: boolean,
  info: RequestInfo,
  waitUntil: (promise: Promise<unknown>) => void,
): Bridge {
  return {
    ...baseBridge(bundle, dev, () => info.fetchArgs ?? []),

    async proxy(request) {
      if (request.headers.some(([k]) => k.toLowerCase() === "upgrade")) {
        // A function answers a request and ends: it can't hold a socket open.
        return problem(501, "Not Implemented", "WebSocket upgrades can't pass through a serverless function");
      }
      const init = {
        method: request.method,
        headers: request.headers,
        body: request.body ?? undefined,
        redirect: "manual",
        // Node needs this for a streamed body.
        duplex: "half",
      };
      const value = toValue(await fetch(request.url, init as RequestInit));
      // fetch() decoded the body, so it no longer matches these.
      if (value.headers.some(([k]) => k === "content-encoding")) {
        value.headers = value.headers.filter(([k]) => k !== "content-encoding" && k !== "content-length");
      }
      return value;
    },

    async asset(path, method, headers) {
      if (!assets) throw new Error("static routes need the bundled assets directory; rebuild with unsworn build");
      return serveFile(assets, path, method, headers);
    },

    rateLimit: (key, requests, perMs) => store().rateLimit(key, requests, perMs),
    cacheGet: (key) => store().cacheGet(key),
    cachePut: (key, value, ttlMs) => store().cachePut(key, value, ttlMs),

    waitUntil,
  };
}

// ---------------------------------------------------------------------------
// The handler

/** Serves the bundled project: a `fetch` that takes a `Request` and answers with a `Response`. */
export function createHandler(bundle: Bundle, options: HandlerOptions = {}): Handler {
  const env = options.env ?? process.env;
  const dev = env.UNSWORN_DEV === "1";
  const assets =
    options.assets === undefined
      ? undefined
      : resolve(options.assets instanceof URL ? fileURLToPath(options.assets) : options.assets);
  jsonLogs(true);

  let app: Unsworn | undefined;
  let store: Store | undefined;
  const load = () => {
    if (app) return app;
    const wasm = options.wasm ?? new URL("../wasm/unsworn_bg.wasm", import.meta.url);
    return (app = loadProject(bundle, wasm instanceof URL ? readFileSync(wasm) : wasm, env, dev));
  };

  const pending = new Set<Promise<unknown>>();
  const track = (promise: Promise<unknown>) => {
    const settled = promise.catch(() => {}).finally(() => pending.delete(settled));
    pending.add(settled);
  };

  return {
    async fetch(request, info = {}) {
      let project: Unsworn;
      try {
        project = load();
      } catch (err) {
        console.error("unsworn: the project didn't load:", errorMessage(err, false));
        return failure(500, errorMessage(err, false), dev);
      }
      const requestId = request.headers.get("x-request-id")?.slice(0, 128) || info.requestId || randomUUID();
      const io = bridge(bundle, () => (store ??= storeFor(project.store)), assets, dev, info, info.waitUntil ?? track);
      const handled = await withRequestId(requestId, () =>
        project.handle(
          {
            method: request.method,
            url: request.url,
            headers: pairs(request.headers),
            body: request.body,
            ip: info.ip ?? "0.0.0.0",
            requestId,
          },
          io,
        ),
      );
      return toResponse(handled, request.method);
    },

    async idle() {
      while (pending.size) await Promise.all(pending);
    },
  };
}

// ---------------------------------------------------------------------------
// Node's own request and response

const first = (value: string | string[] | undefined) => (Array.isArray(value) ? value[0] : value)?.split(",")[0].trim();

/**
 * The handler as a Node `(req, res)` listener, for platforms that call
 * functions that way and for `http.createServer`. `info` says what the
 * platform knows about a request. The URL's scheme and host come from
 * `X-Forwarded-Proto` and `X-Forwarded-Host`, so only use this behind a
 * proxy that sets them.
 */
export function nodeListener(
  app: Handler,
  info: (req: IncomingMessage) => RequestInfo = () => ({}),
): (req: IncomingMessage, res: ServerResponse) => Promise<void> {
  return async (req, res) => {
    const proto = first(req.headers["x-forwarded-proto"]) ?? "http";
    const host = first(req.headers["x-forwarded-host"]) ?? req.headers.host ?? "localhost";
    const headers = new Headers();
    for (const [name, value] of Object.entries(req.headers)) {
      for (const v of Array.isArray(value) ? value : [value]) {
        try {
          if (v !== undefined) headers.append(name, v);
        } catch {
          // skip headers the platform refuses
        }
      }
    }
    // The host the client asked for, which is what the core matches
    // routes' `host` against and tells upstreams.
    headers.set("host", host);
    const method = req.method ?? "GET";
    const init = {
      method,
      headers,
      body: method === "GET" || method === "HEAD" ? undefined : Readable.toWeb(req),
      duplex: "half",
    };
    let response: Response;
    try {
      response = await app.fetch(new Request(`${proto}://${host}${req.url ?? "/"}`, init as RequestInit), info(req));
    } catch (err) {
      console.error("unsworn: the request failed:", errorMessage(err, false));
      response = failure(500, errorMessage(err, false), false);
    }

    const out = new Map<string, string | string[]>();
    for (const [name, value] of pairs(response.headers)) {
      const seen = out.get(name);
      out.set(name, seen === undefined ? value : [...(Array.isArray(seen) ? seen : [seen]), value]);
    }
    res.writeHead(response.status, Object.fromEntries(out));
    if (response.body) {
      const reader = response.body.getReader();
      res.on("close", () => void reader.cancel().catch(() => {}));
      try {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          if (!res.write(value)) await once(res, "drain");
        }
      } catch (err) {
        // The status is sent, so all that's left is to cut the body short.
        console.error("unsworn: the response body failed:", errorMessage(err, false));
        res.destroy();
        return;
      }
    }
    res.end();
  };
}
