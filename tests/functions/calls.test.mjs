// Functions, outbound services and shadows where handlers run in the
// core's own process: the Node host under the serverless targets. The core
// applies the rules the server does (unsworn_core::calls and ::outbound).
// Needs the wasm core built (crates/unsworn-wasm/build.sh).
//
//   node --test tests/functions/calls.test.mjs

import assert from "node:assert/strict";
import { createServer } from "node:http";
import { after, before, test } from "node:test";

import { baseBridge } from "../../sdk/js/src/host.ts";
import { createHandler } from "../../sdk/js/src/node.ts";

const TRACE = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

const config = `
[server]
response_validation = "enforce"

[runtimes.ts]
extensions = [".ts"]
command = "bun run node_modules/@unswornjs/sdk/src/worker.ts"

[outbound.billing]
url = "{env.SERVICE}/api"
headers = { Authorization = "Bearer {env.BILLING_KEY}", "X-Account" = "acme" }
methods = ["GET", "POST"]
paths = ["/charges", "/charges/{id}", "/moved", "/big", "/slow", "/zipped"]
timeout = "300ms"
max_response = "1KB"

# With the default time limit, which is the request's own.
[outbound.open]
url = "{env.SERVICE}"

[[function]]
name = "count"
handler = "src/fn.ts#count"
input = { type = "object", required = ["text"], properties = { text = { type = "string" } } }
output = { type = "object", required = ["words"], properties = { words = { type = "integer" } } }

[[function]]
name = "whoami"
handler = "src/fn.ts#whoami"

[[function]]
name = "relay"
handler = "src/fn.ts#relay"

[[function]]
name = "loop"
handler = "src/fn.ts#loop"

[[function]]
name = "lies"
handler = "src/fn.ts#lies"
output = { type = "object", required = ["words"] }

[[function]]
name = "sleepy"
handler = "src/fn.ts#sleepy"
timeout = "100ms"

[[function]]
name = "refuse"
handler = "src/fn.ts#refuse"

[[function]]
name = "says"
handler = "src/fn.ts#says"

[[function]]
name = "streams"
handler = "src/fn.ts#streams"

[[function]]
name = "charge"
handler = "src/fn.ts#charge"

[[route]]
path = "/do/{what}"
handler = "src/routes.ts#route"
before = ["src/routes.ts#tenant"]

[[route]]
path = "/search"
method = "GET"
handler = "src/routes.ts#search"
shadow = "src/routes.ts#searchV2"
`;

const functions = {
  count: (req) => ({ json: { words: String(req.json.text).split(" ").length } }),
  whoami: (req, ctx) => ({ json: { ctx, route: req.route, headers: req.headers } }),
  relay: async (req, ctx) => ({ json: { relayed: await ctx.call("whoami") } }),
  loop: async (_req, ctx) => ({ json: await ctx.call("loop") }),
  lies: () => ({ json: { nothing: true } }),
  sleepy: async () => {
    await new Promise((resolve) => setTimeout(resolve, 2000));
    return { json: { woke: true } };
  },
  refuse: () => ({ status: 409, json: { error: "not now" } }),
  says: () => ({ text: "hello" }),
  streams: () => ({ body: new ReadableStream({ start: (c) => c.close() }) }),
  // A function reaches a service for the request it was called for.
  charge: async (_req, ctx) => ({ json: (await ctx.fetch("billing", "/charges")).json() }),
};

const failed = (err) => ({ failed: err.status, name: err.name, message: err.message, json: err.json ?? null });

const routes = {
  tenant: () => ({ continue: { tenant: "acme" } }),
  async route(req, ctx) {
    const what = req.params.what;
    try {
      if (what === "call") return { json: await ctx.call(req.query.fn, req.json) };
      // The body is the call's options.
      if (what === "own") return { json: await ctx.call("whoami", null, req.json) };
      if (what === "both") {
        const [a, b] = await Promise.all([ctx.call("count", { text: "a b" }), ctx.call("count", { text: "a b c" })]);
        return { json: { a, b } };
      }
      if (what === "fetch") {
        const { service, path, ...init } = req.json;
        const r = await ctx.fetch(service, path, init);
        return { json: { status: r.status, ok: r.ok, headers: r.headers, text: r.text() } };
      }
      // The context is still plain data.
      if (what === "ctx") return { json: { keys: Object.keys(ctx), text: JSON.stringify(ctx) } };
    } catch (err) {
      return { json: failed(err) };
    }
    return { status: 404 };
  },
  search: (req) => ({ json: { hits: [req.query.q], total: 1 } }),
  searchV2: (req) => ({ json: { total: 1.0, hits: [req.query.q === "bug" ? "WRONG" : req.query.q] } }),
};

let app;
let service;
const seen = [];

before(async () => {
  // A service that says what it was sent.
  service = createServer((req, res) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      const request = { method: req.method, url: req.url, headers: req.headers, body: Buffer.concat(chunks).toString() };
      seen.push(request);
      res.setHeader("x-service", "yes");
      if (req.url.startsWith("/api/moved")) {
        res.writeHead(302, { location: "/api/charges" }).end();
      } else if (req.url.startsWith("/api/big")) {
        res.end("x".repeat(4096));
      } else if (req.url.startsWith("/api/slow")) {
        setTimeout(() => res.end("late"), 1000);
      } else {
        res.setHeader("content-type", "application/json");
        res.end(JSON.stringify(request));
      }
    });
  });
  await new Promise((resolve) => service.listen(0, "127.0.0.1", resolve));
  const env = { SERVICE: `http://127.0.0.1:${service.address().port}`, BILLING_KEY: "sk_live_s3cret" };
  const bundle = {
    config: "unsworn.toml",
    files: { "unsworn.toml": config, "src/fn.ts": null, "src/routes.ts": null },
    modules: { "src/fn.ts": functions, "src/routes.ts": routes },
  };
  app = createHandler(bundle, { env });
});

after(() => {
  service?.closeAllConnections();
  service?.close();
});

const json = { "content-type": "application/json" };
const ask = async (path, body, headers = {}) => {
  const init = body === undefined ? { headers } : { method: "POST", headers: { ...json, ...headers }, body: JSON.stringify(body) };
  const r = await app.fetch(new Request(`https://api.example.com${path}`, init), { ip: "203.0.113.7" });
  assert.equal(r.status, 200, await r.clone().text());
  return r.json();
};
const call = (fn, input, headers) => ask(`/do/call?fn=${fn}`, input ?? null, headers);
const fetched = (init, headers) => ask("/do/fetch", { service: "billing", ...init }, headers);

test("a handler calls a function, and a function calls a function", async () => {
  assert.deepEqual(await call("count", { text: "one two three" }), { words: 3 });
  assert.deepEqual(await ask("/do/both"), { a: { words: 2 }, b: { words: 3 } });
  // A text answer arrives as a JSON string.
  assert.equal(await call("says"), "hello");
  const relayed = (await call("relay")).relayed;
  assert.deepEqual(relayed.ctx.caller, { function: "whoami", from: "src/fn.ts#relay", depth: 2 });
});

test("a function gets the caller's context and trace, and says who called", async () => {
  const who = await call("whoami", null, { traceparent: TRACE, tracestate: "vendor=1" });
  // What the route's `before` handler added, which the caller didn't send.
  assert.equal(who.ctx.tenant, "acme");
  assert.deepEqual(who.ctx.caller, { function: "whoami", from: "src/routes.ts#route", depth: 1 });
  assert.equal(who.route, "fn:whoami");
  assert.deepEqual(who.headers, { traceparent: TRACE, tracestate: "vendor=1" });
  assert.deepEqual((await call("whoami")).headers, {});

  const ctx = await ask("/do/ctx");
  assert.deepEqual(ctx.keys, ["tenant"]);
  assert.equal(ctx.text, '{"tenant":"acme"}');
});

test("a handler names its own span for a function to be called under", async () => {
  const OWN = "00-4bf92f3577b34da6a3ce929d0e0e4736-b7ad6b7169203331-01";
  const request = { traceparent: TRACE, tracestate: "vendor=1" };
  const under = async (options) => (await ask("/do/own", options, request)).headers;
  assert.deepEqual(await under({ traceparent: OWN, tracestate: "mine=1" }), { traceparent: OWN, tracestate: "mine=1" });
  // The request's `tracestate` doesn't go with another `traceparent`.
  assert.deepEqual(await under({ traceparent: OWN }), { traceparent: OWN });
  // One a receiver wouldn't accept is ignored, as is a `tracestate` alone.
  assert.deepEqual(await under({ traceparent: "nonsense", tracestate: "mine=1" }), request);
  assert.deepEqual(await under({ tracestate: "mine=1" }), request);
});

test("calls that fail say why, as on the server", async () => {
  const bad = await call("count", { text: 5 });
  assert.equal(bad.failed, 422);
  assert.equal(bad.name, "CallError");
  assert.match(bad.message, /the input doesn't match the schema of the function "count"/);
  assert.equal(bad.json.errors[0].location, "input.text");

  const missing = await call("nope");
  assert.equal(missing.failed, 404);
  assert.equal(
    missing.message,
    'no [[function]] named "nope" in the config (it declares: charge, count, lies, loop, refuse, relay, says, sleepy, streams, whoami)',
  );

  assert.equal((await call("lies")).failed, 502);
  assert.match((await call("lies")).message, /the output of the function "lies" doesn't match its schema/);
  assert.deepEqual([(await call("sleepy")).failed, (await call("sleepy")).message], [504, "the function timed out"]);
  // A function that calls itself is stopped. The innermost call gets a
  // 508; each function above it fails for not handling that, which is a
  // 500 carrying the reason.
  const looped = await call("loop");
  assert.equal(looped.failed, 500);
  assert.match(looped.message, /nested more than 16 deep/);
  // A function's own error status, with what it answered.
  const refused = await call("refuse");
  assert.deepEqual([refused.failed, refused.json], [409, { error: "not now" }]);
  assert.match((await call("streams")).message, /a function answers with JSON/);
});

test("Unsworn adds a service's credential, and the request's trace", async () => {
  const got = await fetched(
    {
      path: "/charges?limit=2",
      json: { amount: 1200 },
      // A handler can't replace what the service sets.
      headers: { Authorization: "Bearer mine", "X-Trace": "t1", Host: "evil.example" },
    },
    { traceparent: TRACE },
  );
  assert.equal(got.status, 200);
  assert.equal(got.ok, true);
  assert.equal(got.headers["x-service"], "yes");
  const sent = JSON.parse(got.text);
  assert.equal(sent.method, "POST");
  assert.equal(sent.url, "/api/charges?limit=2");
  assert.equal(sent.body, '{"amount":1200}');
  assert.equal(sent.headers.authorization, "Bearer sk_live_s3cret");
  assert.equal(sent.headers["x-account"], "acme");
  assert.equal(sent.headers["x-trace"], "t1");
  assert.equal(sent.headers["content-type"], "application/json");
  assert.equal(sent.headers.host, `127.0.0.1:${service.address().port}`);
  assert.equal(sent.headers.traceparent, TRACE);

  // A service with the default time limit, and no headers of its own.
  const open = await ask("/do/fetch", { service: "open", path: "/anything" });
  assert.equal(open.status, 200, JSON.stringify(open));
  assert.equal(JSON.parse(open.text).headers.authorization, undefined);

  // A plain GET, and a function reaching the service for its caller.
  assert.equal(JSON.parse((await fetched({ path: "/charges/42" })).text).method, "GET");
  const viaFunction = await call("charge", null, { traceparent: TRACE });
  assert.equal(viaFunction.headers.authorization, "Bearer sk_live_s3cret");
  assert.equal(viaFunction.headers.traceparent, TRACE);
});

test("only what the config allows is sent, and a service's own answers come back", async () => {
  const before = seen.length;
  const refused = await fetched({ path: "/admin" });
  assert.equal(refused.failed, 403);
  assert.equal(refused.name, "FetchError");
  assert.equal((await fetched({ path: "/charges", method: "DELETE" })).failed, 403);
  assert.equal((await fetched({ path: "/charges/../admin" })).failed, 403);
  const unknown = await ask("/do/fetch", { service: "stripe", path: "/" });
  assert.deepEqual(
    [unknown.failed, unknown.message],
    [404, "no [outbound.stripe] in the config (it declares: billing, open)"],
  );
  assert.equal(seen.length, before, "nothing was sent");

  // A redirect is the answer. Following it could take the credential elsewhere.
  const moved = await fetched({ path: "/moved" });
  assert.deepEqual([moved.status, moved.ok, moved.headers.location], [302, false, "/api/charges"]);
  assert.equal(seen.length, before + 1);

  const big = await fetched({ path: "/big" });
  assert.deepEqual([big.failed, big.message], [502, "the response is larger than [outbound.billing].max_response"]);
  const slow = await fetched({ path: "/slow" });
  assert.deepEqual([slow.failed, slow.message], [504, "the service took too long to answer"]);
});

test("a shadow gets a copy, and only a difference is logged", async () => {
  const warnings = [];
  const warn = console.warn;
  console.warn = (...args) => warnings.push(args.join(" "));
  try {
    // Another key order and `1.0` for `1` are the same answer.
    const same = await ask("/search?q=cats");
    assert.deepEqual(same, { hits: ["cats"], total: 1 });
    await app.idle();
    assert.deepEqual(warnings, []);

    // A wrong answer from the shadow: the client still gets the right one.
    assert.deepEqual(await ask("/search?q=bug"), { hits: ["bug"], total: 1 });
    await app.idle();
    assert.equal(warnings.length, 1, warnings.join("\n"));
    assert.match(warnings[0], /shadow route=\/search shadow=src\/routes\.ts#searchV2 id=\S+: \$\.hits\[0\] is "bug", and "WRONG" in the shadow's answer/);
  } finally {
    console.warn = warn;
  }
});

test("a time limit that isn't a whole number of milliseconds", async () => {
  // What's left of a request rarely is one, and Node 22, which Vercel
  // and Lambda run, refuses anything else: found on Vercel, where every
  // `ctx.fetch` failed with "couldn't reach the service".
  const timeout = AbortSignal.timeout;
  const asked = [];
  AbortSignal.timeout = (ms) => {
    asked.push(ms);
    return timeout.call(AbortSignal, ms);
  };
  try {
    const url = `http://127.0.0.1:${service.address().port}/api/charges`;
    const bridge = baseBridge({ config: "", files: {}, modules: {} }, false);
    const answer = await bridge.fetch({ method: "GET", url, headers: [], body: null }, 29997.999907, 1024 * 1024);
    assert.equal(answer.status, 200, JSON.stringify(answer));
    assert.deepEqual(asked, [29997]);
  } finally {
    AbortSignal.timeout = timeout;
  }
});
