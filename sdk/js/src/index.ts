// Types for writing Unsworn handlers in JavaScript or TypeScript.
//
//   import type { Handler } from "@unswornjs/sdk";
//
//   export const getUser: Handler<{ params: { id: number } }> = async (req, ctx) => {
//     return { json: await db.users.find(req.params.id) };
//   };

export interface Shape {
  params?: Record<string, unknown>;
  query?: Record<string, unknown>;
  json?: unknown;
}

/** A request, already matched, authenticated and validated by Unsworn. */
export interface UnswornRequest<S extends Shape = Shape> {
  method: string;
  /** The request path, e.g. `/users/42`. */
  path: string;
  /** The route pattern, e.g. `/users/{id}`. */
  route: string;
  /** Path parameters, converted to their declared types. */
  params: S["params"] extends object ? S["params"] : Record<string, unknown>;
  /** Query parameters, converted to their declared types. */
  query: S["query"] extends object ? S["query"] : Record<string, unknown>;
  /** The query string as sent, without the `?`. */
  rawQuery?: string;
  /** Request headers, with lowercase names. */
  headers: Record<string, string>;
  /** The parsed JSON body, when the request had one. */
  json: S["json"];
  /** A text body. */
  text?: string;
  /** A raw body, for anything that isn't JSON or text. */
  body?: Uint8Array;
  /** The body as it arrives, on routes with `stream_body = true`. */
  stream?: ReadableStream<Uint8Array>;
  /** Aborted when Unsworn cancels the request (timeout or client gone). */
  signal: AbortSignal;
  /** The handler reference, e.g. `src/users.ts#getUser`. */
  handler: string;
  phase: "handle" | "before";
  /** Logs to Unsworn, tagged with this request. */
  log: Logger;
}

/** Sends log records to Unsworn. `fields` are kept as structured data. */
export interface Logger {
  error(message: string, fields?: Record<string, unknown>): void;
  warn(message: string, fields?: Record<string, unknown>): void;
  info(message: string, fields?: Record<string, unknown>): void;
  debug(message: string, fields?: Record<string, unknown>): void;
  trace(message: string, fields?: Record<string, unknown>): void;
}

function fallback(level: string) {
  return (message: string, fields?: Record<string, unknown>) =>
    console.error(`[${level}] ${message}`, ...(fields ? [fields] : []));
}

function worker(): Logger | undefined {
  return (globalThis as Record<symbol, Logger | undefined>)[Symbol.for("unsworn.log")];
}

/**
 * Logs to Unsworn from anywhere in handler code. Records written while a
 * request is being handled are tagged with it. Outside the worker (in
 * tests, say), records go to stderr.
 */
export const log: Logger = {
  error: (m, f) => (worker()?.error ?? fallback("error"))(m, f),
  warn: (m, f) => (worker()?.warn ?? fallback("warn"))(m, f),
  info: (m, f) => (worker()?.info ?? fallback("info"))(m, f),
  debug: (m, f) => (worker()?.debug ?? fallback("debug"))(m, f),
  trace: (m, f) => (worker()?.trace ?? fallback("trace"))(m, f),
};

/** Details of an MCP tool call, in `ctx.mcp`. */
export interface McpCall {
  /** The tool's name. */
  tool: string;
  /** The MCP protocol version of the call, e.g. `2026-07-28`. */
  protocol_version: string;
  /** What the client says about itself, if anything. For logs, not decisions. */
  client?: { name: string; version: string; [key: string]: unknown };
}

/**
 * The functions `ctx.call` can call: each `[[function]]` in the config,
 * with what it takes and answers with. `unsworn gen ts` fills this in, so
 * calls are checked against the functions' schemas.
 */
// biome-ignore lint/suspicious/noEmptyInterface: filled in by generated code
export interface Functions {}

type FunctionName = keyof Functions extends never ? string : keyof Functions;
type Input<K> = K extends keyof Functions ? (Functions[K] extends { input: infer I } ? I : unknown) : unknown;
type Output<K> = K extends keyof Functions ? (Functions[K] extends { output: infer O } ? O : unknown) : unknown;

/** A function answered with an error status, or couldn't be called. */
export class CallError extends Error {
  /**
   * The function's status, or `404` for a name with no `[[function]]`,
   * `422` for input its schema refuses, `504` for a timeout.
   */
  status: number;
  /** What the function answered with, if anything. */
  json: unknown;

  constructor(status: number, message: string, json?: unknown) {
    super(message);
    this.name = "CallError";
    this.status = status;
    this.json = json;
  }
}

/**
 * The services `ctx.fetch` can reach: each `[outbound.<name>]` in the
 * config. `unsworn gen ts` fills this in, so a name that isn't one is a
 * compile error.
 */
// biome-ignore lint/suspicious/noEmptyInterface: filled in by generated code
export interface Services {}

type ServiceName = keyof Services extends never ? string : keyof Services;

/** A request for `ctx.fetch`. */
export interface FetchInit {
  /** Default: `GET`, or `POST` with a `json` or `body`. */
  method?: string;
  headers?: Record<string, string>;
  /** Sent as the JSON body, with `content-type: application/json`. */
  json?: unknown;
  /** A text or raw body. */
  body?: string | Uint8Array;
}

/** A service's answer to `ctx.fetch`. */
export interface Fetched {
  status: number;
  /** Whether `status` is 200 to 299. */
  ok: boolean;
  /** Response headers, with lowercase names. */
  headers: Record<string, string>;
  body: Uint8Array;
  text(): string;
  json<T = unknown>(): T;
}

/** `ctx.fetch` couldn't make the request, or got no answer. */
export class FetchError extends Error {
  /**
   * `404` for a name with no `[outbound.<name>]`, `403` for a method or
   * path the config doesn't allow, `502` for a service that couldn't be
   * reached, `504` for one that took too long.
   */
  status: number;

  constructor(status: number, message: string) {
    super(message);
    this.name = "FetchError";
    this.status = status;
  }
}

/** Who called a function, in the function's `ctx.caller`. */
export interface Caller {
  /** The function's name. */
  function: string;
  /** The handler that called it. */
  from: string;
  depth: number;
}

/** What a `ctx.call` can say besides the function and its input. */
export interface CallOptions {
  /**
   * A W3C `traceparent` for the function to be called under, such as that
   * of the handler's own span. Without one, or with one that isn't valid,
   * the function gets the request's.
   */
  traceparent?: string;
  /** The `tracestate` that goes with `traceparent`. Ignored without it. */
  tracestate?: string;
}

/** Verified credentials plus anything added by `before` handlers. */
export interface Context {
  /** Claims from the verified token or API key, plus `scheme`. */
  auth?: Record<string, unknown> & { scheme?: string; sub?: string };
  /** Set when the handler runs as an MCP tool. */
  mcp?: McpCall;
  /** Set when the handler runs as a function another handler called. */
  caller?: Caller;
  /**
   * Calls a `[[function]]`: a handler in any language, declared in the
   * config. Unsworn checks the input against the function's schema, runs
   * it with this request's context and what is left of its time, and
   * gives back the JSON it answered with. Throws a `CallError` when the
   * function answers `400` or above, or can't be called.
   */
  call<K extends FunctionName>(name: K, input?: Input<K>, options?: CallOptions): Promise<Output<K>>;
  /**
   * Sends a request to an outbound service: one the config declares under
   * `[outbound.<name>]`. Unsworn makes the request and adds the service's
   * own headers, such as its API key, so this handler never holds them.
   * `path` goes after the service's URL and may carry a query. Resolves
   * with the service's answer whatever its status, and throws a
   * `FetchError` when the request couldn't be made or got no answer.
   */
  fetch(service: ServiceName, path: string, init?: FetchInit): Promise<Fetched>;
  [key: string]: unknown;
}

/**
 * An MCP content block, such as `{ type: "text", text }`,
 * `{ type: "image", data, mimeType }` (base64 data) or
 * `{ type: "resource_link", uri, name }`.
 */
export interface McpContent {
  type: string;
  [key: string]: unknown;
}

export type Body =
  | string
  | Uint8Array
  | ArrayBuffer
  | ReadableStream<Uint8Array>
  | AsyncIterable<Uint8Array | string>;

/** What a handler returns. A web `Response` works too. */
export interface ReplyObject {
  status?: number;
  headers?: Record<string, string | string[]>;
  json?: unknown;
  text?: string;
  /** Raw bytes, or a stream/async iterable to stream the response. */
  body?: Body;
  /** MCP content blocks, for handlers that run as MCP tools. */
  mcpContent?: McpContent[];
}

/** A `before` handler can let the request continue, adding context. */
export interface Continue {
  continue: Record<string, unknown>;
}

export type Reply = ReplyObject | Response;

export type Handler<S extends Shape = Shape> = (
  req: UnswornRequest<S>,
  ctx: Context,
) => Reply | Promise<Reply>;

export type Before<S extends Shape = Shape> = (
  req: UnswornRequest<S>,
  ctx: Context,
) => Reply | Continue | Promise<Reply | Continue>;

/** `return next({ tenant: "acme" })` from a `before` handler. */
export function next(context: Record<string, unknown> = {}): Continue {
  return { continue: context };
}

export function json(value: unknown, status = 200, headers?: Record<string, string>): ReplyObject {
  return { status, json: value, headers };
}

export function text(value: string, status = 200, headers?: Record<string, string>): ReplyObject {
  return { status, text: value, headers };
}

/**
 * An MCP tool result made of content blocks, such as images. With `json`
 * too, the agent also gets it as structured content.
 */
export function content(blocks: McpContent[], json?: unknown): ReplyObject {
  return json === undefined ? { status: 200, mcpContent: blocks } : { status: 200, mcpContent: blocks, json };
}

/**
 * A tool error the model sees and can react to, such as "city not found".
 * Unsworn turns it into an MCP result with `isError: true`. As an HTTP
 * reply, it's a `400` (or `status`) with `{ error: message }`.
 */
export function toolError(message: string, status = 400): ReplyObject {
  return { status, json: { error: message } };
}
