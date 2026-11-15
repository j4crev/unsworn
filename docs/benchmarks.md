# Benchmarks

What Unsworn costs, measured: in front of an existing app, around a
handler call, for each policy, and at startup. The harness is in
[`bench/`](../bench), and every number here comes from one run of it,
saved in
[`bench/results/2026-11-15-intel-core-i5-1145g7.json`](../bench/results/2026-11-15-intel-core-i5-1145g7.json).

## Summary

On two cores of a laptop CPU (an Intel i5-1145G7, details in
[Setup](#setup)):

| What Unsworn does | Requests/s | CPU per request | p50 latency at 1,000 req/s |
|---|--:|--:|--:|
| Answers a static `respond`, `redirect`, 404 or rejection | 144,000–183,000 | 19–24 µs | 0.16 ms (Go `net/http`: 0.16 ms) |
| `proxy` to an existing app | 61,000 | 58 µs | 0.33 ms (the app directly: 0.15 ms) |
| `proxy` with JWT and a rate limit | 42,000 | 85 µs | 0.35 ms |
| Calls a TypeScript handler on Bun (Tier 2) | 61,000 | 60 µs | 0.25 ms (Bun on its own: 0.15 ms) |
| Calls a Rust handler (Tier 2) | 46,000 | 81 µs | 0.19 ms |

- **Requests Unsworn answers itself cost about as much as Go's standard
  library serving "hello":** about 20 µs of CPU each, with the same
  latency.
- **A handler call or a proxied request adds about 0.1 to 0.2 ms** when
  the server isn't overloaded, and costs about three times the CPU of a
  static answer.
- **Policies are cheap,** a few µs each, except JWT verification (17 µs)
  and JSON Schema validation of a body (about 60 µs for 1 KB).
- **Route count doesn't matter:** 10,000 routes match as fast as 10.
- **The binary is ready in 3 ms** with 10 routes and 94 ms with 10,000.
  On a serverless function, the wasm core answers its first request
  about 140 ms after Node starts.
- **A trivial handler won't beat its language on its own.** Bun serves
  "hello" at 163,000 req/s on one core by itself, and at 61,000 through
  Unsworn. What Unsworn adds is the rules in front of it, and the same
  rules for every language.

## How they're measured

Everything runs on one machine over loopback, so these numbers are
Unsworn's own cost, without a network. Read them as relative to each
other and to the baselines in each table; absolute numbers depend on the
machine.

- **Separate cores.** The load generator, the server under test and the
  upstream app each get their own physical cores (`taskset`): one core
  (2 threads) for load, two cores (4 threads) for the server under test,
  and one core for the upstream app and Redis. Unsworn's handler workers
  share the server's cores with Unsworn, as they would on a real host.
- **Throughput** is closed-loop: [wrk](https://github.com/wg/wrk) with 64
  connections (16 for 1 MB bodies) sends requests as fast as they're
  answered. Its latencies are at full load, so they mostly show queueing.
- **Latency** is open-loop: [oha](https://github.com/hatoo/oha) sends a
  fixed 1,000 requests a second and corrects for coordinated omission, so
  a stall shows up in the percentiles instead of slowing the sender. This
  is what a request costs when the server isn't overloaded.
- **CPU µs/req** is the CPU time the server under test used during the
  throughput run (Unsworn and its workers, read from `/proc`), divided by
  the requests it answered. It doesn't depend on whether the load
  generator kept up, so it's the fairest way to compare scenarios. It's
  measured in hyperthread time.
- **Peak memory** is the resident memory of the server and its workers,
  sampled during the throughput run.
- Each scenario gets 2 seconds of warmup, then 8 seconds of each kind of
  run. The whole suite runs 3 times and the tables show the median.
- Every scenario's response is checked before it's measured (status code,
  and the echoed body for the body tests), so a misconfigured route can't
  produce a fast-looking number.
- Unsworn is the `--profile dist` build, as released, with its default
  logging: every request writes an access log line (to `/dev/null` here).
- Handlers are the [conformance handlers](conformance.md) each SDK ships
  with. `text` answers `201` with `hello`, and `binary` echoes the request
  body. The baselines answer the same thing.

**How precise they are.** This is a laptop with a `powersave` CPU
governor, and the same scenario varied by up to about 20% between the
three runs (rarely 30%). Treat smaller differences as noise. The load
generator tops out at about 200,000 req/s here (see the Go rows), so the
fastest scenarios are partly measuring wrk; their CPU µs/req column is
the better guide.

**Which build these are.** The tables come from a glibc build
(`cargo build --profile dist`) made before Unsworn switched its allocator
to mimalloc. The binary `install.sh` downloads is a static musl build, and
with musl's own allocator it was 25 to 40% slower than these numbers. With
mimalloc, spot checks of the musl build match the tables for proxied
requests and handler calls, and reach about 80% of them for static
answers. On glibc, mimalloc uses about 10% less CPU on proxied requests
and handler calls than the tables show, and about 15 MB more memory.

### Setup

| | |
|---|---|
| CPU | Intel Core i5-1145G7 (4 cores, 8 threads), `powersave` governor |
| Memory | 16 GB |
| OS | Linux 7.2 |
| Unsworn | 0.2.0 at `081f46a`, plus the worker fix described under [Handler calls](#handler-calls), `--profile dist` |
| Runtimes | Bun 1.4.4, Node 26.13.0, Python 3.14.8, Go 1.27.1, Rust 1.97.1 |
| Comparisons | Caddy 2.11.9, nginx 1.30.6 and Redis 7.4.12, in Docker with host networking |
| Load | wrk (git `f8eb608`), oha 1.17.0 |

## Results

### Answers without a handler

Routes Unsworn answers itself, with a handler call on the same server for
comparison. Go's `net/http` answering "hello" is a reference for what any
HTTP server costs on this machine.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Go net/http, for reference | 203,760 | 16.5 | 0.282 | 1.23 | 0.158 | 0.382 | 0.441 | 15 MB |
| `respond` (static JSON) | 145,809 | 23.1 | 0.435 | 0.772 | 0.162 | 0.396 | 0.482 | 121 MB |
| `redirect` | 177,744 | 19.3 | 0.354 | 0.642 | 0.162 | 0.384 | 0.410 | 119 MB |
| No route (404) | 182,670 | 18.7 | 0.343 | 0.643 | 0.158 | 0.395 | 0.426 | 119 MB |
| Rejected: missing JWT (401) | 144,105 | 24.4 | 0.438 | 0.837 | 0.166 | 0.423 | 0.478 | 120 MB |
| Rejected: rate limited (429) | 161,812 | 21.3 | 0.388 | 0.744 | 0.166 | 0.351 | 0.441 | 120 MB |
| Cache hit | 136,210 | 25.6 | 0.458 | 1.08 | 0.168 | 0.409 | 0.593 | 121 MB |
| Same handler, uncached (Bun, Tier 2) | 60,405 | 59.9 | 0.976 | 5.27 | 0.241 | 0.526 | 0.826 | 340 MB |

- A static answer, a redirect, a 404, a rejection and a cache hit all cost
  about the same: 19 to 26 µs of CPU, and the latency of the Go server.
  None of them reaches a worker.
- A cache hit costs about 40% of the handler call it replaces.
- Memory here includes the four Bun workers this project starts for its
  handler routes. Unsworn on its own uses 11 to 18 MB (see
  [Startup](#startup)).

### In front of an existing app

Unsworn's `proxy` in front of a Go server, compared with calling the
server directly and with Caddy and nginx in the same place.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Direct to the app, no proxy | 196,127 | 17.1 | 0.292 | 1.31 | 0.154 | 0.362 | 0.463 | 15 MB |
| Unsworn `proxy` | 60,608 | 57.5 | 1.02 | 2.06 | 0.327 | 0.718 | 0.884 | 19 MB |
| Unsworn `proxy` with JWT and a rate limit | 41,821 | 84.7 | 1.49 | 3.08 | 0.352 | 0.590 | 0.832 | 19 MB |
| Unsworn `proxy`, access log off | 54,494 | 62.1 | 1.14 | 2.49 | 0.288 | 0.701 | 0.867 | 19 MB |
| Caddy `reverse_proxy` | 31,413 | 117 | 1.83 | 6.33 | 0.192 | 0.422 | 0.705 | 25 MB |
| nginx `proxy_pass`, keepalive, access log off | 93,100 | 22.8 | 0.644 | 2.10 | 0.231 | 0.475 | 0.695 | 13 MB |

- Unsworn adds about 0.17 ms at the median when it isn't busy, and
  handles 61,000 req/s on two cores. Its upstream connections are reused,
  not opened per request.
- nginx uses about 40% of Unsworn's CPU per request, so it reaches about
  1.5 times the throughput. Unsworn's proxy adds the `Forwarded`,
  `X-Forwarded-*`, `Via` and request-id headers and runs the full policy
  pipeline on every request, but there's likely room to close some of
  that gap.
- The access log's cost is within the noise.
- Caddy ran with its default `reverse_proxy` settings and wasn't tuned.
  Its result is lower than it usually reports, so don't read much into
  that row.

### Handler calls

A trivial handler (answer `201` with `hello`), on its own and through
Unsworn, in each SDK. "4 workers" is Unsworn's default on this machine.
Unsworn, its workers and the baselines all get the same four hardware
threads.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Go on its own | 189,554 | 17.8 | 0.303 | 1.30 | 0.161 | 0.342 | 0.388 | 15 MB |
| Bun on its own, 1 process | 162,540 | 4.5 | 0.378 | 0.704 | 0.146 | 0.308 | 0.352 | 36 MB |
| Bun on its own, 4 processes (reusePort) | 235,363 | 11.1 | 0.129 | 0.765 | 0.151 | 0.353 | 0.388 | 145 MB |
| Node on its own, 1 process | 82,262 | 10.5 | 0.742 | 1.05 | 0.159 | 0.408 | 0.446 | 88 MB |
| Node on its own, 4 processes (cluster) | 155,895 | 22.3 | 0.367 | 1.09 | 0.169 | 0.440 | 0.563 | 408 MB |
| Unsworn → TypeScript on Bun, Tier 2, 1 worker | 65,185 | 49.3 | 0.903 | 3.10 | 0.239 | 0.676 | 1.35 | 107 MB |
| Unsworn → TypeScript on Bun, Tier 2, 4 workers | 60,869 | 59.8 | 0.963 | 5.18 | 0.246 | 0.648 | 0.850 | 342 MB |
| Unsworn → TypeScript on Bun, Tier 1, 4 workers | 55,852 | 63.9 | 1.07 | 5.09 | 0.242 | 0.661 | 0.865 | 320 MB |
| Unsworn → TypeScript on Node, Tier 2, 4 workers | 52,821 | 68.1 | 1.15 | 3.02 | 0.219 | 0.872 | 1.29 | 604 MB |
| Unsworn → Python, Tier 2, 4 workers | 17,227 | 217 | 3.50 | 8.33 | 0.260 | 1.09 | 10.47 | 125 MB |
| Unsworn → Python, Tier 1, 4 workers | 17,211 | 216 | 3.50 | 8.27 | 0.252 | 1.10 | 1.79 | 125 MB |
| Unsworn → Python, no SDK, Tier 1, 4 workers | 32,987 | 97.8 | 1.97 | 9.94 | 0.237 | 0.864 | 1.30 | 83 MB |
| Unsworn → Go, Tier 2, 4 workers | 37,274 | 96.2 | 1.62 | 4.68 | 0.227 | 0.611 | 0.783 | 90 MB |
| Unsworn → Go, Tier 1, 4 workers | 40,293 | 89.9 | 1.51 | 3.97 | 0.133 | 0.365 | 0.652 | 90 MB |
| Unsworn → Rust, Tier 2, 4 workers | 45,617 | 80.8 | 1.34 | 3.02 | 0.186 | 0.479 | 0.757 | 54 MB |
| Unsworn → Rust, Tier 1, 4 workers | 39,609 | 92.5 | 1.56 | 3.35 | 0.145 | 0.430 | 0.676 | 56 MB |

- When the server isn't busy, a handler call adds up to about 0.1 ms over
  the language on its own.
- At full load, a call costs 50 to 100 µs of CPU, Unsworn's side and the
  worker's together, and four threads handle 37,000 to 65,000 calls a
  second in every SDK except Python's.
- More workers don't help a handler this small: one Bun worker does as
  well as four, since Unsworn's own work is most of the cost. They matter
  for handlers that do real work.
- **Tier 2 isn't faster than Tier 1 for small requests.** The two are
  within the noise for Bun, Go and Rust, and Go and Rust show slightly
  lower latency on Tier 1. Tier 2's advantage shows with large bodies (see
  [Request bodies](#request-bodies)).
- **The Python SDK costs twice what a worker without an SDK does** (217
  against 98 µs per request). The difference is the SDK's own per-request
  work, which is worth profiling.
- Memory is mostly the workers: four Node processes take about 600 MB,
  four Bun processes about 340 MB, and four Rust processes about 54 MB.

These numbers include a fix these benchmarks prompted. Before it, every
handler call scanned the record of the worker's recent calls, which keeps
each finished call for 5 seconds so late log lines can still name their
request. The scan grew with traffic until it capped every runtime at
about 4,500 calls a second, whatever the language or the number of
workers. Finished calls now expire from a queue in order, so a call no
longer scans anything.

### Policies

One policy at a time on a static `respond` route, so each row's CPU µs/req
less the first row's is that policy's cost. A body schema only applies to
routes that pass the body on, so the body rows are a `proxy` to the Go
server, with and without the schema.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| No policy | 168,956 | 20.3 | 0.371 | 0.704 | 0.165 | 0.351 | 0.412 | 18 MB |
| JWT (HS256) | 99,025 | 36.9 | 0.645 | 1.19 | 0.179 | 0.421 | 0.445 | 18 MB |
| API key | 141,795 | 24.5 | 0.447 | 0.850 | 0.171 | 0.354 | 0.413 | 18 MB |
| Rate limit, memory store | 157,425 | 21.9 | 0.398 | 0.754 | 0.168 | 0.395 | 0.445 | 18 MB |
| Rate limit, Redis store | 84,184 | 34.2 | 0.720 | 1.30 | 0.365 | 0.686 | 0.814 | 17 MB |
| CORS (request with Origin) | 154,339 | 22.5 | 0.405 | 0.784 | 0.167 | 0.391 | 0.430 | 18 MB |
| Path and query validation | 146,302 | 23.8 | 0.433 | 0.821 | 0.172 | 0.365 | 0.416 | 18 MB |
| All of JWT, rate limit, CORS, path and query | 83,690 | 44.2 | 0.762 | 1.43 | 0.184 | 0.463 | 0.582 | 18 MB |
| `proxy`, 1 KB JSON body | 54,448 | 63.3 | 1.13 | 2.87 | 0.277 | 0.971 | 1.31 | 22 MB |
| `proxy`, same body checked against a JSON Schema | 29,409 | 123 | 2.15 | 3.55 | 0.290 | 0.516 | 0.798 | 23 MB |

| Policy | Added CPU per request |
|---|--:|
| API key, rate limit (memory), CORS, path and query validation | 1.5 to 4 µs each |
| JWT, HS256 | 17 µs |
| Rate limit, Redis store | 14 µs, plus a round trip to Redis (0.2 ms here) |
| JSON Schema on a 1 KB body | 60 µs |

- None of them adds measurable latency at 1,000 req/s, except Redis's
  round trip.
- JWT and body validation are the two to look at. An HS256 signature
  check on its own should take about a microsecond, so most of JWT's
  17 µs is likely decoding the token and its claims. The body schema here has 12 array items, each with a regex
  `pattern`.

### MCP

The same Bun handler as a route and as an MCP tool (a route with
`mcp = { tool = ... }`), a `[[mcp.tool]]` whose arguments are checked
against its input schema, and `tools/list`.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| `GET /text` (the route) | 61,998 | 58.4 | 0.948 | 5.38 | 0.263 | 0.670 | 0.854 | 347 MB |
| `tools/call text` (the same handler) | 33,773 | 110 | 1.83 | 4.47 | 0.322 | 0.650 | 0.898 | 269 MB |
| `tools/call echo`, arguments checked against its schema | 25,815 | 144 | 2.43 | 5.04 | 0.355 | 0.780 | 0.928 | 257 MB |
| `tools/list` | 75,351 | 49.4 | 0.820 | 1.77 | 0.194 | 0.570 | 1.23 | 198 MB |

- A tool call costs about twice the route it wraps (110 against 58 µs of
  CPU), and adds about 0.06 ms of latency: the JSON-RPC body is parsed and
  checked against the MCP headers, and the reply is turned into a tool
  result.
- `tools/list` is answered by Unsworn alone, at 49 µs.

### Route table size

A `respond` route at the end of a table of 10, 1,000 and 10,000 routes,
each with a path parameter, and a catch-all behind them.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| 10 routes | 168,454 | 20.3 | 0.376 | 0.699 | 0.164 | 0.398 | 0.477 | 14 MB |
| 1,000 routes | 164,587 | 20.7 | 0.385 | 0.707 | 0.166 | 0.355 | 0.395 | 21 MB |
| 10,000 routes | 167,834 | 20.3 | 0.378 | 0.685 | 0.163 | 0.381 | 0.435 | 86 MB |

Matching costs the same at every size, since routes are matched with a
radix tree. Memory grows by about 7 KB per route.

### Request bodies

A `POST` body echoed back: by the Go server on its own, through
Unsworn's `proxy` to it, and by a Bun or Go handler on each protocol tier.
"MB/s in" counts request bodies only; the same amount goes back out.

| Scenario | Req/s | MB/s in | CPU µs/req | p50 ms | p99 ms | Peak memory |
|---|--:|--:|--:|--:|--:|--:|
| 1 KB: Go on its own | 152,097 | 156 | 22.3 | 0.379 | 1.71 | 15 MB |
| 64 KB: Go on its own | 18,668 | 1,223 | 173 | 3.71 | 23.34 | 18 MB |
| 1 MB: Go on its own | 2,186 | 2,292 | 1310 | 6.72 | 23.46 | 47 MB |
| 1 KB: Unsworn `proxy` → Go | 51,438 | 53 | 66.5 | 1.19 | 2.83 | 19 MB |
| 64 KB: Unsworn `proxy` → Go | 8,665 | 568 | 169 | 7.58 | 44.24 | 40 MB |
| 1 MB: Unsworn `proxy` → Go | 1,169 | 1,225 | 1121 | 12.68 | 42.46 | 50 MB |
| 1 KB: Unsworn → Bun, Tier 2 | 54,503 | 56 | 66.9 | 1.08 | 5.11 | 328 MB |
| 64 KB: Unsworn → Bun, Tier 2 | 11,637 | 763 | 317 | 5.30 | 11.30 | 501 MB |
| 1 MB: Unsworn → Bun, Tier 2 | 652 | 684 | 5793 | 24.19 | 37.82 | 565 MB |
| 1 KB: Unsworn → Bun, Tier 1 | 58,908 | 60 | 61.5 | 1.01 | 4.49 | 318 MB |
| 64 KB: Unsworn → Bun, Tier 1 | 9,437 | 618 | 386 | 6.64 | 11.51 | 448 MB |
| 1 MB: Unsworn → Bun, Tier 1 | 437 | 458 | 8478 | 36.08 | 56.99 | 554 MB |
| 1 KB: Unsworn → Go, Tier 2 | 38,483 | 39 | 95.1 | 1.57 | 4.38 | 91 MB |
| 64 KB: Unsworn → Go, Tier 2 | 10,791 | 707 | 344 | 5.65 | 13.33 | 129 MB |
| 1 MB: Unsworn → Go, Tier 2 | 837 | 877 | 4556 | 18.24 | 35.77 | 195 MB |
| 1 KB: Unsworn → Go, Tier 1 | 47,821 | 49 | 75.2 | 1.27 | 3.53 | 90 MB |
| 64 KB: Unsworn → Go, Tier 1 | 10,809 | 708 | 345 | 5.64 | 11.55 | 128 MB |
| 1 MB: Unsworn → Go, Tier 1 | 860 | 901 | 4279 | 18.12 | 30.09 | 198 MB |

- The proxy streams bodies: it moves 1.2 GB/s of 1 MB bodies in about
  50 MB of memory. Its 64 KB and 1 MB rows are limited by the upstream,
  which has a single core here.
- A handler has to receive the whole body and send it back, so large
  bodies cost more than through the proxy: 460 to 900 MB/s for Bun and Go.
- Tier 2 beats Tier 1 for Bun at 64 KB and above (by 25% and 50%), since
  Tier 1 bodies go through the JSON line. Go's two tiers are the same.
- 1 MB bodies cost 4 to 8 ms of CPU each through a handler, three to six
  times what the Go server takes on its own.

### Startup

How long `unsworn serve` takes to answer its first request, by the size
of its route table:

| Routes | Start to first response, ms | Memory at idle |
|--:|--:|--:|
| 10 | 3.20 | 11 MB |
| 1,000 | 12.04 | 18 MB |
| 10,000 | 94.31 | 84 MB |

The first request to a handler, with the runtime's workers started on
that first request (the default) or at boot (`eager = true`):

| Runtime | Lazy: ready, ms | Lazy: first request, ms | Lazy: second request, ms | Eager: ready, ms |
|---|--:|--:|--:|--:|
| Bun, Tier 2 | 2.59 | 65.80 | 0.450 | 68.28 |
| Node, Tier 2 | 2.55 | 273 | 0.634 | 263 |
| Python, Tier 2 | 2.54 | 197 | 0.576 | 202 |
| Go, Tier 2 | 2.49 | 10.72 | 0.396 | 13.48 |
| Rust, Tier 2 | 2.52 | 8.05 | 0.327 | 11.25 |

Lazy start moves the workers' startup from boot to the first request:
about 10 ms for compiled workers, 66 ms for Bun, and 200 to 270 ms for
Python and Node.

**The wasm core on a serverless function.** A fresh Node process loads
the core through the Node host (`@unswornjs/sdk/node`), the way a Vercel,
Netlify or Lambda function does on a cold start, and answers a `POST`
with a body schema. The platform's own startup isn't included.

| Routes | Node started, ms | SDK imported, ms | First response, ms | Total, ms | Warm request, µs | First request to another route, ms |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 15.00 | 48.66 | 72.46 | 136 | 159 | – |
| 100 | 16.52 | 49.44 | 94.66 | 164 | 171 | 5.64 |
| 1,000 | 15.63 | 48.74 | 160 | 225 | 159 | 5.14 |

- A cold start answers about 140 ms after Node starts, with most of it in
  importing the SDK and compiling the wasm module.
- Warm, a request through the wasm core costs about 160 µs, against about
  20 µs for the binary.
- Each route's first request costs about 5 ms more, since the core
  compiles a route's schemas when a request first needs them. On a busy
  function that's paid once per route per instance.

### Sizes

| | |
|---|--:|
| `unsworn` binary (x86_64, `--profile dist`) | 17.7 MB |
| wasm core (`unsworn_bg.wasm`) | 4.4 MB, 1.36 MB gzipped |

## Running them

```sh
cargo install oha                 # wrk comes from your package manager
python3 bench/run.py build        # Unsworn, the SDK workers, the Go upstream, the wasm core
python3 bench/run.py run          # about an hour; writes bench/results/<date>-<cpu>.json
python3 bench/run.py report bench/results/<file>.json   # these tables, as markdown
```

`run` takes `--only` with a comma-separated list of groups
(`declarative`, `proxy`, `handlers`, `policies`, `mcp`, `routes`,
`bodies`, `startup`), and `--quick` for a single 3-second pass to check the
harness. It needs Go, Bun, Node and Python 3 for the handler groups, and
Docker for the Caddy, nginx and Redis rows, which it skips without Docker.
It waits for the machine to go quiet before it starts: anything else
running (a build, a browser) skews the results.

Each scenario's config is in [`bench/projects/`](../bench/projects), or
generated by `bench/run.py` into `target/bench/projects/` for the handler
and route-count groups.

A dedicated server with a fixed CPU frequency would give steadier
numbers than this laptop; results from one are welcome in
`bench/results/`.
