// A serverless cold start, minus the platform: one fresh Node process loads
// the wasm core through the Node host (sdk/js/src/node.ts), loads a project
// of N routes and answers its first request. Then it times warm requests to
// that route, and the first request to other routes, which compiles their
// validation schemas. run.py starts this many times and takes the median.
//
//   node bench/coldstart.mjs <routes> [warm-requests]
//
// Prints one JSON line. Times are milliseconds since the process started,
// except where the name says otherwise.

const start = performance.now(); // roughly the time Node took to start
const routes = Number(process.argv[2] ?? 10);
const warm = Number(process.argv[3] ?? 1000);

const { createHandler } = await import("../sdk/js/src/node.ts");
const imported = performance.now();

// Every route has a handler, a path parameter and a body schema. The
// handler is bundled in, as `unsworn build` does for a function, so the
// runtime's command is never run.
const schema = `{ type = "object", required = ["name"], properties = { name = { type = "string", maxLength = 100 }, count = { type = "integer", minimum = 0 } } }`;
let config = `[runtimes.ts]\nextensions = [".ts"]\ncommand = "unused"\n`;
for (let i = 0; i < routes; i++) {
  config += `\n[[route]]\npath = "/r${i}/items/{id}"\nmethod = "POST"\nparams = { id = { type = "integer", min = 1 } }\nbody = ${schema}\nhandler = "src/app.ts#ok"\n`;
}
const modules = { "src/app.ts": { ok: () => ({ json: { ok: true } }) } };
const files = { "unsworn.toml": config, "src/app.ts": null };

const app = createHandler({ config: "unsworn.toml", files, modules }, { env: {} });

const post = async (i) => {
  const r = await app.fetch(
    new Request(`https://bench.example/r${i}/items/42`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: '{"name":"widget","count":3}',
    }),
    { ip: "203.0.113.7" },
  );
  if (r.status !== 200) throw new Error(`/r${i}: ${r.status} ${await r.text()}`);
  await r.arrayBuffer();
};

// The project and the wasm module load on the first request.
await post(0);
const answered = performance.now();

let t = performance.now();
for (let n = 0; n < warm; n++) await post(0);
const warmUs = warm ? ((performance.now() - t) * 1000) / warm : null;

const others = Math.min(routes - 1, 20);
t = performance.now();
for (let i = 1; i <= others; i++) await post(i);
const firstUseMs = others ? (performance.now() - t) / others : null;

console.log(
  JSON.stringify({
    routes,
    node_start_ms: start,
    import_ms: imported - start,
    first_request_ms: answered - imported,
    total_ms: answered,
    warm_us_per_request: warmUs,
    first_use_ms_per_route: firstUseMs,
  }),
);
