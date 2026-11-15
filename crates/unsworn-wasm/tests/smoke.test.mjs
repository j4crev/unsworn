// Drives the built wasm core (sdk/js/wasm) through a fake bridge, checking
// what crosses the boundary: requests, handler calls, bodies, the store,
// assets and proxying. Build first: crates/unsworn-wasm/build.sh
//
//   node --test crates/unsworn-wasm/tests/

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import { Unsworn, initSync, takeToken, version } from "../../../sdk/js/wasm/unsworn.js";

initSync({ module: readFileSync(new URL("../../../sdk/js/wasm/unsworn_bg.wasm", import.meta.url)) });

const CONFIG = `
[runtimes.ts]
extensions = [".ts"]
command = "bun run worker.ts"

[[route]]
path = "/health"
method = "GET"
respond = { status = 200, json = { ok = true } }

[[route]]
path = "/users/{id}"
method = "GET"
handler = "src/users.ts#get"
params = { id = { type = "integer" } }
before = ["src/auth.ts#tenant"]

[[route]]
path = "/users"
method = "POST"
handler = "src/users.ts#create"
body = { type = "object", required = ["name"] }

[[route]]
path = "/upload"
method = "POST"
handler = "src/users.ts#upload"
stream_body = true

[[route]]
path = "/limited"
method = "GET"
respond = { status = 200, text = "ok" }
rate_limit = { requests = 2, per = "1m" }

[[route]]
path = "/cached"
method = "GET"
handler = "src/users.ts#count"
cache = { ttl = "30s" }

[[route]]
path = "/files/{*rest}"
method = "GET"
static = { dir = "public", spa_fallback = "index.html" }

[[route]]
path = "/up/{*rest}"
proxy = { upstreams = ["{env.UPSTREAM}"], strip_prefix = "/up" }
`;

const FILES = {
  "unsworn.toml": CONFIG,
  "src/users.ts": null,
  "src/auth.ts": null,
  "public/index.html": null,
};

function stream(text) {
  const bytes = new TextEncoder().encode(text);
  return new ReadableStream({
    start(c) {
      c.enqueue(bytes);
      c.close();
    },
  });
}

async function readAll(body) {
  if (body == null) return new Uint8Array();
  if (body instanceof Uint8Array) return body;
  return new Uint8Array(await new Response(body).arrayBuffer());
}

let count = 0;
const handlers = {
  "src/auth.ts#tenant": (req) => ({ type: "continue", context: { tenant: req.headers["x-tenant"] ?? "none" } }),
  "src/users.ts#get": (req) => ({
    type: "response",
    response: { status: 200, json: { id: req.params.id, tenant: req.context.tenant, rid: req.id } },
  }),
  "src/users.ts#create": (req) => ({ type: "response", response: { status: 201, json: req.json } }),
  "src/users.ts#upload": async (_req, body) => {
    const text = new TextDecoder().decode(await readAll(body));
    return { type: "response", response: { status: 200, headers: { "x-len": String(text.length) } }, body: stream(text.toUpperCase()) };
  },
  "src/users.ts#count": () => ({ type: "response", response: { status: 200, text: `call ${++count}` } }),
};

function bridge() {
  const buckets = new Map();
  const cache = new Map();
  const background = [];
  const b = {
    background,
    proxied: [],
    async call(request, body) {
      const req = JSON.parse(request);
      const h = handlers[req.handler];
      if (!h) return { type: "missing", message: req.handler };
      return h(req, body);
    },
    async readBody(body, max) {
      const bytes = await readAll(body);
      return bytes.length > max ? null : bytes;
    },
    limitBody: (body) => body,
    async collect(body, limit) {
      const bytes = await readAll(body);
      return bytes.length <= limit ? { bytes } : { body: new Response(bytes).body };
    },
    async fetch() {
      return { error: "connect", message: "no network in tests" };
    },
    async proxy(req) {
      b.proxied.push(req);
      return { status: 200, headers: [["x-up", "1"], ["connection", "close"]], body: new TextEncoder().encode("from upstream") };
    },
    async asset(path) {
      if (path === "/public/index.html") return { status: 200, headers: [["content-type", "text/html"]], body: new TextEncoder().encode("<h1>app</h1>") };
      return null;
    },
    async rateLimit(key, requests, perMs) {
      const s = buckets.get(key);
      const [tokens, lastMs, waitMs] = takeToken(s?.[0], s?.[1], Date.now(), requests, perMs);
      buckets.set(key, [tokens, lastMs]);
      return waitMs < 0 ? null : waitMs;
    },
    async cacheGet(key) {
      return cache.get(key) ?? null;
    },
    async cachePut(key, value) {
      cache.set(key, value.slice());
    },
    waitUntil(p) {
      background.push(p);
    },
    sleep: (ms) => new Promise((r) => setTimeout(r, ms)),
  };
  return b;
}

const app = new Unsworn("unsworn.toml", FILES, { UPSTREAM: "http://upstream.test" }, false);
const shared = bridge();

async function send(method, path, { headers = [], body = null, b = shared } = {}) {
  const res = await app.handle(
    { method, url: `https://example.com${path}`, headers, body, ip: "203.0.113.9", requestId: "req-1" },
    b,
  );
  await Promise.all(b.background);
  const bytes = await readAll(res.body);
  const text = new TextDecoder().decode(bytes);
  const header = (name) => res.headers.filter(([k]) => k === name).map(([, v]) => v).join(", ") || undefined;
  return { ...res, text, header, json: () => JSON.parse(text) };
}

test("loads a project and reports errors", () => {
  assert.equal(version(), JSON.parse(readFileSync(new URL("../../../sdk/js/package.json", import.meta.url))).version);
  assert.deepEqual(app.handlers, ["src/auth.ts#tenant", "src/users.ts#count", "src/users.ts#create", "src/users.ts#get", "src/users.ts#upload"]);
  assert.throws(() => new Unsworn("unsworn.toml", FILES, {}, false), /UPSTREAM/);
});

test("answers fixed routes and unknown paths", async () => {
  const r = await send("GET", "/health");
  assert.equal(r.status, 200);
  assert.deepEqual(r.json(), { ok: true });
  assert.equal(r.route, "/health");
  assert.equal(r.header("x-request-id"), "req-1");
  assert.equal((await send("GET", "/nope")).status, 404);
});

test("calls handlers with validated input and before context", async () => {
  const r = await send("GET", "/users/7", { headers: [["x-tenant", "acme"]] });
  assert.equal(r.status, 200, r.text);
  assert.deepEqual(r.json(), { id: 7, tenant: "acme", rid: "req-1" });
  assert.equal((await send("GET", "/users/abc")).status, 400);

  const json = [["content-type", "application/json"]];
  const created = await send("POST", "/users", { headers: json, body: stream('{"name":"Ada"}') });
  assert.equal(created.status, 201);
  assert.deepEqual(created.json(), { name: "Ada" });
  assert.equal((await send("POST", "/users", { headers: json, body: stream("{}") })).status, 422);
});

test("streams bodies both ways", async () => {
  const r = await send("POST", "/upload", { body: stream("hello") });
  assert.equal(r.status, 200, r.text);
  assert.equal(r.text, "HELLO");
  assert.equal(r.header("x-len"), "5");
});

test("rate limits through the bridge", async () => {
  const b = bridge();
  assert.equal((await send("GET", "/limited", { b })).status, 200);
  assert.equal((await send("GET", "/limited", { b })).status, 200);
  const r = await send("GET", "/limited", { b });
  assert.equal(r.status, 429);
  assert.ok(Number(r.header("retry-after")) > 0);
});

test("caches responses through the bridge", async () => {
  const b = bridge();
  assert.equal((await send("GET", "/cached", { b })).text, "call 1");
  const again = await send("GET", "/cached", { b });
  assert.equal(again.text, "call 1");
  assert.ok(again.header("age") !== undefined);
});

test("serves assets with a fallback", async () => {
  const r = await send("GET", "/files/deep/link");
  assert.equal(r.status, 200);
  assert.equal(r.text, "<h1>app</h1>");
  assert.equal((await send("GET", "/files/../unsworn.toml")).status, 404);
});

test("proxies with rewritten headers", async () => {
  const b = bridge();
  const r = await send("GET", "/up/a?b=1", { b, headers: [["host", "example.com"]] });
  assert.equal(r.status, 200);
  assert.equal(r.text, "from upstream");
  assert.equal(r.header("connection"), undefined);
  const sent = b.proxied[0];
  assert.equal(sent.url, "http://upstream.test/a?b=1");
  const h = Object.fromEntries(sent.headers);
  assert.equal(h["x-forwarded-for"], "203.0.113.9");
  assert.equal(h["x-forwarded-proto"], "https");
  assert.equal(h["x-forwarded-host"], "example.com");
});
