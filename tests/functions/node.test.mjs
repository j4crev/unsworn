// The Node host (sdk/js/src/node.ts) that the serverless targets run on,
// driven directly: a bundle as `unsworn build` would write it, requests in,
// responses out. Needs the wasm core built (crates/unsworn-wasm/build.sh).
//
//   node --test tests/functions/node.test.mjs

import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, before, test } from "node:test";

import { createHandler } from "../../sdk/js/src/node.ts";
import { redisRest, upstream } from "./harness.mjs";

const fixture = new URL("./fixture/", import.meta.url);
const modules = ["src/app.ts", "src/auth.ts", "src/tools.ts", "src/users.ts"];

let app;
let up;
let redis;

before(async () => {
  up = await upstream();
  redis = await redisRest();
  const env = { UPSTREAM: up.url, REDIS_REST_URL: redis.url, REDIS_REST_TOKEN: redis.token, UNSWORN_DEV: "1" };
  process.env.GREETING = "hello";
  const bundle = {
    config: "unsworn.toml",
    files: {
      "unsworn.toml": readFileSync(new URL("unsworn.toml", fixture), "utf8"),
      public: null,
      ...Object.fromEntries(modules.map((m) => [m, null])),
    },
    modules: Object.fromEntries(await Promise.all(modules.map(async (m) => [m, await import(new URL(m, fixture))]))),
  };
  app = createHandler(bundle, { assets: fixture, env });
});

after(() => {
  up?.close();
  redis?.close();
});

const get = (path, init, info = { ip: "203.0.113.7" }) =>
  app.fetch(new Request(`https://api.example.com${path}`, init), info);

test("fixed routes and 404s", async () => {
  const r = await get("/health");
  assert.equal(r.status, 200);
  assert.deepEqual(await r.json(), { ok: true });
  assert.ok(r.headers.get("x-request-id"));
  assert.equal((await get("/nope")).status, 404);
  const named = await get("/health", {}, { requestId: "from-the-platform" });
  assert.equal(named.headers.get("x-request-id"), "from-the-platform");
});

test("handlers get typed params, before context and env", async () => {
  const r = await get("/users/7", { headers: { "x-tenant": "acme" } });
  assert.equal(r.status, 200, await r.clone().text());
  assert.deepEqual(await r.json(), { id: 7, tenant: "acme", greeting: "hello" });
  assert.equal((await get("/users/0")).status, 400);
});

test("bodies are validated, and cookies kept apart", async () => {
  const json = { "content-type": "application/json" };
  const r = await get("/users", { method: "POST", headers: json, body: '{"name":"Ada"}' });
  assert.equal(r.status, 201);
  assert.deepEqual(await r.json(), { name: "Ada" });
  assert.deepEqual(r.headers.getSetCookie(), ["a=1", "b=2"]);
  const bad = await get("/users", { method: "POST", headers: json, body: '{"name":1}' });
  assert.equal(bad.status, 422);
});

test("streamed bodies, with max_body", async () => {
  const r = await get("/echo", { method: "POST", body: "hello" });
  assert.equal(await r.text(), "HELLO");
  const big = await get("/echo", { method: "POST", body: "x".repeat(5000) });
  assert.equal(big.status, 413);
});

test("streamed responses", async () => {
  const r = await get("/events");
  assert.equal(r.headers.get("content-type"), "text/event-stream");
  assert.equal(await r.text(), "data: 1\n\ndata: 2\n\ndata: 3\n\n");
});

test("timeouts and handler errors", async () => {
  assert.equal((await get("/slow")).status, 504);
  const r = await get("/boom");
  assert.equal(r.status, 500);
  assert.match(await r.text(), /kaboom/);
});

test("fetch() apps", async () => {
  const r = await get("/app/x");
  assert.equal(await r.text(), "app /app/x hello");
  assert.equal(r.headers.get("x-app"), "1");
});

test("rate limits count in Redis, with the shared script and keys", async () => {
  assert.equal((await get("/limited")).status, 200);
  assert.equal((await get("/limited")).status, 200);
  const r = await get("/limited");
  assert.equal(r.status, 429);
  assert.ok(Number(r.headers.get("retry-after")) > 0);
  const evals = redis.commands.filter((c) => c[0] === "EVAL");
  assert.equal(evals.length, 3);
  assert.match(evals[0][3], /^fixture:rl:/);
  assert.deepEqual(evals[0].slice(4).map(Number), [2, 60000]);
  // Another client has a bucket of its own.
  assert.equal((await get("/limited", {}, { ip: "203.0.113.8" })).status, 200);
});

test("responses are cached in Redis", async () => {
  const first = await (await get("/counter")).text();
  await app.idle();
  const second = await get("/counter");
  assert.equal(await second.text(), first);
  assert.ok(second.headers.get("age") !== null);
  assert.ok(redis.keys().some((k) => k.startsWith("fixture:cache64:")));
});

test("static files, with index, fallback, ranges and validators", async () => {
  const css = await get("/site/style.css");
  assert.equal(css.status, 200);
  assert.equal(css.headers.get("content-type"), "text/css; charset=utf-8");
  assert.equal(css.headers.get("cache-control"), "public, max-age=60");
  assert.equal(await css.text(), "body { color: red }\n");
  assert.equal(await (await get("/site/docs/")).text(), "docs index\n");
  assert.equal(await (await get("/site/docs")).text(), "docs index\n");
  assert.equal(await (await get("/site/some/route")).text(), "<h1>spa</h1>\n");

  const part = await get("/site/style.css", { headers: { range: "bytes=0-3" } });
  assert.equal(part.status, 206);
  assert.equal(part.headers.get("content-range"), "bytes 0-3/20");
  assert.equal(await part.text(), "body");
  assert.equal((await get("/site/style.css", { headers: { range: "bytes=99-" } })).status, 416);

  // Validators come from the file's bytes. Platforms give every deployed
  // file the same time, so a date would call a changed file unchanged.
  assert.equal(css.headers.get("last-modified"), null);
  const since = { "if-modified-since": new Date(Date.now() + 86_400_000).toUTCString() };
  assert.equal((await get("/site/style.css", { headers: since })).status, 200);

  const etag = css.headers.get("etag");
  assert.match(etag, /^"[A-Za-z0-9_-]{27}"$/);
  const conditional = (headers) => get("/site/style.css", { headers });
  assert.equal((await conditional({ "if-none-match": `"x", W/${etag}` })).status, 304);
  assert.equal((await conditional({ "if-none-match": '"stale"' })).status, 200);
  const ranged = async (ifRange) => {
    const r = await conditional({ range: "bytes=0-3", "if-range": ifRange });
    return [r.status, await r.text()];
  };
  assert.deepEqual(await ranged(etag), [206, "body"]);
  assert.deepEqual(await ranged("Sat, 20 Oct 2018 01:46:40 GMT"), [200, "body { color: red }\n"]);
  assert.deepEqual(await ranged(`W/${etag}`), [200, "body { color: red }\n"]);
  const head = await get("/site/style.css", { method: "HEAD" });
  assert.equal(head.headers.get("content-length"), "20");
  assert.equal(await head.text(), "");
  // Nothing outside a route's directory, such as the config next to it.
  assert.equal((await get("/site/..%2Funsworn.toml")).status, 404);
});

test("a file that changes without changing its size or time gets a new ETag", async () => {
  // What a redeploy does on a platform that stamps every file with one time.
  const dir = mkdtempSync(join(tmpdir(), "unsworn-etag-"));
  try {
    mkdirSync(join(dir, "public"));
    const file = join(dir, "public/style.css");
    const stamp = new Date("2018-10-20T01:46:40Z");
    const write = (text) => {
      writeFileSync(file, text);
      utimesSync(file, stamp, stamp);
    };
    const config = '[[route]]\npath = "/site/{*rest}"\nmethod = "GET"\nstatic = { dir = "public" }\n';
    const bundle = { config: "unsworn.toml", files: { "unsworn.toml": config, public: null }, modules: {} };
    // Each deployment's instances are new, so each gets a handler of its own.
    const deploy = () => {
      const site = createHandler(bundle, { assets: dir, env: {} });
      return (headers) => site.fetch(new Request("https://api.example.com/site/style.css", { headers }), {});
    };

    write("body { color: red }\n");
    let css = deploy();
    const before = (await css()).headers.get("etag");
    assert.equal((await css({ "if-none-match": before })).status, 304);

    write("body { color: blu }\n");
    css = deploy();
    const changed = await css({ "if-none-match": before });
    assert.equal(changed.status, 200);
    assert.equal(await changed.text(), "body { color: blu }\n");
    assert.notEqual(changed.headers.get("etag"), before);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("proxy routes forward with rewritten headers and bodies", async () => {
  const r = await get("/up/a?b=1", { method: "POST", body: "payload" });
  assert.equal(r.status, 200);
  assert.equal(r.headers.get("connection"), null);
  const seen = await r.json();
  assert.equal(seen.url, "/a?b=1");
  assert.equal(seen.body, "payload");
  assert.equal(seen.headers["x-forwarded-proto"], "https");
  assert.equal(seen.headers["x-forwarded-for"], "203.0.113.7");
  assert.equal(seen.headers["x-forwarded-host"], "api.example.com");
  assert.ok(seen.headers["x-request-id"]);
});

test("WebSocket upgrades are refused", async () => {
  const r = await get("/up/chat", { headers: { connection: "Upgrade", upgrade: "websocket" } });
  assert.equal(r.status, 501);
});

test("MCP tools run as handlers", async () => {
  const send = async (method, params, name) => {
    params._meta = {
      "io.modelcontextprotocol/protocolVersion": "2026-07-28",
      "io.modelcontextprotocol/clientCapabilities": {},
      "io.modelcontextprotocol/clientInfo": { name: "test", version: "1" },
    };
    const headers = { "content-type": "application/json", "mcp-protocol-version": "2026-07-28", "mcp-method": method };
    if (name) headers["mcp-name"] = name;
    const r = await get("/mcp", { method: "POST", headers, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }) });
    assert.equal(r.status, 200);
    return (await r.json()).result;
  };
  const list = await send("tools/list", {});
  assert.deepEqual(list.tools.map((t) => t.name), ["weather"]);
  const result = await send("tools/call", { name: "weather", arguments: { city: "Oslo" } }, "weather");
  assert.deepEqual(result.structuredContent, { city: "Oslo", sky: "clear", tool: "weather" });
  const bad = await send("tools/call", { name: "weather", arguments: {} }, "weather");
  assert.equal(bad.isError, true);
});

test("store settings a function can't use are reported, and memory works per instance", async () => {
  const { storeFor, memoryStore } = await import("../../sdk/js/src/store.ts");
  assert.throws(() => storeFor({ backend: "redis", url: "redis://cache:6379", token: null, prefix: "" }), /REST API/);
  const wrong = storeFor({ backend: "redis", url: redis.url, token: "nope", prefix: "" });
  await assert.rejects(wrong.rateLimit("k", 1, 1000), /WRONGPASS/);

  const memory = memoryStore();
  assert.equal(await memory.rateLimit("k", 1, 60_000), null);
  assert.ok((await memory.rateLimit("k", 1, 60_000)) > 0);
  await memory.cachePut("c", new Uint8Array([1, 2]), 60_000);
  assert.deepEqual(await memory.cacheGet("c"), new Uint8Array([1, 2]));
  assert.equal(await memory.cacheGet("missing"), null);
});
