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

There's nothing to configure for that. To also have Unsworn record spans
of its own, see [below](#unsworns-own-spans).

```ts
// src/search.ts: the request came with a traceparent
export async function search(req, ctx) {
  const vector = await ctx.call("embed", { text: req.query.q });      // embed sees it in req.headers
  const hits = await ctx.fetch("index", "/query", { json: { vector } }); // and so does the index service
  return { json: hits.json() };
}
```

## Unsworn's own spans

With [`[telemetry]`](config.md#telemetry), the server records a span for
each request it samples and sends it to your collector. Its time then shows
in the trace, rejected requests included. For a sampled request:

- The span is a child of the incoming `traceparent`, or starts the trace
  when there wasn't one.
- Everything in the table above gets a `traceparent` naming that span, so
  the spans behind Unsworn nest under it. A `tracestate` goes along with
  the trace it came with, and is removed when Unsworn starts a new one.

A request that arrives with a `traceparent` follows its sampled flag.
`sample` says what share of the others start a trace. A request that isn't
sampled is treated as it is without `[telemetry]`.

Unsworn's span covers the whole request. It doesn't yet record spans for
the handler call, the upstream or its own steps, such as auth and rate
limits: those are [planned](telemetry-design.md).

## Without `[telemetry]`

- **Unsworn records no spans** and exports nothing. A handler that wants
  spans uses its language's OpenTelemetry SDK, reading the context from
  `req.headers`.
- **It starts no traces.** A request that arrives without a `traceparent`
  gets none. A made-up one would carry a sampling decision Unsworn has no
  grounds for: marked sampled, every service behind it would record every
  request, and marked unsampled, none would. A `sample` in `[telemetry]`
  is such grounds.

## What's passed on

**Only what a receiver would accept.** A malformed `traceparent` still
reaches the handler and a proxied upstream, as any header does, but isn't
given to functions or outbound services. A `tracestate` is only sent with
the `traceparent` it came with. A sampled request's malformed one is
replaced, as a missing one is.

## A handler's own span

Without help, a function or service called for a request becomes a child
of whatever sent the request, next to the handler's own span, not below
it. For a request Unsworn samples, they're children of Unsworn's span, as
the handler's own span is. The trace is still one trace.

For an outbound request, a handler that has a span of its own sends that
instead, and Unsworn leaves it alone:

```ts
await ctx.fetch("index", "/query", { json: { vector }, headers: { traceparent: mySpan } });
```

A service whose `headers` in the config set `traceparent` keeps that one.

`ctx.call` takes no headers yet, so a function is always a child of the
`traceparent` the handler got: the sender's, or Unsworn's for a request it
sampled. It also means that when the handler got none and started the
trace itself, the function doesn't join it.

## Where it works

Passing the headers on works on every target: servers and containers,
Cloudflare Workers and serverless functions. Unsworn's own spans come from
the server only, in a container and on AWS Lambda included.
