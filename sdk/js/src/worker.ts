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
import { type Callable, handlerRequest, outcome, pickHandler, splitRef, toBytes } from "./handler.ts";
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
// Set by Unsworn versions that accept `log` messages. Without it, logs go to stderr.
const logLevel = process.env.UNSWORN_LOG_LEVEL;

type Write = (bytes: Uint8Array) => void;

let codec: Codec;
let write: Write;

function send(message: Outgoing, body?: Uint8Array) {
  write(codec.encode(message, body));
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
      body,
      stream: bodies.get(msg.id)?.stream,
      signal: controller.signal,
      log: logger(() => msg.id),
    });
    const result = await currentId.run(msg.id, () => fn(req, msg.context ?? {}));
    await reply(msg.id, result, controller.signal);
  } catch (err) {
    if (!controller.signal.aborted) {
      const message = err instanceof Error ? (dev && err.stack ? err.stack : err.message) : String(err);
      console.error(`[${msg.handler}]`, err);
      send({ type: "error", id: msg.id, message });
    }
  } finally {
    inFlight.delete(msg.id);
    bodies.delete(msg.id);
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
    sdk: "unsworn-js/0.2.0",
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
