// The Lambda target end to end, as far as it goes without AWS: a project
// built with `unsworn build --target lambda`, then its handlers called
// with the events API Gateway and function URLs send. Needs the wasm core
// built (crates/unsworn-wasm/build.sh) and `npm install` here.

import assert from "node:assert/strict";
import { readdirSync } from "node:fs";
import { join } from "node:path";
import { Writable } from "node:stream";
import { after, before, test } from "node:test";

import { build, redisRest, upstream } from "./harness.mjs";

const fixture = new URL("./fixture", import.meta.url).pathname;
const dir = join(fixture, "dist/lambda");

let built;
let fn;
let up;
let redis;

// What Lambda's Node.js runtime provides for streamed responses.
globalThis.awslambda = {
  streamifyResponse: (handler) => handler,
  HttpResponseStream: {
    from(stream, prelude) {
      stream.prelude = prelude;
      return stream;
    },
  },
};

before(async () => {
  up = await upstream();
  redis = await redisRest();
  built = build(fixture, "lambda", { REDIS_REST_URL: redis.url, REDIS_REST_TOKEN: redis.token });
  Object.assign(process.env, {
    UPSTREAM: up.url,
    REDIS_REST_URL: redis.url,
    REDIS_REST_TOKEN: redis.token,
    GREETING: "hello",
    UNSWORN_DEV: "1",
  });
  fn = await import(join(dir, "index.mjs"));
});

after(() => {
  up?.close();
  redis?.close();
});

const context = { awsRequestId: "req-123" };

/** An event in payload format 2.0, as HTTP APIs and function URLs send. */
function v2(path, { method = "GET", headers = {}, body, base64 = false, ip = "203.0.113.7", cookies, stage = "$default" } = {}) {
  const [rawPath, rawQueryString = ""] = path.split("?");
  const domainName = "abc123.lambda-url.eu-west-1.on.aws";
  return {
    version: "2.0",
    rawPath,
    rawQueryString,
    cookies,
    headers: { host: domainName, "x-forwarded-proto": "https", ...headers },
    requestContext: { domainName, stage, http: { method, path: rawPath, sourceIp: ip } },
    body,
    isBase64Encoded: base64,
  };
}

const call = (path, options) => fn.handler(v2(path, options), context);
const text = (result) => (result.isBase64Encoded ? Buffer.from(result.body, "base64").toString() : result.body);

test("the build writes only the function's code", () => {
  assert.match(built.output, /built dist\/lambda: 14 routes, 4 handler modules, 3 asset files/);
  assert.match(built.output, /index\.handler/);
  assert.match(built.output, /environment variables/);
  assert.deepEqual(readdirSync(dir).sort(), [".unsworn-build", "assets", "index.mjs", "index.mjs.map", "unsworn_bg.wasm"]);
  assert.equal(typeof fn.handler, "function");
  assert.equal(typeof fn.streamHandler, "function");
});

test("format 2.0 events: handlers, validation and cookies", async () => {
  const health = await call("/health");
  assert.equal(health.statusCode, 200);
  assert.equal(health.isBase64Encoded, false);
  assert.deepEqual(JSON.parse(health.body), { ok: true });
  assert.equal(health.headers["x-request-id"], "req-123");

  const user = await call("/users/7", { headers: { "x-tenant": "acme" } });
  assert.deepEqual(JSON.parse(user.body), { id: 7, tenant: "acme", greeting: "hello" });
  assert.equal((await call("/users/0")).statusCode, 400);
  assert.equal((await call("/nope")).statusCode, 404);

  const json = { "content-type": "application/json" };
  const created = await call("/users", { method: "POST", headers: json, body: '{"name":"Ada"}' });
  assert.equal(created.statusCode, 201);
  assert.deepEqual(created.cookies, ["a=1", "b=2"]);
  assert.equal(created.headers["set-cookie"], undefined);
  const encoded = Buffer.from('{"name":1}').toString("base64");
  assert.equal((await call("/users", { method: "POST", headers: json, body: encoded, base64: true })).statusCode, 422);
});

test("a named stage on the API's own domain isn't part of the path", async () => {
  const event = v2("/prod/health", { stage: "prod" });
  event.requestContext.domainName = "abc123.execute-api.eu-west-1.amazonaws.com";
  assert.equal((await fn.handler(event, context)).statusCode, 200);
  // On a custom domain, the path is the path.
  assert.equal((await call("/prod/health", { stage: "prod" })).statusCode, 404);
});

test("format 1.0 events, as REST APIs send", async () => {
  const event = {
    httpMethod: "GET",
    path: "/users/7",
    multiValueHeaders: { Host: ["api.example.com"], "X-Tenant": ["acme"] },
    multiValueQueryStringParameters: null,
    requestContext: { identity: { sourceIp: "203.0.113.9" }, stage: "prod" },
    body: null,
    isBase64Encoded: false,
  };
  const result = await fn.handler(event, context);
  assert.equal(result.statusCode, 200);
  assert.deepEqual(JSON.parse(result.body), { id: 7, tenant: "acme", greeting: "hello" });
  assert.deepEqual(result.multiValueHeaders["content-type"], ["application/json"]);
  assert.equal(result.headers, undefined);

  const proxied = await fn.handler({ ...event, path: "/up/a b", multiValueQueryStringParameters: { b: ["1", "2"] } }, context);
  const seen = JSON.parse(proxied.body);
  assert.equal(seen.url, "/a%20b?b=1&b=2");
  assert.equal(seen.headers["x-forwarded-for"], "203.0.113.9");
});

test("bodies in and out, text and binary", async () => {
  const echo = await call("/echo", { method: "POST", body: "hello" });
  assert.equal(text(echo), "HELLO");
  const events = await call("/events");
  assert.equal(events.headers["content-type"], "text/event-stream");
  assert.equal(events.body, "data: 1\n\ndata: 2\n\ndata: 3\n\n");
  // Text types go out as they are, not as base64.
  const css = await call("/site/style.css");
  assert.equal(css.isBase64Encoded, false);
  assert.equal(css.body, "body { color: red }\n");
  const app = await call("/app/x");
  assert.equal(text(app), "app /app/x hello");
});

test("rate limits use the source IP, and cache writes finish before the handler returns", async () => {
  assert.equal((await call("/limited")).statusCode, 200);
  assert.equal((await call("/limited")).statusCode, 200);
  assert.equal((await call("/limited")).statusCode, 429);
  assert.equal((await call("/limited", { ip: "203.0.113.8" })).statusCode, 200);

  const first = await call("/counter");
  assert.ok(redis.keys().some((k) => k.startsWith("fixture:cache64:")), "written by the time the handler returned");
  const second = await call("/counter");
  assert.equal(second.body, first.body);
  assert.ok(second.headers.age !== undefined);
});

test("streamHandler writes the status and headers, then the body as it comes", async () => {
  const chunks = [];
  const stream = new Writable({
    write(chunk, _encoding, done) {
      chunks.push(chunk.toString());
      done();
    },
  });
  const ended = new Promise((resolve) => stream.on("finish", resolve));
  await fn.streamHandler(v2("/events"), stream, context);
  await ended;
  assert.equal(stream.prelude.statusCode, 200);
  assert.equal(stream.prelude.headers["content-type"], "text/event-stream");
  assert.deepEqual(stream.prelude.cookies, []);
  assert.deepEqual(chunks, ["data: 1\n\n", "data: 2\n\n", "data: 3\n\n"]);
});
