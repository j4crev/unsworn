# Benchmarks

What Unsworn costs, measured: in front of an existing app, around a
handler call, in front of a server it starts (an HTTP runtime), for each
policy, for MCP, for embedded `[dimedb]`, for OpenTelemetry export, and
at startup. The harness is in [`bench/`](../bench), and every number
here comes from one run of it against v0.9.0 on a small cloud server,
saved in
[`bench/results/2026-11-28-amd-epyc-9354p-32-core-processor.json`](../bench/results/2026-11-28-amd-epyc-9354p-32-core-processor.json).

**The throughput measured here is not the most Unsworn can serve in
real use.** The load generator, the server under test and the app behind
it all run on one machine, each on a few of its cores, so the fastest
rows are partly limited by how quickly the load generator can send
requests. Read each figure against the baseline measured beside it
(Go, Bun or Node on their own, nginx, the app directly), and compare
costs with the CPU µs/req column. A host that gives Unsworn all of its
cores, with traffic arriving over the network, will answer more requests
a second than these tables show.

## Summary

On two vCPUs of a cloud server (an AMD EPYC 9354P, details in
[Setup](#setup)):

| What Unsworn does | Requests/s | CPU per request | p50 latency at 1,000 req/s |
|---|--:|--:|--:|
| Answers a static `respond`, `redirect`, 404 or rejection | 73,700–79,200* | 21–24 µs | 0.11–0.12 ms (Go `net/http`: 0.13 ms) |
| `proxy` to an existing app | 41,700 (51,100 Unix socket) | 45 µs (38 µs socket) | 0.24 ms (the app directly: 0.12 ms) |
| `proxy` with JWT and a rate limit | 33,500 | 58 µs | 0.25 ms |
| Calls a TypeScript handler on Bun that only answers "hello" | 38,000 | 48 µs | 0.21 ms (Bun on its own: 0.10 ms) |
| Calls a Rust handler that only answers "hello" | 37,800 | 52 µs | 0.20 ms |
| Forwards to a Go `net/http` server it started (HTTP runtime) that only answers "hello" | 31,600 | 57 µs | 0.24 ms (Go on its own: 0.12 ms) |
| Forwards to a `Bun.serve` server it started that only answers "hello" | 36,600 | 51 µs | 0.19 ms |
| Embedded `[dimedb]` read / unique transactional write (WAL + Wasm) over MCP | 67,900 / 60,900 | 29 / 32 µs | 0.14 ms (read) |
| Calls a TypeScript handler on Bun that searches 2,000 names | 3,000 | 0.67 ms | 1.36 ms at 20 req/s |
| Calls the same search handler in Rust | 5,700 | 0.34 ms | 0.97 ms at 20 req/s |

\* The most the load generator sends on this machine; see
[How they're measured](#how-theyre-measured).

The "hello" rows measure the call into a handler, which costs about the
same in every language. They say nothing about how fast a language runs
your code. The search rows do: see
[A handler that does work](#a-handler-that-does-work).

- **Requests Unsworn answers itself cost 21 to 24 µs of CPU each.** Go's
  standard library serving "hello" takes 20.4 µs.
- **A handler call or a proxied request adds about 0.09 to 0.13 ms**
  when the server isn't overloaded (0.25 ms for the Python SDK). It
  costs 38 to 58 µs of CPU for the proxy, 48 to 76 µs for a Bun, Node,
  Rust or Go handler, and 81 to 86 µs for a Python one.
- **Embedded `[dimedb]` answers MCP reads at 67,900 req/s (29 µs CPU)
  and durable Wasm-checked writes at 60,900 req/s (32 µs CPU)** in
  36–41 MB of memory—faster than calling a trivial Bun MCP tool
  (23,100 req/s, 86 µs CPU) because there is no worker hop.
- **Policies are cheap,** 1 to 2.5 µs each, except a Redis-backed rate
  limit (10 µs) and JSON Schema validation of a body (about 13 µs for
  1 KB).
- **1% OpenTelemetry sampling adds <1 µs to `respond` and `proxy`**,
  while sampling 100% of requests adds 2 µs to `respond`, 9 µs to
  `proxy`, and 25 µs to a Bun handler.
- **A JSON body costs what's done with it.** A 64 KB JSON body that a
  handler doesn't read costs 0.14 to 0.20 ms of CPU. Read by the
  handler and sent back, it costs 0.52 to 0.63 ms.
- **Route count doesn't matter:** 10,000 routes match as fast as 10.
- **An HTTP runtime costs what a handler call does, or less.** Unsworn
  forwarding to a Go or Bun server it started, on a Unix socket, costs
  51 to 57 µs of CPU a request, the server's own work included, against
  48 to 76 µs for a handler in the same language. A 1 MB body costs
  65 to 80% of what it does through a handler, since it's streamed. A
  request that makes one call to a function or a service, through
  Unsworn's local endpoint, costs 155 to 220 µs in all.
- **The binary is ready in about 8 ms** with 10 routes and 87 ms with
  10,000 (see [Startup](#startup)). On a serverless function, the wasm
  core answers its first request about 173 ms after Node starts.
- **The language matters once the handler does work.** One search
  handler, written the same way in each language, costs 0.34 ms of CPU a
  request in Rust, 0.57 ms in Go, 0.67 ms in TypeScript on Bun, 0.90 ms
  on Node and 23 ms in Python.

## Against native servers

What Unsworn adds is best read against the same request without it. The
three tables below take the baselines from [Results](#results) and set
them beside Unsworn doing the same job.

**Without a handler.** Go's `net/http` answering "hello", against routes
Unsworn answers itself.

| Scenario | Req/s | CPU µs/req | p50 ms @ 1k/s | p99 ms @ 1k/s |
|---|--:|--:|--:|--:|
| Go `net/http` on its own | 68,014 | 20.4 | 0.125 | 0.245 |
| Unsworn `respond` (static JSON) | 73,910 | 21.4 | 0.114 | 0.250 |
| Unsworn `respond` with JWT, rate limit, CORS, path and query | 70,036 | 26.3 | 0.136 | 0.273 |

- **A request Unsworn answers itself costs about what Go's standard
  library takes to say "hello":** 21.4 µs of CPU against 20.4, and
  0.11 ms at the median at 1,000 req/s against 0.13.
- A redirect, a 404, a rejection or a cache hit costs 21 to 24 µs. All
  five policies together bring a request to 26 µs.

**In front of an existing app.** The Go app called directly, against
Unsworn's `proxy`, nginx and Caddy in front of it.

| Scenario | Req/s | CPU µs/req | p50 ms @ 1k/s | p99 ms @ 1k/s |
|---|--:|--:|--:|--:|
| The app directly, no proxy | 69,548 | 20.2 | 0.124 | 0.219 |
| Unsworn `proxy` | 41,669 | 45.4 | 0.235 | 0.427 |
| Unsworn `proxy`, the app on a Unix socket | 51,081 | 38.0 | 0.219 | 0.378 |
| Unsworn `proxy` with JWT and a rate limit | 33,506 | 58.3 | 0.248 | 0.429 |
| nginx `proxy_pass`, keepalive, access log off | 46,820 | – | 0.213 | 0.410 |
| Caddy `reverse_proxy` | 17,957 | – | 0.320 | 0.601 |

- **Unsworn's `proxy` answers 89% as many requests a second as nginx**
  over TCP (41,700 against 46,800), and **109%** with the app on a Unix
  socket (51,100). It answers 2.3 times as many as Caddy with its
  default settings.
- **When it isn't busy, it adds 0.11 ms at the median** to calling the
  app directly (0.24 against 0.12 ms), close to nginx (0.21 ms).
- The proxy's CPU column is Unsworn's alone; the app has a core of its
  own. nginx's and Caddy's CPU wasn't recorded on this machine: the
  harness reads it from a cgroup path this host's Docker doesn't use.

**Handlers and HTTP runtimes.** Bun, Go and Node answering "hello" on
their own, against the same answer from a handler in that language
through Unsworn, and from the same server started by Unsworn as an
[HTTP runtime](http-runtime.md).

| Scenario | Req/s | CPU µs/req | p50 ms @ 1k/s | p99 ms @ 1k/s |
|---|--:|--:|--:|--:|
| Bun on its own, 1 process | 73,250 | 13.6 | 0.102 | 0.204 |
| Unsworn → TypeScript handler on Bun, Tier 2, 1 worker | 37,984 | 47.7 | 0.212 | 0.416 |
| Unsworn → `Bun.serve` server, HTTP runtime | 36,634 | 51.4 | 0.194 | 0.348 |
| Go on its own | 64,924 | 21.3 | 0.126 | 0.237 |
| Unsworn → Go handler, Tier 2, 4 workers | 25,997 | 75.9 | 0.252 | 0.481 |
| Unsworn → Go `net/http` server, HTTP runtime | 31,636 | 57.3 | 0.235 | 0.402 |
| Node on its own, 1 process | 52,281 | 19.5 | 0.117 | 0.213 |
| Unsworn → TypeScript handler on Node, Tier 2, 4 workers | 28,719 | 68.7 | 0.226 | 0.508 |

- **Through Unsworn, a "hello" costs 34 to 55 µs of CPU more than the
  language on its own:** 48 µs through a Bun handler and 51 through the
  Bun server against 14 for Bun alone, 76 and 57 against 21 for Go, and
  69 against 20 for Node. That's Unsworn's side of the request plus the
  SDK's or the server's.
- **When it isn't busy, it adds 0.09 to 0.13 ms at the median.**
- **A trivial handler won't beat its language on its own.** What Unsworn
  adds is the rules in front of it, and the same rules for every
  language. These handlers do nothing, so the added cost is the whole
  request. For a handler that does work it's a small part: the search
  handler costs 343 µs of CPU in Rust, 569 in Go and 673 on Bun.
- Bun and Go on their own are at the load generator's limit here, so the
  req/s column understates the gap; the CPU column is the measure.

## How they're measured

Everything runs on one machine over loopback, so these numbers are
Unsworn's own cost, without a network. Read them as relative to each
other and to the baselines in each table; absolute numbers depend on the
machine.

- **Separate cores.** The load generator, the server under test and the
  upstream app each get their own vCPUs (`taskset`): one for load, two
  for the server under test, and one for the upstream app, Redis and the
  telemetry collector. Unsworn's handler workers share the server's
  vCPUs with Unsworn, as they would on a real host.
- **Throughput** is closed-loop: [wrk](https://github.com/wg/wrk) with 64
  connections (16 for 1 MB bodies) sends requests as fast as they're
  answered. Its latencies are at full load, so they mostly show queueing.
- **Latency** is open-loop: [oha](https://github.com/hatoo/oha) sends a
  fixed 1,000 requests a second and corrects for coordinated omission, so
  a stall shows up in the percentiles instead of slowing the sender. This
  is what a request costs when the server isn't overloaded. It starts
  once the server has finished whatever the throughput run left behind.
- **CPU µs/req** is the CPU time the server under test used during the
  throughput run (Unsworn and its workers, read from `/proc`), divided by
  the requests it answered. It doesn't depend on whether the load
  generator kept up, so it's the fairest way to compare scenarios. It's
  measured in vCPU time; this machine has no hyperthreads.
- **Peak memory** is the resident memory of the server and its workers,
  sampled during the throughput run.
- Each scenario got 1 second of warmup, then 4 seconds of each kind of
  run. The whole suite ran 3 times and the tables show the median.
- Every scenario's response is checked before it's measured (status code,
  and the echoed body for the body tests), so a misconfigured route can't
  produce a fast-looking number.
- Unsworn is v0.9.0 built from source by `bench/run.py build` (glibc,
  dynamically linked, with the default `dimedb` feature enabled), with
  its default logging: every request writes an access log line (to
  `/dev/null` here).
- Handlers are the [conformance handlers](conformance.md) each SDK ships
  with. `text` answers `201` with `hello`, `binary` echoes the request
  body, and `status` answers with the status it's asked for. The baselines
  answer the same thing. The exceptions are the search handler in
  [`bench/handlers/`](../bench/handlers), described under
  [A handler that does work](#a-handler-that-does-work), and the HTTP
  runtimes, which are the baseline servers in
  [`bench/servers/`](../bench/servers) started by Unsworn.

**The load generator is the limit for the fastest rows.** With one vCPU,
wrk sends at most about 68,000 to 82,000 requests a second, and Go and
Bun on their own, Node with four processes, the app directly, and every
route Unsworn answers itself all land in that range. So for those rows,
req/s shows the load generator; CPU µs/req and the latency at 1,000
req/s are what compare them.

### Setup

| | |
|---|---|
| CPU | AMD EPYC 9354P, 4 vCPUs under KVM (no hyperthreads) |
| Memory | 15.6 GB |
| OS | Debian 13, Linux 6.12 |
| Unsworn | 0.9.0 (`1a8d084`), built by `bench/run.py build` (glibc, default `dimedb` feature) |
| Runtimes | Bun 1.4.5, Node 26.13.1, Python 3.13.5, Go 1.27.4, Rust 1.100.0 |
| Comparisons | Caddy 2.11.9, nginx 1.30.7 and Redis 7.4.12, in Docker with host networking |
| Load | wrk 4.1.0 (Debian's package), oha 1.17.0 |

## Results

### Answers without a handler

Routes Unsworn answers itself, with a handler call on the same server for
comparison. Go's `net/http` answering "hello" is a reference for what any
HTTP server costs on this machine.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Go net/http, for reference | 68,014 | 20.4 | 0.673 | 5.49 | 0.125 | 0.245 | 0.472 | 17 MB |
| `respond` (static JSON) | 73,910 | 21.4 | 0.421 | 0.873 | 0.114 | 0.250 | 0.458 | 170 MB |
| `redirect` | 75,488 | 21.3 | 0.411 | 0.907 | 0.115 | 0.303 | 0.503 | 172 MB |
| No route (404) | 76,375 | 20.7 | 0.409 | 0.871 | 0.112 | 0.218 | 0.453 | 170 MB |
| Rejected: missing JWT (401) | 79,178 | 21.6 | 0.394 | 0.861 | 0.116 | 0.239 | 0.431 | 170 MB |
| Rejected: rate limited (429) | 73,723 | 23.5 | 0.419 | 0.907 | 0.115 | 0.224 | 0.420 | 170 MB |
| Cache hit | 78,702 | 22.9 | 0.400 | 0.913 | 0.126 | 0.267 | 0.394 | 171 MB |
| Same handler, uncached (Bun, Tier 2) | 29,334 | 66.9 | 2.02 | 7.18 | 0.224 | 0.512 | 1.34 | 308 MB |

- A static answer, a redirect, a 404, a rejection and a cache hit all cost
  about the same: 21 to 24 µs of CPU. None of them reaches a worker.
- A cache hit costs about a third of the handler call it replaces (23
  against 67 µs).
- Memory here includes the four Bun workers this project starts for its
  handler routes. Unsworn on its own uses 23 to 31 MB (see
  [Startup](#startup) and [Policies](#policies)).

### In front of an existing app

Unsworn's `proxy` in front of a Go server, compared with calling the
server directly and with Caddy and nginx in the same place. One row has
the Go server on a Unix socket (`proxy = "unix:/path"`) in place of a
TCP port.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Direct to the app, no proxy | 69,548 | 20.2 | 0.660 | 5.42 | 0.124 | 0.219 | 0.506 | 17 MB |
| Unsworn `proxy` | 41,669 | 45.4 | 1.50 | 2.92 | 0.235 | 0.427 | 0.656 | 31 MB |
| Unsworn `proxy` with JWT and a rate limit | 33,506 | 58.3 | 1.86 | 3.36 | 0.248 | 0.429 | 0.832 | 31 MB |
| Unsworn `proxy`, access log off | 41,535 | 45.9 | 1.49 | 2.99 | 0.230 | 0.424 | 0.781 | 29 MB |
| Unsworn `proxy`, the app on a Unix socket | 51,081 | 38.0 | 1.23 | 2.25 | 0.219 | 0.378 | 0.762 | 30 MB |
| Caddy `reverse_proxy` | 17,957 | – | 3.05 | 9.84 | 0.320 | 0.601 | 1.60 | 38 MB |
| nginx `proxy_pass`, keepalive, access log off | 46,820 | – | 1.13 | 4.09 | 0.213 | 0.410 | 0.738 | 12 MB |

- Unsworn adds about 0.11 ms at the median when it isn't busy, and
  handles 41,700 req/s over TCP (51,100 over Unix socket) on two vCPUs.
  Its upstream connections are reused, not opened per request.
- **With the app on a Unix socket, the proxy reaches 51,100 req/s** for
  38 µs of CPU a request, beating nginx over TCP (46,800 req/s).
- **The access log costs nothing measurable here:** 45.4 µs a request
  with it and 45.9 without, at 41,700 and 41,500 req/s.
- JWT and a rate limit add 13 µs a request to the proxy, and 0.01 ms at
  the median when it isn't busy.

### Handler calls

A trivial handler (answer `201` with `hello`), on its own and through
Unsworn, in each SDK. "4 workers" is `workers = 4`. Unsworn, its workers
and the baselines all get the same two vCPUs.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Go on its own | 64,924 | 21.3 | 0.712 | 5.65 | 0.126 | 0.237 | 0.414 | 17 MB |
| Bun on its own, 1 process | 73,250 | 13.6 | 0.715 | 1.55 | 0.102 | 0.204 | 0.387 | 45 MB |
| Bun on its own, 4 processes (reusePort) | 67,867 | 19.5 | 0.458 | 1.06 | 0.123 | 0.225 | 0.397 | 163 MB |
| Node on its own, 1 process | 52,281 | 19.5 | 1.16 | 1.98 | 0.117 | 0.213 | 0.387 | 89 MB |
| Node on its own, 4 processes (cluster) | 72,880 | 24.6 | 0.424 | 7.28 | 0.133 | 0.283 | 0.837 | 399 MB |
| Unsworn → TypeScript on Bun, Tier 2, 1 worker | 37,984 | 47.7 | 1.57 | 4.20 | 0.212 | 0.416 | 2.45 | 120 MB |
| Unsworn → TypeScript on Bun, Tier 2, 4 workers | 30,876 | 63.1 | 1.93 | 6.54 | 0.216 | 0.471 | 0.932 | 314 MB |
| Unsworn → TypeScript on Bun, Tier 1, 4 workers | 31,728 | 62.2 | 1.89 | 6.06 | 0.203 | 0.415 | 0.900 | 307 MB |
| Unsworn → TypeScript on Node, Tier 2, 4 workers | 28,719 | 68.7 | 2.10 | 6.40 | 0.226 | 0.508 | 0.985 | 493 MB |
| Unsworn → Python, Tier 2, 4 workers | 22,879 | 85.6 | 2.64 | 6.01 | 0.375 | 0.683 | 0.997 | 137 MB |
| Unsworn → Python, Tier 1, 4 workers | 24,192 | 81.2 | 2.49 | 5.34 | 0.357 | 0.609 | 1.31 | 134 MB |
| Unsworn → Python, no SDK, Tier 1, 4 workers | 23,949 | 82.0 | 2.65 | 15.81 | 0.201 | 0.392 | 0.972 | 96 MB |
| Unsworn → Go, Tier 2, 4 workers | 25,997 | 75.9 | 2.30 | 6.58 | 0.252 | 0.481 | 5.80 | 116 MB |
| Unsworn → Go, Tier 1, 4 workers | 31,880 | 61.8 | 1.83 | 7.19 | 0.241 | 0.419 | 0.899 | 124 MB |
| Unsworn → Rust, Tier 2, 4 workers | 37,797 | 52.0 | 1.60 | 4.29 | 0.199 | 0.364 | 0.728 | 76 MB |
| Unsworn → Rust, Tier 1, 4 workers | 37,180 | 52.9 | 1.67 | 3.81 | 0.191 | 0.343 | 1.15 | 77 MB |
| Unsworn → Rust, 4 threads in each worker, Tier 2, 4 workers | 24,509 | 80.1 | 2.51 | 5.68 | 0.259 | 0.457 | 0.805 | 75 MB |

- **These rows compare the SDKs, not the languages.** The handler does
  nothing, so each row is Unsworn's side of the call plus that SDK's
  reading and writing of one small message.
- At full load, a call costs 48 to 86 µs of CPU, Unsworn's side and the
  worker's together, and two vCPUs handle 22,900 to 38,000 calls a
  second.
- **More workers don't help a handler this small.** One Bun worker
  answers 38,000 calls a second for 48 µs each; four answer 30,900 for
  63 µs. They matter for handlers that do real work.
- **Memory is mostly the workers:** four Node processes take about
  493 MB, four Bun processes about 314 MB, and four Rust processes about
  76 MB.

### HTTP runtimes

An ordinary server that Unsworn starts, supervises and forwards to
(`protocol = "http"`, see [`http-runtime.md`](http-runtime.md)). The
servers are the "on its own" baselines from
[`bench/servers/`](../bench/servers), Go's `net/http` and `Bun.serve`,
answering `/text` as the conformance `text` handler does, and started
with Unsworn (`eager = true`).

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Go on its own | 68,178 | 20.5 | 0.671 | 5.57 | 0.121 | 0.230 | 0.419 | 17 MB |
| Unsworn `proxy` → Go, which Unsworn didn't start (Unix socket) | 46,945 | 41.5 | 1.33 | 2.48 | 0.225 | 0.417 | 0.725 | 29 MB |
| Bun on its own, 1 process | 81,521 | 12.2 | 0.651 | 1.49 | 0.103 | 0.189 | 0.381 | 45 MB |
| Unsworn → TypeScript handler on Bun, Tier 2, 4 workers | 30,600 | 64.4 | 1.99 | 5.51 | 0.215 | 0.461 | 1.51 | 352 MB |
| Unsworn → Go handler, Tier 2, 4 workers | 25,605 | 76.4 | 2.34 | 6.96 | 0.246 | 0.412 | 0.625 | 122 MB |
| Unsworn → Go server, on a socket, 1 server | 31,636 | 57.3 | 1.94 | 4.89 | 0.235 | 0.402 | 0.924 | 51 MB |
| Unsworn → Go server, on a port, 1 server | 26,070 | 73.0 | 2.31 | 5.49 | 0.246 | 0.422 | 0.912 | 52 MB |
| Unsworn → Go server, on a socket, 4 servers | 26,633 | 71.9 | 2.14 | 7.67 | 0.235 | 0.412 | 0.612 | 110 MB |
| Unsworn → Bun server, on a socket, 1 server | 36,634 | 51.4 | 1.68 | 2.87 | 0.194 | 0.348 | 0.807 | 78 MB |
| Unsworn → Bun server, on a socket, 4 servers | 28,734 | 69.2 | 2.17 | 4.73 | 0.211 | 0.470 | 0.806 | 205 MB |

- **Forwarding to a server costs less than calling a 4-worker handler in
  the same language.** The Go server takes 57 µs a request and answers
  31,600 a second, where a Go handler takes 76 µs and answers 25,600.
  The Bun server takes 51 µs and answers 36,600, where a TypeScript
  handler on Bun with four workers takes 64 µs and answers 30,600.
- **The socket is cheaper than the port at load:** 57 µs vs. 73 µs for
  the Go server (31,600 vs. 26,100 req/s).

### Calls from an HTTP runtime

A server reaches a [function](functions.md) or an
[outbound service](outbound.md) through a local endpoint Unsworn runs for
it (see
[`http-runtime.md`](http-runtime.md#calling-functions-and-outbound-services)).
`POST /relay?fn=text` calls the function `text` on four Bun workers;
`POST /outbound?path=/echo` sends a 1 KB body to the Go upstream.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Go server, `GET /text`, with `calls` (a token per request) | 28,907 | 62.4 | 2.09 | 5.28 | 0.238 | 0.394 | 0.648 | 199 MB |
| Go server → function, endpoint's socket | 10,767 | 176 | 5.59 | 12.82 | 0.511 | 1.42 | 6.14 | 363 MB |
| Go server → the app directly, without Unsworn | 14,526 | 127 | 3.97 | 10.85 | 0.381 | 0.633 | 1.35 | 319 MB |
| Go server → service, endpoint's socket | 10,453 | 176 | 5.75 | 11.44 | 0.534 | 1.000 | 2.24 | 300 MB |
| Go server → function, endpoint's port | 8,602 | 220 | 7.07 | 15.36 | 0.555 | 1.45 | 6.47 | 343 MB |
| Go server → service, endpoint's port | 9,618 | 195 | 6.27 | 12.45 | 0.547 | 0.924 | 1.66 | 301 MB |
| Go handler → function (`req.Call`), Tier 2 | 11,433 | 170 | 5.30 | 13.61 | 0.456 | 1.78 | 10.78 | 422 MB |
| Go handler → service (`req.Fetch`), Tier 2 | 11,413 | 171 | 5.35 | 12.22 | 0.464 | 0.806 | 1.41 | 376 MB |
| Bun server → function, endpoint's socket | 10,960 | 171 | 5.55 | 11.17 | 0.444 | 1.10 | 4.90 | 392 MB |
| Bun server → service, endpoint's socket | 12,230 | 155 | 5.08 | 8.67 | 0.483 | 0.844 | 1.55 | 372 MB |
| Bun server → function, endpoint's port | 11,251 | 171 | 5.41 | 10.94 | 0.462 | 1.53 | 9.62 | 387 MB |
| TypeScript handler on Bun → function (`ctx.call`), Tier 2 | 14,209 | 138 | 4.33 | 11.74 | 0.374 | 1.15 | 2.68 | 387 MB |
| TypeScript handler on Bun → service (`ctx.fetch`), Tier 2 | 12,482 | 158 | 4.89 | 12.11 | 0.421 | 0.862 | 3.16 | 341 MB |

### A handler that does work

A search handler holding 2,000 product names in memory and returning the
five closest to `?q=` by edit distance, written the same way in each
language ([`bench/handlers/`](../bench/handlers)), on Tier 2 with 4
workers. Latency is measured at 20 req/s.

| Scenario | Req/s | CPU µs/req | p50 ms @ 20/s | p99 ms @ 20/s | Peak memory |
|---|--:|--:|--:|--:|--:|
| Unsworn → TypeScript on Bun | 2,970 | 673 | 1.36 | 1.89 | 248 MB |
| Unsworn → TypeScript on Node | 2,237 | 900 | 1.52 | 2.29 | 482 MB |
| Unsworn → Python | 88 | 22,843 | 21.96 | 32.88 | 119 MB |
| Unsworn → Go | 3,518 | 569 | 1.23 | 1.62 | 112 MB |
| Unsworn → Rust | 5,742 | 343 | 0.972 | 1.60 | 70 MB |

- **Rust does this work for 0.34 ms of CPU, Go for 0.57 ms, Bun for
  0.67 ms, Node for 0.90 ms and Python for 22.8 ms.** Moving this
  endpoint from TypeScript on Bun to Rust gives 1.9× the throughput,
  from Node 2.6×, and from Python 65×.

### Policies

One policy at a time on a static `respond` route, plus a 1 KB JSON body
checked against a JSON Schema on a `proxy` route.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| No policy | 76,487 | 21.4 | 0.409 | 0.869 | 0.112 | 0.219 | 0.412 | 28 MB |
| JWT (HS256) | 78,408 | 23.8 | 0.428 | 1.07 | 0.129 | 0.267 | 0.485 | 28 MB |
| API key | 76,812 | 22.8 | 0.404 | 0.906 | 0.122 | 0.262 | 0.478 | 28 MB |
| Rate limit, memory store | 75,444 | 23.2 | 0.411 | 0.896 | 0.119 | 0.260 | 0.389 | 28 MB |
| Rate limit, Redis store | 62,024 | 31.1 | 0.981 | 1.67 | 0.227 | 0.420 | 0.607 | 27 MB |
| CORS (request with Origin) | 75,523 | 22.5 | 0.410 | 0.897 | 0.116 | 0.220 | 0.333 | 28 MB |
| Path and query validation | 80,531 | 22.3 | 0.380 | 0.885 | 0.124 | 0.256 | 0.424 | 28 MB |
| All of JWT, rate limit, CORS, path and query | 70,036 | 26.3 | 0.527 | 1.39 | 0.136 | 0.273 | 0.464 | 28 MB |
| `proxy`, 1 KB JSON body | 39,829 | 45.9 | 1.58 | 3.50 | 0.241 | 0.431 | 0.624 | 33 MB |
| `proxy`, same body checked against a JSON Schema | 33,343 | 59.1 | 1.87 | 3.39 | 0.259 | 0.445 | 0.631 | 32 MB |

| Policy | Added CPU per request |
|---|--:|
| API key, rate limit (memory), CORS, path and query validation | 1 to 2 µs each |
| JWT, HS256 | 2.4 µs |
| Rate limit, Redis store | 9.7 µs |
| JSON Schema on a 1 KB body | 13.2 µs |

### MCP

The same Bun handler as a route and as an MCP tool (a route with
`mcp = { tool = ... }`), a `[[mcp.tool]]` whose arguments are checked
against its input schema, and `tools/list`.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| `GET /text` (the route) | 32,606 | 60.3 | 1.83 | 6.94 | 0.206 | 0.437 | 1.30 | 313 MB |
| `tools/call text` (the same handler) | 23,084 | 86.0 | 2.64 | 6.59 | 0.233 | 0.488 | 1.41 | 343 MB |
| `tools/call echo`, arguments checked against its schema | 19,720 | 101 | 3.09 | 7.93 | 0.259 | 0.637 | 2.63 | 348 MB |
| `tools/list` | 72,485 | 27.6 | 0.847 | 1.61 | 0.138 | 0.265 | 0.455 | 261 MB |

### Embedded dimeDB

An embedded `[dimedb]` transactional store (`mode = "embedded"`,
`DynamicEngine` + sandboxed WebAssembly wallet rules) exposed
automatically as MCP tools on `/mcp`. The unique-writes row generates a
fresh `request_id` on every request so each call executes Wasm rules and
commits to the WAL via group commit.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| `dimedb_get_accounts` (in-memory read) | 67,897 | 29.3 | 0.906 | 1.63 | 0.141 | 0.265 | 0.571 | 36 MB |
| `dimedb_lookup` (in-memory hit) | 43,535 | 45.5 | 1.40 | 2.82 | 0.194 | 0.340 | 0.515 | 41 MB |
| `dimedb_lookup` (miss) | 66,886 | 29.8 | 0.919 | 1.65 | 0.147 | 0.307 | 0.506 | 38 MB |
| `dimedb_debit` (idempotent retry) | 42,678 | 46.4 | 1.43 | 2.85 | 0.191 | 0.316 | 0.614 | 41 MB |
| `dimedb_debit` (unique writes, WAL + Wasm) | 60,883 | 32.4 | 0.769 | 1.57 | – | – | – | 38 MB |

- **In-process reads (`dimedb_get_accounts`) run at 67,900 req/s** for
  29 µs of CPU—nearly as fast as `tools/list` (28 µs) and almost 3×
  faster than invoking a Bun MCP tool (23,100 req/s, 86 µs).
- **Durable, Wasm-validated writes (`dimedb_debit`) reach 60,900 req/s**
  (32 µs CPU, 1.57 ms p99 at 64 connections) with 38 MB peak memory.

### Telemetry

OpenTelemetry OTLP/HTTP span export (`[telemetry]`) sending batches to a
local collector on a separate core, measured with `[telemetry]` off, at
1% sampling, and at 100% sampling.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| `respond`, no `[telemetry]` | 74,847 | 21.3 | 0.420 | 0.855 | 0.116 | 0.242 | 0.383 | 170 MB |
| `proxy`, no `[telemetry]` | 41,671 | 46.6 | 1.49 | 2.88 | 0.235 | 0.454 | 1.45 | 176 MB |
| handler (Bun), no `[telemetry]` | 34,578 | 56.1 | 1.70 | 6.19 | 0.204 | 0.411 | 1.16 | 324 MB |
| `respond`, 1% sampled | 76,313 | 21.1 | 0.408 | 0.880 | 0.120 | 0.245 | 0.404 | 176 MB |
| `proxy`, 1% sampled | 41,328 | 47.2 | 1.51 | 3.07 | 0.236 | 0.419 | 0.898 | 182 MB |
| handler (Bun), 1% sampled | 28,517 | 69.0 | 2.11 | 6.84 | 0.205 | 0.419 | 1.15 | 312 MB |
| `respond`, every request sampled | 74,364 | 23.3 | 0.459 | 1.16 | 0.120 | 0.292 | 0.777 | 191 MB |
| `proxy`, every request sampled | 35,224 | 55.6 | 1.77 | 3.43 | 0.241 | 0.449 | 0.711 | 185 MB |
| handler (Bun), every request sampled | 24,530 | 80.6 | 2.49 | 6.96 | 0.225 | 0.509 | 2.49 | 329 MB |

### Route table size

A `respond` route at the end of a table of 10, 1,000 and 10,000 routes,
each with a path parameter, and a catch-all behind them.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| 10 routes | 76,855 | 21.6 | 0.404 | 0.884 | 0.118 | 0.229 | 0.756 | 24 MB |
| 1,000 routes | 77,314 | 21.5 | 0.400 | 0.860 | 0.120 | 0.249 | 0.384 | 29 MB |
| 10,000 routes | 76,051 | 21.9 | 0.408 | 0.866 | 0.112 | 0.219 | 0.393 | 68 MB |

Matching costs the same at every size, since routes are matched with a
radix tree. Memory grows with the table: 10,000 routes take 68 MB under
load, and 141 MB just after startup (see [Startup](#startup)).

### Request bodies

A `POST` body echoed back: by the Go server on its own, through
Unsworn's `proxy` to it, and by a Bun or Go handler on each protocol tier.
"MB/s in" counts request bodies only; the same amount goes back out.

| Scenario | Req/s | MB/s in | CPU µs/req | p50 ms | p99 ms | Peak memory |
|---|--:|--:|--:|--:|--:|--:|
| 1 KB: Go on its own | 65,861 | 67 | 23.8 | 0.713 | 5.30 | 17 MB |
| 64 KB: Go on its own | 8,607 | 564 | 196 | 8.43 | 36.97 | 19 MB |
| 1 MB: Go on its own | 880 | 923 | 1858 | 16.34 | 68.65 | 35 MB |
| 64 KB JSON: Go on its own | 8,283 | 527 | 205 | 8.37 | 42.17 | 19 MB |
| 1 KB: Unsworn `proxy` → Go | 39,576 | 41 | 48.6 | 1.60 | 3.31 | 30 MB |
| 64 KB: Unsworn `proxy` → Go | 7,566 | 496 | 167 | 6.57 | 105 | 95 MB |
| 1 MB: Unsworn `proxy` → Go | 933 | 979 | 1476 | 12.10 | 70.47 | 97 MB |
| 64 KB JSON: Unsworn `proxy` → Go | 8,191 | 521 | 149 | 6.07 | 107 | 64 MB |
| 1 KB: Unsworn → Bun, Tier 2 | 27,550 | 28 | 71.7 | 2.17 | 7.20 | 375 MB |
| 64 KB: Unsworn → Bun, Tier 2 | 8,592 | 563 | 232 | 7.18 | 14.20 | 441 MB |
| 1 MB: Unsworn → Bun, Tier 2 | 643 | 674 | 3056 | 24.43 | 38.70 | 539 MB |
| 64 KB JSON: Unsworn → Bun, Tier 2 | 13,852 | 881 | 144 | 4.25 | 11.65 | 412 MB |
| 64 KB JSON, sent back as JSON: Unsworn → Bun, Tier 2 | 3,890 | 247 | 515 | 15.67 | 33.25 | 398 MB |
| 1 KB: Unsworn → Bun, Tier 1 | 29,924 | 31 | 66.4 | 2.01 | 5.83 | 349 MB |
| 64 KB: Unsworn → Bun, Tier 1 | 6,915 | 453 | 287 | 8.93 | 18.83 | 474 MB |
| 1 MB: Unsworn → Bun, Tier 1 | 574 | 602 | 3463 | 27.54 | 42.33 | 516 MB |
| 1 KB: Unsworn → Go, Tier 2 | 24,566 | 25 | 80.3 | 2.45 | 6.78 | 120 MB |
| 64 KB: Unsworn → Go, Tier 2 | 7,579 | 497 | 261 | 7.92 | 19.47 | 154 MB |
| 1 MB: Unsworn → Go, Tier 2 | 595 | 624 | 3314 | 26.21 | 47.62 | 220 MB |
| 64 KB JSON: Unsworn → Go, Tier 2 | 10,136 | 645 | 195 | 5.88 | 17.14 | 139 MB |
| 64 KB JSON, sent back as JSON: Unsworn → Go, Tier 2 | 3,187 | 203 | 625 | 18.98 | 45.59 | 142 MB |
| 1 KB: Unsworn → Go, Tier 1 | 26,757 | 27 | 74.0 | 2.23 | 6.79 | 123 MB |
| 64 KB: Unsworn → Go, Tier 1 | 6,267 | 411 | 310 | 9.54 | 23.23 | 172 MB |
| 1 MB: Unsworn → Go, Tier 1 | 549 | 576 | 3519 | 28.57 | 47.98 | 241 MB |
| 1 KB: Unsworn → Go server (HTTP runtime), 1 server | 29,901 | 31 | 62.0 | 2.04 | 5.31 | 50 MB |
| 64 KB: Unsworn → Go server (HTTP runtime), 1 server | 5,745 | 376 | 308 | 11.61 | 39.70 | 107 MB |
| 1 MB: Unsworn → Go server (HTTP runtime), 1 server | 712 | 747 | 2630 | 21.97 | 37.94 | 154 MB |
| 1 KB: Unsworn → Bun server (HTTP runtime), 1 server | 32,129 | 33 | 56.4 | 1.92 | 3.36 | 78 MB |
| 64 KB: Unsworn → Bun server (HTTP runtime), 1 server | 12,095 | 793 | 155 | 4.91 | 8.76 | 127 MB |
| 1 MB: Unsworn → Bun server (HTTP runtime), 1 server | 935 | 980 | 1992 | 16.95 | 25.99 | 177 MB |

### Startup

How long `unsworn serve` takes to answer its first request, by the size
of its route table:

| Routes | Start to first response, ms | Memory at idle |
|--:|--:|--:|
| 10 | 7.83 | 23 MB |
| 1,000 | 16.65 | 41 MB |
| 10,000 | 86.80 | 141 MB |

The first request to a handler or an HTTP runtime, with the runtime's
workers or server started on that first request (the default) or at
boot (`eager = true`):

| Runtime | Lazy: ready, ms | Lazy: first request, ms | Lazy: second request, ms | Eager: ready, ms |
|---|--:|--:|--:|--:|
| Bun, Tier 2 | 7.20 | 145 | 0.681 | 144 |
| Node, Tier 2 | 6.12 | 439 | 1.69 | 420 |
| Python, Tier 2 | 7.20 | 216 | 0.801 | 219 |
| Go, Tier 2 | 7.08 | 18.56 | 0.525 | 21.95 |
| Rust, Tier 2 | 7.20 | 8.78 | 0.513 | 13.95 |
| Go `net/http` server, HTTP runtime | 4.81 | 6.44 | 0.481 | 12.66 |
| `Bun.serve` server, HTTP runtime | 5.97 | 38.95 | 0.522 | 44.13 |

**The wasm core on a serverless function.** A fresh Node process loads
the core through the Node host (`@unswornjs/sdk/node`), the way a Vercel,
Netlify or Lambda function does on a cold start, and answers a `POST`
with a body schema. The platform's own startup isn't included.

| Routes | Node started, ms | SDK imported, ms | First response, ms | Total, ms | Warm request, µs | First request to another route, ms |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 19.44 | 57.83 | 93.31 | 173 | 201 | – |
| 100 | 20.63 | 59.75 | 107 | 189 | 211 | 4.99 |
| 1,000 | 20.82 | 58.28 | 185 | 267 | 213 | 5.13 |

### Sizes

| | |
|---|--:|
| `unsworn` binary (x86_64 glibc, default `dimedb` feature enabled) | 30.6 MB |
| wasm core (`unsworn_bg.wasm`) | 4.8 MB, 1.49 MB gzipped |

## Running them

```sh
cargo install oha                 # wrk comes from your package manager
python3 bench/run.py build        # Unsworn, the SDK workers, the Go upstream and HTTP runtime, the wasm core
python3 bench/run.py run          # all groups; writes bench/results/<date>-<cpu>.json
python3 bench/run.py report bench/results/<file>.json   # these tables, as markdown
```

`run` measures `target/dist/unsworn`, which `build` makes. To measure
another binary, such as a released one, set `UNSWORN_BIN` to its path.

`run` takes `--only` with a comma-separated list of groups
(`declarative`, `proxy`, `telemetry`, `handlers`, `http`, `calls`, `work`,
`policies`, `mcp`, `dimedb`, `routes`, `bodies`, `startup`), and `--quick`
for a single 3-second pass to check the harness. It needs Go, Bun, Node
and Python 3 for the handler groups, and Docker for the Caddy, nginx and
Redis rows, which it skips without Docker. It waits for the machine to go
quiet before it starts: anything else running (a build, a browser) skews
the results.

Each scenario's config is in [`bench/projects/`](../bench/projects), or
generated by `bench/run.py` into `target/bench/projects/` for the handler,
HTTP runtime, telemetry, dimedb and route-count groups.

A dedicated server with more cores and a fixed CPU frequency would give
steadier numbers, and lift the load generator's limit on the fastest
rows; results from one are welcome in `bench/results/`.
