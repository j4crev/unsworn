# Benchmarks

What Unsworn costs, measured: in front of an existing app, around a
handler call, for each policy, and at startup. The harness is in
[`bench/`](../bench), and every number here comes from one run of it
against the released v0.3.0 binary, saved in
[`bench/results/2026-11-15-v0.3.0-intel-core-i5-1145g7.json`](../bench/results/2026-11-15-v0.3.0-intel-core-i5-1145g7.json).

## Summary

On two cores of a laptop CPU (an Intel i5-1145G7, details in
[Setup](#setup)):

| What Unsworn does | Requests/s | CPU per request | p50 latency at 1,000 req/s |
|---|--:|--:|--:|
| Answers a static `respond`, `redirect`, 404 or rejection | 113,000–144,000 | 24–31 µs | 0.16 ms (Go `net/http`: 0.09 ms) |
| `proxy` to an existing app | 58,000 | 59 µs | 0.29 ms (the app directly: 0.13 ms) |
| `proxy` with JWT and a rate limit | 40,000 | 89 µs | 0.33 ms |
| Calls a TypeScript handler on Bun (Tier 2) | 60,000 | 60 µs | 0.21 ms (Bun on its own: 0.15 ms) |
| Calls a Rust handler (Tier 2) | 47,000 | 79 µs | 0.20 ms |

- **Requests Unsworn answers itself cost 24 to 31 µs of CPU each,**
  against 17 µs for Go's standard library serving "hello".
- **A handler call or a proxied request adds about 0.05 to 0.15 ms** when
  the server isn't overloaded, and costs two to three times the CPU of a
  static answer.
- **Policies are cheap,** 1 to 3 µs each, except JWT verification (19 µs)
  and JSON Schema validation of a body (about 26 µs for 1 KB).
- **Large JSON bodies are the expensive case.** A 64 KB JSON body to a
  handler costs about 3 ms of CPU, nine times what the same size costs as
  raw bytes.
- **Route count doesn't matter:** 10,000 routes match as fast as 10.
- **The binary is ready in 3 ms** with 10 routes and 89 ms with 10,000.
  On a serverless function, the wasm core answers its first request
  about 140 ms after Node starts.
- **A trivial handler won't beat its language on its own.** Bun serves
  "hello" at 157,000 req/s on one core by itself, and at 60,000 through
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
- Unsworn is the released binary, a static musl build, with its default
  logging: every request writes an access log line (to `/dev/null` here).
- Handlers are the [conformance handlers](conformance.md) each SDK ships
  with. `text` answers `201` with `hello`, `binary` echoes the request
  body, and `status` answers with the status it's asked for. The baselines
  answer the same thing.

**How precise they are.** This is a laptop with a `powersave` CPU
governor. The same scenario typically varied by about 10% between the
three runs, and a few by 25% or more. Treat smaller differences as noise.
The load generator tops out at about 200,000 req/s here (see the Go
rows), so the fastest scenarios are partly measuring wrk; their CPU
µs/req column is the better guide.

**Against v0.2.0.** The two releases' binaries, in one run each on this
machine: a static `respond` went from 52,700 to 148,200 req/s, and a Bun
handler with 4 workers from 4,100 to 50,800. v0.2.0 used musl's
allocator, which takes one lock for every thread, and scanned a record of
recent calls on every handler call, which capped every runtime at about
4,500 calls a second. v0.3.0 uses mimalloc and expires that record
without scanning it.

### Setup

| | |
|---|---|
| CPU | Intel Core i5-1145G7 (4 cores, 8 threads), `powersave` governor |
| Memory | 16 GB |
| OS | Linux 7.2 |
| Unsworn | 0.3.0, the released `x86_64-unknown-linux-musl` binary |
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
| Go net/http, for reference | 191,216 | 17.2 | 0.300 | 1.32 | 0.094 | 0.541 | 0.902 | 16 MB |
| `respond` (static JSON) | 124,720 | 27.1 | 0.501 | 1.04 | 0.157 | 0.354 | 1.99 | 127 MB |
| `redirect` | 143,932 | 23.6 | 0.433 | 1.11 | 0.160 | 0.355 | 0.484 | 126 MB |
| No route (404) | 143,643 | 23.5 | 0.422 | 1.09 | 0.159 | 0.367 | 0.451 | 126 MB |
| Rejected: missing JWT (401) | 112,537 | 30.9 | 0.548 | 1.33 | 0.166 | 0.403 | 0.493 | 128 MB |
| Rejected: rate limited (429) | 127,387 | 26.5 | 0.478 | 1.25 | 0.165 | 0.402 | 1.11 | 128 MB |
| Cache hit | 114,279 | 30.7 | 0.543 | 1.25 | 0.170 | 0.424 | 0.871 | 128 MB |
| Same handler, uncached (Bun, Tier 2) | 60,693 | 59.5 | 0.951 | 5.43 | 0.224 | 0.636 | 1.28 | 347 MB |

- A static answer, a redirect, a 404, a rejection and a cache hit all cost
  about the same: 24 to 31 µs of CPU. None of them reaches a worker.
- A cache hit costs about half of the handler call it replaces.
- Memory here includes the four Bun workers this project starts for its
  handler routes. Unsworn on its own uses 19 to 35 MB (see
  [Startup](#startup)).

### In front of an existing app

Unsworn's `proxy` in front of a Go server, compared with calling the
server directly and with Caddy and nginx in the same place.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Direct to the app, no proxy | 193,842 | 17.1 | 0.300 | 1.28 | 0.129 | 0.324 | 0.386 | 16 MB |
| Unsworn `proxy` | 58,082 | 59.3 | 1.06 | 2.44 | 0.290 | 0.491 | 0.783 | 29 MB |
| Unsworn `proxy` with JWT and a rate limit | 40,413 | 89.2 | 1.56 | 2.73 | 0.330 | 0.523 | 0.847 | 29 MB |
| Unsworn `proxy`, access log off | 58,108 | 58.9 | 1.06 | 2.69 | 0.258 | 0.537 | 0.789 | 28 MB |
| Caddy `reverse_proxy` | 31,580 | 116 | 1.82 | 6.33 | 0.183 | 0.390 | 0.842 | 33 MB |
| nginx `proxy_pass`, keepalive, access log off | 98,436 | 22.8 | 0.611 | 1.93 | 0.212 | 0.447 | 0.668 | 13 MB |

- Unsworn adds about 0.16 ms at the median when it isn't busy, and
  handles 58,000 req/s on two cores. Its upstream connections are reused,
  not opened per request.
- nginx uses about 40% of Unsworn's CPU per request, so it reaches about
  1.7 times the throughput. Unsworn's proxy adds the `Forwarded`,
  `X-Forwarded-*`, `Via` and request-id headers and runs the full policy
  pipeline on every request, but there's likely room to close some of
  that gap.
- Turning the access log off made no measurable difference.
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
| Go on its own | 166,344 | 18.8 | 0.333 | 1.55 | 0.085 | 0.393 | 1.44 | 18 MB |
| Bun on its own, 1 process | 156,977 | 4.8 | 0.388 | 0.736 | 0.145 | 0.338 | 0.413 | 36 MB |
| Bun on its own, 4 processes (reusePort) | 238,673 | 10.7 | 0.129 | 0.856 | 0.148 | 0.314 | 0.398 | 144 MB |
| Node on its own, 1 process | 84,659 | 10.2 | 0.724 | 1.02 | 0.161 | 0.392 | 3.07 | 90 MB |
| Node on its own, 4 processes (cluster) | 134,563 | 25.1 | 0.423 | 2.73 | 0.167 | 0.411 | 0.857 | 419 MB |
| Unsworn → TypeScript on Bun, Tier 2, 1 worker | 62,436 | 49.0 | 0.952 | 3.28 | 0.211 | 0.670 | 1.79 | 114 MB |
| Unsworn → TypeScript on Bun, Tier 2, 4 workers | 60,217 | 60.0 | 0.961 | 5.36 | 0.205 | 0.643 | 1.67 | 346 MB |
| Unsworn → TypeScript on Bun, Tier 1, 4 workers | 54,098 | 65.8 | 1.08 | 5.32 | 0.223 | 0.681 | 1.25 | 326 MB |
| Unsworn → TypeScript on Node, Tier 2, 4 workers | 50,718 | 71.0 | 1.17 | 3.44 | 0.256 | 0.559 | 0.848 | 573 MB |
| Unsworn → Python, Tier 2, 4 workers | 20,268 | 188 | 2.96 | 7.08 | 0.252 | 0.584 | 0.870 | 135 MB |
| Unsworn → Python, Tier 1, 4 workers | 20,235 | 188 | 2.97 | 7.00 | 0.248 | 0.600 | 0.788 | 133 MB |
| Unsworn → Python, no SDK, Tier 1, 4 workers | 36,650 | 87.1 | 1.76 | 9.19 | 0.252 | 0.537 | 0.762 | 89 MB |
| Unsworn → Go, Tier 2, 4 workers | 43,173 | 86.2 | 1.40 | 4.24 | 0.187 | 0.402 | 0.692 | 105 MB |
| Unsworn → Go, Tier 1, 4 workers | 40,989 | 90.5 | 1.47 | 4.20 | 0.134 | 0.353 | 0.481 | 107 MB |
| Unsworn → Rust, Tier 2, 4 workers | 46,803 | 79.4 | 1.30 | 3.23 | 0.197 | 0.463 | 0.735 | 66 MB |
| Unsworn → Rust, Tier 1, 4 workers | 39,914 | 92.6 | 1.54 | 3.53 | 0.147 | 0.393 | 0.532 | 65 MB |

- When the server isn't busy, a handler call adds up to about 0.1 ms over
  the language on its own.
- At full load, a call costs 50 to 90 µs of CPU, Unsworn's side and the
  worker's together, and four threads handle 40,000 to 62,000 calls a
  second in every SDK except Python's. Unsworn's own side is 40 to 50 µs
  of that whatever the language, so it sets the ceiling for a handler
  this small.
- More workers don't help a handler this small: one Bun worker does as
  well as four. They matter for handlers that do real work.
- **Tier 2 isn't faster than Tier 1 for small requests.** The two are
  within the noise for Bun, Go and Rust, and Go and Rust show slightly
  lower latency on Tier 1. Tier 2's advantage shows with large bodies (see
  [Request bodies](#request-bodies)).
- **The Python SDK costs twice what a worker without an SDK does** (188
  against 87 µs per request). The difference is the SDK's own per-request
  work, which is worth profiling.
- Memory is mostly the workers: four Node processes take about 570 MB,
  four Bun processes about 350 MB, and four Rust processes about 65 MB.

### Policies

One policy at a time on a static `respond` route, so each row's CPU µs/req
less the first row's is that policy's cost. A body schema only applies to
routes that pass the body on, so the body rows are a `proxy` to the Go
server, with and without the schema.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| No policy | 152,317 | 23.0 | 0.411 | 0.818 | 0.163 | 0.396 | 0.428 | 28 MB |
| JWT (HS256) | 88,069 | 41.6 | 0.732 | 1.35 | 0.181 | 0.499 | 0.570 | 27 MB |
| API key | 138,981 | 25.5 | 0.452 | 0.875 | 0.169 | 0.365 | 0.485 | 26 MB |
| Rate limit, memory store | 140,051 | 24.9 | 0.452 | 0.851 | 0.164 | 0.416 | 0.481 | 26 MB |
| Rate limit, Redis store | 82,192 | 33.9 | 0.736 | 1.39 | 0.335 | 0.682 | 0.832 | 27 MB |
| CORS (request with Origin) | 145,404 | 24.3 | 0.433 | 0.835 | 0.166 | 0.400 | 0.490 | 26 MB |
| Path and query validation | 134,702 | 26.0 | 0.471 | 0.885 | 0.169 | 0.388 | 0.511 | 26 MB |
| All of JWT, rate limit, CORS, path and query | 72,796 | 50.7 | 0.877 | 1.65 | 0.190 | 0.466 | 2.87 | 26 MB |
| `proxy`, 1 KB JSON body | 54,709 | 64.2 | 1.12 | 2.63 | 0.257 | 0.499 | 0.846 | 30 MB |
| `proxy`, same body checked against a JSON Schema | 40,237 | 90.4 | 1.55 | 2.93 | 0.273 | 0.525 | 4.65 | 30 MB |

| Policy | Added CPU per request |
|---|--:|
| API key, rate limit (memory), CORS, path and query validation | 1 to 3 µs each |
| JWT, HS256 | 19 µs |
| Rate limit, Redis store | 11 µs, plus a round trip to Redis (0.17 ms here) |
| JSON Schema on a 1 KB body | 26 µs |

- None of them adds measurable latency at 1,000 req/s, except Redis's
  round trip.
- JWT is the one to look at. An HS256 signature check on its own should
  take about a microsecond, so most of its 19 µs is likely decoding the
  token and its claims.
- The body schema here has 12 array items, each with a regex `pattern`.

### MCP

The same Bun handler as a route and as an MCP tool (a route with
`mcp = { tool = ... }`), a `[[mcp.tool]]` whose arguments are checked
against its input schema, and `tools/list`.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| `GET /text` (the route) | 62,884 | 58.3 | 0.917 | 5.31 | 0.231 | 0.611 | 0.804 | 348 MB |
| `tools/call text` (the same handler) | 41,994 | 89.9 | 1.42 | 4.71 | 0.274 | 0.683 | 0.942 | 279 MB |
| `tools/call echo`, arguments checked against its schema | 33,296 | 115 | 1.82 | 4.88 | 0.326 | 0.718 | 0.856 | 274 MB |
| `tools/list` | 81,200 | 45.7 | 0.759 | 1.65 | 0.184 | 0.454 | 0.571 | 200 MB |

- A tool call costs about one and a half times the route it wraps (90
  against 58 µs of CPU), and adds about 0.04 ms of latency: the JSON-RPC
  body is parsed and checked against the MCP headers, and the reply is
  turned into a tool result.
- `tools/list` is answered by Unsworn alone, at 46 µs.

### Route table size

A `respond` route at the end of a table of 10, 1,000 and 10,000 routes,
each with a path parameter, and a catch-all behind them.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| 10 routes | 155,058 | 22.6 | 0.408 | 0.758 | 0.162 | 0.392 | 0.491 | 24 MB |
| 1,000 routes | 155,834 | 22.5 | 0.405 | 0.745 | 0.165 | 0.396 | 0.466 | 26 MB |
| 10,000 routes | 155,750 | 22.3 | 0.408 | 0.747 | 0.165 | 0.399 | 0.492 | 62 MB |

Matching costs the same at every size, since routes are matched with a
radix tree. Memory grows with the table: 10,000 routes take 62 MB under
load, and 131 MB just after startup (see [Startup](#startup)).

### Request bodies

A `POST` body echoed back: by the Go server on its own, through
Unsworn's `proxy` to it, and by a Bun or Go handler on each protocol tier.
"MB/s in" counts request bodies only; the same amount goes back out. The
JSON rows are different for handlers: Unsworn parses the body and sends
it on as the request's `json`, and the handler answers `204` without
sending it back.

| Scenario | Req/s | MB/s in | CPU µs/req | p50 ms | p99 ms | Peak memory |
|---|--:|--:|--:|--:|--:|--:|
| 1 KB: Go on its own | 136,108 | 139 | 24.7 | 0.420 | 1.97 | 18 MB |
| 64 KB: Go on its own | 17,868 | 1,171 | 178 | 3.89 | 23.83 | 21 MB |
| 1 MB: Go on its own | 2,136 | 2,240 | 1312 | 6.88 | 24.31 | 49 MB |
| 64 KB JSON: Go on its own | 17,480 | 1,112 | 183 | 3.96 | 25.29 | 24 MB |
| 1 KB: Unsworn `proxy` → Go | 51,757 | 53 | 67.7 | 1.18 | 2.76 | 28 MB |
| 64 KB: Unsworn `proxy` → Go | 9,173 | 601 | 169 | 7.33 | 41.88 | 93 MB |
| 1 MB: Unsworn `proxy` → Go | 1,178 | 1,236 | 1075 | 12.58 | 42.99 | 126 MB |
| 64 KB JSON: Unsworn `proxy` → Go | 9,520 | 606 | 154 | 5.64 | 42.62 | 55 MB |
| 1 KB: Unsworn → Bun, Tier 2 | 50,950 | 52 | 71.0 | 1.12 | 5.92 | 320 MB |
| 64 KB: Unsworn → Bun, Tier 2 | 10,973 | 719 | 336 | 5.64 | 12.01 | 506 MB |
| 1 MB: Unsworn → Bun, Tier 2 | 607 | 636 | 6224 | 25.98 | 41.19 | 638 MB |
| 64 KB JSON: Unsworn → Bun, Tier 2 | 1,356 | 86 | 2923 | 45.45 | 86.76 | 377 MB |
| 1 KB: Unsworn → Bun, Tier 1 | 55,950 | 57 | 63.8 | 1.02 | 5.30 | 331 MB |
| 64 KB: Unsworn → Bun, Tier 1 | 8,926 | 585 | 403 | 7.04 | 11.88 | 482 MB |
| 1 MB: Unsworn → Bun, Tier 1 | 410 | 430 | 8965 | 38.43 | 59.76 | 632 MB |
| 1 KB: Unsworn → Go, Tier 2 | 41,248 | 42 | 89.0 | 1.46 | 4.53 | 106 MB |
| 64 KB: Unsworn → Go, Tier 2 | 10,369 | 680 | 367 | 5.84 | 14.01 | 179 MB |
| 1 MB: Unsworn → Go, Tier 2 | 840 | 880 | 4555 | 18.26 | 35.58 | 313 MB |
| 64 KB JSON: Unsworn → Go, Tier 2 | 1,147 | 73 | 3440 | 54.32 | 102 | 165 MB |
| 1 KB: Unsworn → Go, Tier 1 | 47,921 | 49 | 76.9 | 1.24 | 3.74 | 106 MB |
| 64 KB: Unsworn → Go, Tier 1 | 10,386 | 681 | 361 | 5.84 | 12.31 | 177 MB |
| 1 MB: Unsworn → Go, Tier 1 | 802 | 841 | 4683 | 19.30 | 33.36 | 308 MB |

- The proxy streams bodies: it moves 1.2 GB/s of 1 MB bodies in about
  130 MB of memory. Its 64 KB and 1 MB rows are limited by the upstream,
  which has a single core here.
- A handler has to receive the whole body and send it back, so large
  bodies cost more than through the proxy: 430 to 880 MB/s for Bun and Go.
- Tier 2 beats Tier 1 for Bun at 64 KB and above (by about 25% and 50%),
  since Tier 1 bodies go through the JSON line. Go's two tiers are the
  same.
- 1 MB bodies cost 4.5 to 9 ms of CPU each through a handler, three to
  seven times what the Go server takes on its own.
- **A JSON body costs a handler route far more than raw bytes do.** 64 KB
  of JSON takes about 3 ms of CPU and 1,100 to 1,400 requests a second,
  against about 0.35 ms and 10,000 to 11,000 for 64 KB of raw bytes.
  Unsworn builds the whole document in memory, to refuse an object with
  two members of one name and to check it against a schema if the route
  has one, then writes it out again for the worker, which parses it once
  more. A `proxy` route without a body schema passes JSON through unread,
  at the speed of raw bytes.

### Startup

How long `unsworn serve` takes to answer its first request, by the size
of its route table:

| Routes | Start to first response, ms | Memory at idle |
|--:|--:|--:|
| 10 | 3.20 | 19 MB |
| 1,000 | 12.19 | 35 MB |
| 10,000 | 88.67 | 131 MB |

The first request to a handler, with the runtime's workers started on
that first request (the default) or at boot (`eager = true`):

| Runtime | Lazy: ready, ms | Lazy: first request, ms | Lazy: second request, ms | Eager: ready, ms |
|---|--:|--:|--:|--:|
| Bun, Tier 2 | 2.62 | 66.18 | 0.559 | 70.06 |
| Node, Tier 2 | 2.60 | 268 | 0.676 | 258 |
| Python, Tier 2 | 2.56 | 197 | 0.539 | 203 |
| Go, Tier 2 | 2.57 | 10.98 | 0.401 | 13.57 |
| Rust, Tier 2 | 2.63 | 8.45 | 0.317 | 11.32 |

Lazy start moves the workers' startup from boot to the first request:
about 10 ms for compiled workers, 66 ms for Bun, and 200 to 270 ms for
Python and Node.

**The wasm core on a serverless function.** A fresh Node process loads
the core through the Node host (`@unswornjs/sdk/node`), the way a Vercel,
Netlify or Lambda function does on a cold start, and answers a `POST`
with a body schema. The platform's own startup isn't included.

| Routes | Node started, ms | SDK imported, ms | First response, ms | Total, ms | Warm request, µs | First request to another route, ms |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 15.84 | 47.63 | 71.73 | 136 | 148 | – |
| 100 | 15.28 | 47.38 | 83.80 | 147 | 150 | 5.16 |
| 1,000 | 14.74 | 46.43 | 148 | 210 | 150 | 5.08 |

- A cold start answers about 140 ms after Node starts, with most of it in
  importing the SDK and compiling the wasm module.
- Warm, a request through the wasm core costs about 150 µs, against about
  25 µs for the binary.
- Each route's first request costs about 5 ms more, since the core
  compiles a route's schemas when a request first needs them. On a busy
  function that's paid once per route per instance.

### Sizes

| | |
|---|--:|
| `unsworn` binary (x86_64 musl, as released) | 18.0 MB |
| wasm core (`unsworn_bg.wasm`) | 4.4 MB, 1.36 MB gzipped |

## Running them

```sh
cargo install oha                 # wrk comes from your package manager
python3 bench/run.py build        # Unsworn, the SDK workers, the Go upstream, the wasm core
python3 bench/run.py run          # about an hour; writes bench/results/<date>-<cpu>.json
python3 bench/run.py report bench/results/<file>.json   # these tables, as markdown
```

`run` measures `target/dist/unsworn`, which `build` makes. To measure
another binary, such as a released one, set `UNSWORN_BIN` to its path.

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
