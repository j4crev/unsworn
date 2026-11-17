# Benchmarks

What Unsworn costs, measured: in front of an existing app, around a
handler call, for each policy, and at startup. The harness is in
[`bench/`](../bench), and every number here comes from one run of it
against the released v0.3.1 binary, saved in
[`bench/results/2026-11-16-v0.3.1-intel-core-i5-1145g7.json`](../bench/results/2026-11-16-v0.3.1-intel-core-i5-1145g7.json).
The two handler tables ([Handler calls](#handler-calls) and
[A handler that does work](#a-handler-that-does-work)) and the handler
rows of the summary come from a second run of those two groups the same
day, saved in
[`bench/results/2026-11-16-v0.3.1-handlers-intel-core-i5-1145g7.json`](../bench/results/2026-11-16-v0.3.1-handlers-intel-core-i5-1145g7.json).
It has the same binary, with two SDK changes made after v0.3.1: a Rust
worker has one thread by default, and the Python SDK runs sync handlers on
one thread until they need more.

## Summary

On two cores of a laptop CPU (an Intel i5-1145G7, details in
[Setup](#setup)):

| What Unsworn does | Requests/s | CPU per request | p50 latency at 1,000 req/s |
|---|--:|--:|--:|
| Answers a static `respond`, `redirect`, 404 or rejection | 131,000–166,000 | 21–26 µs | 0.16 ms (Go `net/http`: 0.14 ms) |
| `proxy` to an existing app | 59,000 | 59 µs | 0.26 ms (the app directly: 0.14 ms) |
| `proxy` with JWT and a rate limit | 48,000 | 75 µs | 0.27 ms |
| Calls a TypeScript handler on Bun that only answers "hello" | 63,000 | 58 µs | 0.23 ms (Bun on its own: 0.15 ms) |
| Calls a Rust handler that only answers "hello" | 57,000 | 61 µs | 0.21 ms |
| Calls a TypeScript handler on Bun that searches 2,000 names | 3,300 | 1.2 ms | 1.3 ms at 20 req/s |
| Calls the same search handler in Rust | 4,700 | 0.84 ms | 1.0 ms at 20 req/s |

The "hello" rows measure the call into a handler, which costs about the
same in every language. They say nothing about how fast a language runs
your code. The search rows do: see
[A handler that does work](#a-handler-that-does-work).

- **Requests Unsworn answers itself cost 21 to 31 µs of CPU each,**
  against 17 µs for Go's standard library serving "hello".
- **A handler call or a proxied request adds about 0.05 to 0.12 ms** when
  the server isn't overloaded, and costs two to four times the CPU of a
  static answer.
- **Policies are cheap,** 1 to 5 µs each, except JWT verification (9 µs)
  and JSON Schema validation of a body (about 24 µs for 1 KB).
- **Large JSON bodies are the expensive case.** A 64 KB JSON body to a
  handler costs 1.2 to 1.9 ms of CPU, five to six times what the same
  size costs as raw bytes.
- **Route count doesn't matter:** 10,000 routes match as fast as 10.
- **The binary is ready in 3 ms** with 10 routes and 90 to 160 ms with
  10,000. On a serverless function, the wasm core answers its first
  request about 135 ms after Node starts.
- **A trivial handler won't beat its language on its own.** Bun serves
  "hello" at 163,000 req/s on one core by itself, and at 63,000 through
  Unsworn. What Unsworn adds is the rules in front of it, and the same
  rules for every language.
- **The language matters once the handler does work.** One search
  handler, written the same way in each language, costs 0.84 ms of CPU a
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

### Setup

| | |
|---|---|
| CPU | Intel Core i5-1145G7 (4 cores, 8 threads), `powersave` governor |
| Memory | 16 GB |
| OS | Linux 7.2 |
| Unsworn | 0.3.1, the released `x86_64-unknown-linux-musl` binary |
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
| Go net/http, for reference | 194,764 | 17.2 | 0.294 | 1.27 | 0.144 | 0.379 | 0.975 | 18 MB |
| `respond` (static JSON) | 130,962 | 26.1 | 0.484 | 0.869 | 0.164 | 0.407 | 0.481 | 129 MB |
| `redirect` | 165,716 | 20.8 | 0.382 | 0.711 | 0.162 | 0.396 | 0.477 | 127 MB |
| No route (404) | 156,446 | 22.0 | 0.405 | 0.788 | 0.162 | 0.381 | 0.417 | 126 MB |
| Rejected: missing JWT (401) | 144,916 | 24.0 | 0.437 | 0.840 | 0.165 | 0.422 | 0.563 | 127 MB |
| Rejected: rate limited (429) | 138,884 | 25.1 | 0.456 | 0.891 | 0.167 | 0.422 | 0.485 | 127 MB |
| Cache hit | 116,753 | 30.8 | 0.539 | 1.04 | 0.170 | 0.424 | 0.526 | 129 MB |
| Same handler, uncached (Bun, Tier 2) | 63,798 | 57.3 | 0.900 | 5.30 | 0.227 | 0.616 | 0.761 | 348 MB |

- A static answer, a redirect, a 404, a rejection and a cache hit all cost
  about the same: 21 to 31 µs of CPU. None of them reaches a worker.
- A cache hit costs about half of the handler call it replaces.
- Memory here includes the four Bun workers this project starts for its
  handler routes. Unsworn on its own uses 18 to 34 MB (see
  [Startup](#startup)).

### In front of an existing app

Unsworn's `proxy` in front of a Go server, compared with calling the
server directly and with Caddy and nginx in the same place.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| Direct to the app, no proxy | 190,133 | 17.5 | 0.306 | 1.32 | 0.142 | 0.366 | 0.439 | 18 MB |
| Unsworn `proxy` | 58,800 | 58.9 | 1.06 | 2.15 | 0.263 | 0.476 | 0.699 | 28 MB |
| Unsworn `proxy` with JWT and a rate limit | 47,640 | 74.9 | 1.30 | 2.55 | 0.265 | 0.510 | 0.861 | 29 MB |
| Unsworn `proxy`, access log off | 57,247 | 59.9 | 1.09 | 2.45 | 0.241 | 0.462 | 0.718 | 28 MB |
| Caddy `reverse_proxy` | 30,822 | 117 | 1.88 | 6.51 | 0.179 | 0.364 | 0.558 | 25 MB |
| nginx `proxy_pass`, keepalive, access log off | 89,138 | 23.5 | 0.680 | 2.12 | 0.219 | 0.429 | 0.588 | 13 MB |

- Unsworn adds about 0.12 ms at the median when it isn't busy, and
  handles 59,000 req/s on two cores. Its upstream connections are reused,
  not opened per request.
- nginx uses about 40% of Unsworn's CPU per request, so it reaches about
  1.5 times the throughput. Unsworn's proxy adds the `Forwarded`,
  `X-Forwarded-*`, `Via` and request-id headers and runs the full policy
  pipeline on every request, but there's likely room to close some of
  that gap.
- Turning the access log off made no measurable difference on this
  route. On a static route, which does far less per request, a separate
  comparison put the log at about 4 µs of CPU per request.
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
| Go on its own | 181,171 | 18.5 | 0.317 | 1.36 | 0.151 | 0.329 | 0.391 | 15 MB |
| Bun on its own, 1 process | 163,105 | 4.5 | 0.380 | 0.671 | 0.146 | 0.343 | 0.397 | 36 MB |
| Bun on its own, 4 processes (reusePort) | 240,058 | 10.8 | 0.128 | 0.526 | 0.149 | 0.330 | 0.389 | 145 MB |
| Node on its own, 1 process | 83,585 | 10.3 | 0.749 | 0.982 | 0.159 | 0.371 | 0.437 | 90 MB |
| Node on its own, 4 processes (cluster) | 158,000 | 21.7 | 0.421 | 1.21 | 0.172 | 0.399 | 0.488 | 409 MB |
| Unsworn → TypeScript on Bun, Tier 2, 1 worker | 66,743 | 46.1 | 0.894 | 2.94 | 0.223 | 0.636 | 0.790 | 111 MB |
| Unsworn → TypeScript on Bun, Tier 2, 4 workers | 63,241 | 58.0 | 0.907 | 5.33 | 0.225 | 0.584 | 1.25 | 351 MB |
| Unsworn → TypeScript on Bun, Tier 1, 4 workers | 66,549 | 54.7 | 0.871 | 4.68 | 0.246 | 0.662 | 0.805 | 326 MB |
| Unsworn → TypeScript on Node, Tier 2, 4 workers | 61,774 | 59.1 | 0.955 | 2.93 | 0.242 | 0.604 | 0.805 | 593 MB |
| Unsworn → Python, Tier 2, 4 workers | 25,344 | 139 | 2.38 | 5.42 | 0.253 | 0.768 | 1.45 | 131 MB |
| Unsworn → Python, Tier 1, 4 workers | 30,441 | 124 | 1.99 | 4.60 | 0.234 | 0.762 | 1.04 | 133 MB |
| Unsworn → Python, no SDK, Tier 1, 4 workers | 40,660 | 83.7 | 1.57 | 8.30 | 0.240 | 0.640 | 0.775 | 90 MB |
| Unsworn → Go, Tier 2, 4 workers | 38,845 | 93.5 | 1.54 | 4.87 | 0.189 | 0.405 | 0.712 | 106 MB |
| Unsworn → Go, Tier 1, 4 workers | 42,190 | 83.3 | 1.40 | 4.64 | 0.137 | 0.860 | 1.25 | 104 MB |
| Unsworn → Rust, Tier 2, 4 workers | 57,159 | 61.0 | 1.02 | 2.89 | 0.209 | 0.592 | 0.737 | 60 MB |
| Unsworn → Rust, Tier 1, 4 workers | 74,352 | 47.8 | 0.805 | 2.48 | 0.217 | 0.610 | 0.705 | 58 MB |
| Unsworn → Rust, 4 threads in each worker, Tier 2, 4 workers | 53,867 | 68.2 | 1.12 | 2.93 | 0.162 | 0.440 | 0.795 | 62 MB |

- **These rows compare the SDKs, not the languages.** The handler does
  nothing, so each row is Unsworn's side of the call plus that SDK's
  reading and writing of one small message. Bun's is native code, which
  is why TypeScript does as well here as Rust.
- When the server isn't busy, a handler call adds up to about 0.1 ms over
  the language on its own.
- At full load, a call costs 46 to 94 µs of CPU, Unsworn's side and the
  worker's together, and four threads handle 39,000 to 74,000 calls a
  second in every SDK except Python's. Unsworn's own side is 40 to 50 µs
  of that whatever the language, so it sets the ceiling for a handler
  this small.
- More workers don't help a handler this small: one Bun worker does as
  well as four. They matter for handlers that do real work.
- **Tier 2 isn't faster than Tier 1 for small requests.** Tier 1 came out
  ahead in every SDK, by 5% for Bun and 30% for Rust. Tier 2's advantage
  shows with large bodies (see [Request bodies](#request-bodies)).
- **The Python SDK costs about one and a half times what a worker
  without an SDK does** (124 against 84 µs per request on Tier 1). These
  handlers are sync functions, which the SDK runs on a thread. An
  `async def` handler skips that and cost about 115 µs in a separate
  comparison.
- Memory is mostly the workers: four Node processes take about 600 MB,
  four Bun processes about 350 MB, and four Rust processes about 60 MB.
- **A Rust worker uses one thread unless it's given more.** With
  `UNSWORN_WORKER_THREADS=4`, the same handler costs 68 µs a call against
  61 µs, because every request crosses threads on its way in and out of
  the worker. The extra threads do answer sooner when the server is
  nearly idle: 0.16 ms against 0.21 ms at 1,000 requests a second.
- **Rounds of one row differ by up to 20%.** The Rust row on Tier 2 was
  69,000, 56,000 and 57,000 req/s in the three rounds, and 57,000, 69,000
  and 68,000 in a run a few hours earlier. Rust and Bun are level here.

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
| Unsworn → TypeScript on Bun | 3,294 | 1,206 | 1.28 | 1.64 | 209 MB |
| Unsworn → TypeScript on Node | 2,377 | 1,679 | 1.56 | 1.91 | 459 MB |
| Unsworn → Python | 75 | 53,340 | 22.58 | 23.82 | 118 MB |
| Unsworn → Go | 4,336 | 915 | 1.09 | 1.50 | 105 MB |
| Unsworn → Rust | 4,744 | 837 | 1.02 | 1.45 | 64 MB |

- **Rust does this work for 0.84 ms of CPU, Go for 0.92 ms, Bun for
  1.2 ms, Node for 1.7 ms and Python for 53 ms.** So moving this endpoint
  from TypeScript on Bun to Rust gives 1.4 times the requests per core,
  from Node twice as many, and from Python about 65 times.
- The call into the handler is about 55 µs of each figure, which is why
  it decides the "hello" rows above and hardly shows here.
- One request on a quiet server takes 1.0 ms in Rust, 1.1 ms in Go,
  1.3 ms on Bun, 1.6 ms on Node and 23 ms in Python.
- The Rust workers take 64 MB between them, a third of Bun's 209 MB and
  a seventh of Node's 459 MB.
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
| No policy | 151,247 | 23.1 | 0.416 | 0.801 | 0.163 | 0.392 | 0.433 | 26 MB |
| JWT (HS256) | 111,469 | 31.8 | 0.566 | 1.09 | 0.176 | 0.386 | 0.475 | 25 MB |
| API key | 135,366 | 26.1 | 0.466 | 0.883 | 0.166 | 0.411 | 0.499 | 26 MB |
| Rate limit, memory store | 144,572 | 24.4 | 0.436 | 0.825 | 0.169 | 0.424 | 0.506 | 26 MB |
| Rate limit, Redis store | 80,127 | 35.6 | 0.753 | 1.43 | 0.336 | 0.713 | 0.829 | 27 MB |
| CORS (request with Origin) | 141,846 | 24.5 | 0.445 | 0.855 | 0.165 | 0.408 | 0.466 | 26 MB |
| Path and query validation | 122,724 | 27.8 | 0.495 | 1.48 | 0.166 | 0.486 | 1.06 | 27 MB |
| All of JWT, rate limit, CORS, path and query | 79,867 | 42.9 | 0.767 | 1.89 | 0.181 | 0.505 | 0.746 | 26 MB |
| `proxy`, 1 KB JSON body | 52,983 | 65.0 | 1.16 | 2.81 | 0.248 | 0.486 | 0.837 | 31 MB |
| `proxy`, same body checked against a JSON Schema | 40,786 | 88.5 | 1.50 | 3.22 | 0.231 | 0.512 | 0.598 | 30 MB |

| Policy | Added CPU per request |
|---|--:|
| API key, rate limit (memory), CORS, path and query validation | 1 to 5 µs each |
| JWT, HS256 | 9 µs |
| Rate limit, Redis store | 13 µs, plus a round trip to Redis (0.17 ms here) |
| JSON Schema on a 1 KB body | 24 µs |

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
| `GET /text` (the route) | 61,675 | 58.7 | 0.938 | 5.27 | 0.225 | 0.616 | 0.822 | 354 MB |
| `tools/call text` (the same handler) | 40,716 | 92.8 | 1.46 | 4.83 | 0.262 | 0.622 | 0.871 | 274 MB |
| `tools/call echo`, arguments checked against its schema | 32,986 | 116 | 1.83 | 5.03 | 0.260 | 0.632 | 0.944 | 276 MB |
| `tools/list` | 69,915 | 52.5 | 0.861 | 2.23 | 0.183 | 0.465 | 0.570 | 200 MB |

- A tool call costs about one and a half times the route it wraps (93
  against 59 µs of CPU), and adds about 0.04 ms of latency: the JSON-RPC
  body is parsed and checked against the MCP headers, and the reply is
  turned into a tool result.
- `tools/list` is answered by Unsworn alone, at about 50 µs. Its three
  runs varied more than most, from 65,000 to 81,000 req/s.

### Route table size

A `respond` route at the end of a table of 10, 1,000 and 10,000 routes,
each with a path parameter, and a catch-all behind them.

| Scenario | Req/s | CPU µs/req | p50 ms | p99 ms | p50 ms @ 1k/s | p99 ms @ 1k/s | p99.9 ms @ 1k/s | Peak memory |
|---|--:|--:|--:|--:|--:|--:|--:|--:|
| 10 routes | 152,910 | 22.6 | 0.414 | 0.772 | 0.164 | 0.415 | 0.472 | 22 MB |
| 1,000 routes | 149,089 | 23.2 | 0.425 | 0.788 | 0.166 | 0.402 | 0.467 | 27 MB |
| 10,000 routes | 153,460 | 22.5 | 0.413 | 0.773 | 0.164 | 0.395 | 0.431 | 61 MB |

Matching costs the same at every size, since routes are matched with a
radix tree. Memory grows with the table: 10,000 routes take 61 MB under
load, and 127 MB just after startup (see [Startup](#startup)).

### Request bodies

A `POST` body echoed back: by the Go server on its own, through
Unsworn's `proxy` to it, and by a Bun or Go handler on each protocol tier.
"MB/s in" counts request bodies only; the same amount goes back out. The
JSON rows are different for handlers: Unsworn checks the body and sends
it on as the request's `json`, and the handler answers `204` without
sending it back. In the "sent back as JSON" rows the handler answers with
the body as its response's `json`, inside a small object.

| Scenario | Req/s | MB/s in | CPU µs/req | p50 ms | p99 ms | Peak memory |
|---|--:|--:|--:|--:|--:|--:|
| 1 KB: Go on its own | 141,063 | 144 | 24.0 | 0.407 | 1.83 | 18 MB |
| 64 KB: Go on its own | 18,292 | 1,199 | 176 | 3.81 | 23.43 | 20 MB |
| 1 MB: Go on its own | 2,068 | 2,169 | 1382 | 6.94 | 25.10 | 47 MB |
| 64 KB JSON: Go on its own | 17,511 | 1,114 | 186 | 3.93 | 25.21 | 21 MB |
| 1 KB: Unsworn `proxy` → Go | 52,162 | 53 | 66.6 | 1.18 | 2.59 | 29 MB |
| 64 KB: Unsworn `proxy` → Go | 9,129 | 598 | 169 | 7.33 | 42.42 | 73 MB |
| 1 MB: Unsworn `proxy` → Go | 1,166 | 1,223 | 1081 | 12.61 | 43.27 | 119 MB |
| 64 KB JSON: Unsworn `proxy` → Go | 9,532 | 606 | 153 | 5.74 | 42.74 | 44 MB |
| 1 KB: Unsworn → Bun, Tier 2 | 55,300 | 57 | 66.3 | 1.03 | 5.57 | 334 MB |
| 64 KB: Unsworn → Bun, Tier 2 | 15,018 | 984 | 248 | 4.09 | 9.34 | 518 MB |
| 1 MB: Unsworn → Bun, Tier 2 | 1,017 | 1,066 | 3657 | 15.45 | 25.92 | 606 MB |
| 64 KB JSON: Unsworn → Bun, Tier 2 | 3,290 | 209 | 1193 | 19.16 | 31.71 | 393 MB |
| 64 KB JSON, sent back as JSON: Unsworn → Bun, Tier 2 | 2,166 | 138 | 1818 | 29.03 | 48.03 | 414 MB |
| 1 KB: Unsworn → Bun, Tier 1 | 59,633 | 61 | 61.5 | 0.965 | 4.67 | 336 MB |
| 64 KB: Unsworn → Bun, Tier 1 | 11,876 | 778 | 307 | 5.21 | 10.02 | 503 MB |
| 1 MB: Unsworn → Bun, Tier 1 | 886 | 929 | 4119 | 17.78 | 25.88 | 546 MB |
| 1 KB: Unsworn → Go, Tier 2 | 40,969 | 42 | 90.0 | 1.46 | 4.53 | 104 MB |
| 64 KB: Unsworn → Go, Tier 2 | 11,618 | 761 | 322 | 5.23 | 12.93 | 127 MB |
| 1 MB: Unsworn → Go, Tier 2 | 1,006 | 1,055 | 3776 | 15.23 | 30.27 | 229 MB |
| 64 KB JSON: Unsworn → Go, Tier 2 | 2,067 | 132 | 1905 | 30.42 | 47.55 | 153 MB |
| 64 KB JSON, sent back as JSON: Unsworn → Go, Tier 2 | 1,266 | 81 | 3108 | 49.52 | 79.21 | 145 MB |
| 1 KB: Unsworn → Go, Tier 1 | 47,432 | 49 | 77.6 | 1.25 | 3.84 | 101 MB |
| 64 KB: Unsworn → Go, Tier 1 | 11,219 | 735 | 336 | 5.46 | 11.84 | 181 MB |
| 1 MB: Unsworn → Go, Tier 1 | 935 | 980 | 4012 | 16.73 | 27.34 | 252 MB |

- The proxy streams bodies: it moves 1.2 GB/s of 1 MB bodies in about
  120 MB of memory. Its 64 KB and 1 MB rows are limited by the upstream,
  which has a single core here.
- A handler has to receive the whole body and send it back, so large
  bodies cost more than through the proxy: 930 to 1,070 MB/s of 1 MB
  bodies for Bun and Go.
- Tier 2 beats Tier 1 for Bun at 64 KB and above (by about 25% and 15%).
  For Go the two are within 4 to 8%. A pipe holds 64 KB, so a large body
  likely takes more reads and writes on Tier 1.
- 1 MB bodies cost 3.7 to 4.1 ms of CPU each through a handler, about
  three times what the Go server takes on its own.
- **A JSON body still costs a handler route more than raw bytes do.**
  64 KB of JSON takes 1.2 ms of CPU to a Bun handler and 1.9 ms to a Go
  one, at 3,300 and 2,100 requests a second, against 0.25 to 0.32 ms and
  11,600 to 15,000 for 64 KB of raw bytes. Unsworn checks the document,
  refusing an object with two members of one name, without building it in
  memory, and builds it only when the route has a body schema. It then
  sends the client's text to the worker, which parses it. In a separate
  measurement the worker's side was a little over half of what's left. A `proxy` route without a body schema passes JSON
  through unread, at the speed of raw bytes.
- **Sending 64 KB of JSON back adds about half as much again:** 1.8 ms
  of CPU for Bun and 3.1 ms for Go with the round trip, at 2,200 and
  1,300 requests a second. The worker writes
  the JSON out, and Unsworn checks it and sends the worker's text to the
  client.

### Startup

How long `unsworn serve` takes to answer its first request, by the size
of its route table:

| Routes | Start to first response, ms | Memory at idle |
|--:|--:|--:|
| 10 | 3.16 | 18 MB |
| 1,000 | 12.41 | 34 MB |
| 10,000 | 157 | 127 MB |

The first request to a handler, with the runtime's workers started on
that first request (the default) or at boot (`eager = true`):

| Runtime | Lazy: ready, ms | Lazy: first request, ms | Lazy: second request, ms | Eager: ready, ms |
|---|--:|--:|--:|--:|
| Bun, Tier 2 | 2.65 | 67.98 | 0.532 | 70.54 |
| Node, Tier 2 | 2.71 | 282 | 0.687 | 270 |
| Python, Tier 2 | 2.60 | 199 | 0.674 | 206 |
| Go, Tier 2 | 2.59 | 13.04 | 0.342 | 14.77 |
| Rust, Tier 2 | 2.92 | 8.63 | 0.322 | 11.31 |

Lazy start moves the workers' startup from boot to the first request:
about 10 ms for compiled workers, 68 ms for Bun, and 200 to 280 ms for
Python and Node.

The 10,000-route start took 89 ms in the v0.3.0 run. Timed again side by
side, the two binaries both ranged from about 115 to 250 ms, so this
number depends on the machine's state more than on the release.

**The wasm core on a serverless function.** A fresh Node process loads
the core through the Node host (`@unswornjs/sdk/node`), the way a Vercel,
Netlify or Lambda function does on a cold start, and answers a `POST`
with a body schema. The platform's own startup isn't included.

| Routes | Node started, ms | SDK imported, ms | First response, ms | Total, ms | Warm request, µs | First request to another route, ms |
|--:|--:|--:|--:|--:|--:|--:|
| 1 | 16.12 | 48.44 | 72.51 | 135 | 159 | – |
| 100 | 14.89 | 47.98 | 87.00 | 149 | 156 | 5.23 |
| 1,000 | 15.40 | 47.47 | 152 | 217 | 154 | 5.12 |

- A cold start answers about 135 ms after Node starts, with most of it in
  importing the SDK and compiling the wasm module.
- Warm, a request through the wasm core costs about 155 µs, against about
  25 µs for the binary.
- Each route's first request costs about 5 ms more, since the core
  compiles a route's schemas when a request first needs them. On a busy
  function that's paid once per route per instance.

### Sizes

| | |
|---|--:|
| `unsworn` binary (x86_64 musl, as released) | 18.1 MB |
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
