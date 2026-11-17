# Which endpoint costs what: `unsworn top`

Unsworn lets one endpoint move to a faster language without touching the
rest. `unsworn top` says which one is worth moving: what each route costs
on a running server, and how much of that is its handler.

```
$ unsworn top
unsworn top: the last 2.0s

ROUTE                    REQ/S       P50       P99  IN HANDLER   CPU/REQ     CPU  ERRORS  RUNTIME
GET /search               41.5      24ms      61ms         98%      23ms    0.95       -  py
GET /api/users/{id}      310.0    0.62ms    1.47ms         71%    0.21ms    0.07       -  ts
POST /api/summarize       12.0    1.80ms    3.10ms         88%    1.10ms    0.01    8.3%  py
GET /health               20.0    0.09ms    0.15ms           -         -       -       -  -

              WORKERS     CPU     MEMORY
py                  2    0.96     118 MB
ts                  1    0.07     117 MB
unsworn                  0.11      28 MB
```

Here `/search` uses nearly a whole core for 41 requests a second, and 98%
of each request is spent in its Python handler. That's the one to rewrite.
`/api/users/{id}` gets seven times the traffic for a fourteenth of the
CPU, and wouldn't repay the work.

## The columns

| Column | What it is |
|---|---|
| `ROUTE` | The method and the route's pattern, as in the access log. Every path that matches one pattern is one row. |
| `REQ/S` | Requests a second over the period. |
| `P50`, `P99` | How long half, and 99 in 100, of the requests took at most, from when Unsworn read the request to when it had the response's headers. |
| `IN HANDLER` | The share of that time Unsworn spent waiting for handlers: the route's own, and any `before` handlers. The rest is Unsworn's: reading the request, the policies, the worker's queue. |
| `CPU/REQ` | The handler's CPU time for one request. An estimate; see below. |
| `CPU` | The same as CPU cores in use: `0.95` is 95% of one core. |
| `ERRORS` | The share of requests answered `500` or above. |
| `RUNTIME` | The runtime that ran the route's handler. |

Routes are listed costliest handler first. Below them is what each
runtime's workers and Unsworn itself used: workers, CPU cores and memory.

### How CPU is worked out

Unsworn can't see inside a worker, but it knows two things: how much CPU
each runtime's worker processes used (from `/proc`), and how long each
route's requests waited for a handler. `top` gives each route the share of
its runtime's CPU that matches its share of the runtime's handler time.

That's right when handlers spend their time computing. It's off when one
spends its time waiting for something else, such as a database: the wait
counts as handler time, so that route is given CPU that a busier route in
the same runtime used. `IN HANDLER` and `P50` are measured, not estimated,
so read those when the CPU figures look odd. A runtime with one route has
nothing to share out, and its figure is exact.

## Running it

```sh
unsworn top               # every 2 seconds, until you stop it
unsworn top -i 10         # every 10 seconds
unsworn top --once        # totals since the server started, then exit
unsworn top --json        # the server's own numbers, for a script
unsworn top -n 50         # list 50 routes instead of 25
```

`top` reads `/_unsworn/stats` from the server's admin listener, so the
config needs one:

```toml
[server]
admin_listen = "127.0.0.1:9090"
```

It finds the address in the project's config. `--addr host:port` points it
somewhere else, such as a port forwarded from a container. Under
`unsworn dev` there's nothing to set up: the dev server answers
`/_unsworn/stats` itself.

Like the metrics endpoint, the admin listener has no authentication. Keep
it on an address only you can reach.

## For dashboards

`/_unsworn/stats` is JSON with the same numbers as totals since the
server started: per route the request count, errors, total seconds,
seconds in handlers and a histogram of durations, and per runtime the
workers, CPU seconds and memory.

The Prometheus endpoint (`/_unsworn/metrics`) has
`unsworn_handler_duration_seconds_sum` next to
`unsworn_request_duration_seconds_sum`, with the same labels, so the
handler's share of a route's time can be graphed.

## Trying the rewrite

Once `top` has picked the endpoint, [`shadow`](shadow.md) runs the new
handler next to the old one on real requests and compares their answers.
`top` then shows the two side by side.
