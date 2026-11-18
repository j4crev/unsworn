// The Unsworn worker for JavaScript and TypeScript handlers.
//
// Run by Unsworn, e.g. in unsworn.toml:
//
//   [runtimes.ts]
//   extensions = [".ts", ".js"]
//   command = "bun run node_modules/@unswornjs/sdk/src/worker.ts"
//   protocol = "framed"
//
// Works on Bun, Node (22.18+, which runs TypeScript directly) and Deno.

import { AsyncLocalStorage } from "node:async_hooks";
import { connect } from "node:net";
import { isAbsolute, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { CallError, type Fetched, FetchError, type FetchInit } from "./index.ts";
import {
  BadJson,
  type Callable,
  fetchedValue,
  handlerRequest,
  outboundRequest,
  outcome,
  pickHandler,
  splitRef,
  toBytes,
} from "./handler.ts";
import {
  type Codec,
  FramedCodec,
  type Frame,
  JsonlCodec,
  type Outgoing,
  PROTOCOL_VERSION,
  type FetchedMessage,
  type RequestMessage,
  type ResultMessage,
  type VerifyMessage,
} from "./protocol.ts";

const root = process.env.UNSWORN_ROOT ?? process.cwd();
const transport = process.env.UNSWORN_TRANSPORT ?? "stdio";
const maxInFlight = Number(process.env.UNSWORN_MAX_IN_FLIGHT ?? 64);
const dev = process.env.UNSWORN_DEV === "1";
// Set by Unsworn versions that take a JSON body as the raw bytes after a
// message (`body_json`), in both directions.
const rawJson = process.env.UNSWORN_RAW_JSON === "1";
// Set by Unsworn versions that answer `call` messages, for `ctx.call`.
const canCall = process.env.UNSWORN_CALLS === "1";
// And `fetch` messages, for `ctx.fetch`.
const canFetch = process.env.UNSWORN_FETCH === "1";
// Set by Unsworn versions that accept `log` messages. Without it, logs go to stderr.
const logLevel = process.env.UNSWORN_LOG_LEVEL;

type Write = (bytes: Uint8Array) => void;

let codec: Codec;
let write: Write;

/** Bodies this large are written after their message instead of being copied in behind it. */
const WRITE_APART = 16 * 1024;

function send(message: Outgoing, body?: Uint8Array) {
  if (body === undefined || body.length < WRITE_APART) {
    write(codec.encode(message, body));
    return;
  }
  // Two writes in a row, with nothing awaited between them, stay together
  // in the stream.
  const [head, raw] = codec.encodeParts(message, body);
  write(head);
  if (raw) write(raw);
}

// ---------------------------------------------------------------------------
// Logging

type Level = "error" | "warn" | "info" | "debug" | "trace";
const LEVELS: Level[] = ["error", "warn", "info", "debug", "trace"];
const maxLevel = logLevel === undefined ? LEVELS.length : LEVELS.indexOf(logLevel as Level);
const currentId = new AsyncLocalStorage<string>();

function plain(value: unknown): unknown {
  if (value instanceof Error) return value.stack ?? value.message;
  if (typeof value === "bigint") return value.toString();
  return value;
}

function sendLog(id: string | undefined, level: Level, message: string, fields?: Record<string, unknown>) {
  if (LEVELS.indexOf(level) > maxLevel) return;
  if (logLevel === undefined) {
    console.error(`[${level}] ${message}`, ...(fields ? [fields] : []));
    return;
  }
  const out: Outgoing = { type: "log", level, message: String(message) };
  if (id) out.id = id;
  if (fields && Object.keys(fields).length) {
    try {
      out.fields = JSON.parse(JSON.stringify(fields, (_k, v) => plain(v)));
    } catch {
      out.fields = { fields: String(fields) };
    }
  }
  send(out);
}

function logger(id: () => string | undefined) {
  const at =
    (level: Level) =>
    (message: string, fields?: Record<string, unknown>) =>
      sendLog(id(), level, message, fields);
  return { error: at("error"), warn: at("warn"), info: at("info"), debug: at("debug"), trace: at("trace") };
}

// The `log` export in index.ts finds this, so it works from any module.
(globalThis as Record<symbol, unknown>)[Symbol.for("unsworn.log")] = logger(() => currentId.getStore());

// ---------------------------------------------------------------------------
// Handler loading

const modules = new Map<string, Promise<Record<string, unknown>>>();

function load(file: string): Promise<Record<string, unknown>> {
  let m = modules.get(file);
  if (!m) {
    const full = isAbsolute(file) ? file : resolve(root, file);
    m = import(pathToFileURL(full).href) as Promise<Record<string, unknown>>;
    modules.set(file, m);
  }
  return m;
}

async function resolveHandler(ref: string): Promise<Callable> {
  const [file] = splitRef(ref);
  return pickHandler(await load(file), ref);
}

// ---------------------------------------------------------------------------
// Calls to functions

interface PendingCall {
  request: string;
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
}

const calls = new Map<string, PendingCall>();
let nextCall = 0;

/** `ctx.call`: asks Unsworn to run a function for the request `request`. */
function call(request: string, name: string, input: unknown): Promise<unknown> {
  if (!canCall) {
    return Promise.reject(new CallError(501, "this Unsworn doesn't run functions for handlers: update it"));
  }
  const answer = new Promise((resolve, reject) => {
    const id = `c-${++nextCall}`;
    calls.set(id, { request, resolve, reject });
    send({ type: "call", id, request, function: name, json: input ?? null });
  });
  // A handler may start a call and not wait for it. Its failure then has
  // nowhere to go, which shouldn't count as an unhandled rejection: that
  // would stop the worker.
  answer.catch(() => {});
  return answer;
}

function result(msg: ResultMessage) {
  const pending = calls.get(msg.id);
  if (!pending) return;
  calls.delete(msg.id);
  if (msg.status < 400) pending.resolve(msg.json ?? null);
  else pending.reject(new CallError(msg.status, msg.error ?? `the function answered ${msg.status}`, msg.json));
}

/** Unsworn drops a request's calls when the request ends, so nothing will answer them. */
function dropCalls(request: string, reason: string) {
  for (const [id, pending] of calls) {
    if (pending.request !== request) continue;
    calls.delete(id);
    pending.reject(new CallError(499, reason));
  }
  for (const [id, pending] of fetches) {
    if (pending.request !== request) continue;
    fetches.delete(id);
    pending.reject(new FetchError(499, reason));
  }
}

// ---------------------------------------------------------------------------
// Requests to outbound services

interface PendingFetch {
  request: string;
  resolve: (value: Fetched) => void;
  reject: (error: Error) => void;
}

const fetches = new Map<string, PendingFetch>();

/** `ctx.fetch`: asks Unsworn to send a request to an outbound service. */
function fetchService(request: string, service: string, path: string, init: FetchInit = {}): Promise<Fetched> {
  if (!canFetch) {
    return Promise.reject(new FetchError(501, "this Unsworn doesn't make requests for handlers: update it"));
  }
  const { method, headers, body } = outboundRequest(init);
  const answer = new Promise<Fetched>((resolve, reject) => {
    const id = `f-${++nextCall}`;
    fetches.set(id, { request, resolve, reject });
    send({ type: "fetch", id, request, service, method, path, headers }, body);
  });
  answer.catch(() => {});
  return answer;
}

function fetched(msg: FetchedMessage, body: Uint8Array | undefined) {
  const pending = fetches.get(msg.id);
  if (!pending) return;
  fetches.delete(msg.id);
  if (msg.error !== undefined) {
    pending.reject(new FetchError(msg.status, msg.error));
    return;
  }
  pending.resolve(fetchedValue(msg.status, msg.headers ?? {}, body ?? new Uint8Array(0)));
}

/** The handler's `ctx`: the request's context, plus `call` and `fetch`. */
function context(msg: RequestMessage): Record<string, unknown> {
  const ctx = msg.context ?? {};
  // Not enumerable, so the context still reads (and prints) as plain data.
  Object.defineProperty(ctx, "call", {
    value: (name: string, input?: unknown) => call(msg.id, name, input),
  });
  Object.defineProperty(ctx, "fetch", {
    value: (service: string, path: string, init?: FetchInit) => fetchService(msg.id, service, path, init),
  });
  return ctx;
}

// ---------------------------------------------------------------------------
// Requests

const inFlight = new Map<string, AbortController>();
const encoder = new TextEncoder();

/** A streamed request body. Each chunk is acknowledged as the handler reads it. */
class IncomingBody {
  private chunks: Uint8Array[] = [];
  private done = false;
  private error?: Error;
  private wake?: () => void;
  private id: string;
  readonly stream: ReadableStream<Uint8Array>;

  constructor(id: string) {
    this.id = id;
    this.stream = new ReadableStream<Uint8Array>(
      {
        pull: async (controller) => {
          const chunk = await this.next();
          if (chunk === undefined) controller.close();
          else controller.enqueue(chunk);
        },
      },
      { highWaterMark: 0 },
    );
  }

  push(chunk: Uint8Array) {
    this.chunks.push(chunk);
    this.wake?.();
  }

  end(error?: Error) {
    this.done = true;
    this.error ??= error;
    this.wake?.();
  }

  private async next(): Promise<Uint8Array | undefined> {
    for (;;) {
      const chunk = this.chunks.shift();
      if (chunk) {
        send({ type: "ack", id: this.id, bytes: chunk.length });
        return chunk;
      }
      if (this.error) throw this.error;
      if (this.done) return undefined;
      await new Promise<void>((resolve) => {
        this.wake = resolve;
      });
      this.wake = undefined;
    }
  }
}

const bodies = new Map<string, IncomingBody>();

async function streamBody(id: string, source: AsyncIterable<unknown>, signal: AbortSignal) {
  for await (const chunk of source) {
    if (signal.aborted) return;
    const bytes = toBytes(chunk);
    if (bytes.length) send({ type: "chunk", id }, bytes);
  }
  if (!signal.aborted) send({ type: "end", id });
}

async function reply(id: string, result: unknown, signal: AbortSignal) {
  if (signal.aborted) return;
  const out = await outcome(result);
  if (signal.aborted) return;
  if (out.type === "continue") {
    send({ type: "continue", id, context: out.context });
  } else if (out.stream) {
    send({ type: "response", id, ...out.response, stream: true });
    await streamBody(id, out.stream, signal);
  } else if (rawJson && out.response.json !== undefined) {
    // Written out here and sent as the raw body, which Unsworn passes on
    // without reading it.
    const { json, ...rest } = out.response;
    const text = JSON.stringify(json);
    if (text === undefined) send({ type: "response", id, ...rest });
    else send({ type: "response", id, ...rest, body_json: true }, encoder.encode(text));
  } else {
    send({ type: "response", id, ...out.response }, out.body);
  }
}

async function handle(msg: RequestMessage, body: Uint8Array | undefined) {
  const controller = new AbortController();
  inFlight.set(msg.id, controller);
  try {
    const fn = await resolveHandler(msg.handler);
    const req = handlerRequest(msg, {
      body: msg.body_json ? undefined : body,
      json: msg.body_json ? body : undefined,
      stream: bodies.get(msg.id)?.stream,
      signal: controller.signal,
      log: logger(() => msg.id),
    });
    const result = await currentId.run(msg.id, () => fn(req, context(msg)));
    await reply(msg.id, result, controller.signal);
  } catch (err) {
    if (controller.signal.aborted) {
      // Nobody is waiting for an answer.
    } else if (err instanceof BadJson) {
      // The body was sent as JSON and isn't. Unsworn left it for the
      // handler to read, so this is the 400 it would have answered with.
      send({
        type: "response",
        id: msg.id,
        status: 400,
        headers: { "content-type": "application/problem+json" },
        json: {
          type: "about:blank",
          title: "Bad Request",
          status: 400,
          detail: `the body isn't valid JSON: ${err.message}`,
        },
      });
    } else {
      const message = err instanceof Error ? (dev && err.stack ? err.stack : err.message) : String(err);
      console.error(`[${msg.handler}]`, err);
      send({ type: "error", id: msg.id, message });
    }
  } finally {
    inFlight.delete(msg.id);
    bodies.delete(msg.id);
    if (calls.size || fetches.size) dropCalls(msg.id, "the request ended before the call did");
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
      if ((msg as RequestMessage).stream) bodies.set(msg.id!, new IncomingBody(msg.id!));
      void handle(msg as RequestMessage, frame.body);
      break;
    case "chunk": {
      const data = frame.body ?? encoder.encode((msg as { text?: string }).text ?? "");
      if (data.length) bodies.get(msg.id!)?.push(data);
      break;
    }
    case "end":
      bodies.get(msg.id!)?.end();
      break;
    case "cancel":
      bodies.get(msg.id!)?.end(new Error("the request was cancelled"));
      inFlight.get((msg as { id: string }).id)?.abort();
      break;
    case "verify":
      void verify(msg as VerifyMessage);
      break;
    case "result":
      result(msg as ResultMessage);
      break;
    case "fetched":
      fetched(msg as FetchedMessage, frame.body);
      break;
    default:
      break; // unknown messages are ignored
  }
}

// ---------------------------------------------------------------------------
// Transport

function ready() {
  send({
    type: "ready",
    protocol: PROTOCOL_VERSION,
    max_in_flight: maxInFlight,
    verify: true,
    stream_body: true,
    raw_json: rawJson,
    calls: canCall,
    fetch: canFetch,
    sdk: "unsworn-js/0.4.0",
  });
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
