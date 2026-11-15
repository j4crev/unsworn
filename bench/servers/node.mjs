// The "Node on its own" baseline: the conformance `text` handler's answer
// from node:http. With a worker count, it runs that many processes with
// node:cluster, the clustering a Node app would need to use more cores.
//
//   node bench/servers/node.mjs 9003 [workers]
import cluster from "node:cluster";
import { createServer } from "node:http";

const port = Number(process.argv[2] ?? 9003);
const workers = Number(process.argv[3] ?? 1);

if (workers > 1 && cluster.isPrimary) {
  for (let i = 0; i < workers; i++) cluster.fork();
} else {
  createServer((req, res) => {
    if (req.method === "POST") {
      const chunks = [];
      req.on("data", (c) => chunks.push(c));
      req.on("end", () => {
        res.writeHead(200, { "content-type": "application/octet-stream" });
        res.end(Buffer.concat(chunks));
      });
      return;
    }
    res.writeHead(201, { "content-type": "text/plain; charset=utf-8", "x-conformance": "yes" });
    res.end("hello");
  }).listen(port, "127.0.0.1");
}
