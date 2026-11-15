// The Netlify target end to end, as far as it goes without Netlify: a
// project built with `unsworn build --target netlify`, then the function
// called the way Netlify calls one, with a Request and its context. Needs
// the wasm core built (crates/unsworn-wasm/build.sh) and `npm install`
// here.

import assert from "node:assert/strict";
import { existsSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { after, before, test } from "node:test";

import { build, redisRest, upstream } from "./harness.mjs";

const fixture = new URL("./fixture", import.meta.url).pathname;
const dir = join(fixture, ".netlify/v1/functions/unsworn");

let built;
let fn;
let up;
let redis;

before(async () => {
  up = await upstream();
  redis = await redisRest();
  built = build(fixture, "netlify", { REDIS_REST_URL: redis.url, REDIS_REST_TOKEN: redis.token });
  Object.assign(process.env, {
    UPSTREAM: up.url,
    REDIS_REST_URL: redis.url,
    REDIS_REST_TOKEN: redis.token,
    GREETING: "hello",
    UNSWORN_DEV: "1",
  });
  fn = await import(join(dir, "unsworn.mjs"));
});

after(() => {
  up?.close();
  redis?.close();
});

const waited = [];
const context = (ip = "203.0.113.7") => ({ ip, requestId: "01JNETLIFY", waitUntil: (p) => waited.push(p) });
const get = (path, init, ctx = context()) => fn.default(new Request(`https://api.example.com${path}`, init), ctx);

test("the build writes a Frameworks API function", () => {
  assert.match(built.output, /built \.netlify\/v1: 14 routes, 4 handler modules, 3 asset files/);
  assert.match(built.output, /netlify env:set UPSTREAM/);
  assert.match(built.output, /netlify\.toml needs/);
  assert.deepEqual(fn.config, {
    path: "/*",
    name: "Unsworn",
    generator: fn.config.generator,
    nodeBundler: "none",
    includedFiles: [".netlify/v1/functions/unsworn/**"],
  });
  assert.match(fn.config.generator, /^unsworn@\d/);
  for (const file of ["index.mjs", "unsworn_bg.wasm", "assets/public/style.css"]) {
    assert.ok(existsSync(join(dir, file)), file);
  }
  // Nothing in the output but the function, and the marker for rebuilds.
  assert.deepEqual(readdirSync(join(fixture, ".netlify/v1")).sort(), [".unsworn-build", "functions"]);
  assert.ok(existsSync(join(fixture, ".netlify/static")));
});

test("fixed routes, handlers and validation", async () => {
  const health = await get("/health");
  assert.equal(health.status, 200);
  assert.equal(health.headers.get("x-request-id"), "01JNETLIFY");
  const r = await get("/users/7", { headers: { "x-tenant": "acme" } });
  assert.deepEqual(await r.json(), { id: 7, tenant: "acme", greeting: "hello" });
  assert.equal((await get("/users/0")).status, 400);
  assert.equal((await get("/nope")).status, 404);
});

test("bodies stream both ways", async () => {
  const echo = await get("/echo", { method: "POST", body: "hello" });
  assert.equal(await echo.text(), "HELLO");
  assert.equal(await (await get("/events")).text(), "data: 1\n\ndata: 2\n\ndata: 3\n\n");
});

test("rate limits use the client IP from Netlify's context", async () => {
  assert.equal((await get("/limited")).status, 200);
  assert.equal((await get("/limited")).status, 200);
  assert.equal((await get("/limited")).status, 429);
  assert.equal((await get("/limited", {}, context("203.0.113.8"))).status, 200);
});

test("cache writes are handed to waitUntil", async () => {
  const before = waited.length;
  const first = await (await get("/counter")).text();
  assert.ok(waited.length > before);
  await Promise.all(waited);
  assert.equal(await (await get("/counter")).text(), first);
});

test("static files and proxy routes", async () => {
  assert.equal(await (await get("/site/style.css")).text(), "body { color: red }\n");
  const seen = await (await get("/up/a?b=1")).json();
  assert.equal(seen.url, "/a?b=1");
  assert.equal(seen.headers["x-forwarded-for"], "203.0.113.7");
  assert.equal(seen.headers["x-forwarded-host"], "api.example.com");
});

test("without a context, background work finishes before the response", async () => {
  const r = await fn.default(new Request("https://api.example.com/health"));
  assert.equal(r.status, 200);
});
