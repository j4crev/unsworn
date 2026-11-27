# OpenTelemetry: the design and its phases

The design of Unsworn's spans and exporter, and the phases they're built
in. How to use what's built is in [`config.md`](config.md#telemetry) and
[`tracing.md`](tracing.md).

## Why

Without spans of its own, a trace has a gap wherever Unsworn is. With them,
once every phase is built:

| What a user gets | Without them |
|---|---|
| Unsworn's own time in the trace: auth, rate limits, validation, the wait for a worker, cache lookups | Invisible. A slow handler and a busy worker pool look the same |
| Rejected and failed requests (`401`, `429`, `413`, `504`, `502`) traced | No span records them unless the handler ran and recorded one |
| Traces that start at the front door, for the share a sampler picks | A request without a `traceparent` is traced only if something behind Unsworn starts a trace |
| A function or service called with `ctx.call` or `ctx.fetch` nested under the call that made it | It sits next to the handler, as a child of whatever sent the request |
| A `trace_id` in the access log | Nothing links a log line to a trace |
| Spans pushed over OTLP to any backend that takes it | Nothing is exported |

[`unsworn top`](top.md) answers a different question. It shows live totals
per route for one server, to find the handler worth rewriting. A trace
shows what happened to one request, across instances and services, and is
there to read later.

Two groups get nothing from this at first: users with no tracing backend,
and projects on Cloudflare, Vercel or Netlify, which wait for the last
phase.

## Constraints

- **Off by default.** Without `[telemetry]`, behaviour and output are
  exactly as today.
- **No sampling decision without grounds.** With telemetry off, Unsworn
  still starts no traces, for the reason [`tracing.md`](tracing.md) gives.
  A sampler set in the config is the grounds.
- **No OpenTelemetry dependency in `unsworn-core` or `unsworn-wasm`.** The
  core keeps building for `wasm32`, and the WebAssembly core stays under
  the 1.6 MB gzipped that CI allows.
- **The cost per request is measured,** off and on.

## Where telemetry goes

`[telemetry]` names an [`[outbound.<name>]`](outbound.md) service. It has
no URL of its own.

```toml
[outbound.otel]
url     = "https://otlp.example.com"
headers = { Authorization = "Bearer {env.OTLP_TOKEN}" }
methods = ["POST"]
paths   = ["/v1/traces"]

[telemetry]
outbound     = "otel"
service_name = "api"
sample       = 0.1
```

When the config is compiled:

- The service has to be declared.
- It has to allow `POST /v1/traces`, checked as a handler's `ctx.fetch`
  is.
- An `http://` URL to a host that isn't loopback gets a warning, since
  spans carry client addresses.

When spans are sent:

- **The environment can't change where they go.**
  `OTEL_EXPORTER_OTLP_ENDPOINT` and `OTEL_EXPORTER_OTLP_HEADERS` aren't
  read. Only the service name (`OTEL_SERVICE_NAME`) and resource
  attributes may come from the environment.
- The request is built the way a `ctx.fetch` is, and sent with the
  server's own HTTP client, with the service's headers and `timeout`.
  Redirects aren't followed, so one can't take spans somewhere the config
  doesn't name.
- The collector's token stays in the service's `headers`, so no worker
  gets the variable it's read from.
- Spans record the path without its query string.

**The trade-off.** A declared service is one handlers can `ctx.fetch`: a
worker handler reaches any of them, and an HTTP runtime those its `calls`
names. The worst a handler can do with this one is send made-up spans with
Unsworn's token. If that matters later, a setting could keep a service for
Unsworn's own use.

## Phases

### Phase 0: config and checks

**Done.**

| | |
|---|---|
| Scope | The `[telemetry]` section; the checks above when the config is compiled; a `telemetry` entry in `unsworn plan`, with `unsworn diff` marking what got looser (telemetry added, a higher `sample`, another service); `unsworn check --target` saying the section is ignored on Cloudflare, Vercel and Netlify. Nothing is exported |
| Files | `unsworn-core`: `config.rs`, `project.rs`, `plan.rs`, `targets.rs`; `schema/unsworn.schema.json`; [`config.md`](config.md) |
| Done when | A bad service name, a service that doesn't allow the export and a `sample` outside 0 to 1 fail `unsworn check`, and the plan shows the entry |

### Phase 1: a span for every sampled request, exported

**Done.**

For the `unsworn` binary only.

| | |
|---|---|
| Scope | Batched OTLP/HTTP export; a sampler that follows an incoming `traceparent`'s sampled flag and picks `sample` of the requests without one; one `SERVER` span per sampled request with the HTTP semantic-convention attributes; that request's `traceparent` replaced by the span's context, so the propagation there already is nests handlers, upstreams, functions and services under it; `trace_id` in the access log, in both formats; a flush on shutdown |
| Files | `crates/unsworn/src/telemetry/` (new), `server/host.rs` (`handle_inner`), `server/access.rs` |
| Done when | A fake collector receives the spans, `lines_match_what_tracing_writes` still passes, and the overhead is measured |

**A request that isn't sampled is left alone.** It keeps its `traceparent`
and `tracestate` as they came, and gets none if it had none. A span id
that's never exported would leave the spans below it with no parent.

**Export.** Spans wait in a queue of 2,048. When it's full, new ones are
dropped and a warning says how many. They're sent in batches of 512, or
5 s after the first one in a batch, and what's waiting is sent when the
server shuts down. A failed batch isn't retried yet. One request is in
flight at a time, so at most 512 spans are sent in each round trip to the
collector: a distant or slow one drops spans under load. A span keeps up to
2,048 bytes of the path, since the client picks its length.

**The exporter is written by hand,** as the metrics and the access log are:
OTLP/HTTP with protobuf, and no new runtime dependencies. The
OpenTelemetry crates aren't used:

- They'd add several crates to the binary, to encode a few messages.
- The crates change their API in most releases.
- Spans have to be sent as the `[outbound]` service says, with the
  server's own HTTP client, whatever builds them.
- A span is built after its request, from times the server already
  records, so a live span object would add cost to every sampled request
  for nothing.
- The part of the protobuf format spans need is small.

The tests decode what's sent with OpenTelemetry's own message definitions.

**The cost.** The `telemetry` group in [`bench/run.py`](../bench/run.py)
measured the cost on a shared server with 4 vCPUs, three runs of each, next
to a build without any of this:

| CPU µs per request (median) | `respond` | `proxy` |
|---|--:|--:|
| A build without telemetry | 20.6 | 42.0 |
| No `[telemetry]` | 18.1 | 39.9 |
| 1% sampled | 17.5 | 38.2 |
| Every request sampled | 23.7 | 43.8 |

With telemetry off, no cost shows. Sampling everything added about 2 to
6 µs. Runs of the same row differed by up to 9 µs, and requests per second
by too much to compare, so these are rough: measure on a quiet machine
before publishing figures in [`benchmarks.md`](benchmarks.md). No span was
dropped at up to 60,000 requests a second, with a collector on the same
machine that discards them.

### Phase 2: spans for the I/O the server does

**Done.**

| | |
|---|---|
| Scope | A span for each handler call (the wait for a worker recorded apart from the time in it), `before` handler, `proxy` upstream, JWKS or API-key verifier request, `ctx.call` and `ctx.fetch`, with the `traceparent` sent along set to that span, and failures (timeouts, crashed workers, unreachable services) as the span's status |
| Files | `server/host.rs`, `server/proxy.rs`, `workers.rs` |
| Done when | A trace through a handler, a function and a service nests as the calls did |

| Span | Name | Child of |
|---|---|---|
| A handler call | `handler <ref>` | The server span |
| A `before` handler call | `before <ref>` | The server span |
| A `proxy` upstream, an HTTP runtime's server included | The method | The server span |
| A JWKS or API-key verifier request | The method | The server span |
| `ctx.call`, or an HTTP runtime's call to a function | `call <function>` | The span the calling handler got |
| `ctx.fetch`, or an HTTP runtime's call to a service | The method | The span the calling handler got |

- **Every one is a `CLIENT` span:** Unsworn asking something else to do
  work.
- **A span fails** when Unsworn got no answer (a crashed worker, a timeout,
  a failed connection, a `ctx.call` that didn't succeed), or when the
  answer's status is `500` or above. The status message says why.
- **Each span's `traceparent` replaces the one sent along,** so what's
  reached nests under it. The `tracestate` is kept.
- **The wait for a worker** is the attribute `unsworn.wait_ms`, starting
  one included. A span of its own would cost more.
- **A shadow handler's call gets no span.** Its answer never reaches the
  client.
- **What's given up on is still recorded,** as failed: a call or request
  that outlives its time, and the request of a client that goes away. A
  span below one that's never recorded would have no parent.

**The cost.** The same server and `telemetry` group as in Phase 1, three
runs of each. A proxied request and a handler call now make two spans.

| CPU µs per request (median) | `respond` | `proxy` | handler (Bun) |
|---|--:|--:|--:|
| No `[telemetry]` | 23.0 | 49.6 | 63.3 |
| 1% sampled | 21.2 | 50.5 | 68.4 |
| Every request sampled | 22.8 | 58.2 | 70.2 |

Sampling everything added about 7 to 9 µs to a `proxy` or handler request,
which misses the goal of a few, and its highest request rate was 10 to 14%
lower. Runs of the same row differed by up to 15 µs, so these are rough
too. Each run now loads a Bun runtime, so don't compare them with Phase
1's. No span was dropped at up to 64,000 spans a second, with a collector
on the same machine.

### Phase 3: the pipeline's own steps

**Done.**

| | |
|---|---|
| Scope | A `Host::step` hook that does nothing by default, called by the pipeline for auth, rate limits, validation, cache lookups and response validation. Span events and attributes first (`unsworn.cache`, `unsworn.rejected_by`), child spans where the timing turns out to matter |
| Files | `unsworn-core`: `pipeline/host.rs`, `pipeline/flow.rs`, `pipeline/flow/mcp.rs`, `pipeline/checks.rs`; `server/host.rs`, `telemetry/` |
| Done when | A rejected request's span says which rule rejected it, and the WebAssembly size check passes |

The core reports what it decided with `Host::step(Step)`. A `Step` is a
small value with nothing to put together: the rule that turned a request
away, the scheme that let it in, what the cache did. The default does
nothing, so the WebAssembly hosts and an unsampled request pay for a call
that returns.

| Step | When | Span event | Attribute on the request's span |
|---|---|---|---|
| `Authenticated` | Auth passed, the route's or an MCP tool's | `unsworn.authenticated` | `unsworn.auth.scheme` |
| `Checked` | Every check before `before` handlers passed: access rules, rate limits, auth, `require`, validation, the body | `unsworn.checked` | |
| `Cache` | The cache was looked in, and filled | `unsworn.cache` | `unsworn.cache`: `hit`, `miss` or `stored` |
| `ResponseInvalid` | A handler's response didn't match its schema | `unsworn.response_invalid` | `unsworn.response_validation`: `failed` |
| `Rejected` | A rule answered instead of the route's action | `unsworn.rejected` | `unsworn.rejected_by` |

- **Rules a request can be rejected by:** `origin`, `ip`, `rate_limit`,
  `auth`, `require`, `validation` (path parameters, query, headers, the
  body or an MCP tool's arguments), `max_body`, `body` (one that couldn't
  be read), `before` (a `before` handler answered), `timeout` and
  `response_validation` (with `enforce`). An MCP tool's rules count too,
  though the client gets a tool error with status `200`.
- **Events, not child spans.** An event's time shows how long the steps
  before it took. What in them waits on the network already has a span: a
  JWKS or verifier request. A rate limit or a cache lookup in Redis
  doesn't, and is the place for a child span if its time turns out to
  matter.
- **Only on the server.** The JavaScript bridge leaves `step` as it is,
  until the edge and serverless targets get spans in Phase 5.

**The cost.** The WebAssembly core is the same size (1,487 KiB gzipped).
With every request sampled, Phase 2's build and this one were run in turn
on the same machine, three times each:

| CPU µs per request (median) | `respond` | `proxy` | handler (Bun) |
|---|--:|--:|--:|
| Phase 2 | 23.6 | 56.4 | 80.0 |
| Phase 3 | 25.1 | 54.0 | 82.9 |

The differences, from −2.4 to +2.9 µs, are within what runs of the same
build differed by, so what Phase 3 adds doesn't show.

### Phase 4: a handler's span as the parent of `ctx.call`

| | |
|---|---|
| Scope | An optional `traceparent` on the protocol's `call` message, used instead of the request's own, and a way to pass it to `ctx.call` in each SDK |
| Files | `unsworn-proto`, `workers.rs`, `sdk/js`, `sdk/python`, `sdk/go`, `sdk/rust`; [`protocol.md`](protocol.md), [`tracing.md`](tracing.md) |
| Done when | In each SDK, a function called from a handler's span is its child |

- **The protocol.** The `call` message takes an optional `traceparent` and
  `tracestate`. A `traceparent` a receiver would accept is used instead of
  the calling request's, with the `tracestate` sent beside it. One that
  isn't is ignored, as is a `tracestate` alone: the call goes ahead with
  the request's trace. An older Unsworn ignores both fields, so a newer SDK
  still works with it.
- **HTTP runtimes.** A call to `$UNSWORN_CALLS_URL/call/<function>` uses
  the request's own `traceparent` and `tracestate` headers by the same
  rule. An app instrumented with OpenTelemetry sets them without any code.
- **With `[telemetry]`,** a sampled `traceparent` from a handler is the
  parent of Unsworn's `call` span, and the function goes below that. One
  that isn't sampled gets no span, and is passed on as it came: the parent
  decides.
- **`ctx.fetch` too.** With `[telemetry]`, a sampled `traceparent` among a
  fetch's headers is the parent of Unsworn's span for the fetch, and the
  service gets that span's, with the handler's `tracestate`. Any other
  header goes to the service as before, and without `[telemetry]` nothing
  changes.
- **Edge and serverless** take the same options for `ctx.call`. They record
  no spans until Phase 5.

### Phase 5: later

- **Metrics over OTLP.** Until then, an OpenTelemetry Collector can scrape
  `/_unsworn/metrics` with its Prometheus receiver. The service then also
  has to allow `POST /v1/metrics`.
- **Logs over OTLP,** worker output included, with `POST /v1/logs` allowed.
- **Edge and serverless targets,** passing the core's steps through the
  JavaScript bridge to an OpenTelemetry API the project brings.

## Verification, in each phase

- An integration test against a fake OTLP/HTTP collector: how spans nest,
  whether an incoming `traceparent` is followed, the sampling decision,
  that exports go only to the service's URL with its headers, and that a
  redirect isn't followed.
- With telemetry off, the existing tests pass unchanged.
- A `telemetry` group in [`bench/run.py`](../bench/run.py) with tracing
  off, sampling 1% and sampling everything, for `respond` and `proxy`,
  and from Phase 2 a handler that answers "hello". The goal: off costs
  nothing measurable, and sampling everything stays within a few µs of CPU
  per request.
- `cargo fmt`, `cargo clippy`, the core's `wasm32` build, `cargo deny` and
  the `dist` binary's size.

## Open questions

- **Unsworn's other requests.** JWKS URLs, API-key verifiers and `proxy`
  upstreams are written in their own sections and aren't checked against
  `[outbound]`. A rule that Unsworn sends nothing to an undeclared host
  would have to cover them too.
- **Clients decide too.** A request that arrives with a sampled
  `traceparent` is recorded whatever `sample` is, so any client can add to
  what's sent. A setting could limit that to `trusted_proxies`.
- **Lambda.** It freezes the process between invocations, so spans may be
  sent late or lost.
