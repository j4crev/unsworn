// Conformance handlers for the JS/TS SDK. See docs/conformance.md.

import {
  type Before,
  CallError,
  FetchError,
  type Handler,
  content as contentReply,
  log,
  next,
} from "../src/index.ts";

export const echo: Handler = (req, ctx) => ({
  json: {
    method: req.method,
    params: req.params,
    query: req.query,
    context: ctx,
    json: req.json,
    header: req.headers["x-test"],
  },
});

export const text: Handler = () => ({ status: 201, text: "hello", headers: { "x-conformance": "yes" } });

export const binary: Handler = (req) => ({ body: req.body ?? new Uint8Array() });

export const status: Handler = (req) => ({ status: Number(req.query.code) });

export const stream: Handler = () => ({
  body: (async function* () {
    yield "a";
    yield "b";
    yield "c";
  })(),
});

export const fail: Handler = () => {
  throw new Error("boom");
};

export const slow: Handler = async () => {
  await new Promise((r) => setTimeout(r, 500));
  return { text: "done" };
};

export const crash: Handler = () => {
  process.exit(3);
};

export const allow: Before = () => next({ checked: true });

export const deny: Before = () => ({ status: 403, json: { error: "denied" } });

export const noisy: Handler = () => {
  console.log("this goes to stderr, not the protocol stream");
  return { text: "ok" };
};

export const log_: Handler = async () => {
  await Promise.resolve(); // the request stays attached across awaits
  log.info("conformance log", { n: 1 });
  return { status: 200 };
};
export { log_ as log };

export const upload: Handler = async (req) => ({
  body: new Uint8Array(await new Response(req.stream).arrayBuffer()),
});

export const content: Handler = () => contentReply([{ type: "text", text: "hi" }], { ok: true });

export const relay: Handler = async (req, ctx) => {
  try {
    return { json: { answer: await ctx.call(String(req.query.fn), req.json) } };
  } catch (err) {
    if (err instanceof CallError) return { json: { failed: err.status } };
    throw err;
  }
};

export const outbound: Handler = async (req, ctx) => {
  try {
    const answer = await ctx.fetch("conformance", String(req.query.path), {
      method: "POST",
      headers: { "x-from": "handler" },
      body: req.body,
    });
    return { body: answer.body, headers: { "x-status": String(answer.status) } };
  } catch (err) {
    if (err instanceof FetchError) return { json: { failed: err.status } };
    throw err;
  }
};

export const mcp: Handler = (_req, ctx) => ({
  json: {
    tool: ctx.mcp?.tool,
    protocol_version: ctx.mcp?.protocol_version,
    client: ctx.mcp?.client?.name,
  },
});
