// What every JavaScript host of the core shares: loading the project into
// the WebAssembly core, calling handlers, reading bodies and logging. Each
// platform's entry (cloudflare.ts, ...) adds its own store, assets and
// proxying to make a full `Bridge`.

import { AsyncLocalStorage } from "node:async_hooks";

import { type Bridge, type CallResult, type HttpResponseValue, initSync, Unsworn } from "../wasm/unsworn.js";
import {
  fetchedValue,
  handlerRequest,
  outboundRequest,
  outcome,
  pickHandler,
  type RequestFields,
  splitRef,
  toBytes,
} from "./handler.ts";
import { CallError, type CallOptions, FetchError, type FetchInit } from "./index.ts";

/**
 * `ctx.call` and `ctx.fetch` for a handler that runs in the core's own
 * process. The core checks each one and runs it, as the server does:
 * `caller` is its record of the call to the handler, and the function or
 * the request goes back out through `bridge`.
 */
function calls(project: Unsworn | undefined, bridge: Bridge, caller: string | undefined) {
  const none = "this project has no functions or outbound services";
  // A handler may start one and not wait for it. Its failure then has
  // nowhere to go, which shouldn't count as an unhandled rejection.
  const quiet = <T>(answer: Promise<T>) => {
    answer.catch(() => {});
    return answer;
  };
  return {
    call: (name: string, input?: unknown, options?: CallOptions) =>
      quiet(
        (async () => {
          if (!project || caller === undefined) throw new CallError(404, none);
          const r = await project.callFunction(
            caller,
            name,
            JSON.stringify(input ?? null),
            bridge,
            options?.traceparent,
            options?.tracestate,
          );
          if (r.status < 400) return r.json ?? null;
          throw new CallError(r.status, r.error ?? `the function answered ${r.status}`, r.json);
        })(),
      ),
    fetch: (service: string, path: string, init?: FetchInit) =>
      quiet(
        (async () => {
          if (!project || caller === undefined) throw new FetchError(404, none);
          const { method, headers, body } = outboundRequest(init);
          const r = await project.fetchService(caller, service, method, path, headers, body ?? null, bridge);
          if (r.error !== undefined) throw new FetchError(r.status, r.error);
          return fetchedValue(r.status, r.headers, r.body ?? new Uint8Array(0));
        })(),
      ),
  };
}

/** What `unsworn build` bundles. */
export interface Bundle {
  /** The config file's path, e.g. `unsworn.toml`. */
  config: string;
  /** Project files by path; `null` for files that only need to exist. */
  files: Record<string, string | null>;
  /** Handler modules by project-relative path. */
  modules: Record<string, Record<string, unknown>>;
}

// ---------------------------------------------------------------------------
// Logging

type Level = "error" | "warn" | "info" | "debug" | "trace";
const currentId = new AsyncLocalStorage<string>();

/** Runs `fn` with `id` as the request id that `log` records carry. */
export function withRequestId<T>(id: string, fn: () => T): T {
  return currentId.run(id, fn);
}

let asJson = false;

/**
 * Writes handler logs as one JSON object per line, which is what log
 * collectors for Node functions parse. Off, records go to the console as
 * objects, which Workers Logs indexes.
 */
export function jsonLogs(on: boolean): void {
  asJson = on;
}

export function logger(id: () => string | undefined) {
  const at = (level: Level) => (message: string, fields?: Record<string, unknown>) => {
    const record = { level, message: String(message), request_id: id(), ...fields };
    const method = level === "trace" ? "debug" : level === "info" ? "log" : level;
    console[method](asJson ? JSON.stringify(record) : record);
  };
  return { error: at("error"), warn: at("warn"), info: at("info"), debug: at("debug"), trace: at("trace") };
}

// The `log` export in index.ts finds this, so it works from any module.
(globalThis as Record<symbol, unknown>)[Symbol.for("unsworn.log")] = logger(() => currentId.getStore());

// ---------------------------------------------------------------------------
// Bodies

/** Thrown by a stream from `limitBody` once it passes its limit. */
export class BodyTooLarge extends Error {
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

export async function readUpTo(
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

export function pairs(headers: Headers): [string, string][] {
  const out: [string, string][] = [];
  headers.forEach((value, key) => {
    if (key !== "set-cookie") out.push([key, value]);
  });
  for (const c of headers.getSetCookie?.() ?? []) out.push(["set-cookie", c]);
  return out;
}

export function toValue(res: Response): HttpResponseValue {
  return { status: res.status, headers: pairs(res.headers), body: res.body };
}


// ---------------------------------------------------------------------------
// The bridge calls every host answers the same way

export function errorMessage(err: unknown, dev: boolean): string {
  if (err instanceof Error) return dev && err.stack ? err.stack : err.message;
  return String(err);
}

/** The `Bridge` calls that don't depend on the platform. */
export type BaseBridge = Pick<Bridge, "call" | "readBody" | "limitBody" | "collect" | "fetch" | "sleep">;

/**
 * `fetchArgs` adds arguments to the `fetch` call of a handler that's an app
 * (Hono and the like), such as a Worker's `env` and `ctx`. `project` is the
 * loaded project, which runs the functions and outbound requests a handler
 * asks for; without it, `ctx.call` and `ctx.fetch` fail.
 */
export function baseBridge(
  bundle: Bundle,
  dev: boolean,
  fetchArgs?: () => unknown[],
  project?: Unsworn,
): BaseBridge {
  return {
    // Called as a method of the whole bridge, which is what a function or
    // an outbound request a handler asks for goes back out through.
    async call(this: Bridge, request: string, body, timeoutMs, caller): Promise<CallResult> {
      const msg = JSON.parse(request) as RequestFields;
      const [file] = splitRef(msg.handler);
      let fn;
      try {
        const mod = bundle.modules[file];
        if (!mod) throw new Error(`${file} isn't in the bundle; rebuild with unsworn build`);
        fn = pickHandler(mod, msg.handler, fetchArgs);
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
      const ctx = msg.context ?? {};
      // Not enumerable, so the context still reads (and prints) as plain data.
      const asks = calls(project, this, caller);
      Object.defineProperty(ctx, "call", { value: asks.call });
      Object.defineProperty(ctx, "fetch", { value: asks.fetch });
      const run = (async () => outcome(await fn(req, ctx)))();
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
          redirect: request.redirect,
          // Whole milliseconds, which is all `AbortSignal.timeout` takes.
          signal: AbortSignal.timeout(Math.max(1, Math.floor(timeoutMs))),
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

    sleep(ms) {
      return new Promise((resolve) => setTimeout(resolve, ms));
    },
  };
}

// ---------------------------------------------------------------------------
// Loading the project, and answering

/**
 * Loads the project into the core. `wasm` is the core's module or its
 * bytes; `vars` is what `{env.NAME}` reads. Throws when the project has
 * errors.
 */
function loadProject(
  bundle: Bundle,
  wasm: WebAssembly.Module | BufferSource,
  vars: Record<string, unknown>,
  dev: boolean,
): Unsworn {
  initSync({ module: wasm });
  const strings = Object.fromEntries(Object.entries(vars).filter(([, v]) => typeof v === "string"));
  const app = new Unsworn(bundle.config, bundle.files, strings, dev);
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
  return app;
}

/**
 * A project that's loaded when first asked for, once per isolate or
 * process: `load()` gives the same one every time, or throws the same
 * error. `wasm` and `vars` are only called for that first load.
 */
export function lazyProject(
  bundle: Bundle,
  wasm: () => WebAssembly.Module | BufferSource,
  vars: () => Record<string, unknown>,
  dev: boolean,
): () => Unsworn {
  let app: Unsworn | undefined;
  let failed: string | undefined;
  return () => {
    if (app) return app;
    if (failed !== undefined) throw new Error(failed);
    try {
      return (app = loadProject(bundle, wasm(), vars(), dev));
    } catch (err) {
      failed = errorMessage(err, false);
      throw err;
    }
  };
}

/** A `500` for a request the core never saw. */
export function failure(status: number, detail: string, dev: boolean): Response {
  const body = { type: "about:blank", title: "Internal Server Error", status, ...(dev ? { detail } : {}) };
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/problem+json" } });
}

const NULL_BODY = new Set([101, 204, 205, 304]);

/** The core's answer as a `Response`. */
export function toResponse(handled: HttpResponseValue, method: string): Response {
  const body = NULL_BODY.has(handled.status) || method === "HEAD" ? null : (handled.body ?? null);
  return new Response(body as BodyInit | null, { status: handled.status, headers: handled.headers });
}
