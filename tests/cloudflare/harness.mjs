// Starts a project under `wrangler dev` (workerd, Cloudflare's runtime) for
// the tests, with a local upstream for proxy routes.

import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
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

/**
 * A local upstream that echoes what it received. WebSocket upgrades get the
 * request as their first message, then an echo of each message.
 */
export async function upstream() {
  const server = createServer((req, res) => {
    res.setHeader("content-type", "application/json");
    res.setHeader("connection", "close");
    res.end(JSON.stringify({ url: req.url, headers: req.headers }));
  });
  server.on("upgrade", echoWebSocket);
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

// A WebSocket server just big enough for the tests: unfragmented frames,
// no extensions.
function echoWebSocket(req, socket) {
  const accept = createHash("sha1")
    .update(req.headers["sec-websocket-key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
    .digest("base64");
  const protocol = req.headers["sec-websocket-protocol"]?.split(",")[0].trim();
  const head = ["HTTP/1.1 101 Switching Protocols", "Upgrade: websocket", "Connection: Upgrade"];
  head.push(`Sec-WebSocket-Accept: ${accept}`);
  if (protocol) head.push(`Sec-WebSocket-Protocol: ${protocol}`);
  socket.write(head.join("\r\n") + "\r\n\r\n");

  const send = (opcode, payload) => {
    const len = payload.length;
    const header =
      len < 126
        ? Buffer.from([0x80 | opcode, len])
        : len < 65536
          ? Buffer.from([0x80 | opcode, 126, len >> 8, len & 0xff])
          : Buffer.concat([Buffer.from([0x80 | opcode, 127]), bigEndian64(len)]);
    socket.write(Buffer.concat([header, payload]));
  };
  send(1, Buffer.from(JSON.stringify({ url: req.url, headers: req.headers })));

  let buf = Buffer.alloc(0);
  socket.on("error", () => {});
  socket.on("data", (data) => {
    buf = Buffer.concat([buf, data]);
    for (;;) {
      if (buf.length < 2) return;
      const opcode = buf[0] & 0x0f;
      let len = buf[1] & 0x7f;
      let at = 2;
      if (len === 126) {
        if (buf.length < 4) return;
        len = buf.readUInt16BE(2);
        at = 4;
      } else if (len === 127) {
        if (buf.length < 10) return;
        len = Number(buf.readBigUInt64BE(2));
        at = 10;
      }
      const masked = buf[1] & 0x80;
      const mask = masked ? buf.subarray(at, at + 4) : null;
      if (masked) at += 4;
      if (buf.length < at + len) return;
      const payload = Buffer.from(buf.subarray(at, at + len));
      if (mask) for (let i = 0; i < len; i++) payload[i] ^= mask[i % 4];
      buf = buf.subarray(at + len);
      if (opcode === 1) send(1, Buffer.concat([Buffer.from("echo:"), payload]));
      else if (opcode === 2) send(2, payload);
      else if (opcode === 9) send(10, payload);
      else if (opcode === 8) {
        send(8, payload);
        socket.end();
        return;
      }
    }
  });
}

function bigEndian64(n) {
  const b = Buffer.alloc(8);
  b.writeBigUInt64BE(BigInt(n));
  return b;
}
