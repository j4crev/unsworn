// The Unsworn worker for JavaScript and TypeScript handlers.
//
// Run by Unsworn, e.g. in unsworn.toml:
//
//   [runtimes.ts]
//   extensions = [".ts", ".js"]
//   command = "bun run node_modules/@unsworn/js/src/worker.ts"
//   protocol = "binary"
//
// Works on Bun, Node (22.18+, which runs TypeScript directly) and Deno.

import { connect } from "node:net";
import { isAbsolute, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import {
  type Codec,
  FramedCodec,
  type Frame,
  JsonlCodec,
  type Outgoing,
  PROTOCOL_VERSION,
  type RequestMessage,
  type VerifyMessage,
} from "./protocol.ts";

const root = process.env.UNSWORN_ROOT ?? process.cwd();
const transport = process.env.UNSWORN_TRANSPORT ?? "stdio";
const maxInFlight = Number(process.env.UNSWORN_MAX_IN_FLIGHT ?? 64);
const dev = process.env.UNSWORN_DEV === "1";

type Write = (bytes: Uint8Array) => void;

let codec: Codec;
let write: Write;

function send(message: Outgoing, body?: Uint8Array) {
  write(codec.encode(message, body));
}

// ---------------------------------------------------------------------------
// Handler loading

const modules = new Map<string, Promise<Record<string, unknown>>>();

function splitRef(ref: string): [string, string | undefined] {
  const i = ref.indexOf("#");
  if (i < 0) return [ref, undefined];
  const name = ref.slice(i + 1);
  return [ref.slice(0, i), name || undefined];
}

function load(file: string): Promise<Record<string, unknown>> {
  let m = modules.get(file);
  if (!m) {
    const full = isAbsolute(file) ? file : resolve(root, file);
    m = import(pathToFileURL(full).href) as Promise<Record<string, unknown>>;
    modules.set(file, m);
  }
  return m;
}

type Callable = (req: unknown, ctx: unknown) => unknown;

async function resolveHandler(ref: string): Promise<Callable> {
  const [file, name] = splitRef(ref);
  const mod = await load(file);
  const exported = name ? mod[name] : (mod.default ?? mod.handler);
  if (typeof exported === "function") return exported as Callable;
  if (exported && typeof (exported as { fetch?: unknown }).fetch === "function") {
    return fetchAdapter(exported as { fetch: (r: Request) => Response | Promise<Response> });
  }
  if (exported === undefined) {
    throw new Error(name ? `${file} has no export "${name}"` : `${file} has no default export`);
  }
  throw new Error(`${ref} isn't a function or an app with a fetch() method`);
}

/** Lets an app with a `fetch(Request)` method (Hono, Elysia, ...) be a handler. */
function fetchAdapter(app: { fetch: (r: Request) => Response | Promise<Response> }): Callable {
  return (req: any) => {
    const host = req.headers.host ?? "localhost";
    const url = `http://${host}${req.path}${req.rawQuery ? `?${req.rawQuery}` : ""}`;
    let body: BodyInit | undefined;
    if (req.method !== "GET" && req.method !== "HEAD") {
      if (req.json !== undefined) body = JSON.stringify(req.json);
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
    return app.fetch(new Request(url, { method: req.method, headers, body, signal: req.signal }));
  };
}

// ---------------------------------------------------------------------------
// Requests

const inFlight = new Map<string, AbortController>();
const encoder = new TextEncoder();

function toBytes(chunk: unknown): Uint8Array {
  if (chunk instanceof Uint8Array) return chunk;
  if (typeof chunk === "string") return encoder.encode(chunk);
  if (chunk instanceof ArrayBuffer) return new Uint8Array(chunk);
  return encoder.encode(String(chunk));
}

function headerRecord(headers: Headers): Record<string, string | string[]> {
  const out: Record<string, string | string[]> = {};
  headers.forEach((value, key) => {
    if (key !== "set-cookie") out[key] = value;
  });
  const cookies = headers.getSetCookie?.() ?? [];
  if (cookies.length) out["set-cookie"] = cookies;
  return out;
}

async function streamBody(id: string, source: AsyncIterable<unknown>, signal: AbortSignal) {
  for await (const chunk of source) {
    if (signal.aborted) return;
    const bytes = toBytes(chunk);
    if (bytes.length) send({ type: "chunk", id }, bytes);
  }
  if (!signal.aborted) send({ type: "end", id });
}

function iterate(stream: ReadableStream<Uint8Array>): AsyncIterable<Uint8Array> {
  if (Symbol.asyncIterator in stream) return stream as unknown as AsyncIterable<Uint8Array>;
  return {
    async *[Symbol.asyncIterator]() {
      const reader = stream.getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) return;
        yield value;
      }
    },
  };
}

async function reply(id: string, result: unknown, signal: AbortSignal) {
  if (signal.aborted) return;
  if (result instanceof Response) {
    const headers = headerRecord(result.headers);
    const contentType = result.headers.get("content-type") ?? "";
    if (result.body && contentType.startsWith("text/event-stream")) {
      send({ type: "response", id, status: result.status, headers, stream: true });
      await streamBody(id, iterate(result.body), signal);
      return;
    }
    const bytes = new Uint8Array(await result.arrayBuffer());
    send({ type: "response", id, status: result.status, headers }, bytes);
    return;
  }
  if (result === undefined || result === null) {
    send({ type: "response", id, status: 204 });
    return;
  }
  if (typeof result !== "object") {
    throw new Error("a handler must return an object like { json }, { text }, { body }, or a Response");
  }
  const r = result as Record<string, any>;
  if ("continue" in r) {
    send({ type: "continue", id, context: r.continue ?? {} });
    return;
  }
  const base: Outgoing = { type: "response", id, status: r.status ?? 200 };
  if (r.headers) base.headers = r.headers;
  if (r.json !== undefined) {
    send({ ...base, json: r.json });
  } else if (r.text !== undefined) {
    send({ ...base, text: String(r.text) });
  } else if (r.body !== undefined) {
    const body = r.body;
    if (typeof body === "string" || body instanceof Uint8Array || body instanceof ArrayBuffer) {
      send(base, toBytes(body));
    } else if (body instanceof ReadableStream) {
      send({ ...base, stream: true });
      await streamBody(id, iterate(body), signal);
    } else if (body && typeof body[Symbol.asyncIterator] === "function") {
      send({ ...base, stream: true });
      await streamBody(id, body, signal);
    } else {
      throw new Error("`body` must be a string, bytes, a ReadableStream or an async iterable");
    }
  } else {
    send(base);
  }
}

async function handle(msg: RequestMessage, body: Uint8Array | undefined) {
  const controller = new AbortController();
  inFlight.set(msg.id, controller);
  try {
    const fn = await resolveHandler(msg.handler);
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
      body,
      signal: controller.signal,
      handler: msg.handler,
      phase: msg.phase ?? "handle",
    };
    const result = await fn(req, msg.context ?? {});
    await reply(msg.id, result, controller.signal);
  } catch (err) {
    if (!controller.signal.aborted) {
      const message = err instanceof Error ? (dev && err.stack ? err.stack : err.message) : String(err);
      console.error(`[${msg.handler}]`, err);
      send({ type: "error", id: msg.id, message });
    }
  } finally {
    inFlight.delete(msg.id);
  }
}

async function verify(msg: VerifyMessage) {
  const missing: { handler: string; reason: string }[] = [];
  for (const ref of msg.handlers) {
    try {
      await resolveHandler(ref);
    } catch (err) {
      missing.push({ handler: ref, reason: err instanceof Error ? err.message : String(err) });
    }
  }
  send({ type: "verified", id: msg.id, missing });
}

function dispatch(frame: Frame) {
  const msg = frame.message;
  switch (msg.type) {
    case "request":
      void handle(msg as RequestMessage, frame.body);
      break;
    case "cancel":
      inFlight.get((msg as { id: string }).id)?.abort();
      break;
    case "verify":
      void verify(msg as VerifyMessage);
      break;
    default:
      break; // unknown messages are ignored
  }
}

// ---------------------------------------------------------------------------
// Transport

function ready() {
  send({ type: "ready", protocol: PROTOCOL_VERSION, max_in_flight: maxInFlight, verify: true, sdk: "unsworn-js/0.1.0" });
}

if (transport.startsWith("unix:")) {
  codec = new FramedCodec();
  const socket = connect(transport.slice(5));
  write = (bytes) => {
    socket.write(bytes);
  };
  socket.on("connect", ready);
  socket.on("data", (chunk: Uint8Array) => {
    for (const frame of codec.push(chunk)) dispatch(frame);
  });
  socket.on("close", () => process.exit(0));
  socket.on("error", (err) => {
    console.error("unsworn: socket error:", err);
    process.exit(1);
  });
} else {
  // stdout carries protocol messages, so console output goes to stderr.
  for (const level of ["log", "info", "debug"] as const) {
    console[level] = (...args: unknown[]) => console.error(...args);
  }
  codec = new JsonlCodec();
  write = (bytes) => {
    process.stdout.write(bytes);
  };
  process.stdin.on("data", (chunk: Uint8Array) => {
    for (const frame of codec.push(chunk)) dispatch(frame);
  });
  process.stdin.on("end", () => process.exit(0));
  ready();
}
