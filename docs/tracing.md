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
| A [function](functions.md) called with `ctx.call` | `traceparent` and `tracestate` as its only headers (the request's, or those the call [names](#a-handlers-own-span)), and so on down through any function it calls |
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

With [`[telemetry]`](config.md#telemetry), the server records spans for
each request it samples and sends them to your collector. Its time then
shows in the trace, rejected requests included. For a sampled request:

- The request's span is a child of the incoming `traceparent`, or starts the
  trace when there wasn't one.
- Below it is a span for each thing Unsworn asks of something else: a
  handler or `before` handler, a `proxy` upstream, a key verifier, a
  function, an outbound service.
- Each gets a `traceparent` naming the span for asking it, so the spans it
  records nest under that one, and what a handler calls or fetches nests
  under the handler's, or under a [span of its own](#a-handlers-own-span)
  that it names. A `tracestate` goes along with the trace it came with, and
  is removed when Unsworn starts a new one.

For a handler that calls a function and a service:

```
GET /search                        the request
├─ before src/auth.ts#tenant
└─ handler src/search.ts#search
   ├─ call embed                   embed's own spans go below this
   └─ POST                         the index service's too
```

A request that a proxy in `[server].trusted_proxies` sends with a
`traceparent` follows its sampled flag. Anyone else's flag is ignored, so a
client can't choose to be recorded: `sample` says what share of those
requests is, and of the ones without a `traceparent`. A request that isn't
sampled is treated as it is without `[telemetry]`, except that a flag that
was ignored is cleared before it goes further. See
[which requests](config.md#which-requests).

The request's span also says what Unsworn decided, and when: the scheme
that let the caller in, whether the cache answered, and for a request it
turned away, the rule that did (`unsworn.rejected_by`, such as
`rate_limit` or `auth`). The spans and what they say are listed in
[`config.md`](config.md#the-spans).

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
it. For a request Unsworn samples, they're below Unsworn's span for the
handler call, next to the handler's own span. The trace is still one trace.

To put them below the handler's span, give the call or the fetch that
span's `traceparent`, and its `tracestate` if it has one. An OpenTelemetry
SDK writes both for the span that's active:

```ts
import { context, propagation } from "@opentelemetry/api";
import type { CallOptions } from "@unswornjs/sdk";

const trace: CallOptions = {};
propagation.inject(context.active(), trace); // sets traceparent and tracestate

const vector = await ctx.call("embed", { text: req.query.q }, trace);
const hits = await ctx.fetch("index", "/query", { json: { vector }, headers: { ...trace } });
```

| | `ctx.call` | `ctx.fetch` |
|---|---|---|
| TypeScript | `ctx.call("embed", input, { traceparent, tracestate })` | `headers: { traceparent, tracestate }` |
| Python | `ctx.call("embed", input, traceparent=..., tracestate=...)` | `headers={"traceparent": ...}` |
| Go | `req.CallWith("embed", input, &out, &unsworn.CallOptions{Traceparent: ...})` | `Headers` in `unsworn.Outbound` |
| Rust | `req.call_with("embed", input, CallOptions::trace(...))` | `Outbound::post(path).header("traceparent", ...)` |
| An [HTTP runtime](http-runtime.md#calling-functions-and-outbound-services) | the headers of `POST /call/embed` | the headers of the fetch |

An HTTP runtime whose app is instrumented with OpenTelemetry sends these
headers on its calls and fetches without any code for it.

- **The function or service is below that span.** With
  [`[telemetry]`](config.md#telemetry), when the `traceparent` is marked
  sampled, Unsworn records its span for the call or fetch below it, and the
  function or service is below Unsworn's.
- **One that isn't marked sampled is passed on as it is,** and Unsworn
  records no span for that call or fetch: the parent decides.
- **A call ignores a `traceparent` a receiver wouldn't accept,** and uses
  the request's. Among a fetch's headers, it goes to the service as any
  header does.
- **A service whose `headers` in the config set `traceparent` keeps that
  one.**

A handler that got no `traceparent`, and started the trace itself, has its
functions and services join it the same way.

## Where it works

Passing the headers on and recording Unsworn's own spans both work on every
target: servers and containers, AWS Lambda, Cloudflare Workers, Vercel and
Netlify. On Cloudflare, Vercel and Netlify, each sampled request sends its
spans in one `POST /v1/traces` without making the response wait, and no
peer is trusted to decide sampling (`trusted_proxies` is ignored there).
OTLP `metrics` and `logs` come from the server only.
