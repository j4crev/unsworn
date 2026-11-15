// The Cloudflare host end to end: a project built with `unsworn build
// --target cloudflare` and run in workerd. Needs the wasm core built
// (crates/unsworn-wasm/build.sh) and `npm install` here. Set UNSWORN_BIN
// to use a built binary instead of `cargo run`.

import assert from "node:assert/strict";
import { after, before, test } from "node:test";

import { build, start, upstream } from "./harness.mjs";

let worker;
let up;

before(async () => {
  up = await upstream();
  const { dir } = build(new URL("./fixture", import.meta.url).pathname);
  worker = await start(dir, { UPSTREAM: up.url, UNSWORN_DEV: "1" });
});

after(() => {
  worker?.stop();
  up?.close();
});

const get = (path, init) => fetch(worker.base + path, init);

test("fixed routes and 404s", async () => {
  const r = await get("/health");
  assert.equal(r.status, 200);
  assert.deepEqual(await r.json(), { ok: true });
  assert.ok(r.headers.get("x-request-id"));
  assert.equal((await get("/nope")).status, 404);
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

test("fetch() apps get the Worker's env", async () => {
  const r = await get("/app/x");
  assert.equal(await r.text(), "app /app/x hello");
  assert.equal(r.headers.get("x-app"), "1");
});

test("rate limits count in a Durable Object", async () => {
  assert.equal((await get("/limited")).status, 200);
  assert.equal((await get("/limited")).status, 200);
  const r = await get("/limited");
  assert.equal(r.status, 429);
  assert.ok(Number(r.headers.get("retry-after")) > 0);
});

test("responses are cached in the Cache API", async () => {
  const first = await (await get("/counter")).text();
  const second = await get("/counter");
  assert.equal(await second.text(), first);
  assert.ok(second.headers.get("age") !== null);
});

test("static assets, with index and fallback", async () => {
  const css = await get("/site/style.css");
  assert.equal(css.status, 200);
  assert.equal(await css.text(), "body { color: red }\n");
  assert.equal(css.headers.get("cache-control"), "public, max-age=60");
  assert.equal(await (await get("/site/docs/")).text(), "docs index\n");
  assert.equal(await (await get("/site/docs")).text(), "docs index\n");
  assert.equal(await (await get("/site/some/route")).text(), "<h1>spa</h1>\n");
});

test("proxy routes forward with rewritten headers", async () => {
  const r = await get("/up/a?b=1");
  assert.equal(r.status, 200);
  assert.equal(r.headers.get("connection"), null);
  const seen = await r.json();
  assert.equal(seen.url, "/a?b=1");
  assert.equal(seen.headers["x-forwarded-proto"], "http");
  assert.ok(seen.headers["x-forwarded-for"]);
  assert.ok(seen.headers["x-request-id"]);
});

test("proxy routes pass WebSockets through", async () => {
  const ws = new WebSocket(`${worker.base.replace("http", "ws")}/up/chat?room=1`, ["v2.chat", "v1.chat"]);
  ws.binaryType = "arraybuffer";
  const messages = [];
  let waiting;
  ws.onmessage = (e) => {
    messages.push(e.data);
    waiting?.();
  };
  const next = async () => {
    while (messages.length === 0) await new Promise((r) => (waiting = r));
    return messages.shift();
  };
  const closed = new Promise((resolve) => (ws.onclose = resolve));
  await new Promise((resolve, reject) => {
    ws.onopen = resolve;
    ws.onerror = () => reject(new Error("the WebSocket didn't open"));
  });
  assert.equal(ws.protocol, "v2.chat");

  const seen = JSON.parse(await next());
  assert.equal(seen.url, "/chat?room=1");
  assert.equal(seen.headers["x-forwarded-proto"], "http");
  assert.ok(seen.headers["x-forwarded-for"]);
  assert.ok(seen.headers["x-request-id"]);

  ws.send("hi");
  assert.equal(await next(), "echo:hi");
  ws.send(new Uint8Array([1, 2, 3]));
  assert.deepEqual([...new Uint8Array(await next())], [1, 2, 3]);
  const big = "x".repeat(100_000);
  ws.send(big);
  assert.equal(await next(), "echo:" + big);

  ws.close(4001, "bye");
  const close = await closed;
  assert.equal(close.code, 4001);
  assert.equal(close.reason, "bye");
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
