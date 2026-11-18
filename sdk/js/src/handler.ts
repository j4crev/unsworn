// Calling a handler, the same way wherever it runs: as a worker process
// (worker.ts) or in the same isolate as the core (cloudflare.ts).

import type { Fetched, FetchInit } from "./index.ts";

export type Callable = (req: unknown, ctx: unknown) => unknown;

/** A web app with a `fetch(Request)` method: Hono, Elysia, itty-router... */
export interface FetchApp {
  fetch: (r: Request, ...rest: unknown[]) => Response | Promise<Response>;
}

export function splitRef(ref: string): [string, string | undefined] {
  const i = ref.indexOf("#");
  if (i < 0) return [ref, undefined];
  const name = ref.slice(i + 1);
  return [ref.slice(0, i), name || undefined];
}

/**
 * The handler `ref` names in a loaded module: the named export, or the
 * default (or `handler`) export. `fetchArgs` adds arguments to an app's
 * `fetch` call, such as a Worker's `env` and `ctx`.
 */
export function pickHandler(
  mod: Record<string, unknown>,
  ref: string,
  fetchArgs?: () => unknown[],
): Callable {
  const [file, name] = splitRef(ref);
  const exported = name ? mod[name] : (mod.default ?? mod.handler);
  if (typeof exported === "function") return exported as Callable;
  if (exported && typeof (exported as FetchApp).fetch === "function") {
    return fetchAdapter(exported as FetchApp, fetchArgs);
  }
  if (exported === undefined) {
    throw new Error(name ? `${file} has no export "${name}"` : `${file} has no default export`);
  }
  throw new Error(`${ref} isn't a function or an app with a fetch() method`);
}

/** Lets an app with a `fetch(Request)` method be a handler. */
export function fetchAdapter(app: FetchApp, fetchArgs?: () => unknown[]): Callable {
  return (req: any) => {
    const host = req.headers.host ?? "localhost";
    const url = `http://${host}${req.path}${req.rawQuery ? `?${req.rawQuery}` : ""}`;
    let body: BodyInit | undefined;
    if (req.method !== "GET" && req.method !== "HEAD") {
      if (req.stream) body = req.stream;
      // The JSON as it arrived, when there is such a thing: the app parses
      // it itself.
      else if (req[RAW_JSON]) body = req[RAW_JSON];
      else if (req.json !== undefined) body = JSON.stringify(req.json);
      else if (req.text !== undefined) body = req.text;
      else if (req.body) body = req.body;
    }
    const headers = new Headers();
    for (const [k, v] of Object.entries(req.headers as Record<string, string>)) {
      try {
        headers.set(k, v);
      } catch {
        // skip headers the platform refuses
      }
    }
    // `duplex` is required by Node and Deno for a streamed body.
    const init = { method: req.method, headers, body, signal: req.signal, duplex: "half" } as RequestInit;
    return app.fetch(new Request(url, init), ...(fetchArgs?.() ?? []));
  };
}

/** The fields of a protocol `request` message a handler sees. */
export interface RequestFields {
  id: string;
  handler: string;
  phase?: "handle" | "before";
  route: string;
  method: string;
  path: string;
  params?: Record<string, unknown>;
  query?: Record<string, unknown>;
  raw_query?: string;
  headers?: Record<string, string>;
  context?: Record<string, unknown>;
  json?: unknown;
  text?: string;
}

/** On a `req` whose JSON body arrived as bytes: those bytes. */
export const RAW_JSON = Symbol.for("unsworn.rawJson");

/** Thrown by `req.json` when the body was sent as JSON and isn't. */
export class BadJson extends Error {}

const decoder = new TextDecoder();

/**
 * The `req` a handler gets. With `extra.json`, the body's JSON text,
 * `req.json` is parsed from it when a handler first reads it.
 */
export function handlerRequest(
  msg: RequestFields,
  extra: {
    body?: Uint8Array;
    json?: Uint8Array;
    stream?: ReadableStream<Uint8Array>;
    signal: AbortSignal;
    log: unknown;
  },
) {
  const req = {
    method: msg.method,
    path: msg.path,
    route: msg.route,
    params: msg.params ?? {},
    query: msg.query ?? {},
    rawQuery: msg.raw_query,
    headers: msg.headers ?? {},
    json: msg.json,
    text: msg.text,
    body: extra.body,
    stream: extra.stream,
    signal: extra.signal,
    handler: msg.handler,
    phase: msg.phase ?? "handle",
    log: extra.log,
  };
  const raw = extra.json;
  if (raw) {
    let parsed: unknown;
    let read = false;
    Object.defineProperty(req, "json", {
      enumerable: true,
      configurable: true,
      get() {
        if (!read) {
          try {
            parsed = JSON.parse(decoder.decode(raw));
          } catch (err) {
            throw new BadJson(err instanceof Error ? err.message : String(err));
          }
          read = true;
        }
        return parsed;
      },
      set(value: unknown) {
        parsed = value;
        read = true;
      },
    });
    Object.defineProperty(req, RAW_JSON, { value: raw });
  }
  return req;
}

/** The fields of a protocol `response` message, without `id` or a body. */
export interface ResponseFields {
  status: number;
  headers?: Record<string, string | string[]>;
  json?: unknown;
  text?: string;
  mcp_content?: unknown;
}

/** What a handler returned, in protocol terms. */
export type Outcome =
  | { type: "continue"; context: Record<string, unknown> }
  | {
      type: "response";
      response: ResponseFields;
      /** A body sent whole. */
      body?: Uint8Array;
      /** A body sent as it's produced. */
      stream?: AsyncIterable<unknown>;
    };

const encoder = new TextEncoder();

export function toBytes(chunk: unknown): Uint8Array {
  if (chunk instanceof Uint8Array) return chunk;
  if (typeof chunk === "string") return encoder.encode(chunk);
  if (chunk instanceof ArrayBuffer) return new Uint8Array(chunk);
  return encoder.encode(String(chunk));
}

export function headerRecord(headers: Headers): Record<string, string | string[]> {
  const out: Record<string, string | string[]> = {};
  headers.forEach((value, key) => {
    if (key !== "set-cookie") out[key] = value;
  });
  const cookies = headers.getSetCookie?.() ?? [];
  if (cookies.length) out["set-cookie"] = cookies;
  return out;
}

export function iterate(stream: ReadableStream<Uint8Array>): AsyncIterable<Uint8Array> {
  if (Symbol.asyncIterator in stream) return stream as unknown as AsyncIterable<Uint8Array>;
  return {
    async *[Symbol.asyncIterator]() {
      // The check above narrows the type to nothing where streams iterate.
      const reader = (stream as ReadableStream<Uint8Array>).getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) return;
        yield value;
      }
    },
  };
}

/** Turns what a handler returned into a protocol outcome. */
export async function outcome(result: unknown): Promise<Outcome> {
  if (result instanceof Response) {
    const headers = headerRecord(result.headers);
    const response = { status: result.status, headers };
    const contentType = result.headers.get("content-type") ?? "";
    if (result.body && contentType.startsWith("text/event-stream")) {
      return { type: "response", response, stream: iterate(result.body) };
    }
    return { type: "response", response, body: new Uint8Array(await result.arrayBuffer()) };
  }
  if (result === undefined || result === null) {
    return { type: "response", response: { status: 204 } };
  }
  if (typeof result !== "object") {
    throw new Error("a handler must return an object like { json }, { text }, { body }, or a Response");
  }
  const r = result as Record<string, any>;
  if ("continue" in r) {
    return { type: "continue", context: r.continue ?? {} };
  }
  const response: ResponseFields = { status: r.status ?? 200 };
  if (r.headers) response.headers = r.headers;
  if (r.mcpContent !== undefined) response.mcp_content = r.mcpContent;
  if (r.json !== undefined) return { type: "response", response: { ...response, json: r.json } };
  if (r.text !== undefined) return { type: "response", response: { ...response, text: String(r.text) } };
  if (r.body === undefined) return { type: "response", response };
  const body = r.body;
  if (typeof body === "string" || body instanceof Uint8Array || body instanceof ArrayBuffer) {
    return { type: "response", response, body: toBytes(body) };
  }
  if (body instanceof ReadableStream) return { type: "response", response, stream: iterate(body) };
  if (body && typeof body[Symbol.asyncIterator] === "function") {
    return { type: "response", response, stream: body };
  }
  throw new Error("`body` must be a string, bytes, a ReadableStream or an async iterable");
}

// ---------------------------------------------------------------------------
// `ctx.fetch`, the same wherever the handler runs

/** What a `ctx.fetch` sends, from its `init`: lowercase header names, and a JSON or raw body as bytes. */
export function outboundRequest(init: FetchInit = {}): {
  method: string;
  headers: Record<string, string>;
  body: Uint8Array | undefined;
} {
  const headers: Record<string, string> = {};
  for (const [name, value] of Object.entries(init.headers ?? {})) headers[name.toLowerCase()] = String(value);
  let body: Uint8Array | undefined;
  if (init.json !== undefined) {
    body = encoder.encode(JSON.stringify(init.json));
    headers["content-type"] ??= "application/json";
  } else if (init.body !== undefined) {
    body = typeof init.body === "string" ? encoder.encode(init.body) : init.body;
  }
  const method = (init.method ?? (body ? "POST" : "GET")).toUpperCase();
  return { method, headers, body: body?.length ? body : undefined };
}

/** A service's answer, as `ctx.fetch` gives it to a handler. */
export function fetchedValue(status: number, headers: Record<string, string>, bytes: Uint8Array): Fetched {
  return {
    status,
    ok: status >= 200 && status < 300,
    headers,
    body: bytes,
    text: () => decoder.decode(bytes),
    json: <T>() => JSON.parse(decoder.decode(bytes)) as T,
  };
}
