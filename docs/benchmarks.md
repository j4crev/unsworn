# Benchmarks

What Unsworn costs, measured: in front of an existing app, around a
handler call, in front of a server it starts (an HTTP runtime), for each
policy, and at startup. The harness is in [`bench/`](../bench), and
every number here comes from one run of it against the released v0.6.0
binary, saved in
[`bench/results/2026-11-22-v0.6.0-intel-core-i5-1145g7.json`](../bench/results/2026-11-22-v0.6.0-intel-core-i5-1145g7.json),
except where a paragraph says it's from a separate comparison.

## Summary

On two cores of a laptop CPU (an Intel i5-1145G7, details in
[Setup](#setup)):

| What Unsworn does | Requests/s | CPU per request | p50 latency at 1,000 req/s |
|---|--:|--:|--:|
| Answers a static `respond`, `redirect`, 404 or rejection | 178,000–219,000 | 15–19 µs | 0.16 ms (Go `net/http`: 0.15 ms) |
| `proxy` to an existing app | 72,000 | 44 µs | 0.21 ms (the app directly: 0.15 ms) |
| `proxy` with JWT and a rate limit | 58,000 | 60 µs | 0.19 ms |
| Calls a TypeScript handler on Bun that only answers "hello" | 70,000 | 51 µs | 0.23 ms (Bun on its own: 0.14 ms) |
| Calls a Rust handler that only answers "hello" | 71,000 | 51 µs | 0.21 ms |
| Forwards to a Go `net/http` server it started (HTTP runtime) that only answers "hello" | 60,000 | 58 µs | 0.16 ms (Go on its own: 0.16 ms) |
| Forwards to a `Bun.serve` server it started that only answers "hello" | 76,000 | 43 µs | 0.24 ms |
| Calls a TypeScript handler on Bun that searches 2,000 names | 3,300 | 1.2 ms | 1.3 ms at 20 req/s |
| Calls the same search handler in Rust | 4,900 | 0.82 ms | 1.0 ms at 20 req/s |

The "hello" rows measure the call into a handler, which costs about the
same in every language. They say nothing about how fast a language runs
your code. The search rows do: see
[A handler that does work](#a-handler-that-does-work).

- **Requests Unsworn answers itself cost 15 to 25 µs of CPU each.** Go's
  standard library serving "hello" takes 17 µs.
- **A handler call or a proxied request adds about 0.05 to 0.11 ms** when
  the server isn't overloaded. It costs 39 to 61 µs of CPU for the proxy
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
  43 to 58 µs of CPU a request, the server's own work included, against
  51 to 80 µs for a handler in the same language. A 1 MB body costs
  half to two thirds of what it does through a handler, since it's
  streamed. A request that makes one call to a function or a service,
  through Unsworn's local endpoint, costs 153 to 214 µs in all: 8 to
  39 µs more than the same request to a handler using `ctx.call` or
  `ctx.fetch`.
- **The binary is ready in a few milliseconds** with 10 routes and about
  160 ms with 10,000, on a machine that's already busy (see
  [Startup](#startup)). On a serverless function, the wasm core answers
  its first request about 145 ms after Node starts.
- **A trivial handler won't beat its language on its own.** Bun serves
  "hello" at 163,000 req/s on one core by itself, and at 70,000 through
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
governor. Of the 107 scenarios, 63 had their three rounds within 2% of
each other, and 97 within 5%, more than in the v0.5.0 run (48 and 75).
Five differed by 12% or more. Two of those had one round far off: nginx
did 47,600 req/s in its third round against 89,000 to 94,000 in the
other two, and Unsworn's `proxy` with the access log off 35,600 in its
first against 61,000. The tables show the median of the three, so one
slow round doesn't move it.

The processor also slows under load that lasts. In a separate test of
the `proxy` route, its clock fell from about 2.9 to 2.6 GHz once the load
had run for about 25 seconds, and the route went from 73,000 to 63,000
req/s. So where a row falls in the run matters, and a difference of
under 15% between two rows may be only that.

The fastest scenarios are close to what the load generator can send from
its one core (the most it sent here was 244,000 req/s), so they are
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

**Against v0.5.0.** v0.6.0 makes the token a server calls with cheaper,
gives it only to a server whose runtime's `calls` names something, and
looks for a server it starts listening sooner. Of the 107 scenarios the
two runs share, a day apart, all but one are within 5% of their v0.5.0
figure, in either direction. The one is the Go server answering
`GET /text` with a token on every request: 61.6 µs of CPU against 65.1,
and 57,700 req/s against 55,000 (see
[Calls from an HTTP runtime](#calls-from-an-http-runtime)). An HTTP
runtime's first request, with the server started on it, now takes 7 ms
with the Go server and 12 ms with the Bun one, against 28 ms with either
(see [Startup](#startup)). v0.5.0's results file is in this repository's
history under `bench/results/`.

### Setup

| | |
|---|---|
| CPU | Intel Core i5-1145G7 (4 cores, 8 threads), `powersave` governor |
| Memory | 16 GB |
| OS | Linux 7.2 |
| Unsworn | 0.6.0, the released `x86_64-unknown-linux-musl` binary |
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
| Go net/http, for reference | 194,402 | 17.3 | 0.296 | 1.28 | 0.151 | 0.325 | 0.386 | 16 MB |
| `respond` (static JSON) | 177,974 | 18.5 | 0.352 | 0.622 | 0.156 | 0.370 | 0.400 | 126 MB |
| `redirect` | 218,726 | 15.3 | 0.285 | 0.508 | 0.157 | 0.392 | 0.426 | 125 MB |
| No route (404) | 211,688 | 15.7 | 0.296 | 0.547 | 0.156 | 0.366 | 0.436 | 126 MB |
| Rejected: missing JWT (401) | 193,026 | 17.5 | 0.325 | 0.601 | 0.159 | 0.398 | 0.546 | 126 MB |
| Rejected: rate limited (429) | 177,878 | 19.3 | 0.352 | 0.666 | 0.159 | 0.372 | 0.392 | 126 MB |
| Cache hit | 140,437 | 25.1 | 0.448 | 0.854 | 0.164 | 0.356 | 0.483 | 127 MB |
| Same handler, uncached (Bun, Tier 2) | 70,993 | 51.1 | 0.805 | 5.00 | 0.220 | 0.606 | 0.795 | 353 MB |

- A static answer, a redirect, a 404, a rejection and a cache hit all cost
  about the same: 15 to 25 µs of CPU. None of them reaches a worker.
- A cache hit costs about half of the handler call it replaces.
- Memory here includes the four Bun workers this project starts for its
  handler routes. Unsworn on its own uses 17 to 30 MB (see
  [Startup](#startup)).

### In front of an existing app

Unsworn's `proxy` in front of a Go server, compared with calling the
server directly and with Caddy and nginx in the same place. One row has
the Go server on a Unix socket (`proxy = "unix:/path"`) in place of a
TCP port.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Direct to the app, no proxy | 189,557 | 17.6 | 0.303 | 1.33 | 0.153 | 0.375 | 0.441 | 16 MB |
| Unsworn `proxy` | 71,596 | 44.0 | 0.847 | 2.45 | 0.208 | 0.468 | 0.724 | 25 MB |
| Unsworn `proxy` with JWT and a rate limit | 57,945 | 60.2 | 1.06 | 2.38 | 0.192 | 0.467 | 0.855 | 26 MB |
| Unsworn `proxy`, access log off | 60,562 | 50.3 | 1.01 | 2.92 | 0.213 | 0.468 | 0.750 | 25 MB |
| Unsworn `proxy`, the app on a Unix socket | 88,070 | 39.8 | 0.695 | 1.82 | 0.135 | 0.353 | 0.474 | 25 MB |
| Caddy `reverse_proxy` | 31,200 | 117 | 1.85 | 6.41 | 0.185 | 0.356 | 0.563 | 25 MB |
| nginx `proxy_pass`, keepalive, access log off | 89,384 | 22.9 | 0.681 | 2.01 | 0.211 | 0.420 | 0.588 | 13 MB |

- Unsworn adds about 0.06 ms at the median when it isn't busy, and
  handles 72,000 req/s on two cores. Its upstream connections are reused,
  not opened per request.
- nginx uses half of Unsworn's CPU per request, so it reaches about 1.3
  times the throughput. Unsworn's proxy adds the `Forwarded`,
  `X-Forwarded-*`, `Via` and request-id headers and runs the full policy
  pipeline on every request.
- **With the app on a Unix socket, the proxy reaches 88,000 req/s** for
  40 µs of CPU a request, and adds nothing measurable when it isn't busy
  (0.14 ms, against 0.15 ms for the app directly over TCP).
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
| Go on its own | 181,966 | 18.6 | 0.318 | 1.35 | 0.156 | 0.337 | 0.437 | 16 MB |
| Bun on its own, 1 process | 163,108 | 4.5 | 0.380 | 0.694 | 0.144 | 0.348 | 0.393 | 37 MB |
| Bun on its own, 4 processes (reusePort) | 243,949 | 10.7 | 0.125 | 0.464 | 0.152 | 0.311 | 0.379 | 146 MB |
| Node on its own, 1 process | 83,980 | 10.2 | 0.742 | 0.963 | 0.161 | 0.372 | 0.443 | 89 MB |
| Node on its own, 4 processes (cluster) | 158,427 | 21.7 | 0.374 | 1.15 | 0.168 | 0.428 | 0.511 | 413 MB |
| Unsworn → TypeScript on Bun, Tier 2, 1 worker | 74,978 | 41.4 | 0.777 | 2.84 | 0.223 | 0.643 | 0.774 | 106 MB |
| Unsworn → TypeScript on Bun, Tier 2, 4 workers | 70,254 | 50.5 | 0.817 | 4.92 | 0.226 | 0.621 | 0.772 | 351 MB |
| Unsworn → TypeScript on Bun, Tier 1, 4 workers | 64,707 | 55.3 | 0.892 | 5.10 | 0.230 | 0.626 | 0.817 | 323 MB |
| Unsworn → TypeScript on Node, Tier 2, 4 workers | 59,040 | 61.2 | 1.00 | 3.07 | 0.269 | 0.680 | 0.806 | 605 MB |
| Unsworn → Python, Tier 2, 4 workers | 32,221 | 116 | 1.87 | 4.33 | 0.251 | 0.586 | 0.902 | 127 MB |
| Unsworn → Python, Tier 1, 4 workers | 32,922 | 114 | 1.83 | 4.26 | 0.224 | 0.584 | 0.884 | 129 MB |
| Unsworn → Python, no SDK, Tier 1, 4 workers | 43,800 | 76.1 | 1.44 | 7.41 | 0.248 | 0.605 | 0.710 | 86 MB |
| Unsworn → Go, Tier 2, 4 workers | 47,447 | 77.8 | 1.26 | 4.14 | 0.193 | 0.395 | 0.730 | 96 MB |
| Unsworn → Go, Tier 1, 4 workers | 45,290 | 80.4 | 1.32 | 4.13 | 0.129 | 0.323 | 0.479 | 93 MB |
| Unsworn → Rust, Tier 2, 4 workers | 70,655 | 50.8 | 0.841 | 2.31 | 0.210 | 0.549 | 0.701 | 58 MB |
| Unsworn → Rust, Tier 1, 4 workers | 89,809 | 39.7 | 0.663 | 1.89 | 0.221 | 0.579 | 0.700 | 58 MB |
| Unsworn → Rust, 4 threads in each worker, Tier 2, 4 workers | 60,982 | 59.9 | 0.984 | 2.61 | 0.155 | 0.403 | 0.730 | 65 MB |

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
  without an SDK does** (114 against 76 µs per request on Tier 1). These
  handlers are sync functions, which the SDK runs on a thread.
- Memory is mostly the workers: four Node processes take about 600 MB,
  four Bun processes about 350 MB, and four Rust processes about 60 MB.
- **A Rust worker uses one thread unless it's given more.** With
  `UNSWORN_WORKER_THREADS=4`, the same handler costs 60 µs a call against
  51 µs, because every request crosses threads on its way in and out of
  the worker. The extra threads do answer sooner when the server is
  nearly idle: 0.16 ms against 0.21 ms at 1,000 requests a second.
- The rounds agreed closely: every row here was within 4% across its
  three.

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
| Go on its own | 182,697 | 18.5 | 0.316 | 1.33 | 0.156 | 0.376 | 0.409 | 16 MB |
| Unsworn `proxy` → Go, which Unsworn didn't start (Unix socket) | 82,222 | 42.3 | 0.739 | 2.09 | 0.146 | 0.385 | 7.93 | 26 MB |
| Unsworn → Go server, on a socket, 1 server | 60,149 | 58.3 | 0.983 | 2.97 | 0.156 | 0.380 | 0.589 | 49 MB |
| Unsworn → Go server, on a port, 1 server | 44,993 | 73.3 | 1.32 | 3.87 | 0.205 | 0.427 | 0.669 | 49 MB |
| Unsworn → Go server, on a socket, 4 servers | 48,992 | 75.4 | 1.22 | 5.70 | 0.175 | 0.375 | 0.603 | 107 MB |
| Unsworn → Go handler, Tier 2, 4 workers | 46,539 | 79.5 | 1.28 | 4.22 | 0.199 | 0.401 | 0.719 | 93 MB |
| Bun on its own, 1 process | 161,095 | 4.6 | 0.380 | 0.719 | 0.144 | 0.342 | 0.407 | 37 MB |
| Unsworn → Bun server, on a socket, 1 server | 76,083 | 43.0 | 0.816 | 1.51 | 0.241 | 0.574 | 0.745 | 68 MB |
| Unsworn → Bun server, on a socket, 4 servers | 60,216 | 59.8 | 1.01 | 2.58 | 0.206 | 0.568 | 0.729 | 180 MB |
| Unsworn → TypeScript handler on Bun, Tier 2, 4 workers | 69,972 | 51.2 | 0.820 | 4.74 | 0.222 | 0.583 | 0.940 | 325 MB |

- **The CPU column counts the server when Unsworn started it.** It's
  Unsworn's child, on Unsworn's cores, as a handler's workers are. The
  `proxy` row's Go server isn't counted: it's the upstream, on a core of
  its own. That's why the `proxy` row costs 42 µs and the HTTP runtime
  58: the difference is about what the Go server takes on its own
  (18 µs).
- **Forwarding to a server costs less than calling a handler in the
  same language.** The Go server takes 58 µs a request and answers
  60,000 a second, where a Go handler takes 80 µs and answers 47,000.
  The Bun server takes 43 µs and answers 76,000, where a TypeScript
  handler on Bun takes 51 µs and answers 70,000. Both servers are built
  to speak HTTP; a handler call goes through the SDK's own reading and
  writing at the worker's end.
- **When it isn't busy, the Go server adds nothing measurable:** 0.16 ms
  at the median, the same as Go on its own. The Bun server adds about
  0.1 ms (0.24 against 0.14 ms).
- **The socket is cheaper than the port.** On a port, the Go server
  costs 73 µs a request against 58, answers 45,000 a second against
  60,000, and takes 0.21 ms at the median against 0.16 when it isn't
  busy. The socket is the default, and the one that keeps other
  processes from reaching the server.
- **One server is enough for these.** Both already use every core they
  get, so four copies on the same four hardware threads cost more: 75 µs
  a request against 58 for Go, 60 against 43 for Bun, with two to three
  times the memory. `workers` is for a server that can't use more than
  one core on its own, which wasn't measured here.
- Every row here, and the HTTP runtime rows under
  [Request bodies](#request-bodies), was within 7% across its three
  rounds. In the v0.5.0 run, nine of them were 11 to 27% slower in
  their first round than in the other two.

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
| Go server, `GET /text`, with `calls` (a token per request) | 57,690 | 61.6 | 1.03 | 3.10 | 0.159 | 0.380 | 0.653 | 154 MB |
| Go server → function, endpoint's socket | 17,353 | 214 | 3.44 | 8.23 | 0.246 | 0.554 | 1.37 | 359 MB |
| Go server → function, endpoint's port | 15,038 | 236 | 3.97 | 9.83 | 0.259 | 0.575 | 1.22 | 343 MB |
| Go handler → function (`req.Call`), Tier 2 | 19,711 | 193 | 3.00 | 9.38 | 0.218 | 0.760 | 7.65 | 417 MB |
| Bun server → function, endpoint's socket | 22,455 | 159 | 2.73 | 5.51 | 0.209 | 0.495 | 1.38 | 399 MB |
| Bun server → function, endpoint's port | 21,002 | 162 | 2.92 | 5.89 | 0.233 | 0.495 | 1.34 | 396 MB |
| TypeScript handler on Bun → function (`ctx.call`), Tier 2 | 30,855 | 120 | 1.90 | 6.68 | 0.179 | 0.467 | 1.26 | 361 MB |
| Go server → the app directly, without Unsworn | 22,983 | 152 | 2.52 | 6.98 | 0.211 | 0.402 | 0.791 | 286 MB |
| Go server → service, endpoint's socket | 16,982 | 211 | 3.52 | 7.97 | 0.253 | 0.476 | 0.943 | 285 MB |
| Go server → service, endpoint's port | 15,435 | 223 | 3.93 | 8.80 | 0.263 | 0.491 | 0.810 | 282 MB |
| Go handler → service (`req.Fetch`), Tier 2 | 18,183 | 203 | 3.32 | 9.08 | 0.256 | 0.440 | 1.12 | 292 MB |
| Bun server → service, endpoint's socket | 21,648 | 153 | 2.87 | 5.04 | 0.252 | 0.529 | 0.687 | 309 MB |
| TypeScript handler on Bun → service (`ctx.fetch`), Tier 2 | 25,487 | 143 | 2.34 | 6.83 | 0.232 | 0.491 | 1.08 | 312 MB |

- **A call costs a little more from a server than from a handler.** A
  request that calls the function costs 214 µs from the Go server
  against 193 from a Go handler, and 159 from the Bun server against 120
  from a TypeScript handler. One that fetches from the service costs 211
  against 203 in Go, and 153 against 143 on Bun. The server's call is a
  second HTTP request that Unsworn parses and matches to its token; a
  handler's goes as a message over the connection its worker already
  has.
- **The endpoint adds about 60 µs to a fetch.** The Go server sending
  the same body straight to the upstream costs 152 µs a request and
  manages 23,000 a second. Through the endpoint, where Unsworn checks the
  token and the service's rules and sends the request on, it's 211 µs
  and 17,000. When the server isn't busy, that's 0.25 ms at the median
  against 0.21.
- **The endpoint's socket is cheaper than its port** for the Go server's
  client, by 12 to 22 µs a call. For Bun's client the two were within
  3 µs.
- **A server that may call pays for a token on every forwarded
  request,** whether it makes a call or not. The Go server's `GET /text`
  costs 62 µs here with `calls = ["*"]`, against 58 for the same server
  without `calls` (in [HTTP runtimes](#http-runtimes)), and 65 in the
  v0.5.0 run. v0.6.0 signs a token with a key instead of drawing it from
  the system's randomness: in a separate comparison, the two binaries
  taken in turn for six rounds, that took the request from 64.8 to
  59.7 µs, against 54.9 with no token. A server whose runtime has no
  `calls` gets no token, so one that never calls pays nothing. In v0.5.0
  every server in a project with functions or services got one.
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
| Unsworn → TypeScript on Bun | 3,254 | 1,223 | 1.28 | 1.62 | 204 MB |
| Unsworn → TypeScript on Node | 2,408 | 1,656 | 1.54 | 1.92 | 461 MB |
| Unsworn → Python | 75 | 52,973 | 21.65 | 22.35 | 116 MB |
| Unsworn → Go | 4,637 | 855 | 1.07 | 1.31 | 89 MB |
| Unsworn → Rust | 4,871 | 816 | 0.969 | 1.37 | 53 MB |

- **Rust does this work for 0.82 ms of CPU, Go for 0.85 ms, Bun for
  1.2 ms, Node for 1.7 ms and Python for 53 ms.** So moving this endpoint
  from TypeScript on Bun to Rust gives 1.5 times the requests per core,
  from Node twice as many, and from Python about 65 times.
- The call into the handler is about 50 µs of each figure, which is why
  it decides the "hello" rows above and hardly shows here.
- One request on a quiet server takes 1.0 ms in Rust, 1.1 ms in Go,
  1.3 ms on Bun, 1.5 ms on Node and 22 ms in Python.
- The Rust workers take 53 MB between them, about a quarter of Bun's
  204 MB and about a ninth of Node's 461 MB.
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
| No policy | 209,485 | 15.9 | 0.300 | 0.537 | 0.161 | 0.351 | 0.425 | 27 MB |
| JWT (HS256) | 146,650 | 23.8 | 0.434 | 0.776 | 0.167 | 0.401 | 0.472 | 26 MB |
| API key | 178,249 | 19.0 | 0.353 | 0.653 | 0.163 | 0.350 | 0.403 | 27 MB |
| Rate limit, memory store | 185,624 | 18.4 | 0.338 | 0.630 | 0.162 | 0.369 | 0.400 | 27 MB |
| Rate limit, Redis store | 87,821 | 28.0 | 0.698 | 1.19 | 0.334 | 0.583 | 0.754 | 27 MB |
| CORS (request with Origin) | 189,800 | 17.9 | 0.331 | 0.610 | 0.160 | 0.398 | 0.449 | 26 MB |
| Path and query validation | 176,543 | 19.4 | 0.355 | 0.670 | 0.164 | 0.392 | 0.448 | 26 MB |
| All of JWT, rate limit, CORS, path and query | 113,843 | 31.5 | 0.555 | 1.04 | 0.180 | 0.397 | 0.440 | 26 MB |
| `proxy`, 1 KB JSON body | 54,847 | 52.8 | 1.11 | 3.26 | 0.230 | 0.468 | 0.816 | 30 MB |
| `proxy`, same body checked against a JSON Schema | 49,742 | 71.3 | 1.23 | 2.64 | 0.221 | 0.450 | 0.526 | 29 MB |

| Policy | Added CPU per request |
|---|--:|
| API key, rate limit (memory), CORS, path and query validation | 2 to 4 µs each |
| JWT, HS256 | 8 µs |
| Rate limit, Redis store | 12 µs |
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
| `GET /text` (the route) | 70,133 | 50.9 | 0.814 | 4.99 | 0.224 | 0.611 | 0.795 | 354 MB |
| `tools/call text` (the same handler) | 45,230 | 83.4 | 1.31 | 4.99 | 0.250 | 0.614 | 0.837 | 284 MB |
| `tools/call echo`, arguments checked against its schema | 36,061 | 106 | 1.66 | 5.41 | 0.267 | 0.656 | 1.04 | 299 MB |
| `tools/list` | 96,077 | 38.3 | 0.639 | 1.39 | 0.178 | 0.491 | 0.571 | 203 MB |

- A tool call costs about 1.6 times the route it wraps (83 against
  51 µs of CPU), and adds about 0.03 ms of latency: the JSON-RPC
  body is parsed and checked against the MCP headers, and the reply is
  turned into a tool result.
- `tools/list` is answered by Unsworn alone, at about 40 µs.

### Route table size

A `respond` route at the end of a table of 10, 1,000 and 10,000 routes,
each with a path parameter, and a catch-all behind them.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| 10 routes | 204,638 | 16.3 | 0.306 | 0.554 | 0.158 | 0.379 | 0.421 | 23 MB |
| 1,000 routes | 207,530 | 16.3 | 0.302 | 0.535 | 0.161 | 0.381 | 0.423 | 23 MB |
| 10,000 routes | 206,803 | 16.4 | 0.303 | 0.543 | 0.159 | 0.377 | 0.415 | 66 MB |

Matching costs the same at every size, since routes are matched with a
radix tree. Memory grows with the table: 10,000 routes take 66 MB under
load, and 132 MB just after startup (see [Startup](#startup)).

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
| 1 KB: Go on its own | 148,322 | 152 | 22.9 | 0.387 | 1.75 | 16 MB |
| 64 KB: Go on its own | 18,307 | 1,200 | 175 | 3.79 | 23.53 | 19 MB |
| 1 MB: Go on its own | 2,185 | 2,291 | 1331 | 6.68 | 23.60 | 46 MB |
| 64 KB JSON: Go on its own | 17,844 | 1,135 | 180 | 3.88 | 24.78 | 22 MB |
| 1 KB: Unsworn `proxy` → Go | 55,710 | 57 | 53.7 | 1.10 | 3.31 | 26 MB |
| 64 KB: Unsworn `proxy` → Go | 9,031 | 592 | 131 | 7.35 | 42.91 | 65 MB |
| 1 MB: Unsworn `proxy` → Go | 1,177 | 1,234 | 1004 | 12.35 | 44.23 | 125 MB |
| 64 KB JSON: Unsworn `proxy` → Go | 9,667 | 615 | 113 | 5.81 | 41.87 | 46 MB |
| 1 KB: Unsworn → Bun, Tier 2 | 62,749 | 64 | 57.6 | 0.904 | 5.11 | 325 MB |
| 64 KB: Unsworn → Bun, Tier 2 | 15,547 | 1,019 | 240 | 3.91 | 9.17 | 509 MB |
| 1 MB: Unsworn → Bun, Tier 2 | 1,012 | 1,061 | 3658 | 15.53 | 27.06 | 623 MB |
| 64 KB JSON: Unsworn → Bun, Tier 2 | 25,260 | 1,607 | 146 | 2.38 | 6.72 | 477 MB |
| 64 KB JSON, sent back as JSON: Unsworn → Bun, Tier 2 | 3,319 | 211 | 1189 | 19.05 | 32.63 | 412 MB |
| 1 KB: Unsworn → Bun, Tier 1 | 67,989 | 70 | 53.3 | 0.835 | 4.66 | 337 MB |
| 64 KB: Unsworn → Bun, Tier 1 | 12,290 | 805 | 295 | 5.03 | 9.81 | 492 MB |
| 1 MB: Unsworn → Bun, Tier 1 | 889 | 932 | 4049 | 17.79 | 26.11 | 534 MB |
| 1 KB: Unsworn → Go, Tier 2 | 42,962 | 44 | 86.2 | 1.38 | 4.71 | 95 MB |
| 64 KB: Unsworn → Go, Tier 2 | 11,702 | 767 | 317 | 5.16 | 12.81 | 118 MB |
| 1 MB: Unsworn → Go, Tier 2 | 1,018 | 1,067 | 3670 | 15.13 | 29.57 | 220 MB |
| 64 KB JSON: Unsworn → Go, Tier 2 | 15,462 | 984 | 239 | 3.93 | 10.16 | 120 MB |
| 64 KB JSON, sent back as JSON: Unsworn → Go, Tier 2 | 2,870 | 183 | 1367 | 21.04 | 58.19 | 130 MB |
| 1 KB: Unsworn → Go, Tier 1 | 49,554 | 51 | 74.1 | 1.19 | 4.00 | 93 MB |
| 64 KB: Unsworn → Go, Tier 1 | 11,288 | 740 | 329 | 5.38 | 11.95 | 167 MB |
| 1 MB: Unsworn → Go, Tier 1 | 943 | 989 | 3959 | 16.55 | 27.41 | 240 MB |
| 1 KB: Unsworn → Go server (HTTP runtime), 1 server | 50,574 | 52 | 69.7 | 1.16 | 3.59 | 49 MB |
| 64 KB: Unsworn → Go server (HTTP runtime), 1 server | 10,100 | 662 | 328 | 6.27 | 26.48 | 81 MB |
| 1 MB: Unsworn → Go server (HTTP runtime), 1 server | 1,414 | 1,482 | 2489 | 11.07 | 19.60 | 166 MB |
| 1 KB: Unsworn → Bun server (HTTP runtime), 1 server | 64,238 | 66 | 53.6 | 0.961 | 1.97 | 69 MB |
| 64 KB: Unsworn → Bun server (HTTP runtime), 1 server | 18,347 | 1,202 | 165 | 3.33 | 6.79 | 104 MB |
| 1 MB: Unsworn → Bun server (HTTP runtime), 1 server | 1,291 | 1,354 | 1877 | 12.21 | 17.16 | 179 MB |

- The proxy streams bodies: it moves 1.23 GB/s of 1 MB bodies in about
  130 MB of memory. Its 64 KB and 1 MB rows are limited by the upstream,
  which has a single core here.
- A handler has to receive the whole body and send it back, so large
  bodies cost more than through the proxy: 930 to 1,070 MB/s of 1 MB
  bodies for Bun and Go.
- Tier 2 beats Tier 1 for Bun at 64 KB and above (by about 25% and 15%).
  For Go the two are within 4 to 8%. A pipe holds 64 KB, so a large body
  likely takes more reads and writes on Tier 1.
- 1 MB bodies cost 3.7 to 4.0 ms of CPU each through a handler, about
  three times what the Go server takes on its own.
- **An HTTP runtime streams bodies both ways,** as `proxy` does, so a
  large one costs half to two thirds of what it does through a handler:
  1.9 ms of CPU for 1 MB through the Bun server and 2.5 ms through the
  Go one, against 3.7 ms through a Tier 2 handler in either, in 166 to
  179 MB of memory against 220 to 623 MB. At 64 KB the Bun server costs
  165 µs against 240 for a Bun handler, and the Go server about what a
  Go handler does (328 against 317 µs). These counts include the server's
  own copy of the body, which it reads whole before it sends it back.
- **A JSON body a handler doesn't read costs what carrying it does.** A
  route with no body schema hands the body to its handler as it arrived,
  and the SDK parses it when the handler first asks for it. Unread, 64 KB
  of JSON costs 0.15 ms of CPU to a Bun handler and 0.24 ms to a Go one,
  at 25,300 and 15,500 requests a second. These rows don't send the body
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
| 10 | 5.44 | 17 MB |
| 1,000 | 16.89 | 29 MB |
| 10,000 | 164 | 132 MB |

The first request to a handler or an HTTP runtime, with the runtime's
workers or server started on that first request (the default) or at
boot (`eager = true`):

| Runtime | Lazy: ready, ms | Lazy: first request, ms | Lazy: second request, ms | Eager: ready, ms |
|---|--:|--:|--:|--:|
| Bun, Tier 2 | 4.77 | 68.65 | 0.705 | 72.60 |
| Node, Tier 2 | 7.08 | 319 | 0.581 | 300 |
| Python, Tier 2 | 4.89 | 202 | 0.488 | 209 |
| Go, Tier 2 | 4.83 | 12.75 | 0.356 | 16.03 |
| Rust, Tier 2 | 5.22 | 8.35 | 0.319 | 11.26 |
| Go `net/http` server, HTTP runtime | 5.08 | 6.81 | 0.614 | 10.51 |
| `Bun.serve` server, HTTP runtime | 5.06 | 12.16 | 0.408 | 12.86 |

Lazy start moves the workers' startup from boot to the first request:
about 10 ms for compiled workers, 69 ms for Bun, and 200 to 320 ms for
Python and Node.

**An HTTP runtime's first request takes 7 ms with the Go server and
12 ms with the Bun one.** Run on their own, the Go server is listening
on its socket 1.5 ms after it starts and the Bun server 3.5 ms after
(the median of seven starts each). Unsworn looks for the server
listening 1 ms after starting it, and then doubles the wait each time,
up to 25 ms. v0.5.0 looked every 25 ms, so both servers were found at
the second look, and the first request took about 28 ms with either.

**These times depend on the machine's state more than on the release.**
The 10,000-route start took 157 ms in the v0.3.1 run and 89 ms in the
v0.3.0 run; timed side by side, those two binaries both ranged from about
115 to 250 ms. The 10-route start was 3.2 ms in the v0.3.1 run, 5.5 ms
in the v0.4.0 run, 3.2 ms in the v0.5.0 run and 5.4 ms here. Started in
turn 30 times each, the v0.5.0 and v0.6.0 binaries took 9.9 and 8.6 ms
at the median with 10 routes, each from about 3.5 ms to over 17. In a separate comparison, the v0.3.1
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
| 1 | 15.57 | 52.76 | 78.00 | 147 | 159 | – |
| 100 | 14.93 | 52.48 | 94.41 | 162 | 160 | 5.24 |
| 1,000 | 14.72 | 52.42 | 164 | 231 | 158 | 5.31 |

- A cold start answers about 145 ms after Node starts, with most of it in
  importing the SDK and compiling the wasm module.
- Warm, a request through the wasm core costs about 160 µs, against
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
