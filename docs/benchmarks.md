# Benchmarks

What Unsworn costs, measured: in front of an existing app, around a
handler call, in front of a server it starts (an HTTP runtime), for each
policy, and at startup. The harness is in [`bench/`](../bench), and
every number here comes from one run of it against v0.6.0 on a small
cloud server, saved in
[`bench/results/2026-11-22-amd-epyc-9354p-32-core-processor.json`](../bench/results/2026-11-22-amd-epyc-9354p-32-core-processor.json).

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
| Answers a static `respond`, `redirect`, 404 or rejection | 76,000–78,000* | 21–22 µs | 0.09 ms (Go `net/http`: 0.11 ms) |
| `proxy` to an existing app | 45,000 | 43 µs | 0.19 ms (the app directly: 0.11 ms) |
| `proxy` with JWT and a rate limit | 36,000 | 54 µs | 0.21 ms |
| Calls a TypeScript handler on Bun that only answers "hello" | 39,000 | 47 µs | 0.18 ms (Bun on its own: 0.08 ms) |
| Calls a Rust handler that only answers "hello" | 36,000 | 55 µs | 0.18 ms |
| Forwards to a Go `net/http` server it started (HTTP runtime) that only answers "hello" | 32,000 | 58 µs | 0.23 ms (Go on its own: 0.11 ms) |
| Forwards to a `Bun.serve` server it started that only answers "hello" | 39,000 | 49 µs | 0.16 ms |
| Calls a TypeScript handler on Bun that searches 2,000 names | 3,100 | 0.64 ms | 1.2 ms at 20 req/s |
| Calls the same search handler in Rust | 5,700 | 0.34 ms | 0.80 ms at 20 req/s |

\* The most the load generator sends on this machine; see
[How they're measured](#how-theyre-measured).

The "hello" rows measure the call into a handler, which costs about the
same in every language. They say nothing about how fast a language runs
your code. The search rows do: see
[A handler that does work](#a-handler-that-does-work).

- **Requests Unsworn answers itself cost 21 to 23 µs of CPU each.** Go's
  standard library serving "hello" takes 19.5 µs.
- **A handler call or a proxied request adds about 0.09 to 0.14 ms**
  when the server isn't overloaded (0.23 ms for the Python SDK). It
  costs 39 to 54 µs of CPU for the proxy, 47 to 70 µs for a Bun, Node,
  Rust or Go handler, and 74 to 80 µs for a Python one.
- **Policies are cheap,** 1 to 3 µs each, except a Redis-backed rate
  limit (9 µs) and JSON Schema validation of a body (about 7 µs for
  1 KB).
- **A JSON body costs what's done with it.** A 64 KB JSON body that a
  handler doesn't read costs 0.14 to 0.19 ms of CPU. Read by the
  handler and sent back, it costs 0.49 to 0.61 ms.
- **Route count doesn't matter:** 10,000 routes match as fast as 10.
- **An HTTP runtime costs what a handler call does, or less.** Unsworn
  forwarding to a Go or Bun server it started, on a Unix socket, costs
  49 to 58 µs of CPU a request, the server's own work included, against
  47 to 70 µs for a handler in the same language. A 1 MB body costs
  65 to 85% of what it does through a handler, since it's streamed. A
  request that makes one call to a function or a service, through
  Unsworn's local endpoint, costs 148 to 205 µs in all.
- **The binary is ready in about 6 ms** with 10 routes and 90 ms with
  10,000 (see [Startup](#startup)). On a serverless function, the wasm
  core answers its first request about 160 ms after Node starts.
- **The language matters once the handler does work.** One search
  handler, written the same way in each language, costs 0.34 ms of CPU a
  request in Rust, 0.64 ms in TypeScript on Bun and 22 ms in Python.

## Against native servers

What Unsworn adds is best read against the same request without it. The
three tables below take the baselines from [Results](#results) and set
them beside Unsworn doing the same job.

**Without a handler.** Go's `net/http` answering "hello", against routes
Unsworn answers itself.

| Scenario | Req/s | CPU µs/req | p50 ms @ 1k/s | p99 ms @ 1k/s |
|---|--:|--:|--:|--:|
| Go `net/http` on its own | 68,792 | 19.5 | 0.105 | 0.205 |
| Unsworn `respond` (static JSON) | 76,310 | 21.4 | 0.091 | 0.187 |
| Unsworn `respond` with JWT, rate limit, CORS, path and query | 74,631 | 26.0 | 0.111 | 0.227 |

- **A request Unsworn answers itself costs about what Go's standard
  library takes to say "hello":** 21 µs of CPU against 19.5, and
  0.09 ms at the median at 1,000 req/s against 0.11.
- A redirect, a 404, a rejection or a cache hit costs 21 to 23 µs. All
  five policies together bring a request to 26 µs.

**In front of an existing app.** The Go app called directly, against
Unsworn's `proxy`, nginx and Caddy in front of it.

| Scenario | Req/s | CPU µs/req | p50 ms @ 1k/s | p99 ms @ 1k/s |
|---|--:|--:|--:|--:|
| The app directly, no proxy | 68,913 | 19.4 | 0.106 | 0.181 |
| Unsworn `proxy` | 44,904 | 43.4 | 0.193 | 0.328 |
| Unsworn `proxy`, the app on a Unix socket | 49,893 | 39.1 | 0.178 | 0.314 |
| Unsworn `proxy` with JWT and a rate limit | 36,216 | 54.3 | 0.209 | 0.359 |
| nginx `proxy_pass`, keepalive, access log off | 53,888 | – | 0.183 | 0.295 |
| Caddy `reverse_proxy` | 18,435 | – | 0.300 | 0.514 |

- **Unsworn's `proxy` answers 83% as many requests a second as nginx**
  (44,900 against 53,900), and 93% with the app on a Unix socket. It
  answers 2.4 times as many as Caddy with its default settings.
- **When it isn't busy, it adds 0.09 ms at the median** to calling the
  app directly (0.19 against 0.11 ms), about what nginx adds (0.18 ms
  through nginx).
- The proxy's CPU column is Unsworn's alone; the app has a core of its
  own. nginx's and Caddy's CPU wasn't recorded on this machine: the
  harness reads it from a cgroup path this host's Docker doesn't use.

**Handlers and HTTP runtimes.** Bun, Go and Node answering "hello" on
their own, against the same answer from a handler in that language
through Unsworn, and from the same server started by Unsworn as an
[HTTP runtime](http-runtime.md).

| Scenario | Req/s | CPU µs/req | p50 ms @ 1k/s | p99 ms @ 1k/s |
|---|--:|--:|--:|--:|
| Bun on its own, 1 process | 77,103 | 13.0 | 0.082 | 0.158 |
| Unsworn → TypeScript handler on Bun, Tier 2, 1 worker | 38,965 | 46.7 | 0.181 | 0.382 |
| Unsworn → `Bun.serve` server, HTTP runtime | 38,872 | 48.6 | 0.163 | 0.327 |
| Go on its own | 68,309 | 19.9 | 0.106 | 0.183 |
| Unsworn → Go handler, Tier 2, 4 workers | 28,122 | 70.2 | 0.233 | 0.416 |
| Unsworn → Go `net/http` server, HTTP runtime | 31,765 | 57.6 | 0.228 | 0.355 |
| Node on its own, 1 process | 55,665 | 18.2 | 0.095 | 0.182 |
| Unsworn → TypeScript handler on Node, Tier 2, 4 workers | 31,690 | 62.4 | 0.185 | 0.403 |

- **Through Unsworn, a "hello" costs 34 to 50 µs of CPU more than the
  language on its own,** about three to four times as much: 47 µs
  through a Bun handler and 49 through the Bun server against 13 for Bun
  alone, 70 and 58 against 20 for Go, and 62 against 18 for Node. That's
  Unsworn's side of the request plus the SDK's or the server's.
- **When it isn't busy, it adds 0.08 to 0.13 ms at the median.**
- **A trivial handler won't beat its language on its own.** What Unsworn
  adds is the rules in front of it, and the same rules for every
  language. These handlers do nothing, so the added cost is the whole
  request. For a handler that does work it's a small part: the search
  handler costs 344 µs of CPU in Rust, 545 in Go and 635 on Bun.
- Bun and Go on their own are at the load generator's limit here, so the
  req/s column understates the gap; the CPU column is the measure.

## How they're measured

Everything runs on one machine over loopback, so these numbers are
Unsworn's own cost, without a network. Read them as relative to each
other and to the baselines in each table; absolute numbers depend on the
machine.

- **Separate cores.** The load generator, the server under test and the
  upstream app each get their own vCPUs (`taskset`): one for load, two
  for the server under test, and one for the upstream app and Redis.
  Unsworn's handler workers share the server's vCPUs with Unsworn, as
  they would on a real host.
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
- Each scenario gets 2 seconds of warmup, then 8 seconds of each kind of
  run. The whole suite runs 3 times and the tables show the median.
- Every scenario's response is checked before it's measured (status code,
  and the echoed body for the body tests), so a misconfigured route can't
  produce a fast-looking number.
- Unsworn is v0.6.0 built from source by `bench/run.py build` (glibc,
  dynamically linked), with its default logging: every request writes an
  access log line (to `/dev/null` here).
- Handlers are the [conformance handlers](conformance.md) each SDK ships
  with. `text` answers `201` with `hello`, `binary` echoes the request
  body, and `status` answers with the status it's asked for. The baselines
  answer the same thing. The exceptions are the search handler in
  [`bench/handlers/`](../bench/handlers), described under
  [A handler that does work](#a-handler-that-does-work), and the HTTP
  runtimes, which are the baseline servers in
  [`bench/servers/`](../bench/servers) started by Unsworn.

**The load generator is the limit for the fastest rows.** With one vCPU,
wrk sends at most about 68,000 to 78,000 requests a second, and Go and
Bun on their own, Node with four processes, the app directly, and every
route Unsworn answers itself all land in that range. In a separate check
of four of those rows, with each vCPU's use recorded every second, wrk's
was at 95 to 100% while the server's two were at 75 to 90%. So for those
rows, req/s shows the load generator. CPU µs/req and the latency at
1,000 req/s are what compare them.

**How precise they are.** Of the 107 scenarios, 41 had their three
rounds within 2% of each other and 90 within 5%. One differed by 15%:
the Go handler on Tier 1, at 29,900 to 34,600 req/s. The tables show the
median of the three, so one slow round doesn't move it.

### Setup

| | |
|---|---|
| CPU | AMD EPYC 9354P, 4 vCPUs under KVM (no hyperthreads) |
| Memory | 16 GB |
| OS | Debian 13, Linux 6.12 |
| Unsworn | 0.6.0 (`06323bd`), built by `bench/run.py build` (glibc) |
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
| Go net/http, for reference | 68,792 | 19.5 | 0.659 | 5.57 | 0.105 | 0.205 | 0.596 | 17 MB |
| `respond` (static JSON) | 76,310 | 21.4 | 0.414 | 0.852 | 0.091 | 0.187 | 0.275 | 170 MB |
| `redirect` | 78,320 | 21.1 | 0.405 | 0.912 | 0.091 | 0.183 | 0.283 | 169 MB |
| No route (404) | 76,197 | 20.7 | 0.412 | 0.843 | 0.091 | 0.189 | 0.323 | 169 MB |
| Rejected: missing JWT (401) | 77,021 | 22.0 | 0.411 | 0.847 | 0.095 | 0.196 | 0.305 | 169 MB |
| Rejected: rate limited (429) | 75,922 | 22.2 | 0.414 | 0.861 | 0.095 | 0.193 | 0.264 | 169 MB |
| Cache hit | 77,136 | 23.3 | 0.406 | 0.857 | 0.098 | 0.202 | 0.289 | 170 MB |
| Same handler, uncached (Bun, Tier 2) | 33,048 | 59.8 | 1.83 | 5.50 | 0.175 | 0.390 | 1.64 | 352 MB |

- A static answer, a redirect, a 404, a rejection and a cache hit all cost
  about the same: 21 to 23 µs of CPU. None of them reaches a worker.
- A cache hit costs about 40% of the handler call it replaces (23 against
  60 µs).
- Memory here includes the four Bun workers this project starts for its
  handler routes. Unsworn on its own uses 22 to 29 MB (see
  [Startup](#startup) and [Policies](#policies)).

### In front of an existing app

Unsworn's `proxy` in front of a Go server, compared with calling the
server directly and with Caddy and nginx in the same place. One row has
the Go server on a Unix socket (`proxy = "unix:/path"`) in place of a
TCP port.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Direct to the app, no proxy | 68,913 | 19.4 | 0.662 | 5.43 | 0.106 | 0.181 | 0.273 | 17 MB |
| Unsworn `proxy` | 44,904 | 43.4 | 1.42 | 2.38 | 0.193 | 0.328 | 0.492 | 28 MB |
| Unsworn `proxy` with JWT and a rate limit | 36,216 | 54.3 | 1.75 | 2.69 | 0.209 | 0.359 | 0.490 | 29 MB |
| Unsworn `proxy`, access log off | 44,379 | 44.1 | 1.42 | 2.31 | 0.192 | 0.333 | 0.488 | 27 MB |
| Unsworn `proxy`, the app on a Unix socket | 49,893 | 39.1 | 1.26 | 2.03 | 0.178 | 0.314 | 0.581 | 26 MB |
| Caddy `reverse_proxy` | 18,435 | – | 2.98 | 10.22 | 0.300 | 0.514 | 1.57 | 39 MB |
| nginx `proxy_pass`, keepalive, access log off | 53,888 | – | 1.03 | 4.56 | 0.183 | 0.295 | 0.471 | 12 MB |

- Unsworn adds about 0.09 ms at the median when it isn't busy, and
  handles 45,000 req/s on two vCPUs. Its upstream connections are
  reused, not opened per request.
- nginx answers about 1.2 times as many requests. Unsworn's proxy adds
  the `Forwarded`, `X-Forwarded-*`, `Via` and request-id headers and runs
  the full policy pipeline on every request.
- **With the app on a Unix socket, the proxy reaches 50,000 req/s** for
  39 µs of CPU a request, 93% of what nginx does over TCP.
- **The access log costs nothing measurable here:** 43.4 µs a request
  with it and 44.1 without, at 44,900 and 44,400 req/s.
- JWT and a rate limit add 11 µs a request to the proxy, and 0.02 ms at
  the median when it isn't busy.
- Caddy ran with its default `reverse_proxy` settings and wasn't tuned,
  so don't read much into that row. nginx's and Caddy's CPU wasn't
  recorded on this machine (see
  [Against native servers](#against-native-servers)).

### Handler calls

A trivial handler (answer `201` with `hello`), on its own and through
Unsworn, in each SDK. "4 workers" is `workers = 4`. Unsworn, its workers
and the baselines all get the same two vCPUs.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Go on its own | 68,309 | 19.9 | 0.669 | 5.50 | 0.106 | 0.183 | 0.282 | 17 MB |
| Bun on its own, 1 process | 77,103 | 13.0 | 0.708 | 1.51 | 0.082 | 0.158 | 0.281 | 45 MB |
| Bun on its own, 4 processes (reusePort) | 72,055 | 18.4 | 0.436 | 0.928 | 0.099 | 0.193 | 0.298 | 162 MB |
| Node on its own, 1 process | 55,665 | 18.2 | 1.12 | 1.49 | 0.095 | 0.182 | 0.336 | 87 MB |
| Node on its own, 4 processes (cluster) | 73,171 | 22.8 | 0.422 | 2.65 | 0.119 | 0.229 | 0.509 | 405 MB |
| Unsworn → TypeScript on Bun, Tier 2, 1 worker | 38,965 | 46.7 | 1.55 | 4.00 | 0.181 | 0.382 | 1.57 | 120 MB |
| Unsworn → TypeScript on Bun, Tier 2, 4 workers | 32,573 | 60.7 | 1.88 | 5.34 | 0.183 | 0.483 | 2.59 | 348 MB |
| Unsworn → TypeScript on Bun, Tier 1, 4 workers | 34,873 | 56.7 | 1.75 | 4.89 | 0.178 | 0.455 | 2.36 | 340 MB |
| Unsworn → TypeScript on Node, Tier 2, 4 workers | 31,690 | 62.4 | 1.93 | 5.40 | 0.185 | 0.403 | 1.26 | 512 MB |
| Unsworn → Python, Tier 2, 4 workers | 24,518 | 80.5 | 2.50 | 5.19 | 0.332 | 0.622 | 0.813 | 133 MB |
| Unsworn → Python, Tier 1, 4 workers | 26,401 | 74.4 | 2.31 | 4.89 | 0.313 | 0.612 | 0.842 | 133 MB |
| Unsworn → Python, no SDK, Tier 1, 4 workers | 26,113 | 74.5 | 2.49 | 15.05 | 0.171 | 0.344 | 0.572 | 93 MB |
| Unsworn → Go, Tier 2, 4 workers | 28,122 | 70.2 | 2.15 | 5.89 | 0.233 | 0.416 | 0.638 | 117 MB |
| Unsworn → Go, Tier 1, 4 workers | 31,125 | 63.3 | 1.92 | 6.09 | 0.241 | 0.384 | 0.645 | 113 MB |
| Unsworn → Rust, Tier 2, 4 workers | 35,842 | 55.2 | 1.75 | 3.83 | 0.175 | 0.355 | 0.561 | 69 MB |
| Unsworn → Rust, Tier 1, 4 workers | 38,802 | 51.0 | 1.62 | 3.31 | 0.164 | 0.313 | 0.899 | 69 MB |
| Unsworn → Rust, 4 threads in each worker, Tier 2, 4 workers | 25,965 | 76.0 | 2.41 | 4.92 | 0.232 | 0.379 | 0.534 | 68 MB |

- **These rows compare the SDKs, not the languages.** The handler does
  nothing, so each row is Unsworn's side of the call plus that SDK's
  reading and writing of one small message. Bun's is native code, which
  is why TypeScript does as well here as Rust.
- When the server isn't busy, a handler call adds 0.09 to 0.14 ms over
  the language on its own, and about 0.23 ms with the Python SDK.
- At full load, a call costs 47 to 80 µs of CPU, Unsworn's side and the
  worker's together, and two vCPUs handle 24,500 to 39,000 calls a
  second.
- **More workers don't help a handler this small.** One Bun worker
  answers 39,000 calls a second for 47 µs each; four answer 32,600 for
  61 µs. They matter for handlers that do real work.
- **For small requests, Tier 1 was 7 to 11% faster than Tier 2 in every
  SDK** (Bun, Python, Go and Rust). Tier 2's advantage shows with large
  bodies (see [Request bodies](#request-bodies)).
- **The Python SDK costs what a worker without an SDK does** (74 µs a
  request on Tier 1 either way), but the worker without it answers
  sooner when the server isn't busy: 0.17 ms against 0.31 at the median.
  These handlers are sync functions, which the SDK runs on a thread.
- Memory is mostly the workers: four Node processes take about 510 MB,
  four Bun processes about 350 MB, and four Rust processes about 70 MB.
- **A Rust worker uses one thread unless it's given more.** With
  `UNSWORN_WORKER_THREADS=4`, the same handler costs 76 µs a call
  against 55, because every request crosses threads on its way in and
  out of the worker, and it answers later when the server isn't busy
  (0.23 against 0.18 ms at the median).
- Twelve of these 17 rows were within 5% across their three rounds. The
  Go handler on Tier 1 differed by 15%, and the four-worker Bun handler
  on Tier 2 by 9%.

### HTTP runtimes

An ordinary server that Unsworn starts, supervises and forwards to
(`protocol = "http"`, see [`http-runtime.md`](http-runtime.md)). The
servers are the "on its own" baselines from
[`bench/servers/`](../bench/servers), Go's `net/http` and `Bun.serve`,
answering `/text` as the conformance `text` handler does, and started
with Unsworn (`eager = true`). The rows for the same server on its own,
for Unsworn's `proxy` to it and for a handler in the same language
repeat scenarios from the tables above, measured again in this group's
turn.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Go on its own | 68,474 | 19.6 | 0.669 | 5.43 | 0.106 | 0.187 | 0.251 | 17 MB |
| Unsworn `proxy` → Go, which Unsworn didn't start (Unix socket) | 48,665 | 40.1 | 1.30 | 2.10 | 0.190 | 0.328 | 0.917 | 28 MB |
| Unsworn → Go server, on a socket, 1 server | 31,765 | 57.6 | 1.95 | 4.77 | 0.228 | 0.355 | 0.590 | 49 MB |
| Unsworn → Go server, on a port, 1 server | 27,669 | 68.8 | 2.26 | 4.82 | 0.212 | 0.351 | 0.558 | 50 MB |
| Unsworn → Go server, on a socket, 4 servers | 27,682 | 69.4 | 2.12 | 7.42 | 0.219 | 0.371 | 0.524 | 109 MB |
| Unsworn → Go handler, Tier 2, 4 workers | 29,533 | 65.8 | 1.97 | 6.68 | 0.220 | 0.384 | 0.540 | 112 MB |
| Bun on its own, 1 process | 76,977 | 13.0 | 0.707 | 1.51 | 0.081 | 0.154 | 0.266 | 45 MB |
| Unsworn → Bun server, on a socket, 1 server | 38,872 | 48.6 | 1.58 | 2.68 | 0.163 | 0.327 | 0.497 | 77 MB |
| Unsworn → Bun server, on a socket, 4 servers | 31,422 | 63.1 | 2.00 | 4.38 | 0.167 | 0.329 | 0.503 | 206 MB |
| Unsworn → TypeScript handler on Bun, Tier 2, 4 workers | 32,907 | 60.1 | 1.88 | 4.78 | 0.175 | 0.401 | 0.824 | 295 MB |

- **The CPU column counts the server when Unsworn started it.** It's
  Unsworn's child, on Unsworn's vCPUs, as a handler's workers are. The
  `proxy` row's Go server isn't counted: it's the upstream, on a vCPU of
  its own. That's why the `proxy` row costs 40 µs and the HTTP runtime
  58: the difference is about what the Go server takes on its own
  (20 µs).
- **Forwarding to a server costs less than calling a handler in the
  same language.** The Go server takes 58 µs a request and answers
  31,800 a second, where a Go handler takes 66 µs and answers 29,500.
  The Bun server takes 49 µs and answers 38,900, where a TypeScript
  handler on Bun with four workers takes 60 µs and answers 32,900. Both
  servers are built to speak HTTP; a handler call goes through the SDK's
  own reading and writing at the worker's end.
- **When it isn't busy, the Go server adds about 0.12 ms** at the median
  over Go on its own (0.23 against 0.11 ms), and the Bun server about
  0.08 ms (0.16 against 0.08 ms).
- **The socket is cheaper than the port at load.** On a port, the Go
  server costs 69 µs a request against 58 and answers 27,700 a second
  against 31,800. When it isn't busy, the two are within 0.02 ms. The
  socket is the default, and the one that keeps other processes from
  reaching the server.
- **One server is enough for these.** Both already use every core they
  get, so four copies on the same two vCPUs cost more: 69 µs a request
  against 58 for Go, 63 against 49 for Bun, with two to three times the
  memory. `workers` is for a server that can't use more than one core on
  its own, which wasn't measured here.
- Seven of these ten rows were within 5% across their three rounds, and
  all within 8%.

### Calls from an HTTP runtime

A server reaches a [function](functions.md) or an
[outbound service](outbound.md) through a local endpoint Unsworn runs for
it (see
[`http-runtime.md`](http-runtime.md#calling-functions-and-outbound-services)).
These rows make one call per request:

- `POST /relay?fn=text` calls the function `text`, which is the
  conformance `text` handler on four Bun workers, and answers
  `{"answer":"hello"}`.
- `POST /outbound?path=/echo` sends its 1 KB body to an outbound service,
  the Go upstream, which sends it back, and answers with what came back.

The servers are the ones above, one of each, on a socket. They call the
endpoint over its Unix socket (`UNSWORN_CALLS_SOCKET`) or its port
(`UNSWORN_CALLS_URL`), with an HTTP client that keeps its connections.
The handlers are the conformance `relay` and `outbound` handlers, with
`ctx.call` and `ctx.fetch` (`req.Call` and `req.Fetch` in Go). The
TypeScript handler shares the four Bun workers with the function, as it
would in a project; the Go handler has four workers of its own. The CPU
column counts Unsworn, the server or the handler's workers, and the
function's workers, but not the Go upstream.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Go server, `GET /text`, with `calls` (a token per request) | 30,209 | 59.9 | 2.04 | 4.80 | 0.219 | 0.365 | 0.569 | 197 MB |
| Go server → function, endpoint's socket | 10,693 | 179 | 5.72 | 12.80 | 0.484 | 1.17 | 5.97 | 379 MB |
| Go server → function, endpoint's port | 9,378 | 205 | 6.49 | 14.40 | 0.512 | 1.30 | 7.10 | 365 MB |
| Go handler → function (`req.Call`), Tier 2 | 13,263 | 148 | 4.54 | 12.29 | 0.419 | 1.40 | 12.70 | 449 MB |
| Bun server → function, endpoint's socket | 12,159 | 156 | 5.08 | 9.91 | 0.452 | 1.14 | 7.75 | 406 MB |
| Bun server → function, endpoint's port | 11,502 | 168 | 5.39 | 10.01 | 0.456 | 1.30 | 6.85 | 402 MB |
| TypeScript handler on Bun → function (`ctx.call`), Tier 2 | 15,964 | 123 | 3.85 | 10.94 | 0.325 | 0.977 | 3.71 | 378 MB |
| Go server → the app directly, without Unsworn | 15,305 | 120 | 3.86 | 10.46 | 0.379 | 0.673 | 1.42 | 287 MB |
| Go server → service, endpoint's socket | 11,084 | 165 | 5.45 | 10.86 | 0.469 | 0.842 | 2.80 | 283 MB |
| Go server → service, endpoint's port | 10,447 | 182 | 5.80 | 11.42 | 0.502 | 0.842 | 2.02 | 281 MB |
| Go handler → service (`req.Fetch`), Tier 2 | 11,850 | 165 | 5.20 | 12.25 | 0.464 | 0.774 | 4.82 | 352 MB |
| Bun server → service, endpoint's socket | 12,700 | 148 | 4.93 | 7.85 | 0.476 | 0.747 | 1.50 | 310 MB |
| TypeScript handler on Bun → service (`ctx.fetch`), Tier 2 | 13,169 | 150 | 4.70 | 9.88 | 0.383 | 0.759 | 3.35 | 332 MB |

- **A call to a function costs more from a server than from a
  handler:** 179 µs a request from the Go server against 148 from a Go
  handler, and 156 from the Bun server against 123 from a TypeScript
  handler. The server's call is a second HTTP request that Unsworn
  parses and matches to its token; a handler's goes as a message over
  the connection its worker already has.
- **A fetch from a service costs the same either way:** 165 µs from the
  Go server or a Go handler, and 148 from the Bun server against 150
  from a TypeScript handler.
- **The endpoint adds about 45 µs to a fetch.** The Go server sending
  the same body straight to the upstream costs 120 µs a request and
  manages 15,300 a second. Through the endpoint, where Unsworn checks the
  token and the service's rules and sends the request on, it's 165 µs
  and 11,100. When the server isn't busy, that's 0.47 ms at the median
  against 0.38.
- **The endpoint's socket is cheaper than its port,** by 17 to 26 µs a
  call for the Go server's client and 12 µs for Bun's.
- **A server that may call pays for a token on every forwarded
  request,** whether it makes a call or not. The Go server's `GET /text`
  costs 60 µs here with `calls = ["*"]`, against 58 for the same server
  without `calls` (in [HTTP runtimes](#http-runtimes)). A server whose
  runtime has no `calls` gets no token, so one that never calls pays
  nothing.
- The memory column includes the four Bun workers that run the function.

### A handler that does work

The handler above does nothing, so it can't show what a language costs.
This one searches: it holds 2,000 product names in memory and answers
with the five closest to `?q=` by edit distance, which takes about a
quarter of a million steps of a loop in the handler's own code. It's
written the same way in each language, in
[`bench/handlers/`](../bench/handlers), and every version's answer is
checked against the same expected result. Each runs on Tier 2 with 4
workers. The latency columns are at 20 requests a second, a rate every
language keeps up with.

| Scenario | Req/s | CPU µs/req | p50 ms @ 20/s | p99 ms @ 20/s | Peak memory |
|---|--:|--:|--:|--:|--:|
| Unsworn → TypeScript on Bun | 3,148 | 635 | 1.16 | 1.56 | 253 MB |
| Unsworn → TypeScript on Node | 2,409 | 831 | 1.28 | 1.82 | 479 MB |
| Unsworn → Python | 93 | 21,620 | 21.04 | 28.44 | 119 MB |
| Unsworn → Go | 3,678 | 545 | 1.05 | 1.43 | 111 MB |
| Unsworn → Rust | 5,693 | 344 | 0.804 | 1.25 | 66 MB |

- **Rust does this work for 0.34 ms of CPU, Go for 0.55 ms, Bun for
  0.64 ms, Node for 0.83 ms and Python for 22 ms.** So moving this
  endpoint from TypeScript on Bun to Rust gives 1.8 times the requests
  per core, from Node 2.4 times as many, and from Python about 60 times.
- The call into the handler is 50 to 80 µs of each figure (see
  [Handler calls](#handler-calls)), which is why it decides the "hello"
  rows above and matters less here.
- One request on a quiet server takes 0.8 ms in Rust, 1.1 ms in Go,
  1.2 ms on Bun, 1.3 ms on Node and 21 ms in Python.
- The Rust workers take 66 MB between them, about a quarter of Bun's
  253 MB and about a seventh of Node's 479 MB.
- A loop over small integers is the kind of code JavaScript engines
  compile well, so the gap between TypeScript and Rust is likely wider
  for work that builds many objects or strings. Python code that needed
  this speed would normally call a C library for it.

### Policies

One policy at a time on a static `respond` route, so each row's CPU µs/req
less the first row's is that policy's cost. A body schema only applies to
routes that pass the body on, so the body rows are a `proxy` to the Go
server, with and without the schema.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| No policy | 75,627 | 21.3 | 0.417 | 0.861 | 0.095 | 0.201 | 0.291 | 27 MB |
| JWT (HS256) | 77,048 | 24.3 | 0.403 | 0.911 | 0.105 | 0.208 | 0.307 | 27 MB |
| API key | 77,744 | 22.6 | 0.403 | 0.845 | 0.101 | 0.205 | 0.323 | 27 MB |
| Rate limit, memory store | 76,684 | 23.0 | 0.409 | 0.853 | 0.093 | 0.185 | 0.290 | 27 MB |
| Rate limit, Redis store | 63,774 | 30.2 | 0.954 | 1.56 | 0.192 | 0.345 | 0.585 | 27 MB |
| CORS (request with Origin) | 75,523 | 22.5 | 0.415 | 0.855 | 0.099 | 0.202 | 0.304 | 27 MB |
| Path and query validation | 76,485 | 22.7 | 0.408 | 0.861 | 0.104 | 0.221 | 0.348 | 27 MB |
| All of JWT, rate limit, CORS, path and query | 74,631 | 26.0 | 0.712 | 1.30 | 0.111 | 0.227 | 0.341 | 27 MB |
| `proxy`, 1 KB JSON body | 40,261 | 48.6 | 1.57 | 2.73 | 0.196 | 0.327 | 0.517 | 32 MB |
| `proxy`, same body checked against a JSON Schema | 35,370 | 55.7 | 1.79 | 2.91 | 0.225 | 0.361 | 0.804 | 31 MB |

| Policy | Added CPU per request |
|---|--:|
| API key, rate limit (memory), CORS, path and query validation | 1 to 2 µs each |
| JWT, HS256 | 3 µs |
| JSON Schema on a 1 KB body | 7 µs |
| Rate limit, Redis store | 9 µs |

- None of them adds more than 0.03 ms of latency at 1,000 req/s, except
  Redis's round trip (0.10 ms).
- The req/s column barely moves for the static rows because they're at
  the load generator's limit; the CPU column is what shows the cost.
- The body schema here has 12 array items, each with a regex `pattern`.

### MCP

The same Bun handler as a route and as an MCP tool (a route with
`mcp = { tool = ... }`), a `[[mcp.tool]]` whose arguments are checked
against its input schema, and `tools/list`.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| `GET /text` (the route) | 32,960 | 59.8 | 1.85 | 5.24 | 0.182 | 0.512 | 3.39 | 353 MB |
| `tools/call text` (the same handler) | 24,353 | 81.4 | 2.52 | 5.94 | 0.216 | 0.463 | 1.25 | 296 MB |
| `tools/call echo`, arguments checked against its schema | 21,615 | 92.1 | 2.85 | 6.74 | 0.214 | 0.456 | 1.33 | 303 MB |
| `tools/list` | 74,285 | 26.8 | 0.830 | 1.40 | 0.118 | 0.208 | 0.307 | 244 MB |

- A tool call costs about 1.4 times the route it wraps (81 against
  60 µs of CPU), and adds about 0.03 ms of latency: the JSON-RPC
  body is parsed and checked against the MCP headers, and the reply is
  turned into a tool result.
- `tools/list` is answered by Unsworn alone, at about 27 µs.

### Route table size

A `respond` route at the end of a table of 10, 1,000 and 10,000 routes,
each with a path parameter, and a catch-all behind them.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| 10 routes | 76,213 | 21.5 | 0.413 | 0.842 | 0.100 | 0.207 | 0.351 | 26 MB |
| 1,000 routes | 76,823 | 21.9 | 0.413 | 0.841 | 0.099 | 0.218 | 0.372 | 32 MB |
| 10,000 routes | 76,775 | 21.9 | 0.411 | 0.846 | 0.092 | 0.184 | 0.300 | 67 MB |

Matching costs the same at every size, since routes are matched with a
radix tree. Memory grows with the table: 10,000 routes take 67 MB under
load, and 139 MB just after startup (see [Startup](#startup)).

### Request bodies

A `POST` body echoed back: by the Go server on its own, through
Unsworn's `proxy` to it, and by a Bun or Go handler on each protocol tier.
"MB/s in" counts request bodies only; the same amount goes back out. The
JSON rows are different for handlers: the route has no body schema, so
Unsworn sends the body on unread, and the handler answers `204` without
reading it or sending it back. In the "sent back as JSON" rows the
handler reads the body and answers with it as its response's `json`,
inside a small object.

| Scenario | Req/s | MB/s in | CPU µs/req | p50 ms | p99 ms | Peak memory |
|---|--:|--:|--:|--:|--:|--:|
| 1 KB: Go on its own | 65,641 | 67 | 23.2 | 0.718 | 5.40 | 17 MB |
| 64 KB: Go on its own | 8,562 | 561 | 193 | 8.47 | 34.89 | 19 MB |
| 1 MB: Go on its own | 829 | 869 | 1971 | 17.93 | 72.09 | 32 MB |
| 64 KB JSON: Go on its own | 8,636 | 549 | 194 | 8.01 | 38.41 | 22 MB |
| 1 KB: Unsworn `proxy` → Go | 40,310 | 41 | 48.2 | 1.57 | 2.76 | 28 MB |
| 64 KB: Unsworn `proxy` → Go | 8,719 | 571 | 172 | 5.69 | 103 | 84 MB |
| 1 MB: Unsworn `proxy` → Go | 950 | 996 | 1556 | 13.61 | 71.29 | 104 MB |
| 64 KB JSON: Unsworn `proxy` → Go | 9,316 | 593 | 159 | 5.43 | 103 | 59 MB |
| 1 KB: Unsworn → Bun, Tier 2 | 29,839 | 31 | 66.3 | 2.02 | 5.61 | 308 MB |
| 64 KB: Unsworn → Bun, Tier 2 | 8,699 | 570 | 229 | 7.10 | 14.84 | 452 MB |
| 1 MB: Unsworn → Bun, Tier 2 | 655 | 687 | 3019 | 24.08 | 38.53 | 532 MB |
| 64 KB JSON: Unsworn → Bun, Tier 2 | 14,806 | 942 | 135 | 4.00 | 10.36 | 408 MB |
| 64 KB JSON, sent back as JSON: Unsworn → Bun, Tier 2 | 4,087 | 260 | 488 | 15.18 | 29.48 | 398 MB |
| 1 KB: Unsworn → Bun, Tier 1 | 32,608 | 33 | 60.7 | 1.85 | 5.18 | 299 MB |
| 64 KB: Unsworn → Bun, Tier 1 | 7,046 | 462 | 283 | 8.88 | 18.12 | 404 MB |
| 1 MB: Unsworn → Bun, Tier 1 | 561 | 588 | 3458 | 28.34 | 40.72 | 455 MB |
| 1 KB: Unsworn → Go, Tier 2 | 26,228 | 27 | 75.0 | 2.27 | 6.66 | 112 MB |
| 64 KB: Unsworn → Go, Tier 2 | 7,570 | 496 | 262 | 8.01 | 18.90 | 144 MB |
| 1 MB: Unsworn → Go, Tier 2 | 602 | 631 | 3317 | 25.91 | 47.26 | 211 MB |
| 64 KB JSON: Unsworn → Go, Tier 2 | 10,561 | 672 | 187 | 5.61 | 17.27 | 129 MB |
| 64 KB JSON, sent back as JSON: Unsworn → Go, Tier 2 | 3,266 | 208 | 611 | 18.64 | 44.36 | 133 MB |
| 1 KB: Unsworn → Go, Tier 1 | 28,813 | 30 | 68.7 | 2.07 | 6.78 | 111 MB |
| 64 KB: Unsworn → Go, Tier 1 | 6,684 | 438 | 296 | 9.07 | 21.31 | 162 MB |
| 1 MB: Unsworn → Go, Tier 1 | 530 | 556 | 3725 | 29.46 | 48.30 | 222 MB |
| 1 KB: Unsworn → Go server (HTTP runtime), 1 server | 28,759 | 29 | 63.4 | 2.13 | 5.38 | 49 MB |
| 64 KB: Unsworn → Go server (HTTP runtime), 1 server | 5,868 | 385 | 306 | 11.30 | 38.70 | 103 MB |
| 1 MB: Unsworn → Go server (HTTP runtime), 1 server | 690 | 723 | 2750 | 22.51 | 38.70 | 143 MB |
| 1 KB: Unsworn → Bun server (HTTP runtime), 1 server | 34,865 | 36 | 53.4 | 1.78 | 3.09 | 78 MB |
| 64 KB: Unsworn → Bun server (HTTP runtime), 1 server | 12,916 | 846 | 144 | 4.51 | 8.05 | 121 MB |
| 1 MB: Unsworn → Bun server (HTTP runtime), 1 server | 938 | 983 | 1975 | 16.90 | 26.16 | 180 MB |

- The proxy streams bodies: it moves about 1 GB/s of 1 MB bodies in
  about 100 MB of memory. Its 64 KB and 1 MB rows are limited by the
  upstream, which has a single vCPU here.
- A handler has to receive the whole body and send it back, so large
  bodies cost more than through the proxy: 630 to 690 MB/s of 1 MB
  bodies for Go and Bun on Tier 2.
- **Tier 2 beats Tier 1 at 64 KB and above:** by 23% and 17% for Bun,
  and 13% and 14% for Go. A pipe holds 64 KB, so a large body likely
  takes more reads and writes on Tier 1.
- 1 MB bodies cost 3.0 to 3.7 ms of CPU each through a handler, 1.5 to
  1.9 times what the Go server takes on its own.
- **An HTTP runtime streams bodies both ways,** as `proxy` does, so a
  large one costs less than through a handler: 2.0 ms of CPU for 1 MB
  through the Bun server and 2.75 ms through the Go one, against 3.0 and
  3.3 ms through a Tier 2 handler in the same language, in 143 to 180 MB
  of memory against 211 to 532 MB. At 64 KB the Bun server costs 144 µs
  against 229 for a Bun handler, but the Go server costs more than a Go
  handler (306 against 262 µs). These counts include the server's own
  copy of the body, which it reads whole before it sends it back.
- **A JSON body a handler doesn't read costs what carrying it does.** A
  route with no body schema hands the body to its handler as it arrived,
  and the SDK parses it when the handler first asks for it. Unread, 64 KB
  of JSON costs 0.14 ms of CPU to a Bun handler and 0.19 ms to a Go one,
  at 14,800 and 10,600 requests a second. These rows don't send the body
  back, which is why they cost less than the 64 KB rows of raw bytes,
  which do. A `proxy` route without a body schema passes JSON through
  unread too.
- **Reading 64 KB of JSON and sending it back costs 0.49 to 0.61 ms of
  CPU,** at 4,100 requests a second for Bun and 3,300 for Go. The worker
  parses the body and writes it out again, and Unsworn sends what the
  worker wrote to the client.

### Startup

How long `unsworn serve` takes to answer its first request, by the size
of its route table:

| Routes | Start to first response, ms | Memory at idle |
|--:|--:|--:|
| 10 | 5.58 | 22 MB |
| 1,000 | 14.06 | 40 MB |
| 10,000 | 90.43 | 139 MB |

The first request to a handler or an HTTP runtime, with the runtime's
workers or server started on that first request (the default) or at
boot (`eager = true`):

| Runtime | Lazy: ready, ms | Lazy: first request, ms | Lazy: second request, ms | Eager: ready, ms |
|---|--:|--:|--:|--:|
| Bun, Tier 2 | 6.92 | 140 | 0.525 | 141 |
| Node, Tier 2 | 5.99 | 407 | 1.15 | 374 |
| Python, Tier 2 | 5.89 | 197 | 0.647 | 199 |
| Go, Tier 2 | 4.74 | 11.89 | 0.392 | 17.92 |
| Rust, Tier 2 | 5.73 | 7.34 | 0.350 | 11.43 |
| Go `net/http` server, HTTP runtime | 4.89 | 7.19 | 0.542 | 12.70 |
| `Bun.serve` server, HTTP runtime | 4.85 | 21.80 | 0.522 | 26.38 |

Lazy start moves the workers' startup from boot to the first request:
about 7 to 12 ms for compiled workers, 140 ms for Bun, and 200 to 400 ms
for Python and Node.

**An HTTP runtime's first request takes 7 ms with the Go server and
22 ms with the Bun one.** Unsworn looks for the server listening 1 ms
after starting it, and then doubles the wait each time, up to 25 ms.

These times depend on the machine's state as much as on Unsworn: the
tables were taken at the end of the run, after more than an hour and a
half of load.

**The wasm core on a serverless function.** A fresh Node process loads
the core through the Node host (`@unswornjs/sdk/node`), the way a Vercel,
Netlify or Lambda function does on a cold start, and answers a `POST`
with a body schema. The platform's own startup isn't included.

| Routes | Node started, ms | SDK imported, ms | First response, ms | Total, ms | Warm request, µs | First request to another route, ms |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 18.70 | 53.74 | 85.63 | 158 | 193 | – |
| 100 | 18.03 | 50.75 | 95.28 | 166 | 192 | 4.77 |
| 1,000 | 18.72 | 53.17 | 165 | 244 | 194 | 4.84 |

- A cold start answers about 160 ms after Node starts, with most of it in
  importing the SDK and compiling the wasm module.
- Warm, a request through the wasm core costs about 190 µs, against
  about 21 µs for a static answer from the binary.
- Each route's first request costs about 5 ms more, since the core
  compiles a route's schemas when a request first needs them. On a busy
  function that's paid once per route per instance.

### Sizes

| | |
|---|--:|
| `unsworn` binary (x86_64 glibc, as built for this run) | 19.7 MB |
| wasm core (`unsworn_bg.wasm`) | 4.7 MB, 1.44 MB gzipped |

## Running them

```sh
cargo install oha                 # wrk comes from your package manager
python3 bench/run.py build        # Unsworn, the SDK workers, the Go upstream and HTTP runtime, the wasm core
python3 bench/run.py run          # about an hour and three quarters; writes bench/results/<date>-<cpu>.json
python3 bench/run.py report bench/results/<file>.json   # these tables, as markdown
```

`run` measures `target/dist/unsworn`, which `build` makes. To measure
another binary, such as a released one, set `UNSWORN_BIN` to its path.

`run` takes `--only` with a comma-separated list of groups
(`declarative`, `proxy`, `handlers`, `http`, `calls`, `work`,
`policies`, `mcp`, `routes`, `bodies`, `startup`), and `--quick` for a single 3-second pass to check the
harness. It needs Go, Bun, Node and Python 3 for the handler groups, and
Docker for the Caddy, nginx and Redis rows, which it skips without Docker.
It waits for the machine to go quiet before it starts: anything else
running (a build, a browser) skews the results.

Each scenario's config is in [`bench/projects/`](../bench/projects), or
generated by `bench/run.py` into `target/bench/projects/` for the handler,
HTTP runtime and route-count groups.

A dedicated server with more cores and a fixed CPU frequency would give
steadier numbers, and lift the load generator's limit on the fastest
rows; results from one are welcome in `bench/results/`.
