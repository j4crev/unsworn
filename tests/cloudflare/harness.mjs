// Starts a project under `wrangler dev` (workerd, Cloudflare's runtime) for
// the tests, with a local upstream for proxy routes.

import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, symlinkSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";

const here = new URL(".", import.meta.url).pathname;
const repo = join(here, "../..");

/**
 * Builds `dir` with `unsworn build --target cloudflare`, linking the SDK
 * from this repository as the project's @unswornjs/sdk.
 */
export function build(dir) {
  const modules = join(dir, "node_modules/@unswornjs");
  mkdirSync(modules, { recursive: true });
  rmSync(join(modules, "sdk"), { recursive: true, force: true });
  symlinkSync(join(repo, "sdk/js"), join(modules, "sdk"));
  const bin = process.env.UNSWORN_BIN;
  const [cmd, args] = bin ? [bin, []] : ["cargo", ["run", "-q", "-p", "unsworn", "--"]];
  const result = spawnSync(cmd, [...args, "build", "--target", "cloudflare", "-C", dir], {
    cwd: repo,
    encoding: "utf8",
  });
  if (result.status !== 0) throw new Error(`unsworn build failed:\n${result.stdout}${result.stderr}`);
  return { dir, output: result.stdout + result.stderr };
}

/** A local upstream that echoes what it received. */
export async function upstream() {
  const server = createServer((req, res) => {
    res.setHeader("content-type", "application/json");
    res.setHeader("connection", "close");
    res.end(JSON.stringify({ url: req.url, headers: req.headers }));
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return { url: `http://127.0.0.1:${server.address().port}`, close: () => server.close() };
}

/**
 * Runs `wrangler dev` in a built project until `stop()`. Wrangler finds the
 * generated config through `.wrangler/deploy/config.json`, as it does for
 * users.
 */
export async function start(dir, vars = {}) {
  const port = 20000 + Math.floor(Math.random() * 20000);
  const persist = mkdtempSync(join(tmpdir(), "unsworn-wrangler-"));
  const wrangler = join(here, "node_modules/.bin/wrangler");
  const args = ["dev", "--ip", "127.0.0.1", "--port", String(port)];
  args.push("--persist-to", persist, "--log-level", "warn", "--show-interactive-dev-session=false");
  for (const [k, v] of Object.entries(vars)) args.push("--var", `${k}:${v}`);
  const child = spawn(wrangler, args, {
    cwd: dir,
    env: { ...process.env, WRANGLER_SEND_METRICS: "false", CI: "1" },
    stdio: ["ignore", "pipe", "pipe"],
  });
  let output = "";
  child.stdout.on("data", (d) => (output += d));
  child.stderr.on("data", (d) => (output += d));
  const base = `http://127.0.0.1:${port}`;
  const deadline = Date.now() + 60_000;
  for (;;) {
    if (child.exitCode !== null) throw new Error(`wrangler dev exited:\n${output}`);
    if (Date.now() > deadline) {
      child.kill();
      throw new Error(`wrangler dev didn't start:\n${output}`);
    }
    try {
      await fetch(`${base}/health`);
      break;
    } catch {
      await new Promise((r) => setTimeout(r, 250));
    }
  }
  return {
    base,
    output: () => output,
    stop() {
      child.kill();
      rmSync(persist, { recursive: true, force: true });
    },
  };
}
