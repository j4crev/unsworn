# Tracing: one trace across handlers, functions and services

A request that arrives with a W3C
[`traceparent`](https://www.w3.org/TR/trace-context/) header belongs to a
trace. Unsworn carries that header, and the `tracestate` that came with
it, to everything done for the request, so the spans your handlers and
services record end up in one trace:

| Where the request goes | What it gets |
|---|---|
| A handler, a `before` handler, a `shadow` handler | The request's headers, as they came (`req.headers.traceparent`) |
| A `proxy` route's upstream | The same two headers, forwarded as they came |
| A [function](functions.md) called with `ctx.call` | `traceparent` and `tracestate` as its only headers, and so on down through any function it calls |
| An [outbound service](outbound.md) reached with `ctx.fetch` | The two headers, added to the request |

There's nothing to configure.

```ts
// src/search.ts: the request came with a traceparent
export async function search(req, ctx) {
  const vector = await ctx.call("embed", { text: req.query.q });      // embed sees it in req.headers
  const hits = await ctx.fetch("index", "/query", { json: { vector } }); // and so does the index service
  return { json: hits.json() };
}
```

## What Unsworn doesn't do

- **It records no spans.** Unsworn's own time (auth, validation, rate
  limits) doesn't show up in a trace, and nothing is exported. A handler
  that wants spans uses its language's OpenTelemetry SDK, reading the
  context from `req.headers`. Spans and an exporter in Unsworn are on the
  [roadmap](roadmap.md#not-scheduled-yet).
- **It starts no traces.** A request that arrives without a `traceparent`
  gets none. A made-up one would carry a sampling decision Unsworn has no
  grounds for: marked sampled, every service behind it would record every
  request, and marked unsampled, none would.
- **It passes on only what a receiver would accept.** A malformed
  `traceparent` still reaches the handler and a proxied upstream, as any
  header does, but isn't given to functions or outbound services. A
  `tracestate` is only sent with the `traceparent` it came with.

## A handler's own span

Without help, a function or service called for a request becomes a child
of whatever sent the request, next to the handler's own span, not below
it. The trace is still one trace.

For an outbound request, a handler that has a span of its own sends that
instead, and Unsworn leaves it alone:

```ts
await ctx.fetch("index", "/query", { json: { vector }, headers: { traceparent: mySpan } });
```

A service whose `headers` in the config set `traceparent` keeps that one.

`ctx.call` takes no headers yet, so a function is always a child of the
incoming request. It also means that when the request had no
`traceparent` and the handler started the trace itself, the function
doesn't join it.

## Where it works

On servers and containers. Functions and outbound services don't run on
edge and serverless targets yet (see the [roadmap](roadmap.md)); a
request's own headers reach handlers there as everywhere.
