// Starts a project under `wrangler dev` (workerd, Cloudflare's runtime) for
// the tests, with a local upstream for proxy routes.

import { spawn } from "node:child_process";
import { cpSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join, relative } from "node:path";

const here = new URL(".", import.meta.url).pathname;
const sdk = join(here, "../../sdk/js/src/cloudflare.ts");

function walk(dir, base = dir) {
  return readdirSync(dir).flatMap((name) => {
    const full = join(dir, name);
    if (name === ".build" || name === "node_modules") return [];
    return statSync(full).isDirectory() ? walk(full, base) : [relative(base, full)];
  });
}

/** Bundles `dir` the way `unsworn build --target cloudflare` does. */
export function bundle(dir) {
  const out = join(dir, ".build");
  rmSync(out, { recursive: true, force: true });
  mkdirSync(join(out, "assets"), { recursive: true });
  const files = {};
  const modules = [];
  for (const file of walk(dir)) {
    if (file.endsWith(".ts")) {
      files[file] = null;
      modules.push(file);
    } else if (file.startsWith("public/")) {
      files[file] = null;
    } else {
      files[file] = readFileSync(join(dir, file), "utf8");
    }
  }
  cpSync(join(dir, "public"), join(out, "assets/public"), { recursive: true });
  const imports = modules.map((m, i) => `import * as m${i} from ${JSON.stringify(`../${m}`)};`).join("\n");
  const map = modules.map((m, i) => `${JSON.stringify(m)}: m${i}`).join(", ");
  writeFileSync(
    join(out, "worker.js"),
    `import { createWorker, UnswornRateLimiter } from ${JSON.stringify(sdk)};
${imports}

export default createWorker({
  config: "unsworn.toml",
  files: ${JSON.stringify(files)},
  modules: { ${map} },
});
export { UnswornRateLimiter };
`,
  );
  writeFileSync(
    join(out, "wrangler.jsonc"),
    JSON.stringify(
      {
        name: "unsworn-test",
        main: "worker.js",
        compatibility_date: "2026-10-01",
        compatibility_flags: ["nodejs_compat"],
        assets: {
          directory: "assets",
          binding: "ASSETS",
          run_worker_first: true,
          html_handling: "none",
          not_found_handling: "none",
        },
        durable_objects: { bindings: [{ name: "UNSWORN_RATE_LIMITER", class_name: "UnswornRateLimiter" }] },
        migrations: [{ tag: "v1", new_sqlite_classes: ["UnswornRateLimiter"] }],
      },
      null,
      2,
    ),
  );
  return join(out, "wrangler.jsonc");
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

/** Runs `wrangler dev` on a config until `stop()`. */
export async function start(config, vars = {}) {
  const port = 20000 + Math.floor(Math.random() * 20000);
  const persist = mkdtempSync(join(tmpdir(), "unsworn-wrangler-"));
  const args = ["wrangler", "dev", "--config", config, "--ip", "127.0.0.1", "--port", String(port)];
  args.push("--persist-to", persist, "--log-level", "warn", "--show-interactive-dev-session=false");
  for (const [k, v] of Object.entries(vars)) args.push("--var", `${k}:${v}`);
  const child = spawn("npx", args, {
    cwd: here,
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
