// The "Bun on its own" baseline: the conformance `text` handler's answer,
// served by Bun.serve. Run several with the same port to use more cores
// (reusePort), the clustering a Bun app would need for that.
//
//   bun run bench/servers/bun.ts 9002
const port = Number(process.argv[2] ?? 9002);

Bun.serve({
  port,
  hostname: "127.0.0.1",
  reusePort: true,
  async fetch(req) {
    if (req.method === "POST") {
      return new Response(await req.arrayBuffer(), { headers: { "content-type": "application/octet-stream" } });
    }
    return new Response("hello", {
      status: 201,
      headers: { "content-type": "text/plain; charset=utf-8", "x-conformance": "yes" },
    });
  },
});
