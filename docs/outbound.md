# Outbound services: credentials a handler never holds

Unsworn's rules decide who may call a handler. An *outbound service* is a
rule for the other direction: what a handler may reach, and with whose
credential.

```toml
[outbound.stripe]
url     = "https://api.stripe.com"
headers = { Authorization = "Bearer {env.STRIPE_KEY}" }
methods = ["GET", "POST"]
paths   = ["/v1/charges", "/v1/charges/{id}", "/v1/customers/{*rest}"]
```

```ts
export const charge: Charge = async (req, ctx) => {
  const answer = await ctx.fetch("stripe", "/v1/charges", { json: { amount: req.json.amount } });
  return { status: answer.status, json: answer.json() };
};
```

The handler names the service and a path. Unsworn makes the request and
adds the `Authorization` header. The key is never in the handler's
process: not in its environment, not in its memory, not in anything it
can log or send somewhere else.

## Why

A handler that calls Stripe usually reads `STRIPE_KEY` from its
environment. Then everything in that process can read it too: every
dependency, and anything that gets the handler to run code or print its
environment. For an MCP tool that's a short path, since the tool's input
is written by a model that may be reading text an attacker wrote.

With an outbound service:

- **A handler that's tricked can't leak the key,** because it doesn't
  have it. The worst it can do is make the calls `methods` and `paths`
  allow.
- **What a handler can reach is in the config,** next to who can call it,
  where a reviewer sees both. `unsworn plan` lists each service, and
  `unsworn diff` marks a method or path list that got wider.
- **Every call is logged** with the request it was made for.

## What Unsworn does with a fetch

1. **Checks it against the service.** The method has to be in `methods`
   and the path has to match one of `paths` (both default to anything). A
   path with a `.` or `..` segment is refused however it's written
   (`%2e%2e` too), so a handler can't step out of an allowed path.
2. **Builds the request.** The URL is the service's `url` followed by the
   handler's path and query. The handler's headers are sent, except the
   ones that belong to a connection (`Host`, `Content-Length`,
   `Connection` and the like), and any the service sets itself.
3. **Adds the service's `headers`,** over anything the handler sent by
   the same name.
4. **Adds the request's trace.** When the request the handler is serving
   came with a `traceparent` header, the service gets it, and its
   `tracestate`, unless the handler or the service's `headers` set one.
   See [`tracing.md`](tracing.md).
5. **Sends it,** within the service's `timeout` and what is left of the
   handler's own request. Redirects aren't followed: the answer goes back
   to the handler as it is.
6. **Gives the handler the answer:** status, headers and body, up to
   `max_response`.
7. **Logs it:** `outbound service=stripe method=POST path=/v1/charges
   status=200 ms=212.4 id=<request id>`. The query string is left out,
   since that's where tokens end up.

The variables a service's `headers` read, such as `STRIPE_KEY`, aren't
passed to workers, like every other credential the config reads (see
[`{env.NAME}`](config.md#envname)).

## Fetching

| | Fetch | The answer | A failed fetch |
|---|---|---|---|
| TypeScript | `await ctx.fetch("stripe", path, { method, headers, json, body })` | `status`, `ok`, `headers`, `body`, `text()`, `json()` | throws `FetchError` (`status`) |
| Python | `ctx.fetch("stripe", path, method=..., headers=..., json=..., body=...)`, with `await` in an `async def` handler | `status`, `ok`, `headers`, `body`, `text`, `json()` | raises `FetchError` (`status`) |
| Go | `req.Fetch("stripe", path, &unsworn.Outbound{Method, Headers, JSON, Body})` | `Status`, `Headers`, `Body`, `OK()`, `Bind(&v)` | a `*unsworn.FetchError` |
| Rust | `req.fetch("stripe", Outbound::post(path).json(&v)?).await?` | `status`, `headers`, `body`, `ok()`, `text()`, `json()` | `Err(FetchError { status, message })` |

The method defaults to `GET`, or `POST` when there's a body. A `json`
body is sent with `content-type: application/json`.

A service's own error status is an answer, not a failure: a `404` from
Stripe comes back with `status` 404. A fetch fails when the request
wasn't made or got no answer:

| Status | Why |
|---|---|
| `403` | the method or path isn't allowed for the service |
| `404` | the config has no `[outbound.<name>]` with that name |
| `502` | the service couldn't be reached, or its answer is larger than `max_response` |
| `504` | it took longer than `timeout`, or than the handler's request had left |

## Config

```toml
[outbound.stripe]
url          = "https://api.stripe.com"   # a handler's path goes after it; may use {env.NAME}
headers      = { Authorization = "Bearer {env.STRIPE_KEY}" }   # added to every request
methods      = ["GET", "POST"]            # default: any
paths        = ["/v1/charges/{id}", "/v1/customers/{*rest}"]   # route patterns; default: any
timeout      = "10s"                      # default: [server].timeout
max_response = "10MB"                     # the default
```

`url` may have a path of its own (`https://internal.example/billing/v2`),
which every request then starts with.

With `unsworn gen ts`, a service name that isn't in the config is a
compile error.

## What this doesn't do

- **It doesn't stop a handler from opening its own connections.** A
  handler is a process and can still use its language's HTTP client. What
  it can't do is use the credential, which it doesn't have. To also keep
  a handler from reaching anything else, run Unsworn where the platform
  limits outbound traffic (a network policy, a firewall rule, a container
  with no route out), and give handlers `ctx.fetch` as their way out.
- **It doesn't hold when Unsworn runs as root.** A handler can't read the
  key from Unsworn's own environment, because Unsworn closes its process
  to the others its user runs. Root can usually read any process, though,
  and workers run as whoever Unsworn does. See
  [`{env.NAME}`](config.md#envname).
- **It doesn't hide what the service answers.** If the service echoes the
  key back, the handler sees that.
- **It's for request and response, not streams.** The whole body is sent
  and the whole answer read, up to `max_response`. A handler that streams
  from a model's API needs to hold that key itself for now.
- **On Cloudflare and serverless targets, the key isn't kept from the
  handler.** `ctx.fetch` works there as it does on a server: Unsworn adds
  the service's headers, checks the method and path, and doesn't follow
  redirects. But handlers share a process with the core there, so a
  handler can read the variable the key comes from. `unsworn check
  --target` says so for a project with such a service. To keep a key out
  of handler code altogether, run the server.
