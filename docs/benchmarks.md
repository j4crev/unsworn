# Benchmarks

What Unsworn costs, measured: in front of an existing app, around a
handler call, in front of a server it starts (an HTTP runtime), for each
policy, and at startup. The harness is in [`bench/`](../bench), and
every number here comes from one run of it against the released v0.5.0
binary, saved in
[`bench/results/2026-11-21-v0.5.0-intel-core-i5-1145g7.json`](../bench/results/2026-11-21-v0.5.0-intel-core-i5-1145g7.json),
except where a paragraph says it's from a separate comparison.

## Summary

On two cores of a laptop CPU (an Intel i5-1145G7, details in
[Setup](#setup)):

| What Unsworn does | Requests/s | CPU per request | p50 latency at 1,000 req/s |
|---|--:|--:|--:|
| Answers a static `respond`, `redirect`, 404 or rejection | 179,000–220,000 | 15–19 µs | 0.16 ms (Go `net/http`: 0.15 ms) |
| `proxy` to an existing app | 70,000 | 45 µs | 0.22 ms (the app directly: 0.16 ms) |
| `proxy` with JWT and a rate limit | 58,000 | 61 µs | 0.25 ms |
| Calls a TypeScript handler on Bun that only answers "hello" | 71,000 | 50 µs | 0.23 ms (Bun on its own: 0.15 ms) |
| Calls a Rust handler that only answers "hello" | 71,000 | 50 µs | 0.21 ms |
| Forwards to a Go `net/http` server it started (HTTP runtime) that only answers "hello" | 60,000 | 60 µs | 0.15 ms (Go on its own: 0.16 ms) |
| Forwards to a `Bun.serve` server it started that only answers "hello" | 75,000 | 43 µs | 0.22 ms |
| Calls a TypeScript handler on Bun that searches 2,000 names | 3,300 | 1.2 ms | 1.2 ms at 20 req/s |
| Calls the same search handler in Rust | 4,900 | 0.82 ms | 1.0 ms at 20 req/s |

The "hello" rows measure the call into a handler, which costs about the
same in every language. They say nothing about how fast a language runs
your code. The search rows do: see
[A handler that does work](#a-handler-that-does-work).

- **Requests Unsworn answers itself cost 15 to 25 µs of CPU each.** Go's
  standard library serving "hello" takes 18 µs.
- **A handler call or a proxied request adds about 0.05 to 0.11 ms** when
  the server isn't overloaded. It costs 39 to 62 µs of CPU for the proxy
  and for a Bun, Node or Rust handler, about 80 µs for a Go handler and
  115 µs for a Python one.
- **Policies are cheap,** 2 to 4 µs each, except JWT verification (8 µs)
  and JSON Schema validation of a body (about 19 µs for 1 KB).
- **A JSON body costs what's done with it.** A 64 KB JSON body that a
  handler doesn't read costs 0.15 to 0.24 ms of CPU, about what raw bytes
  do. Read by the handler and sent back, it costs 1.2 to 1.4 ms.
- **Route count doesn't matter:** 10,000 routes match as fast as 10.
- **An HTTP runtime costs what a handler call does, or less.** Unsworn
  forwarding to a Go or Bun server it started, on a Unix socket, costs
  43 to 60 µs of CPU a request, the server's own work included, against
  50 to 79 µs for a handler in the same language. A 1 MB body costs
  half to two thirds of what it does through a handler, since it's
  streamed. A request that makes one call to a function or a service,
  through Unsworn's local endpoint, costs 157 to 213 µs in all: 12 to
  43 µs more than the same request to a handler using `ctx.call` or
  `ctx.fetch`.
- **The binary is ready in about 3 ms** with 10 routes and about 160 ms
  with 10,000, on a machine that's already busy (see
  [Startup](#startup)). On a serverless function, the wasm core answers
  its first request about 145 ms after Node starts.
- **A trivial handler won't beat its language on its own.** Bun serves
  "hello" at 166,000 req/s on one core by itself, and at 71,000 through
  Unsworn. What Unsworn adds is the rules in front of it, and the same
  rules for every language.
- **The language matters once the handler does work.** One search
  handler, written the same way in each language, costs 0.82 ms of CPU a
  request in Rust, 1.2 ms in TypeScript on Bun and 53 ms in Python.

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
  is what a request costs when the server isn't overloaded. It starts
  once the server has finished whatever the throughput run left behind.
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
  answer the same thing. The exceptions are the search handler in
  [`bench/handlers/`](../bench/handlers), described under
  [A handler that does work](#a-handler-that-does-work), and the HTTP
  runtimes, which are the baseline servers in
  [`bench/servers/`](../bench/servers) started by Unsworn.

**How precise they are.** This is a laptop with a `powersave` CPU
governor. Of the 107 scenarios, 48 had their three rounds within 2% of
each other, and 75 within 5%. But 17 differed by 12% or more, 12 of
those because the first round was the slow one. Three policy rows each
had one round far off, during the same minute: the API key did 82,500
req/s in its first round and 180,000 in the other two, CORS 114,000 in
one round against 190,000, and all the policies together 84,000 against
114,000. The tables show the median of the three, so one slow round
doesn't move it.

The processor also slows under load that lasts. In a separate test of
the `proxy` route, its clock fell from about 2.9 to 2.6 GHz once the load
had run for about 25 seconds, and the route went from 73,000 to 63,000
req/s. So where a row falls in the run matters, and a difference of
under 15% between two rows may be only that.

The fastest scenarios are close to what the load generator can send from
its one core (the most it sent here was 245,000 req/s), so they are
partly measuring wrk; their CPU µs/req column is the better guide.

**Against v0.2.0.** The two releases' binaries, in one run each on this
machine: a static `respond` went from 52,700 to 148,200 req/s, and a Bun
handler with 4 workers from 4,100 to 50,800. v0.2.0 used musl's
allocator, which takes one lock for every thread, and scanned a record of
recent calls on every handler call, which capped every runtime at about
4,500 calls a second. v0.3.0 uses mimalloc and expires that record
without scanning it.

**Against v0.3.0.** The same suite on this machine a day apart, so only
the larger changes mean anything:

| Scenario | v0.3.0 req/s | v0.3.1 req/s |
|---|--:|--:|
| JWT (HS256) on a static route | 88,069 | 111,469 |
| `proxy` with JWT and a rate limit | 40,413 | 47,640 |
| 64 KB JSON body to a Bun handler | 1,356 | 3,290 |
| 64 KB JSON body to a Go handler | 1,147 | 2,067 |
| 1 MB body to a Bun handler, Tier 2 | 607 | 1,017 |
| 1 MB body to a Bun handler, Tier 1 | 410 | 886 |
| Rust handler, Tier 1 | 39,914 | 59,075 |

v0.3.0 found a JWT scheme's keys under one lock shared by every thread,
built a JSON body into a value and wrote it out again for the worker,
copied large bodies several times on their way through Unsworn and the JS
SDK, and had the Rust SDK read stdin through a thread pool.

**Against v0.3.1.** The same suite on this machine three days apart.
v0.3.1's figures are from its run and, for the "hello" handler rows, a
second run of the handler groups that day; both files are in this
repository's history under `bench/results/`. The rows that don't involve
Unsworn (Go, Bun and Node on their own, the app directly, Caddy, nginx)
moved by 7% at most between the two.

| Scenario | v0.3.1 req/s | v0.4.0 req/s | v0.3.1 CPU µs/req | v0.4.0 CPU µs/req |
|---|--:|--:|--:|--:|
| `respond` (static JSON) | 130,962 | 178,977 | 26.1 | 18.7 |
| All of JWT, rate limit, CORS, path and query | 79,867 | 117,088 | 42.9 | 30.4 |
| `proxy` | 58,800 | 69,999 | 58.9 | 45.0 |
| `proxy` with JWT and a rate limit | 47,640 | 59,368 | 74.9 | 58.8 |
| `tools/list` | 69,915 | 94,610 | 52.5 | 38.5 |
| Bun handler, Tier 2, 4 workers | 63,241 | 70,796 | 58.0 | 50.5 |
| Rust handler, Tier 2 | 57,159 | 70,489 | 61.0 | 50.6 |
| Python handler, Tier 2 | 25,344 | 32,403 | 139 | 116 |
| 64 KB JSON body to a Bun handler | 3,290 | 25,472 | 1,193 | 146 |
| 64 KB JSON body to a Go handler | 2,067 | 15,528 | 1,905 | 243 |
| 64 KB JSON, sent back, Bun handler | 2,166 | 3,324 | 1,818 | 1,187 |
| 64 KB JSON, sent back, Go handler | 1,266 | 2,885 | 3,108 | 1,360 |

v0.4.0 puts each access log line together itself and writes them in
batches, counts its metrics per thread, reaches a `proxy` upstream
through an HTTP client of its own that keeps idle connections per
thread, and sends a JSON body to a worker as it arrived, to be read by
whoever needs it. The search handler's rows didn't move: they measure
the handler's own code.

**Against v0.4.0.** v0.5.0 adds HTTP runtimes and their calls, and
changes nothing on the paths the earlier tables measure. Every one of
the 78 scenarios the two runs share, two days apart, is within 5% of
its v0.4.0 figure, in either direction. v0.4.0's results file is in this
repository's history under `bench/results/`.

### Setup

| | |
|---|---|
| CPU | Intel Core i5-1145G7 (4 cores, 8 threads), `powersave` governor |
| Memory | 16 GB |
| OS | Linux 7.2 |
| Unsworn | 0.5.0, the released `x86_64-unknown-linux-musl` binary |
| Runtimes | Bun 1.4.4, Node 26.13.0, Python 3.14.8, Go 1.27.1, Rust 1.97.1 |
| Comparisons | Caddy 2.11.9, nginx 1.30.7 and Redis 7.4.12, in Docker with host networking |
| Load | wrk (git `f8eb608`), oha 1.17.0 |

## Results

### Answers without a handler

Routes Unsworn answers itself, with a handler call on the same server for
comparison. Go's `net/http` answering "hello" is a reference for what any
HTTP server costs on this machine.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Go net/http, for reference | 189,524 | 17.7 | 0.304 | 1.33 | 0.151 | 0.375 | 0.427 | 17 MB |
| `respond` (static JSON) | 178,550 | 18.7 | 0.351 | 0.621 | 0.158 | 0.348 | 0.390 | 130 MB |
| `redirect` | 220,375 | 15.2 | 0.282 | 0.502 | 0.155 | 0.374 | 0.397 | 128 MB |
| No route (404) | 211,586 | 15.9 | 0.294 | 0.549 | 0.154 | 0.394 | 0.502 | 129 MB |
| Rejected: missing JWT (401) | 195,149 | 17.4 | 0.320 | 0.598 | 0.158 | 0.407 | 0.467 | 129 MB |
| Rejected: rate limited (429) | 184,473 | 18.6 | 0.340 | 0.630 | 0.161 | 0.381 | 0.408 | 128 MB |
| Cache hit | 145,865 | 23.9 | 0.432 | 0.806 | 0.164 | 0.390 | 0.438 | 129 MB |
| Same handler, uncached (Bun, Tier 2) | 70,653 | 50.6 | 0.811 | 5.09 | 0.222 | 0.612 | 0.842 | 357 MB |

- A static answer, a redirect, a 404, a rejection and a cache hit all cost
  about the same: 15 to 25 µs of CPU. None of them reaches a worker.
- A cache hit costs about half of the handler call it replaces.
- Memory here includes the four Bun workers this project starts for its
  handler routes. Unsworn on its own uses 20 to 33 MB (see
  [Startup](#startup)).

### In front of an existing app

Unsworn's `proxy` in front of a Go server, compared with calling the
server directly and with Caddy and nginx in the same place. One row has
the Go server on a Unix socket (`proxy = "unix:/path"`) in place of a
TCP port.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Direct to the app, no proxy | 186,699 | 17.9 | 0.308 | 1.39 | 0.155 | 0.350 | 0.452 | 17 MB |
| Unsworn `proxy` | 69,593 | 44.6 | 0.873 | 2.48 | 0.219 | 0.485 | 0.824 | 30 MB |
| Unsworn `proxy` with JWT and a rate limit | 57,575 | 60.6 | 1.06 | 2.48 | 0.254 | 0.468 | 0.803 | 31 MB |
| Unsworn `proxy`, access log off | 59,137 | 51.2 | 1.04 | 2.98 | 0.255 | 0.467 | 0.721 | 28 MB |
| Unsworn `proxy`, the app on a Unix socket | 87,567 | 39.6 | 0.701 | 1.85 | 0.140 | 0.374 | 0.445 | 29 MB |
| Caddy `reverse_proxy` | 31,117 | 116 | 1.85 | 6.39 | 0.178 | 0.353 | 0.632 | 25 MB |
| nginx `proxy_pass`, keepalive, access log off | 87,998 | 23.0 | 0.696 | 2.08 | 0.216 | 0.463 | 0.694 | 13 MB |

- Unsworn adds about 0.06 ms at the median when it isn't busy, and
  handles 70,000 req/s on two cores. Its upstream connections are reused,
  not opened per request.
- nginx uses half of Unsworn's CPU per request, so it reaches about 1.3
  times the throughput. Unsworn's proxy adds the `Forwarded`,
  `X-Forwarded-*`, `Via` and request-id headers and runs the full policy
  pipeline on every request.
- **With the app on a Unix socket, the proxy reaches 88,000 req/s** for
  40 µs of CPU a request, and adds nothing measurable when it isn't busy
  (0.14 ms, against 0.16 ms for the app directly over TCP).
- **The "access log off" row is lower because of where it falls in the
  run, not because of the log.** It's measured third, on a processor
  that has slowed by then (see
  [How precise they are](#how-theyre-measured)). In a separate test that
  ran the two settings straight after each other, in both orders, the
  route cost 43.5 µs of CPU a request with the log and 41.7 µs without
  it, at 72,000 to 73,000 req/s either way. So the access log is about 2 µs a
  request. It measured the same on a static route: 20.1 µs with it and
  17.9 µs without.
- The JWT and rate limit row is measured second on the same server, so
  part of its 16 µs over the plain row is likely the slower processor
  too. On a static route the two policies cost about 10 µs together (see
  [Policies](#policies)).
- Caddy ran with its default `reverse_proxy` settings and wasn't tuned.
  Its result is lower than it usually reports, so don't read much into
  that row.

### Handler calls

A trivial handler (answer `201` with `hello`), on its own and through
Unsworn, in each SDK. "4 workers" is `workers = 4`, one for each
hardware thread Unsworn has here. Unsworn, its workers and the baselines
all get the same four hardware threads.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Go on its own | 180,133 | 18.8 | 0.320 | 1.36 | 0.154 | 0.362 | 0.407 | 17 MB |
| Bun on its own, 1 process | 165,526 | 4.5 | 0.374 | 0.685 | 0.145 | 0.312 | 0.417 | 37 MB |
| Bun on its own, 4 processes (reusePort) | 244,147 | 10.8 | 0.126 | 0.481 | 0.149 | 0.350 | 0.397 | 146 MB |
| Node on its own, 1 process | 84,298 | 10.2 | 0.737 | 0.960 | 0.159 | 0.399 | 0.468 | 89 MB |
| Node on its own, 4 processes (cluster) | 156,302 | 21.9 | 0.412 | 1.12 | 0.169 | 0.398 | 0.485 | 411 MB |
| Unsworn → TypeScript on Bun, Tier 2, 1 worker | 75,077 | 40.8 | 0.779 | 2.83 | 0.232 | 0.610 | 0.767 | 113 MB |
| Unsworn → TypeScript on Bun, Tier 2, 4 workers | 71,148 | 50.4 | 0.807 | 4.84 | 0.228 | 0.622 | 0.761 | 354 MB |
| Unsworn → TypeScript on Bun, Tier 1, 4 workers | 64,881 | 56.0 | 0.886 | 5.03 | 0.233 | 0.607 | 0.863 | 329 MB |
| Unsworn → TypeScript on Node, Tier 2, 4 workers | 59,144 | 60.7 | 1.00 | 3.01 | 0.252 | 0.699 | 0.843 | 625 MB |
| Unsworn → Python, Tier 2, 4 workers | 32,075 | 116 | 1.88 | 4.36 | 0.236 | 0.576 | 0.940 | 135 MB |
| Unsworn → Python, Tier 1, 4 workers | 32,597 | 114 | 1.85 | 4.33 | 0.222 | 0.569 | 0.877 | 135 MB |
| Unsworn → Python, no SDK, Tier 1, 4 workers | 43,357 | 74.6 | 1.47 | 7.58 | 0.245 | 0.623 | 0.703 | 92 MB |
| Unsworn → Go, Tier 2, 4 workers | 47,695 | 77.4 | 1.25 | 4.05 | 0.193 | 0.385 | 0.704 | 105 MB |
| Unsworn → Go, Tier 1, 4 workers | 45,231 | 80.2 | 1.32 | 4.20 | 0.130 | 0.340 | 0.436 | 106 MB |
| Unsworn → Rust, Tier 2, 4 workers | 71,008 | 50.2 | 0.841 | 2.31 | 0.210 | 0.546 | 0.660 | 64 MB |
| Unsworn → Rust, Tier 1, 4 workers | 90,169 | 39.5 | 0.664 | 1.91 | 0.208 | 0.569 | 0.698 | 62 MB |
| Unsworn → Rust, 4 threads in each worker, Tier 2, 4 workers | 61,169 | 60.6 | 0.979 | 2.57 | 0.159 | 0.428 | 0.627 | 71 MB |

- **These rows compare the SDKs, not the languages.** The handler does
  nothing, so each row is Unsworn's side of the call plus that SDK's
  reading and writing of one small message. Bun's is native code, which
  is why TypeScript does as well here as Rust.
- When the server isn't busy, a handler call adds up to about 0.1 ms over
  the language on its own.
- At full load, a call costs 39 to 80 µs of CPU, Unsworn's side and the
  worker's together, and four threads handle 45,000 to 90,000 calls a
  second in every SDK except Python's, which costs about 115 µs and
  handles 32,000 to 33,000.
- More workers don't help a handler this small: one Bun worker does as
  well as four. They matter for handlers that do real work.
- **The two tiers are close for small requests, except in Rust.** Bun,
  Python and Go were within 10% of each other on the two, in either
  direction. Rust was 27% faster on Tier 1. Tier 2's advantage shows
  with large bodies (see [Request bodies](#request-bodies)).
- **The Python SDK costs about one and a half times what a worker
  without an SDK does** (114 against 75 µs per request on Tier 1). These
  handlers are sync functions, which the SDK runs on a thread.
- Memory is mostly the workers: four Node processes take about 630 MB,
  four Bun processes about 350 MB, and four Rust processes about 60 MB.
- **A Rust worker uses one thread unless it's given more.** With
  `UNSWORN_WORKER_THREADS=4`, the same handler costs 61 µs a call against
  50 µs, because every request crosses threads on its way in and out of
  the worker. The extra threads do answer sooner when the server is
  nearly idle: 0.16 ms against 0.21 ms at 1,000 requests a second.
- The rounds agreed more closely than in earlier runs: every row here
  was within 13% across its three, and all but one within 6%.

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
| Go on its own | 183,837 | 18.2 | 0.315 | 1.34 | 0.158 | 0.333 | 0.373 | 17 MB |
| Unsworn `proxy` → Go, which Unsworn didn't start (Unix socket) | 81,056 | 43.4 | 0.749 | 1.99 | 0.161 | 0.389 | 0.504 | 29 MB |
| Unsworn → Go server, on a socket, 1 server | 59,694 | 59.5 | 0.992 | 3.00 | 0.146 | 0.384 | 0.724 | 53 MB |
| Unsworn → Go server, on a port, 1 server | 45,166 | 73.8 | 1.31 | 3.90 | 0.203 | 0.452 | 0.648 | 54 MB |
| Unsworn → Go server, on a socket, 4 servers | 49,064 | 75.6 | 1.21 | 5.55 | 0.162 | 0.409 | 0.719 | 114 MB |
| Unsworn → Go handler, Tier 2, 4 workers | 46,079 | 79.0 | 1.29 | 4.21 | 0.183 | 0.391 | 0.681 | 105 MB |
| Bun on its own, 1 process | 160,830 | 4.6 | 0.382 | 0.740 | 0.144 | 0.306 | 0.379 | 36 MB |
| Unsworn → Bun server, on a socket, 1 server | 75,486 | 43.4 | 0.822 | 1.53 | 0.218 | 0.551 | 0.705 | 72 MB |
| Unsworn → Bun server, on a socket, 4 servers | 59,721 | 61.4 | 1.01 | 2.59 | 0.206 | 0.566 | 0.730 | 176 MB |
| Unsworn → TypeScript handler on Bun, Tier 2, 4 workers | 70,564 | 50.8 | 0.816 | 4.68 | 0.223 | 0.602 | 0.779 | 330 MB |

- **The CPU column counts the server when Unsworn started it.** It's
  Unsworn's child, on Unsworn's cores, as a handler's workers are. The
  `proxy` row's Go server isn't counted: it's the upstream, on a core of
  its own. That's why the `proxy` row costs 43 µs and the HTTP runtime
  60: the difference is about what the Go server takes on its own
  (18 µs).
- **Forwarding to a server costs less than calling a handler in the
  same language.** The Go server takes 60 µs a request and answers
  60,000 a second, where a Go handler takes 79 µs and answers 46,000.
  The Bun server takes 43 µs and answers 75,000, where a TypeScript
  handler on Bun takes 51 µs and answers 71,000. Both servers are built
  to speak HTTP; a handler call goes through the SDK's own reading and
  writing at the worker's end.
- **When it isn't busy, the Go server adds nothing measurable:** 0.15 ms
  at the median, against 0.16 ms for Go on its own. The Bun server adds
  about 0.07 ms (0.22 against 0.14 ms).
- **The socket is cheaper than the port.** On a port, the Go server
  costs 74 µs a request against 60, answers 45,000 a second against
  60,000, and takes 0.20 ms at the median against 0.15 when it isn't
  busy. The socket is the default, and the one that keeps other
  processes from reaching the server.
- **One server is enough for these.** Both already use every core they
  get, so four copies on the same four hardware threads cost more: 76 µs
  a request against 60 for Go, 61 against 43 for Bun, with two to three
  times the memory. `workers` is for a server that can't use more than
  one core on its own, which wasn't measured here.
- Five of these rows, and the 1 KB and 64 KB HTTP runtime rows under
  [Request bodies](#request-bodies), were 11 to 27% slower in their
  first round than in the other two.

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
| Go server, `GET /text`, in a project with functions (a token per request) | 54,965 | 65.1 | 1.07 | 3.39 | 0.147 | 0.363 | 0.610 | 160 MB |
| Go server → function, endpoint's socket | 17,321 | 213 | 3.46 | 8.29 | 0.243 | 0.573 | 1.47 | 364 MB |
| Go server → function, endpoint's port | 14,971 | 238 | 3.98 | 9.76 | 0.261 | 0.589 | 1.54 | 350 MB |
| Go handler → function (`req.Call`), Tier 2 | 19,818 | 192 | 2.99 | 9.43 | 0.203 | 0.442 | 1.86 | 424 MB |
| Bun server → function, endpoint's socket | 21,934 | 161 | 2.79 | 5.63 | 0.210 | 0.494 | 1.74 | 403 MB |
| Bun server → function, endpoint's port | 20,727 | 164 | 2.96 | 5.94 | 0.218 | 0.480 | 1.35 | 400 MB |
| TypeScript handler on Bun → function (`ctx.call`), Tier 2 | 31,062 | 118 | 1.90 | 6.67 | 0.178 | 0.476 | 1.22 | 369 MB |
| Go server → the app directly, without Unsworn | 22,199 | 156 | 2.64 | 7.26 | 0.211 | 0.388 | 0.720 | 289 MB |
| Go server → service, endpoint's socket | 16,865 | 211 | 3.55 | 7.98 | 0.263 | 0.493 | 0.940 | 289 MB |
| Go server → service, endpoint's port | 15,310 | 226 | 3.95 | 8.72 | 0.266 | 0.489 | 0.823 | 288 MB |
| Go handler → service (`req.Fetch`), Tier 2 | 18,552 | 199 | 3.24 | 8.94 | 0.259 | 0.469 | 1.24 | 346 MB |
| Bun server → service, endpoint's socket | 21,177 | 157 | 2.94 | 5.07 | 0.252 | 0.519 | 0.715 | 313 MB |
| TypeScript handler on Bun → service (`ctx.fetch`), Tier 2 | 25,411 | 144 | 2.35 | 6.86 | 0.239 | 0.537 | 1.25 | 320 MB |

- **A call costs a little more from a server than from a handler.** A
  request that calls the function costs 213 µs from the Go server
  against 192 from a Go handler, and 161 from the Bun server against 118
  from a TypeScript handler. One that fetches from the service costs 211
  against 199 in Go, and 157 against 144 on Bun. The server's call is a
  second HTTP request that Unsworn parses and matches to its token; a
  handler's goes as a message over the connection its worker already
  has.
- **The endpoint adds about 55 µs to a fetch.** The Go server sending
  the same body straight to the upstream costs 156 µs a request and
  manages 22,200 a second. Through the endpoint, where Unsworn checks the
  token and the service's rules and sends the request on, it's 211 µs
  and 16,900. When the server isn't busy, that's 0.26 ms at the median
  against 0.21.
- **The endpoint's socket is cheaper than its port** for the Go server's
  client, by 15 to 25 µs a call. For Bun's client the two were within
  3 µs.
- **A server that may call pays for a token on every forwarded
  request,** whether it makes a call or not. In 0.5.0 that was every
  server in a project with functions or services. The Go server's
  `GET /text` costs 65 µs here against 60 in the project without them. In
  a separate comparison, the two configs taken in turn for six rounds, in
  both orders, the token cost about 7 µs a request (57.4 against
  64.7 µs), and throughput fell from 61,400 to 55,100 req/s. Since this
  run, a token is signed instead of drawn from the system's randomness,
  which took the same request from 64.8 to 59.7 µs in an interleaved
  comparison. A server gets tokens only when its runtime's `calls` names
  something, so one that never calls pays nothing.
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
| Unsworn → TypeScript on Bun | 3,283 | 1,210 | 1.23 | 1.61 | 222 MB |
| Unsworn → TypeScript on Node | 2,404 | 1,658 | 1.56 | 1.84 | 484 MB |
| Unsworn → Python | 76 | 52,848 | 21.59 | 22.48 | 122 MB |
| Unsworn → Go | 4,643 | 854 | 1.08 | 1.34 | 110 MB |
| Unsworn → Rust | 4,863 | 816 | 1.04 | 1.31 | 67 MB |

- **Rust does this work for 0.82 ms of CPU, Go for 0.85 ms, Bun for
  1.2 ms, Node for 1.7 ms and Python for 53 ms.** So moving this endpoint
  from TypeScript on Bun to Rust gives 1.5 times the requests per core,
  from Node twice as many, and from Python about 65 times.
- The call into the handler is about 50 µs of each figure, which is why
  it decides the "hello" rows above and hardly shows here.
- One request on a quiet server takes 1.0 ms in Rust, 1.1 ms in Go,
  1.2 ms on Bun, 1.6 ms on Node and 22 ms in Python.
- The Rust workers take 67 MB between them, under a third of Bun's
  222 MB and about a seventh of Node's 484 MB.
- A loop over small integers is the kind of code JavaScript engines
  compile well, so the gap between TypeScript and Rust is likely wider
  for work that builds many objects or strings. Python code that needed
  this speed would normally call a C library for it.
- **CPU per request is higher at full load than for one request.** One
  Python search takes 22 ms on a quiet server and 53 ms of CPU at full
  load, because the four workers share two cores and the column counts
  hyperthread time. Every language shows it: in a separate comparison
  Rust went from 0.54 ms with one worker busy to 0.84 ms with four.
  Python gains the least from the second hyperthread: two busy workers
  and four both managed 74 searches a second.

### Pools that size themselves

The tables above set `workers` to a number. Left unset, a runtime starts
one worker and adds more while its workers are busy, up to one for each
CPU (see [`workers`](config.md#how-many-workers)). This compares the two,
for the handler that does nothing and the one that searches. It's a
separate comparison from the rest of this page: a build of the branch
that added it (glibc, not the released musl binary), three rounds with
the variants taken in turn, 5 seconds of warmup and 6 of measurement, on
the same machine and cores. Unsworn has four CPUs here, which are two
cores with two hyperthreads each. Each figure is the median of the three
rounds.

| Scenario | `workers` | Req/s | CPU µs/req | Peak memory | Workers it ended with |
|---|---|--:|--:|--:|--:|
| "hello", TypeScript on Bun | 1 | 65,077 | 45.7 | 115 MB | 1 |
| | 4 | 67,330 | 52.7 | 355 MB | 4 |
| | unset | 67,474 | 47.5 | 197 MB | 2 |
| "hello", TypeScript on Node | 1 | 69,677 | 43.3 | 230 MB | 1 |
| | 4 | 65,250 | 55.4 | 630 MB | 4 |
| | unset | 70,589 | 47.4 | 409 MB | 2 |
| "hello", Python | 2 | 27,096 | 112.2 | 85 MB | 2 |
| | 4 | 28,513 | 129.2 | 139 MB | 4 |
| | unset | 28,278 | 125.1 | 109 MB | 3 |
| "hello", Go | 1 | 52,732 | 62.7 | 55 MB | 1 |
| | 4 | 43,084 | 83.8 | 106 MB | 4 |
| | unset | 49,401 | 71.2 | 72 MB | 2 |
| Search, TypeScript on Bun | 4 | 2,661 | 1,493 | 212 MB | 4 |
| | unset | 2,881 | 1,381 | 215 MB | 4 |
| Search, Python | 4 | 64 | 62,578 | 122 MB | 4 |
| | unset | 63 | 63,044 | 126 MB | 4 |
| Search, Rust | 4 | 4,170 | 953 | 63 MB | 4 |
| | unset | 4,157 | 956 | 69 MB | 4 |

- **For the search handler, the pool grew to all four CPUs** in every
  round, within the warmup, and then matched `workers = 4`. Bun's lower
  figure for 4 comes from one slow round (2,074 req/s; the other two were
  2,873 and 2,661), not from the setting.
- **For a handler that does little, it stopped at two workers** (three
  for Python), because two workers sharing the work of one are no longer
  busy. That's 10 to 15% less CPU a call than four workers (3% for
  Python) and about half to three quarters of the memory.
- **It ends one worker above the cheapest number for "hello".** One worker
  (two for Python) answers about as many calls for 4 to 12% less CPU a
  call, and 7% more of them on Go. A single worker that is busy all the
  time has no room left, though, so the pool doesn't stay there: this
  test sends as many requests as the server will take, which is the one
  case where the second worker's room is never used. Set `workers = 1`
  for a runtime where that saving matters more.
- The rounds differ by more than the settings do in places: Bun's "hello"
  with one worker ran from 64,659 to 84,590 req/s, and the third round was
  about a tenth slower than the first two for every runtime.
- Earlier runs of this comparison, with an earlier version of the rule,
  had two slow rounds of "hello" on Python with `workers` unset (13,100
  and 19,600 req/s) that were never explained. None of these three was.

### Policies

One policy at a time on a static `respond` route, so each row's CPU µs/req
less the first row's is that policy's cost. A body schema only applies to
routes that pass the body on, so the body rows are a `proxy` to the Go
server, with and without the schema.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| No policy | 210,083 | 16.0 | 0.298 | 0.526 | 0.160 | 0.340 | 0.383 | 29 MB |
| JWT (HS256) | 145,611 | 24.1 | 0.435 | 0.781 | 0.168 | 0.404 | 0.452 | 28 MB |
| API key | 180,103 | 19.1 | 0.348 | 0.649 | 0.164 | 0.389 | 0.496 | 27 MB |
| Rate limit, memory store | 182,970 | 18.5 | 0.344 | 0.654 | 0.161 | 0.349 | 0.397 | 27 MB |
| Rate limit, Redis store | 86,254 | 27.7 | 0.709 | 1.27 | 0.343 | 0.643 | 0.814 | 28 MB |
| CORS (request with Origin) | 190,105 | 17.7 | 0.330 | 0.619 | 0.160 | 0.401 | 0.491 | 28 MB |
| Path and query validation | 178,131 | 19.1 | 0.354 | 0.658 | 0.160 | 0.403 | 0.493 | 27 MB |
| All of JWT, rate limit, CORS, path and query | 114,001 | 31.1 | 0.557 | 1.03 | 0.176 | 0.441 | 0.519 | 27 MB |
| `proxy`, 1 KB JSON body | 55,505 | 52.6 | 1.10 | 3.30 | 0.256 | 0.466 | 0.759 | 32 MB |
| `proxy`, same body checked against a JSON Schema | 49,788 | 71.2 | 1.23 | 2.69 | 0.253 | 0.467 | 0.571 | 33 MB |

| Policy | Added CPU per request |
|---|--:|
| API key, rate limit (memory), CORS, path and query validation | 2 to 4 µs each |
| JWT, HS256 | 8 µs |
| Rate limit, Redis store | 86,254 | 27.7 | 0.709 | 1.27 | 0.343 | 0.643 | 0.814 | 28 MB |
| JSON Schema on a 1 KB body | 19 µs |

- None of them adds measurable latency at 1,000 req/s, except Redis's
  round trip.
- JWT cost 19 µs in v0.3.0, most of it threads waiting for each other at
  a lock around the key cache. What's left is the signature check and
  decoding the token and its claims.
- The body schema here has 12 array items, each with a regex `pattern`.

### MCP

The same Bun handler as a route and as an MCP tool (a route with
`mcp = { tool = ... }`), a `[[mcp.tool]]` whose arguments are checked
against its input schema, and `tools/list`.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| `GET /text` (the route) | 70,693 | 51.0 | 0.806 | 4.86 | 0.225 | 0.632 | 0.787 | 358 MB |
| `tools/call text` (the same handler) | 45,549 | 82.4 | 1.29 | 4.60 | 0.258 | 0.675 | 0.850 | 290 MB |
| `tools/call echo`, arguments checked against its schema | 36,119 | 106 | 1.65 | 5.18 | 0.298 | 0.638 | 0.886 | 293 MB |
| `tools/list` | 97,725 | 37.2 | 0.632 | 1.36 | 0.176 | 0.495 | 0.583 | 214 MB |

- A tool call costs about one and a half times the route it wraps (82
  against 50 µs of CPU), and adds about 0.03 ms of latency: the JSON-RPC
  body is parsed and checked against the MCP headers, and the reply is
  turned into a tool result.
- `tools/list` is answered by Unsworn alone, at about 40 µs.

### Route table size

A `respond` route at the end of a table of 10, 1,000 and 10,000 routes,
each with a path parameter, and a catch-all behind them.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| 10 routes | 205,911 | 16.4 | 0.303 | 0.551 | 0.158 | 0.403 | 0.445 | 25 MB |
| 1,000 routes | 206,641 | 16.4 | 0.304 | 0.547 | 0.159 | 0.388 | 0.448 | 28 MB |
| 10,000 routes | 206,483 | 16.4 | 0.303 | 0.539 | 0.161 | 0.347 | 0.372 | 67 MB |

Matching costs the same at every size, since routes are matched with a
radix tree. Memory grows with the table: 10,000 routes take 67 MB under
load, and 137 MB just after startup (see [Startup](#startup)).

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
| 1 KB: Go on its own | 144,844 | 148 | 23.4 | 0.397 | 1.77 | 17 MB |
| 64 KB: Go on its own | 18,476 | 1,211 | 174 | 3.76 | 23.56 | 21 MB |
| 1 MB: Go on its own | 2,173 | 2,278 | 1335 | 6.69 | 23.96 | 47 MB |
| 64 KB JSON: Go on its own | 18,024 | 1,147 | 179 | 3.85 | 24.99 | 24 MB |
| 1 KB: Unsworn `proxy` → Go | 53,423 | 55 | 54.6 | 1.14 | 3.40 | 30 MB |
| 64 KB: Unsworn `proxy` → Go | 8,956 | 587 | 130 | 7.37 | 43.69 | 78 MB |
| 1 MB: Unsworn `proxy` → Go | 1,177 | 1,235 | 997 | 12.36 | 44.05 | 130 MB |
| 64 KB JSON: Unsworn `proxy` → Go | 9,483 | 603 | 111 | 5.81 | 43.16 | 56 MB |
| 1 KB: Unsworn → Bun, Tier 2 | 62,885 | 64 | 58.0 | 0.904 | 5.24 | 343 MB |
| 64 KB: Unsworn → Bun, Tier 2 | 15,668 | 1,027 | 238 | 3.89 | 9.24 | 533 MB |
| 1 MB: Unsworn → Bun, Tier 2 | 1,023 | 1,073 | 3618 | 15.39 | 25.04 | 621 MB |
| 64 KB JSON: Unsworn → Bun, Tier 2 | 25,302 | 1,610 | 146 | 2.38 | 6.64 | 497 MB |
| 64 KB JSON, sent back as JSON: Unsworn → Bun, Tier 2 | 3,328 | 212 | 1185 | 19.00 | 33.16 | 435 MB |
| 1 KB: Unsworn → Bun, Tier 1 | 68,381 | 70 | 53.0 | 0.832 | 4.65 | 344 MB |
| 64 KB: Unsworn → Bun, Tier 1 | 12,294 | 806 | 293 | 5.03 | 9.86 | 504 MB |
| 1 MB: Unsworn → Bun, Tier 1 | 887 | 930 | 4028 | 17.91 | 26.07 | 540 MB |
| 1 KB: Unsworn → Go, Tier 2 | 43,230 | 44 | 85.2 | 1.37 | 4.59 | 106 MB |
| 64 KB: Unsworn → Go, Tier 2 | 11,664 | 764 | 315 | 5.19 | 12.64 | 132 MB |
| 1 MB: Unsworn → Go, Tier 2 | 1,007 | 1,056 | 3765 | 15.26 | 30.56 | 226 MB |
| 64 KB JSON: Unsworn → Go, Tier 2 | 15,477 | 985 | 240 | 3.92 | 10.07 | 127 MB |
| 64 KB JSON, sent back as JSON: Unsworn → Go, Tier 2 | 2,868 | 182 | 1367 | 21.08 | 59.50 | 137 MB |
| 1 KB: Unsworn → Go, Tier 1 | 48,990 | 50 | 75.3 | 1.20 | 3.93 | 105 MB |
| 64 KB: Unsworn → Go, Tier 1 | 11,417 | 748 | 328 | 5.31 | 11.77 | 179 MB |
| 1 MB: Unsworn → Go, Tier 1 | 957 | 1,003 | 3907 | 16.41 | 26.35 | 257 MB |
| 1 KB: Unsworn → Go server (HTTP runtime), 1 server | 50,257 | 51 | 70.1 | 1.17 | 3.60 | 51 MB |
| 64 KB: Unsworn → Go server (HTTP runtime), 1 server | 10,185 | 667 | 326 | 6.20 | 26.38 | 102 MB |
| 1 MB: Unsworn → Go server (HTTP runtime), 1 server | 1,432 | 1,502 | 2461 | 10.92 | 19.19 | 183 MB |
| 1 KB: Unsworn → Bun server (HTTP runtime), 1 server | 63,952 | 65 | 54.1 | 0.966 | 1.98 | 73 MB |
| 64 KB: Unsworn → Bun server (HTTP runtime), 1 server | 18,412 | 1,207 | 165 | 3.33 | 6.77 | 122 MB |
| 1 MB: Unsworn → Bun server (HTTP runtime), 1 server | 1,288 | 1,351 | 1871 | 12.27 | 17.14 | 193 MB |

- The proxy streams bodies: it moves 1.24 GB/s of 1 MB bodies in about
  130 MB of memory. Its 64 KB and 1 MB rows are limited by the upstream,
  which has a single core here.
- A handler has to receive the whole body and send it back, so large
  bodies cost more than through the proxy: 930 to 1,070 MB/s of 1 MB
  bodies for Bun and Go.
- Tier 2 beats Tier 1 for Bun at 64 KB and above (by about 25% and 15%).
  For Go the two are within 2 to 5%. A pipe holds 64 KB, so a large body
  likely takes more reads and writes on Tier 1.
- 1 MB bodies cost 3.6 to 4.0 ms of CPU each through a handler, about
  three times what the Go server takes on its own.
- **An HTTP runtime streams bodies both ways,** as `proxy` does, so a
  large one costs half to two thirds of what it does through a handler:
  1.9 ms of CPU for 1 MB through the Bun server and 2.5 ms through the
  Go one, against 3.6 and 3.8 ms through a Tier 2 handler, in 183 to 193 MB of
  memory against 226 to 621 MB. At 64 KB the Bun server costs 165 µs
  against 238 for a Bun handler, and the Go server about what a Go
  handler does (326 against 315 µs). These counts include the server's
  own copy of the body, which it reads whole before it sends it back.
- **A JSON body a handler doesn't read costs what carrying it does.** A
  route with no body schema hands the body to its handler as it arrived,
  and the SDK parses it when the handler first asks for it. Unread, 64 KB
  of JSON costs 0.15 ms of CPU to a Bun handler and 0.24 ms to a Go one,
  at 25,500 and 15,500 requests a second. These rows don't send the body
  back, which is why they cost less than the 64 KB rows of raw bytes,
  which do. In v0.3.1, which read every JSON body, they cost 1.2 and
  1.9 ms. A `proxy` route without a body schema passes JSON through
  unread too.
- **Reading 64 KB of JSON and sending it back costs 1.2 to 1.4 ms of
  CPU,** at 3,300 requests a second for Bun and 2,900 for Go. The worker
  parses the body and writes it out again, and Unsworn sends what the
  worker wrote to the client. In v0.3.1 that cost 1.8 and 3.1 ms.

### Startup

How long `unsworn serve` takes to answer its first request, by the size
of its route table:

| Routes | Start to first response, ms | Memory at idle |
|--:|--:|--:|
| 10 | 3.20 | 20 MB |
| 1,000 | 12.10 | 36 MB |
| 10,000 | 161 | 137 MB |

The first request to a handler or an HTTP runtime, with the runtime's
workers or server started on that first request (the default) or at
boot (`eager = true`):

| Runtime | Lazy: ready, ms | Lazy: first request, ms | Lazy: second request, ms | Eager: ready, ms |
|---|--:|--:|--:|--:|
| Bun, Tier 2 | 2.61 | 68.18 | 0.462 | 70.46 |
| Node, Tier 2 | 2.59 | 309 | 0.583 | 289 |
| Python, Tier 2 | 2.61 | 200 | 0.577 | 207 |
| Go, Tier 2 | 2.66 | 11.59 | 0.380 | 16.98 |
| Rust, Tier 2 | 2.50 | 8.35 | 0.301 | 11.31 |
| Go `net/http` server, HTTP runtime | 2.55 | 27.53 | 0.493 | 30.89 |
| `Bun.serve` server, HTTP runtime | 2.54 | 27.90 | 0.487 | 31.14 |

Lazy start moves the workers' startup from boot to the first request:
about 10 ms for compiled workers, 68 ms for Bun, and 200 to 310 ms for
Python and Node.

**An HTTP runtime's first request takes about 28 ms, whatever the
server.** Run on their own, the Go server is listening on its socket
1.5 ms after it starts and the Bun server 3.5 ms after (the median of
seven starts each). Unsworn checks whether the server is listening
every 25 ms, so both are found at the second check, and most of the
28 ms is that wait. Since v0.5.0, Unsworn looks again after 1 ms and
doubles the wait each time, up to 25 ms. In a separate comparison, the
v0.5.0 binary and a build with that change started in turn nine times
each, the first request took 7 ms with the Go server (28 before) and
8 to 12 ms with the Bun server (28 before).

**These times depend on the machine's state more than on the release.**
The 10,000-route start took 157 ms in the v0.3.1 run and 89 ms in the
v0.3.0 run; timed side by side, those two binaries both ranged from about
115 to 250 ms. The 10-route start was 3.2 ms in the v0.3.1 run, 5.5 ms
in the v0.4.0 run and 3.2 ms here. In a separate comparison, the v0.3.1
and v0.4.0 binaries were started in turn 25 times each on an idle
machine, and took the same time: about 21 ms at the median with 10 routes and 56 ms with 1,000. Their
fastest starts with 10 routes were 3.1 and 3.2 ms. The tables above were
taken at the end of the run, on a processor that had been busy for over
an hour.

**The wasm core on a serverless function.** A fresh Node process loads
the core through the Node host (`@unswornjs/sdk/node`), the way a Vercel,
Netlify or Lambda function does on a cold start, and answers a `POST`
with a body schema. The platform's own startup isn't included.

| Routes | Node started, ms | SDK imported, ms | First response, ms | Total, ms | Warm request, µs | First request to another route, ms |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 14.94 | 51.13 | 77.11 | 145 | 152 | – |
| 100 | 15.25 | 51.79 | 92.28 | 160 | 154 | 5.18 |
| 1,000 | 14.81 | 52.26 | 162 | 230 | 156 | 5.21 |

- A cold start answers about 145 ms after Node starts, with most of it in
  importing the SDK and compiling the wasm module.
- Warm, a request through the wasm core costs about 155 µs, against
  under 20 µs for a static answer from the binary.
- Each route's first request costs about 5 ms more, since the core
  compiles a route's schemas when a request first needs them. On a busy
  function that's paid once per route per instance.

### Sizes

| | |
|---|--:|
| `unsworn` binary (x86_64 musl, as released) | 19.8 MB |
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

A dedicated server with a fixed CPU frequency would give steadier
numbers than this laptop; results from one are welcome in
`bench/results/`.
