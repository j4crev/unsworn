# Benchmarks

What Unsworn costs, measured: in front of an existing app, around a
handler call, for each policy, and at startup. The harness is in
[`bench/`](../bench), and every number here comes from one run of it
against the released v0.4.0 binary, saved in
[`bench/results/2026-11-19-v0.4.0-intel-core-i5-1145g7.json`](../bench/results/2026-11-19-v0.4.0-intel-core-i5-1145g7.json),
except where a paragraph says it's from a separate comparison.

## Summary

On two cores of a laptop CPU (an Intel i5-1145G7, details in
[Setup](#setup)):

| What Unsworn does | Requests/s | CPU per request | p50 latency at 1,000 req/s |
|---|--:|--:|--:|
| Answers a static `respond`, `redirect`, 404 or rejection | 179,000–223,000 | 15–19 µs | 0.16 ms (Go `net/http`: 0.15 ms) |
| `proxy` to an existing app | 70,000 | 45 µs | 0.24 ms (the app directly: 0.15 ms) |
| `proxy` with JWT and a rate limit | 59,000 | 59 µs | 0.26 ms |
| Calls a TypeScript handler on Bun that only answers "hello" | 71,000 | 51 µs | 0.22 ms (Bun on its own: 0.15 ms) |
| Calls a Rust handler that only answers "hello" | 70,000 | 51 µs | 0.21 ms |
| Calls a TypeScript handler on Bun that searches 2,000 names | 3,300 | 1.2 ms | 1.3 ms at 20 req/s |
| Calls the same search handler in Rust | 4,900 | 0.82 ms | 1.1 ms at 20 req/s |

The "hello" rows measure the call into a handler, which costs about the
same in every language. They say nothing about how fast a language runs
your code. The search rows do: see
[A handler that does work](#a-handler-that-does-work).

- **Requests Unsworn answers itself cost 15 to 25 µs of CPU each.** Go's
  standard library serving "hello" takes 17 µs.
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
- **The binary is ready in about 5 ms** with 10 routes and about 150 ms
  with 10,000, on a machine that's already busy (see
  [Startup](#startup)). On a serverless function, the wasm core answers
  its first request about 145 ms after Node starts.
- **A trivial handler won't beat its language on its own.** Bun serves
  "hello" at 158,000 req/s on one core by itself, and at 71,000 through
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
  answer the same thing. The one exception is the search handler in
  [`bench/handlers/`](../bench/handlers), described under
  [A handler that does work](#a-handler-that-does-work).

**How precise they are.** This is a laptop with a `powersave` CPU
governor. Half of the 78 scenarios had their three rounds within 2% of
each other, and two thirds within 5%. But 15 differed by 12% or more, 12
of those because the first round was the slow one, and one differed by
47% (see [Handler calls](#handler-calls)).

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

### Setup

| | |
|---|---|
| CPU | Intel Core i5-1145G7 (4 cores, 8 threads), `powersave` governor |
| Memory | 16 GB |
| OS | Linux 7.2 |
| Unsworn | 0.4.0, the released `x86_64-unknown-linux-musl` binary |
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
| Go net/http, for reference | 198,139 | 16.7 | 0.293 | 1.25 | 0.147 | 0.348 | 0.393 | 16 MB |
| `respond` (static JSON) | 178,977 | 18.7 | 0.349 | 0.618 | 0.156 | 0.335 | 0.416 | 130 MB |
| `redirect` | 223,378 | 15.0 | 0.278 | 0.492 | 0.156 | 0.402 | 0.461 | 128 MB |
| No route (404) | 207,225 | 16.3 | 0.300 | 0.563 | 0.156 | 0.361 | 0.417 | 128 MB |
| Rejected: missing JWT (401) | 195,450 | 17.4 | 0.320 | 0.586 | 0.157 | 0.397 | 0.500 | 130 MB |
| Rejected: rate limited (429) | 182,515 | 18.8 | 0.343 | 0.638 | 0.155 | 0.416 | 0.462 | 129 MB |
| Cache hit | 139,056 | 24.9 | 0.456 | 0.852 | 0.166 | 0.354 | 0.384 | 129 MB |
| Same handler, uncached (Bun, Tier 2) | 71,539 | 50.0 | 0.805 | 5.16 | 0.217 | 0.591 | 0.844 | 358 MB |

- A static answer, a redirect, a 404, a rejection and a cache hit all cost
  about the same: 15 to 25 µs of CPU. None of them reaches a worker.
- A cache hit costs about half of the handler call it replaces.
- Memory here includes the four Bun workers this project starts for its
  handler routes. Unsworn on its own uses 19 to 29 MB (see
  [Startup](#startup)).

### In front of an existing app

Unsworn's `proxy` in front of a Go server, compared with calling the
server directly and with Caddy and nginx in the same place. One row has
the Go server on a Unix socket (`proxy = "unix:/path"`) in place of a
TCP port.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Direct to the app, no proxy | 192,427 | 17.2 | 0.301 | 1.33 | 0.149 | 0.320 | 0.440 | 16 MB |
| Unsworn `proxy` | 69,999 | 45.0 | 0.866 | 2.50 | 0.239 | 0.464 | 0.735 | 29 MB |
| Unsworn `proxy` with JWT and a rate limit | 59,368 | 58.8 | 1.04 | 2.37 | 0.258 | 0.475 | 0.773 | 30 MB |
| Unsworn `proxy`, access log off | 60,110 | 50.7 | 1.01 | 2.91 | 0.243 | 0.467 | 0.716 | 28 MB |
| Unsworn `proxy`, the app on a Unix socket | 87,975 | 39.5 | 0.691 | 1.86 | 0.139 | 0.368 | 0.452 | 28 MB |
| Caddy `reverse_proxy` | 31,108 | 117 | 1.86 | 6.47 | 0.189 | 0.370 | 0.552 | 27 MB |
| nginx `proxy_pass`, keepalive, access log off | 89,631 | 22.5 | 0.677 | 2.03 | 0.217 | 0.434 | 0.625 | 13 MB |

- Unsworn adds about 0.09 ms at the median when it isn't busy, and
  handles 70,000 req/s on two cores. Its upstream connections are reused,
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
  part of its 14 µs over the plain row is likely the slower processor
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
| Go on its own | 174,691 | 18.9 | 0.332 | 1.41 | 0.149 | 0.344 | 0.414 | 16 MB |
| Bun on its own, 1 process | 157,689 | 4.7 | 0.390 | 0.730 | 0.145 | 0.355 | 0.398 | 36 MB |
| Bun on its own, 4 processes (reusePort) | 243,501 | 10.9 | 0.127 | 0.503 | 0.148 | 0.331 | 0.381 | 144 MB |
| Node on its own, 1 process | 84,231 | 10.1 | 0.733 | 1.00 | 0.156 | 0.408 | 0.489 | 90 MB |
| Node on its own, 4 processes (cluster) | 152,951 | 22.7 | 0.446 | 1.19 | 0.171 | 0.389 | 0.476 | 418 MB |
| Unsworn → TypeScript on Bun, Tier 2, 1 worker | 74,978 | 41.4 | 0.776 | 2.81 | 0.222 | 0.653 | 0.806 | 113 MB |
| Unsworn → TypeScript on Bun, Tier 2, 4 workers | 70,796 | 50.5 | 0.812 | 4.90 | 0.218 | 0.611 | 0.887 | 351 MB |
| Unsworn → TypeScript on Bun, Tier 1, 4 workers | 65,281 | 55.0 | 0.886 | 5.00 | 0.230 | 0.625 | 0.796 | 330 MB |
| Unsworn → TypeScript on Node, Tier 2, 4 workers | 59,206 | 61.6 | 0.999 | 2.96 | 0.264 | 0.668 | 0.770 | 642 MB |
| Unsworn → Python, Tier 2, 4 workers | 32,403 | 116 | 1.86 | 4.28 | 0.222 | 0.499 | 0.812 | 131 MB |
| Unsworn → Python, Tier 1, 4 workers | 33,029 | 114 | 1.82 | 4.20 | 0.252 | 0.675 | 0.934 | 132 MB |
| Unsworn → Python, no SDK, Tier 1, 4 workers | 43,998 | 75.7 | 1.44 | 7.45 | 0.243 | 0.615 | 0.759 | 90 MB |
| Unsworn → Go, Tier 2, 4 workers | 47,616 | 77.1 | 1.26 | 4.15 | 0.197 | 0.388 | 0.693 | 106 MB |
| Unsworn → Go, Tier 1, 4 workers | 45,663 | 80.0 | 1.31 | 4.01 | 0.130 | 0.336 | 0.413 | 106 MB |
| Unsworn → Rust, Tier 2, 4 workers | 70,489 | 50.6 | 0.846 | 2.31 | 0.212 | 0.506 | 0.698 | 59 MB |
| Unsworn → Rust, Tier 1, 4 workers | 89,770 | 39.4 | 0.667 | 1.92 | 0.213 | 0.566 | 0.694 | 59 MB |
| Unsworn → Rust, 4 threads in each worker, Tier 2, 4 workers | 61,300 | 60.4 | 0.973 | 2.62 | 0.165 | 0.410 | 0.616 | 67 MB |

- **These rows compare the SDKs, not the languages.** The handler does
  nothing, so each row is Unsworn's side of the call plus that SDK's
  reading and writing of one small message. Bun's is native code, which
  is why TypeScript does as well here as Rust.
- When the server isn't busy, a handler call adds up to about 0.1 ms over
  the language on its own.
- At full load, a call costs 39 to 80 µs of CPU, Unsworn's side and the
  worker's together, and four threads handle 46,000 to 90,000 calls a
  second in every SDK except Python's, which costs about 115 µs and
  handles 32,000 to 33,000.
- More workers don't help a handler this small: one Bun worker does as
  well as four. They matter for handlers that do real work.
- **The two tiers are close for small requests, except in Rust.** Bun,
  Python and Go were within 9% of each other on the two, in either
  direction. Rust was 27% faster on Tier 1. Tier 2's advantage shows
  with large bodies (see [Request bodies](#request-bodies)).
- **The Python SDK costs about one and a half times what a worker
  without an SDK does** (114 against 76 µs per request on Tier 1). These
  handlers are sync functions, which the SDK runs on a thread.
- Memory is mostly the workers: four Node processes take about 640 MB,
  four Bun processes about 350 MB, and four Rust processes about 60 MB.
- **A Rust worker uses one thread unless it's given more.** With
  `UNSWORN_WORKER_THREADS=4`, the same handler costs 60 µs a call against
  51 µs, because every request crosses threads on its way in and out of
  the worker. The extra threads do answer sooner when the server is
  nearly idle: 0.17 ms against 0.21 ms at 1,000 requests a second.
- **One round of a row can be far off.** Go on Tier 2 did 25,100 req/s
  in the first round and 47,600 in the other two. Five of the Bun and
  Node rows were 15 to 22% slower in their first round than in the
  others. The table shows the median of the three, so one slow round
  doesn't move it.

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
| Unsworn → TypeScript on Bun | 3,330 | 1,195 | 1.27 | 1.61 | 223 MB |
| Unsworn → TypeScript on Node | 2,406 | 1,659 | 1.58 | 1.87 | 498 MB |
| Unsworn → Python | 75 | 52,951 | 21.55 | 22.32 | 119 MB |
| Unsworn → Go | 4,606 | 863 | 1.05 | 1.36 | 113 MB |
| Unsworn → Rust | 4,870 | 815 | 1.10 | 1.39 | 62 MB |

- **Rust does this work for 0.82 ms of CPU, Go for 0.86 ms, Bun for
  1.2 ms, Node for 1.7 ms and Python for 53 ms.** So moving this endpoint
  from TypeScript on Bun to Rust gives 1.5 times the requests per core,
  from Node twice as many, and from Python about 65 times.
- The call into the handler is about 50 µs of each figure, which is why
  it decides the "hello" rows above and hardly shows here.
- One request on a quiet server takes 1.1 ms in Rust and in Go, 1.3 ms
  on Bun, 1.6 ms on Node and 22 ms in Python.
- The Rust workers take 62 MB between them, about a quarter of Bun's
  223 MB and an eighth of Node's 498 MB.
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
| No policy | 211,103 | 15.8 | 0.298 | 0.525 | 0.160 | 0.397 | 0.431 | 26 MB |
| JWT (HS256) | 148,029 | 23.4 | 0.430 | 0.764 | 0.168 | 0.368 | 0.400 | 26 MB |
| API key | 178,875 | 19.1 | 0.350 | 0.646 | 0.161 | 0.383 | 0.414 | 26 MB |
| Rate limit, memory store | 186,642 | 18.1 | 0.338 | 0.625 | 0.161 | 0.393 | 0.434 | 27 MB |
| Rate limit, Redis store | 87,129 | 27.7 | 0.703 | 1.23 | 0.335 | 0.600 | 0.763 | 27 MB |
| CORS (request with Origin) | 190,533 | 17.7 | 0.330 | 0.603 | 0.159 | 0.386 | 0.442 | 27 MB |
| Path and query validation | 175,240 | 19.5 | 0.358 | 0.674 | 0.162 | 0.387 | 0.424 | 27 MB |
| All of JWT, rate limit, CORS, path and query | 117,088 | 30.4 | 0.543 | 1.01 | 0.173 | 0.426 | 0.528 | 27 MB |
| `proxy`, 1 KB JSON body | 56,326 | 51.2 | 1.09 | 3.16 | 0.246 | 0.470 | 0.623 | 31 MB |
| `proxy`, same body checked against a JSON Schema | 50,259 | 70.4 | 1.23 | 2.65 | 0.229 | 0.453 | 0.552 | 31 MB |

| Policy | Added CPU per request |
|---|--:|
| API key, rate limit (memory), CORS, path and query validation | 2 to 4 µs each |
| JWT, HS256 | 8 µs |
| Rate limit, Redis store | 12 µs, plus a round trip to Redis (0.17 ms here) |
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
| `GET /text` (the route) | 71,099 | 50.4 | 0.809 | 4.96 | 0.223 | 0.592 | 0.787 | 356 MB |
| `tools/call text` (the same handler) | 45,407 | 81.9 | 1.30 | 4.61 | 0.252 | 0.673 | 0.819 | 285 MB |
| `tools/call echo`, arguments checked against its schema | 36,293 | 104 | 1.66 | 5.24 | 0.279 | 0.581 | 0.890 | 293 MB |
| `tools/list` | 94,610 | 38.5 | 0.652 | 1.41 | 0.178 | 0.415 | 0.558 | 206 MB |

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
| 10 routes | 206,831 | 16.1 | 0.304 | 0.546 | 0.162 | 0.366 | 0.460 | 23 MB |
| 1,000 routes | 199,786 | 16.5 | 0.312 | 0.657 | 0.157 | 0.344 | 0.401 | 28 MB |
| 10,000 routes | 204,562 | 16.5 | 0.308 | 0.552 | 0.158 | 0.346 | 0.417 | 65 MB |

Matching costs the same at every size, since routes are matched with a
radix tree. Memory grows with the table: 10,000 routes take 65 MB under
load, and 127 MB just after startup (see [Startup](#startup)).

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
| 1 KB: Go on its own | 147,495 | 151 | 22.9 | 0.390 | 1.77 | 16 MB |
| 64 KB: Go on its own | 18,426 | 1,208 | 175 | 3.78 | 23.39 | 20 MB |
| 1 MB: Go on its own | 2,207 | 2,315 | 1321 | 6.56 | 22.92 | 48 MB |
| 64 KB JSON: Go on its own | 17,938 | 1,141 | 181 | 3.87 | 24.95 | 24 MB |
| 1 KB: Unsworn `proxy` → Go | 55,415 | 57 | 53.0 | 1.10 | 3.32 | 29 MB |
| 64 KB: Unsworn `proxy` → Go | 9,109 | 597 | 127 | 7.33 | 42.92 | 87 MB |
| 1 MB: Unsworn `proxy` → Go | 1,196 | 1,254 | 976 | 12.36 | 43.67 | 124 MB |
| 64 KB JSON: Unsworn `proxy` → Go | 9,706 | 617 | 111 | 5.63 | 43.45 | 58 MB |
| 1 KB: Unsworn → Bun, Tier 2 | 62,939 | 64 | 57.9 | 0.902 | 5.21 | 334 MB |
| 64 KB: Unsworn → Bun, Tier 2 | 15,673 | 1,027 | 235 | 3.89 | 9.18 | 520 MB |
| 1 MB: Unsworn → Bun, Tier 2 | 1,017 | 1,066 | 3689 | 15.47 | 25.73 | 611 MB |
| 64 KB JSON: Unsworn → Bun, Tier 2 | 25,472 | 1,620 | 146 | 2.36 | 6.70 | 493 MB |
| 64 KB JSON, sent back as JSON: Unsworn → Bun, Tier 2 | 3,324 | 211 | 1187 | 19.00 | 33.29 | 429 MB |
| 1 KB: Unsworn → Bun, Tier 1 | 68,536 | 70 | 52.9 | 0.832 | 4.49 | 341 MB |
| 64 KB: Unsworn → Bun, Tier 1 | 12,442 | 815 | 295 | 4.96 | 9.87 | 497 MB |
| 1 MB: Unsworn → Bun, Tier 1 | 891 | 934 | 4052 | 17.82 | 25.93 | 550 MB |
| 1 KB: Unsworn → Go, Tier 2 | 42,966 | 44 | 86.2 | 1.38 | 4.51 | 102 MB |
| 64 KB: Unsworn → Go, Tier 2 | 11,794 | 773 | 319 | 5.13 | 12.46 | 125 MB |
| 1 MB: Unsworn → Go, Tier 2 | 1,011 | 1,060 | 3711 | 15.17 | 30.52 | 226 MB |
| 64 KB JSON: Unsworn → Go, Tier 2 | 15,528 | 988 | 243 | 3.90 | 9.87 | 126 MB |
| 64 KB JSON, sent back as JSON: Unsworn → Go, Tier 2 | 2,885 | 184 | 1360 | 20.90 | 59.08 | 131 MB |
| 1 KB: Unsworn → Go, Tier 1 | 49,786 | 51 | 73.6 | 1.19 | 3.95 | 105 MB |
| 64 KB: Unsworn → Go, Tier 1 | 11,456 | 751 | 326 | 5.32 | 11.71 | 181 MB |
| 1 MB: Unsworn → Go, Tier 1 | 959 | 1,005 | 3911 | 16.29 | 26.90 | 252 MB |

- The proxy streams bodies: it moves 1.25 GB/s of 1 MB bodies in about
  125 MB of memory. Its 64 KB and 1 MB rows are limited by the upstream,
  which has a single core here.
- A handler has to receive the whole body and send it back, so large
  bodies cost more than through the proxy: 930 to 1,070 MB/s of 1 MB
  bodies for Bun and Go.
- Tier 2 beats Tier 1 for Bun at 64 KB and above (by about 25% and 15%).
  For Go the two are within 3 to 5%. A pipe holds 64 KB, so a large body
  likely takes more reads and writes on Tier 1.
- 1 MB bodies cost 3.7 to 4.1 ms of CPU each through a handler, about
  three times what the Go server takes on its own.
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
| 10 | 5.52 | 19 MB |
| 1,000 | 14.31 | 29 MB |
| 10,000 | 153 | 127 MB |

The first request to a handler, with the runtime's workers started on
that first request (the default) or at boot (`eager = true`):

| Runtime | Lazy: ready, ms | Lazy: first request, ms | Lazy: second request, ms | Eager: ready, ms |
|---|--:|--:|--:|--:|
| Bun, Tier 2 | 3.30 | 67.34 | 0.452 | 70.54 |
| Node, Tier 2 | 4.78 | 313 | 0.627 | 289 |
| Python, Tier 2 | 2.62 | 199 | 0.558 | 207 |
| Go, Tier 2 | 3.54 | 12.53 | 0.359 | 15.81 |
| Rust, Tier 2 | 4.74 | 8.39 | 0.374 | 11.26 |

Lazy start moves the workers' startup from boot to the first request:
about 10 ms for compiled workers, 67 ms for Bun, and 200 to 310 ms for
Python and Node.

**These times depend on the machine's state more than on the release.**
The 10,000-route start took 157 ms in the v0.3.1 run and 89 ms in the
v0.3.0 run; timed side by side, those two binaries both ranged from about
115 to 250 ms. The 10-route start was 3.2 ms in the v0.3.1 run and is
5.5 ms here. In a separate comparison, the v0.3.1 and v0.4.0 binaries
were started in turn 25 times each on an idle machine, and took the same
time: about 21 ms at the median with 10 routes and 56 ms with 1,000. Their
fastest starts with 10 routes were 3.1 and 3.2 ms. The tables above were
taken at the end of the run, on a processor that had been busy for over
an hour.

**The wasm core on a serverless function.** A fresh Node process loads
the core through the Node host (`@unswornjs/sdk/node`), the way a Vercel,
Netlify or Lambda function does on a cold start, and answers a `POST`
with a body schema. The platform's own startup isn't included.

| Routes | Node started, ms | SDK imported, ms | First response, ms | Total, ms | Warm request, µs | First request to another route, ms |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 15.45 | 53.55 | 77.89 | 146 | 157 | – |
| 100 | 15.39 | 54.95 | 94.72 | 169 | 159 | 5.30 |
| 1,000 | 15.38 | 53.57 | 159 | 229 | 156 | 5.25 |

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
| `unsworn` binary (x86_64 musl, as released) | 19.4 MB |
| wasm core (`unsworn_bg.wasm`) | 4.6 MB, 1.42 MB gzipped |

## Running them

```sh
cargo install oha                 # wrk comes from your package manager
python3 bench/run.py build        # Unsworn, the SDK workers, the Go upstream, the wasm core
python3 bench/run.py run          # about an hour and a quarter; writes bench/results/<date>-<cpu>.json
python3 bench/run.py report bench/results/<file>.json   # these tables, as markdown
```

`run` measures `target/dist/unsworn`, which `build` makes. To measure
another binary, such as a released one, set `UNSWORN_BIN` to its path.

`run` takes `--only` with a comma-separated list of groups
(`declarative`, `proxy`, `handlers`, `work`, `policies`, `mcp`,
`routes`, `bodies`, `startup`), and `--quick` for a single 3-second pass to check the
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
