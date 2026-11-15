// The Vercel target end to end, as far as it goes without Vercel: a project
// built with `unsworn build --target vercel`, then the bundled function run
// behind a local HTTP server that calls it the way Vercel's Node.js
// launcher does. Needs the wasm core built (crates/unsworn-wasm/build.sh)
// and `npm install` here.

import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { after, before, test } from "node:test";

import { build, redisRest, serve, upstream } from "./harness.mjs";

const fixture = new URL("./fixture", import.meta.url).pathname;
const output = join(fixture, ".vercel/output");
const func = join(output, "functions/unsworn.func");

let built;
let fn;
let up;
let redis;

before(async () => {
  up = await upstream();
  redis = await redisRest();
  // Only UPSTREAM is missing at build time, as a secret would be.
  built = build(fixture, "vercel", { REDIS_REST_URL: redis.url, REDIS_REST_TOKEN: redis.token });
  Object.assign(process.env, {
    UPSTREAM: up.url,
    REDIS_REST_URL: redis.url,
    REDIS_REST_TOKEN: redis.token,
    GREETING: "hello",
    UNSWORN_DEV: "1",
  });
  const { default: handler } = await import(join(func, "index.mjs"));
  fn = await serve(handler);
});

after(() => {
  fn?.close();
  up?.close();
  redis?.close();
});

// What Vercel's proxy adds to every request.
const platform = { "x-real-ip": "203.0.113.7", "x-forwarded-proto": "https", "x-forwarded-host": "api.example.com" };
const get = (path, init = {}) => fetch(fn.base + path, { ...init, headers: { ...platform, ...init.headers } });

test("the build writes a Build Output API deployment", () => {
  assert.match(built.output, /built \.vercel\/output: 14 routes, 4 handler modules, 3 asset files/);
  assert.match(built.output, /\{env\.UPSTREAM\} isn't set here.*vercel env add UPSTREAM/);
  const config = JSON.parse(readFileSync(join(output, "config.json"), "utf8"));
  assert.deepEqual(config, { version: 3, routes: [{ src: "/(.*)", dest: "/unsworn" }] });
  const vc = JSON.parse(readFileSync(join(func, ".vc-config.json"), "utf8"));
  assert.equal(vc.handler, "index.mjs");
  assert.equal(vc.launcherType, "Nodejs");
  assert.equal(vc.supportsResponseStreaming, true);
  // From the project's vercel.json.
  assert.equal(vc.maxDuration, 30);
  assert.equal(vc.memory, 1024);
  assert.deepEqual(vc.regions, ["fra1"]);
  for (const file of ["index.mjs", "index.mjs.map", "unsworn_bg.wasm", "assets/public/style.css"]) {
    assert.ok(existsSync(join(func, file)), file);
  }
  // The bundle stands alone: nothing left to resolve from node_modules.
  assert.doesNotMatch(readFileSync(join(func, "index.mjs"), "utf8"), /from "@unswornjs/);
});

test("fixed routes, handlers and validation", async () => {
  const health = await get("/health");
  assert.equal(health.status, 200);
  assert.deepEqual(await health.json(), { ok: true });
  const r = await get("/users/7", { headers: { "x-tenant": "acme", "x-vercel-id": "fra1::abc" } });
  assert.deepEqual(await r.json(), { id: 7, tenant: "acme", greeting: "hello" });
  assert.equal(r.headers.get("x-request-id"), "fra1::abc");
  assert.equal((await get("/users/0")).status, 400);
  assert.equal((await get("/nope")).status, 404);

  const json = { "content-type": "application/json" };
  const created = await get("/users", { method: "POST", headers: json, body: '{"name":"Ada"}' });
  assert.equal(created.status, 201);
  assert.deepEqual(created.headers.getSetCookie(), ["a=1", "b=2"]);
  assert.equal((await get("/users", { method: "POST", headers: json, body: '{"name":1}' })).status, 422);
});

test("bodies stream both ways", async () => {
  const echo = await get("/echo", { method: "POST", body: "hello" });
  assert.equal(await echo.text(), "HELLO");
  const events = await get("/events");
  assert.equal(events.headers.get("content-type"), "text/event-stream");
  assert.equal(await events.text(), "data: 1\n\ndata: 2\n\ndata: 3\n\n");
});

test("rate limits use the client IP Vercel reports", async () => {
  assert.equal((await get("/limited")).status, 200);
  assert.equal((await get("/limited")).status, 200);
  assert.equal((await get("/limited")).status, 429);
  assert.equal((await get("/limited", { headers: { "x-real-ip": "203.0.113.8" } })).status, 200);
});

test("responses are cached", async () => {
  const first = await (await get("/counter")).text();
  const second = await get("/counter");
  assert.equal(await second.text(), first);
  assert.ok(second.headers.get("age") !== null);
});

test("static files come from the function's own directory", async () => {
  const css = await get("/site/style.css");
  assert.equal(css.headers.get("content-type"), "text/css; charset=utf-8");
  assert.equal(await css.text(), "body { color: red }\n");
  assert.equal(await (await get("/site/some/route")).text(), "<h1>spa</h1>\n");
});

test("proxy routes see the public URL and the client", async () => {
  const seen = await (await get("/up/a?b=1")).json();
  assert.equal(seen.url, "/a?b=1");
  assert.equal(seen.headers["x-forwarded-proto"], "https");
  assert.equal(seen.headers["x-forwarded-host"], "api.example.com");
  assert.equal(seen.headers["x-forwarded-for"], "203.0.113.7");
});
