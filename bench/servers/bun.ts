// The "Bun on its own" baseline: the conformance `text` handler's answer,
// served by Bun.serve. Run several with the same port to use more cores
// (reusePort), the clustering a Bun app would need for that.
//
//   bun run bench/servers/bun.ts 9002
//
// Started by Unsworn as an HTTP runtime (no arguments), it listens where
// Unsworn says: UNSWORN_PORT, or else the socket in UNSWORN_SOCKET. In a
// project with functions or outbound services it also answers like the
// conformance `relay` and `outbound` handlers, through Unsworn's local
// endpoint (UNSWORN_CALLS_SOCKET, or UNSWORN_CALLS_URL when BENCH_CALLS=url):
//
//   POST /relay?fn=<name>       {"answer": <the function's JSON>} or {"failed": <status>}
//   POST /outbound?path=<path>  the service "conformance"'s answer to the body: its body, with x-status
const env = process.env;
const socket = env.UNSWORN_PORT ? undefined : env.UNSWORN_SOCKET;
const port = Number(env.UNSWORN_PORT ?? process.argv[2] ?? 9002);

const viaUrl = env.BENCH_CALLS === "url";
const calls = viaUrl ? env.UNSWORN_CALLS_URL : env.UNSWORN_CALLS_SOCKET ? "http://unsworn" : undefined;
const callsUnix = viaUrl ? undefined : env.UNSWORN_CALLS_SOCKET;

async function call(path: string, req: Request, headers: Record<string, string>): Promise<Response> {
  return fetch(calls + path, {
    method: "POST",
    headers: { ...headers, "unsworn-call": req.headers.get("unsworn-call") ?? "" },
    body: await req.arrayBuffer(),
    // @ts-ignore: Bun's own option
    unix: callsUnix,
  });
}

const hello = () =>
  new Response("hello", {
    status: 201,
    headers: { "content-type": "text/plain; charset=utf-8", "x-conformance": "yes" },
  });

Bun.serve({
  ...(socket ? { unix: socket } : { port, hostname: "127.0.0.1", reusePort: true }),
  async fetch(req) {
    if (req.method !== "POST") return hello();
    const url = new URL(req.url);
    if (calls && url.pathname === "/relay") {
      const res = await call(`/call/${url.searchParams.get("fn")}`, req, { "content-type": "application/json" });
      const answer = await res.text();
      return new Response(res.status >= 400 ? `{"failed":${res.status}}` : `{"answer":${answer}}`, {
        headers: { "content-type": "application/json" },
      });
    }
    if (calls && url.pathname === "/outbound") {
      const res = await call(`/fetch/conformance${url.searchParams.get("path")}`, req, { "x-from": "handler" });
      return new Response(await res.arrayBuffer(), {
        headers: { "content-type": "application/octet-stream", "x-status": String(res.status) },
      });
    }
    return new Response(await req.arrayBuffer(), { headers: { "content-type": "application/octet-stream" } });
  },
});
