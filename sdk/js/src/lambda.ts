// Runs an Unsworn project as an AWS Lambda function, on the Node host
// (node.ts). `unsworn build --target lambda` writes the entry that uses
// this and bundles it into `dist/lambda`:
//
//   import { createLambdaHandlers } from "@unswornjs/sdk/lambda";
//   import * as m0 from "../../src/users.ts";
//
//   export const { handler, streamHandler } = createLambdaHandlers(
//     { config: "unsworn.toml", files: { ... }, modules: { "src/users.ts": m0 } },
//     { wasm: new URL("./unsworn_bg.wasm", import.meta.url), assets: new URL("./assets/", import.meta.url) },
//   );
//
// Pick the handler that matches how the function is invoked:
//
//   index.handler        answers whole: API Gateway (HTTP and REST APIs)
//                        and function URLs in the default BUFFERED mode
//   index.streamHandler  streams: function URLs with RESPONSE_STREAM
//
// One function runs the whole project, so route every path to it. The
// function's environment variables are what `{env.NAME}` in the config
// reads. A handler that's an app with `fetch()` gets the Lambda event and
// context as its second and third arguments.

import { type Bundle, createHandler, type Handler, type HandlerOptions } from "./node.ts";

export type { Bundle, HandlerOptions };

/** The fields this reads from an API Gateway or function URL event, in either payload format. */
interface HttpEvent {
  // Payload format 2.0: HTTP APIs and function URLs.
  version?: string;
  rawPath?: string;
  rawQueryString?: string;
  cookies?: string[];
  // Payload format 1.0: REST APIs.
  httpMethod?: string;
  path?: string;
  multiValueHeaders?: Record<string, string[] | undefined> | null;
  queryStringParameters?: Record<string, string | undefined> | null;
  multiValueQueryStringParameters?: Record<string, string[] | undefined> | null;
  // Both.
  headers?: Record<string, string | undefined> | null;
  body?: string | null;
  isBase64Encoded?: boolean;
  requestContext?: {
    domainName?: string;
    stage?: string;
    http?: { method?: string; sourceIp?: string };
    identity?: { sourceIp?: string };
  };
}

interface LambdaContext {
  awsRequestId?: string;
}

interface HttpResult {
  statusCode: number;
  headers?: Record<string, string>;
  multiValueHeaders?: Record<string, string[]>;
  cookies?: string[];
  body: string;
  isBase64Encoded: boolean;
}

interface ResponseStream {
  write(chunk: Uint8Array): boolean;
  end(): void;
  once(event: "drain", listener: () => void): unknown;
}

/** What Lambda's Node.js runtime provides for streamed responses. */
interface AwsLambda {
  streamifyResponse<T>(handler: (event: HttpEvent, stream: ResponseStream, context: LambdaContext) => Promise<T>): unknown;
  HttpResponseStream: {
    from(
      stream: ResponseStream,
      prelude: { statusCode: number; headers: Record<string, string>; cookies: string[] },
    ): ResponseStream;
  };
}

const isV2 = (event: HttpEvent) => event.version === "2.0" || event.requestContext?.http !== undefined;

/** The event as a `Request`. */
function toRequest(event: HttpEvent): Request {
  const headers = new Headers();
  const add = (name: string, value: string | undefined) => {
    try {
      if (value !== undefined) headers.append(name, value);
    } catch {
      // skip headers the platform refuses
    }
  };
  if (event.multiValueHeaders) {
    for (const [name, values] of Object.entries(event.multiValueHeaders)) for (const v of values ?? []) add(name, v);
  } else {
    for (const [name, value] of Object.entries(event.headers ?? {})) add(name, value);
  }
  // Format 2.0 takes cookies out of the headers.
  if (event.cookies?.length) headers.set("cookie", event.cookies.join("; "));

  let path: string;
  let query: string;
  let method: string;
  if (isV2(event)) {
    method = event.requestContext?.http?.method ?? "GET";
    path = event.rawPath ?? "/";
    query = event.rawQueryString ?? "";
    // On an API's own domain, a named stage is the first path segment.
    const stage = event.requestContext?.stage;
    const own = event.requestContext?.domainName?.includes(".execute-api.");
    if (own && stage && stage !== "$default" && (path === `/${stage}` || path.startsWith(`/${stage}/`))) {
      path = path.slice(stage.length + 1) || "/";
    }
  } else {
    method = event.httpMethod ?? "GET";
    // Format 1.0 gives the path decoded, and without the stage.
    path = (event.path ?? "/").split("/").map(encodeURIComponent).join("/");
    const params = new URLSearchParams();
    if (event.multiValueQueryStringParameters) {
      for (const [k, values] of Object.entries(event.multiValueQueryStringParameters)) {
        for (const v of values ?? []) params.append(k, v);
      }
    } else {
      for (const [k, v] of Object.entries(event.queryStringParameters ?? {})) params.append(k, v ?? "");
    }
    query = params.toString();
  }

  const host = headers.get("host") ?? event.requestContext?.domainName ?? "localhost";
  const proto = headers.get("x-forwarded-proto")?.split(",")[0].trim() || "https";
  const url = `${proto}://${host}${path}${query ? `?${query}` : ""}`;
  const hasBody = method !== "GET" && method !== "HEAD" && event.body != null && event.body !== "";
  const body = hasBody ? Buffer.from(event.body as string, event.isBase64Encoded ? "base64" : "utf8") : undefined;
  return new Request(url, { method, headers, body });
}

/** Whether a body of this type can go out as text instead of base64. */
function isText(contentType: string | null): boolean {
  const type = (contentType ?? "").split(";")[0].trim().toLowerCase();
  return (
    type.startsWith("text/") ||
    /^application\/(json|xml|javascript|x-www-form-urlencoded)$/.test(type) ||
    type.endsWith("+json") ||
    type.endsWith("+xml")
  );
}

/** A response's headers, with the cookies apart, as format 2.0 wants them. */
function splitHeaders(response: Response): { headers: Record<string, string>; cookies: string[] } {
  const headers: Record<string, string> = {};
  response.headers.forEach((value, name) => {
    if (name !== "set-cookie") headers[name] = value;
  });
  return { headers, cookies: response.headers.getSetCookie?.() ?? [] };
}

async function toResult(response: Response, v2: boolean): Promise<HttpResult> {
  const bytes = Buffer.from(await response.arrayBuffer());
  const text = isText(response.headers.get("content-type"));
  const { headers, cookies } = splitHeaders(response);
  const result: HttpResult = {
    statusCode: response.status,
    body: bytes.toString(text ? "utf8" : "base64"),
    isBase64Encoded: !text && bytes.length > 0,
  };
  if (v2) {
    result.headers = headers;
    if (cookies.length) result.cookies = cookies;
  } else {
    result.multiValueHeaders = Object.fromEntries(Object.entries(headers).map(([k, v]) => [k, [v]]));
    if (cookies.length) result.multiValueHeaders["set-cookie"] = cookies;
  }
  return result;
}

function answer(app: Handler, event: HttpEvent, context: LambdaContext | undefined): Promise<Response> {
  return app.fetch(toRequest(event), {
    ip: event.requestContext?.http?.sourceIp ?? event.requestContext?.identity?.sourceIp,
    requestId: context?.awsRequestId,
    fetchArgs: [event, context],
  });
}

/** The Lambda handlers that serve the bundled project. */
export function createLambdaHandlers(bundle: Bundle, options: HandlerOptions = {}) {
  const app = createHandler(bundle, options);

  /** Answers whole: for API Gateway, and function URLs in BUFFERED mode. */
  async function handler(event: HttpEvent, context?: LambdaContext): Promise<HttpResult> {
    const result = await toResult(await answer(app, event, context), isV2(event));
    // Lambda freezes the process once this returns, so finish background
    // work (a cache write, say) first.
    await app.idle();
    return result;
  }

  async function stream(event: HttpEvent, responseStream: ResponseStream, context: LambdaContext): Promise<void> {
    const aws = (globalThis as { awslambda?: AwsLambda }).awslambda as AwsLambda;
    const response = await answer(app, event, context);
    const out = aws.HttpResponseStream.from(responseStream, { statusCode: response.status, ...splitHeaders(response) });
    if (response.body) {
      const reader = response.body.getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        if (!out.write(value)) await new Promise<void>((resolve) => out.once("drain", resolve));
      }
    }
    out.end();
    await app.idle();
  }

  const aws = (globalThis as { awslambda?: AwsLambda }).awslambda;
  /** Streams: for function URLs with `InvokeMode: RESPONSE_STREAM`. */
  const streamHandler = aws
    ? aws.streamifyResponse(stream)
    : async () => {
        throw new Error("streamHandler needs Lambda's Node.js runtime; use `handler` elsewhere");
      };

  return { handler, streamHandler };
}
