// What the serverless tests share: a local upstream for proxy routes, and a
// stand-in for a Redis REST API.

import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync, symlinkSync } from "node:fs";
import { createServer } from "node:http";
import { join } from "node:path";

const here = new URL(".", import.meta.url).pathname;
const repo = join(here, "../..");

/**
 * Builds `dir` with `unsworn build --target <target>`, linking the SDK from
 * this repository as the project's @unswornjs/sdk. esbuild comes from this
 * directory's node_modules. Set UNSWORN_BIN to use a built binary instead
 * of `cargo run`.
 */
export function build(dir, target, env = {}) {
  const modules = join(dir, "node_modules/@unswornjs");
  mkdirSync(modules, { recursive: true });
  rmSync(join(modules, "sdk"), { recursive: true, force: true });
  symlinkSync(join(repo, "sdk/js"), join(modules, "sdk"));
  const bin = process.env.UNSWORN_BIN;
  const [cmd, args] = bin ? [bin, []] : ["cargo", ["run", "-q", "-p", "unsworn", "--"]];
  const result = spawnSync(cmd, [...args, "build", "--target", target, "-C", dir], {
    cwd: repo,
    encoding: "utf8",
    env: { ...process.env, ...env },
  });
  const output = result.stdout + result.stderr;
  if (result.status !== 0) throw new Error(`unsworn build failed:\n${output}`);
  return { dir, output };
}

/** Serves a Node `(req, res)` listener on a local port. */
export async function serve(listener) {
  const server = createServer(listener);
  return { base: await listen(server), close: () => server.close() };
}

async function listen(server) {
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return `http://127.0.0.1:${server.address().port}`;
}

/** A local upstream that echoes what it received. */
export async function upstream() {
  const server = createServer((req, res) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      res.setHeader("content-type", "application/json");
      res.setHeader("connection", "close");
      const body = Buffer.concat(chunks).toString();
      res.end(JSON.stringify({ method: req.method, url: req.url, headers: req.headers, body }));
    });
  });
  return { url: await listen(server), close: () => server.close() };
}

/**
 * A Redis REST API in Upstash's protocol, with just the commands the store
 * sends. `EVAL` doesn't run Lua: it does what the rate-limit script does,
 * after checking it was given that script.
 */
export async function redisRest(token = "secret") {
  const strings = new Map();
  const buckets = new Map();
  const commands = [];

  const run = ([name, ...args]) => {
    switch (String(name).toUpperCase()) {
      case "GET": {
        const entry = strings.get(args[0]);
        return entry && entry.until > Date.now() ? entry.value : null;
      }
      case "SET": {
        const [key, value, px, ms] = args;
        if (px !== "PX") throw new Error("ERR only SET key value PX ms");
        strings.set(key, { value, until: Date.now() + Number(ms) });
        return "OK";
      }
      case "EVAL": {
        const [script, keys, key, capacity, perMs] = args;
        if (!script.includes("HMGET") || Number(keys) !== 1) throw new Error("ERR not the rate-limit script");
        const now = Date.now();
        const b = buckets.get(key) ?? { tokens: Number(capacity), ts: now };
        const rate = Number(capacity) / Number(perMs);
        let tokens = Math.min(Number(capacity), b.tokens + Math.max(0, now - b.ts) * rate);
        let wait = 0;
        if (tokens >= 1) tokens -= 1;
        else wait = Math.max(1, Math.ceil((1 - tokens) / rate));
        buckets.set(key, { tokens, ts: now });
        return wait;
      }
      default:
        throw new Error(`ERR unknown command ${name}`);
    }
  };

  const server = createServer((req, res) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => {
      res.setHeader("content-type", "application/json");
      if (req.headers.authorization !== `Bearer ${token}`) {
        res.statusCode = 401;
        return res.end(JSON.stringify({ error: "WRONGPASS invalid token" }));
      }
      try {
        const command = JSON.parse(Buffer.concat(chunks).toString());
        commands.push(command);
        res.end(JSON.stringify({ result: run(command) }));
      } catch (err) {
        res.statusCode = 400;
        res.end(JSON.stringify({ error: err.message }));
      }
    });
  });
  return {
    url: await listen(server),
    token,
    /** Every command received, as sent. */
    commands,
    /** The keys holding strings and rate-limit buckets. */
    keys: () => [...strings.keys(), ...buckets.keys()],
    close: () => server.close(),
  };
}
