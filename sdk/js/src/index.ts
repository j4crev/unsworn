// Types for writing Unsworn handlers in JavaScript or TypeScript.
//
//   import type { Handler } from "@unsworn/js";
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
  /** Aborted when Unsworn cancels the request (timeout or client gone). */
  signal: AbortSignal;
  /** The handler reference, e.g. `src/users.ts#getUser`. */
  handler: string;
  phase: "handle" | "before";
}

/** Verified credentials plus anything added by `before` handlers. */
export interface Context {
  /** Claims from the verified token or API key, plus `scheme`. */
  auth?: Record<string, unknown> & { scheme?: string; sub?: string };
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
