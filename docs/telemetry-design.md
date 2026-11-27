# OpenTelemetry: the design and its phases

The plan for spans and an exporter in Unsworn. It isn't a page about using
them yet: today Unsworn records no spans (see [`tracing.md`](tracing.md)).

## Why

Today a trace has a gap wherever Unsworn is. With spans of its own:

| What a user gets | Today |
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

| | |
|---|---|
| Scope | The `[telemetry]` section; the checks above when the config is compiled; a `telemetry` entry in `unsworn plan`, with `unsworn diff` marking what got looser (telemetry added, a higher `sample`, another service); `unsworn check --target` saying the section is ignored on Cloudflare, Vercel and Netlify. Nothing is exported |
| Files | `unsworn-core`: `config.rs`, `project.rs`, `plan.rs`, `targets.rs`; `schema/unsworn.schema.json`; [`config.md`](config.md) |
| Done when | A bad service name, a service that doesn't allow the export and a `sample` outside 0 to 1 fail `unsworn check`, and the plan shows the entry |

### Phase 1: a span for every sampled request, exported

For the `unsworn` binary only.

| | |
|---|---|
| Scope | A tracer provider and batched OTLP/HTTP export; a sampler that follows an incoming `traceparent`'s sampled flag and picks `sample` of the requests without one; one `SERVER` span per request with the HTTP semantic-convention attributes; the request's `traceparent` replaced by that span's context, so the propagation there already is nests handlers, upstreams, functions and services under it; `trace_id` in the access log, in both formats; a flush on shutdown |
| Files | `crates/unsworn/src/telemetry.rs` (new), `server/host.rs` (`handle_inner`), `server/access.rs` |
| Done when | A fake collector receives the spans, `lines_match_what_tracing_writes` still passes, and the overhead is measured |

**The dependency gate comes first.** Add `opentelemetry`,
`opentelemetry_sdk` and `opentelemetry-otlp` with HTTP/protobuf only (no
`tonic`, no `reqwest`), then measure the `dist` binary's size and run
`cargo deny`. If either result isn't acceptable, write a small OTLP/HTTP
span exporter by hand instead, as the metrics are. `tracing-opentelemetry`
isn't used: it adds work to every span and ties export to the log filter.

### Phase 2: spans for the I/O the server does

| | |
|---|---|
| Scope | A span for each handler call (the wait for a worker recorded apart from the time in it), `before` handler, `proxy` upstream, JWKS or API-key verifier request, `ctx.call` and `ctx.fetch`, with the `traceparent` sent along set to that span, and failures (timeouts, crashed workers, unreachable services) as the span's status |
| Files | `server/host.rs`, `workers.rs` |
| Done when | A trace through a handler, a function and a service nests as the calls did |

### Phase 3: the pipeline's own steps

| | |
|---|---|
| Scope | A `Host::step` hook that does nothing by default, called by the pipeline for auth, rate limits, validation, cache lookups and response validation. Span events and attributes first (`unsworn.cache`, `unsworn.rejected_by`), child spans where the timing turns out to matter |
| Files | `unsworn-core`: `pipeline/host.rs`, `pipeline/flow.rs`; `server/host.rs` |
| Done when | A rejected request's span says which rule rejected it, and the WebAssembly size check passes |

### Phase 4: a handler's span as the parent of `ctx.call`

| | |
|---|---|
| Scope | An optional `traceparent` on the protocol's `call` message, used instead of the request's own, and a way to pass it to `ctx.call` in each SDK |
| Files | `unsworn-proto`, `workers.rs`, `sdk/js`, `sdk/python`, `sdk/go`, `sdk/rust`; [`protocol.md`](protocol.md), [`tracing.md`](tracing.md) |
| Done when | In each SDK, a function called from a handler's span is its child |

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
- Benchmark rows in [`bench/suite.sh`](../bench/suite.sh) with tracing
  off, sampling 1% and sampling everything, for `respond`, `proxy` and a
  handler that answers "hello". The goal: off costs nothing measurable,
  and sampling everything stays within a few µs of CPU per request.
- `cargo fmt`, `cargo clippy`, the core's `wasm32` build, `cargo deny` and
  the `dist` binary's size.

## Open questions

- **Unsworn's other requests.** JWKS URLs, API-key verifiers and `proxy`
  upstreams are written in their own sections and aren't checked against
  `[outbound]`. A rule that Unsworn sends nothing to an undeclared host
  would have to cover them too.
