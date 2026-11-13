// Conformance handlers for the JS/TS SDK. See docs/conformance.md.

import { type Before, type Handler, next } from "../src/index.ts";

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
