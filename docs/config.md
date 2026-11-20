# Unsworn configuration

An Unsworn project is a config file plus handlers. Routes can come from any
mix of these [route sources](#route-sources):

| Source | Best for |
|---|---|
| `[[route]]` entries in `unsworn.toml` | Projects that want no route folders at all |
| A routes directory, nested or flat | File-based routing, as in SvelteKit or xink |
| An OpenAPI spec | Teams that write the API contract first |
| A catch-all route to one handler | Existing apps that already have their own router |

A config-only project can be as small as this:

```
unsworn.toml
src/
  users.ts        # getUser, createUser, ...
  billing.go
```

## Format

- **TOML is the primary format** and the one used in all documentation.
- **YAML is also accepted** (`unsworn.yaml`). Both parse into the same
  structures, so features never differ between them.
- A **JSON Schema** for the config is published with each release, so editors
  can autocomplete and flag errors.
- `unsworn plan` writes what all of it adds up to, for every route, to a
  file to commit, and `unsworn diff` shows what a change does to the rules.
  See [`plan.md`](plan.md).
- `unsworn check` validates the config and the routes directory and prints
  errors with a path and a fix, for example:

  ```
  ✗ unsworn.toml:12 [[route]].handler: no runtime handles ".rb"
    → add [runtimes.<name>] with extensions = [".rb"], or change the handler
  ```

## The rule: config never computes

> **Config may match, validate, transform and reject. It may not compute.**

There are no conditionals, loops, variables or expression languages in
config. The only templating is substituting values Unsworn already has, such
as `{id}` or `{env.API_URL}`. If an endpoint needs an `if`, it belongs in a
handler. This keeps configs readable and checkable, and stops them turning
into a second programming language.

## Top-level sections

```toml
[server]       # listen address, TLS, limits
[routes]       # optional: route directory
[openapi]      # optional: OpenAPI spec as a route source
[mcp]          # optional: an MCP endpoint and its tools (see mcp.md)
[runtimes.*]   # how to run handlers for each language
[auth.*]       # named authentication schemes
[store]        # shared state for rate limits and caches
[gen]          # optional: files of generated types for handlers
[defaults]     # policies applied to every route unless overridden
[policies.*]   # named policy sets that routes can `use`
[[route]]      # explicit routes, and policies for routes from other sources
[[function]]   # optional: handlers that other handlers call by name
[outbound.*]   # optional: services Unsworn calls for handlers, adding their credentials
```

### `[server]`

```toml
[server]
listen              = "0.0.0.0:8080"   # the default; `unsworn dev` uses 127.0.0.1:8080
max_body            = "1MB"          # default for every route (1024-based units)
timeout             = "30s"          # default for every route
trailing_slash      = "redirect"     # "redirect" | "strict" | "ignore"
compression         = true           # br, zstd or gzip, from Accept-Encoding
trusted_proxies     = ["10.0.0.0/8"] # trust X-Forwarded-For from these ("all" trusts any)
admin_listen        = "127.0.0.1:9090"  # optional: metrics (Prometheus), health details, and what `unsworn top` reads
response_validation = "log"          # "off" | "log" | "enforce", for `response` schemas
shutdown_timeout    = "10s"          # how long to wait for open requests on shutdown
max_connections     = 10000          # the most connections open at once (the default)
idle_timeout        = "5m"           # how long a connection may wait for its next request (the default)

[server.tls]
cert = "certs/fullchain.pem"
key  = "certs/privkey.pem"
```

`unsworn serve --listen <addr>` overrides `listen`. Without `listen` in the
config, `unsworn dev` serves on `127.0.0.1:8080` only, since its responses
carry error details meant for you; give it `--listen 0.0.0.0:8080` to reach
it from another machine or from outside a container. Every server also answers
`/_unsworn/health`: `200` and `{"status": "ok"}` when it's healthy, `503`
and `"degraded"` when a runtime has no workers or keeps crashing. That's
all it says, since anyone can ask. The same path on `admin_listen` adds the
version, the store and the state of each runtime. Keep `admin_listen` on an
address only you can reach: nothing on it asks for credentials.

Connections are limited, so clients that open them and then stall can't
tie the server up:

- At `max_connections`, new connections wait to be accepted until one
  closes. A proxied WebSocket counts until it closes, and it's closed once
  neither side has sent anything for `idle_timeout`, so a WebSocket app
  should ping more often than that (most libraries do, every 30 seconds or
  so).
- A new connection has 10 seconds to finish its TLS handshake, and 30
  seconds (or `idle_timeout`, if that's shorter) to send its first request.
- `idle_timeout` is how long an HTTP/1.1 connection may then wait for its
  next request, which includes the time that request's headers take to
  arrive. Behind a load balancer that reuses connections, keep it longer
  than the balancer's own idle timeout, or the balancer will now and then
  send a request down a connection Unsworn has just closed.
- An HTTP/2 connection is pinged every 30 seconds and closed when it
  doesn't answer.

### `[runtimes.<name>]`

```toml
[runtimes.ts]
extensions      = [".ts", ".js"]
command         = "bun run node_modules/@unswornjs/sdk/src/worker.ts"
protocol        = "framed"        # "jsonl" (Tier 1, the default) | "framed" (Tier 2)
workers         = { min = 1 }     # the default: one, and more under load (see below)
eager           = false           # start workers at boot instead of on first request
startup_timeout = "30s"           # time allowed for `build` and `ready`
env             = { NODE_ENV = "production" }

[runtimes.go]
extensions = [".go"]
build      = "go build -o .unsworn/go-worker ./routes"
command    = ".unsworn/go-worker"
protocol   = "framed"
workers    = 2

[runtimes.newlang]
extensions = [".nl"]
command    = "newlang run worker.nl"
protocol   = "jsonl"              # no SDK needed
```

Runtime names are arbitrary. Unsworn knows nothing about any specific language.
With an SDK either `protocol` works, and the handler code is the same. `"jsonl"`
is slightly faster for small requests. `"framed"` is faster once bodies reach
about 64 KB, and leaves the worker's stdout to the handler.
`protocol = "binary"`, the name used before 0.2, still works as an alias for
`"framed"`, with a warning. Commands run through `sh -c` in the project root. `build` runs once, before
the runtime's first workers start, and again on each reload.

#### How many workers

A runtime starts with one worker process and adds more when its workers
can't keep up, up to one for each CPU Unsworn may use (hyperthreads count,
and so does a container's CPU limit). A handler that does little needs one
or two workers, and one that computes needs a process per CPU, so the
number follows the work:

```toml
workers = { min = 1, max = 8 }                 # the default on a machine with 8 CPUs, written out
workers = { min = 2, max = 4, idle = "5m" }    # keep two, never more than four; let a spare one go after 5 minutes
workers = 4                                    # exactly four, always
```

A worker is added when:

- **a call has to wait** because every worker is at the `max_in_flight`
  it declared. That's how a worker that takes one request at a time shows
  it's busy.
- **every worker has used 90% of a CPU core for half a second, and there
  is idle CPU** to run another one. That's for workers that take many
  requests at once, such as the SDKs': they never make a call wait, so
  Unsworn reads their CPU time instead.

The pool keeps adding workers for as long as either is true, so work
that fills a core for every worker it's given ends with one worker per
CPU. Work that doesn't stops sooner: once two workers share what kept one
busy, neither is busy any more. Each worker holds memory (from about
10 MB for Go and Rust to more than 100 MB for Node), so on a machine with
many CPUs, set `max` if a full pool would be more than you want to give
one runtime.

A worker is retired when the others could do its work at half their
capacity, and that has been so for `idle` (30 seconds by default). It
finishes the requests it has first.

Starting a worker takes from about 10 ms (Go, Rust) to about 300 ms
(Node), and the pool adds one at a time, about one a second. So a burst
of heavy requests on a quiet server is served by fewer workers for its
first few seconds. If that matters more than the memory, set `min` to
what the burst needs, or give `workers` a number.

Unsworn logs each worker it adds or retires, and why.

For production, build compiled workers ahead of time (in CI or a Docker build
stage) and run `unsworn serve --no-build` (or set `UNSWORN_NO_BUILD=1`), so
the server needs no compiler or source code. The official image sets
`UNSWORN_NO_BUILD=1`, since its project directory belongs to root and a
build at startup couldn't write there; a worker that won't start says when
its runtime's `build` was skipped. Mark those runtimes
`eager = true`, so a missing worker or handler stops startup instead of
failing the first request.

### `[auth.<name>]`

Each scheme has a name you choose (`users`, `customers` and so on) and a
`type`. A scheme checks credentials against exactly one source, and `url`
means something different for each type.

#### `type = "jwt"`

```toml
[auth.users]
type       = "jwt"
url        = "https://auth.example.com/.well-known/jwks.json"   # or `secret`, or `public_key` (a PEM file)
audience   = "api"                # a string or a list; checked when set
issuer     = "https://auth.example.com/"
algorithms = ["RS256"]            # default: HS256 for `secret`, else RS256/PS256/ES256/EdDSA
header     = "Authorization"      # default; other headers carry the bare token
leeway     = "30s"                # clock skew allowed for exp/nbf
```

Set exactly one of `url`, `secret` or `public_key`. `url` is a JWKS
document holding the public keys. Unsworn verifies tokens itself, so there's
no network call per request: JWKS documents are fetched on first use,
refreshed every 10 minutes, and refetched (at most once a minute) when a
token names an unknown key id. `jwks_url`, the old name for `url`, still
works, with a warning.

A reload that changes `secret`, the `public_key` file or `url` switches to
the new keys at once: tokens signed with the old key stop working. To keep
both working while clients move over, serve both keys from a JWKS `url`.

`unsworn check` looks for the ways a scheme ends up accepting tokens it
shouldn't:

- **A `url` without `issuer` or `audience`** (a warning). Every token the
  key set signs passes, and some key sets, such as Firebase's, sign tokens
  for every customer's project. Set `audience`, and `issuer` when the
  provider has one per tenant.
- **An `http://` `url`** that isn't on this machine (a warning). Keys
  fetched in the clear can be replaced on the way, and whoever replaces
  them can sign tokens. The same goes for an `api_key` verifier's `url`,
  which every key is sent to.
- **An empty `secret`** (an error), which is what `{env.JWT_SECRET}` gives
  when the variable is set to nothing. A `secret` under 32 bytes is a
  warning: one token is enough to guess a short one offline.

#### `type = "api_key"`

For a handful of keys, list them:

```toml
[auth.service]
type   = "api_key"
header = "X-API-Key"              # default when neither header nor query is set
query  = "api_key"                # optional: also accept ?api_key=
keys   = "{env.SERVICE_KEYS}"     # a list, or a comma-separated string
```

Matching keys get the claims `{ "type": "api_key", "key": <index> }`.

When keys live somewhere else, such as a database, a key-management service
or an OAuth authorization server, point `url` at an endpoint that verifies
them instead. Unsworn asks it with OAuth 2.0 token introspection
([RFC 7662](https://www.rfc-editor.org/rfc/rfc7662)), so an authorization
server's introspection endpoint works as it is:

```toml
[auth.customers]
type    = "api_key"
header  = "X-API-Key"
url     = "https://auth.example.com/oauth/introspect"
timeout = "500ms"                 # default: 2s
cache   = "30s"                   # default: ask on every request
headers = { Authorization = "Bearer {env.AUTH_SERVICE_TOKEN}", "X-Service" = "unsworn" }
```

Set exactly one of `keys` or `url`. `header` and `query` say where the
**client's** key is; `headers` are sent **to the verifier**, typically
Unsworn's own credentials for it (a bearer token, or `Basic` client
credentials), and only apply with `url`.

For each request that carries a key, Unsworn calls the verifier with the
key as the form field `token`:

```http
POST /oauth/introspect
Content-Type: application/x-www-form-urlencoded
Accept: application/json
Authorization: Bearer <AUTH_SERVICE_TOKEN>

token=the-client-supplied-key
```

It answers `200` with a JSON object. A valid key:

```json
{"active": true, "sub": "customer_123", "scope": "reports:read", "exp": 1767225600}
```

An invalid one:

```json
{"active": false}
```

Everything beside `active` is optional, and becomes `ctx.auth` as is, so it
works with `scopes`, `rate_limit = { key = "auth.sub" }` and handlers like
JWT claims do. An invalid key gets `401`. If the verifier can't be reached,
takes longer than `timeout`, or answers anything else (another status,
including a `401` for Unsworn's own credentials, or JSON without a boolean
`active`), the request gets `503` and the error is logged, so an outage or
a wrong URL doesn't look like a bad key. A request with no key never
reaches the verifier.

Without `cache`, every request with a key waits for the verifier. With it,
Unsworn remembers each answer, valid or invalid, in [`[store]`](#store), so
instances sharing a Redis share answers too. The trade-off is revocation:
a key the verifier stops accepting keeps working until its cached answer
expires, so pick the longest delay you can accept. Answers that include
`exp` are cached no longer than that. Outages aren't cached. The
store holds SHA-256 hashes of keys, never the keys themselves. With the
`memory` store, each instance remembers up to 10,000 verdicts (8 MB), apart
from cached responses, so made-up keys can't push responses out; with many
active keys, use `redis`.

Requests to the verifier carry the key, so use `https://` unless the
verifier runs on the same host or private network. A verifier with a
different API needs a small adapter that speaks introspection.

In YAML, write `headers` as an indented mapping:

```yaml
auth:
  customers:
    type: api_key
    header: X-API-Key
    url: https://auth.example.com/oauth/introspect
    headers:
      Authorization: "Bearer {env.AUTH_SERVICE_TOKEN}"
      X-Service: unsworn
```

#### Using schemes

Verified claims are available to handlers as `ctx.auth`, plus `scheme`, the
name of the scheme that matched. Routes refer to schemes by name:

```toml
auth = "jwt"                                      # one scheme
auth = ["jwt", "service"]                         # either one
auth = { scheme = "jwt", scopes = ["users:read"] }  # with required scopes
auth = "none"                                     # public
```

**Once a project has an auth scheme, every route says who may call it.**
A route with no `auth` from anywhere is an error from `unsworn check`:

```
✗ unsworn.toml:12 [[route]]: `auth` isn't set for * /admin/export
  → say who may call it: `auth = "<scheme>"`, or `auth = "none"` if anyone may. ...
```

A route left without `auth` would be public, and nothing would tell one
left that way on purpose from one whose `auth` line was forgotten. So a
public route has `auth = "none"` written for it: on the route, on an entry
for its path or a prefix, or once for the whole project:

```toml
[defaults]
auth = "none"        # public unless a route names a scheme
```

or the other way round, which is the safer default for an API that's
mostly private:

```toml
[defaults]
auth = "users"       # a token unless a route says `auth = "none"`
```

Either way the choice is a line in the config, and it shows in the
[plan](plan.md). A project with no `[auth.*]` at all has nothing to forget,
and isn't asked.

Scopes are read from the token's `scope` (space-separated) or `scp` claim.
A missing or invalid credential gets `401`; a valid one without the scopes
gets `403`; one whose verifier is unavailable gets `503`. A `401` names the
scheme to use in `WWW-Authenticate`: `Bearer` for a `jwt` scheme, and
`ApiKey` for an `api_key` scheme (HTTP has no registered scheme for API
keys, and clients ignore challenges they don't know).

A token must have an `exp` claim: one without it, or with an `exp` (or
`nbf`) that isn't a number, is rejected.

A `jwt` scheme can also describe the OAuth resource it protects, with
`[auth.<name>].protected_resource`. Unsworn then serves the OAuth
protected-resource metadata and answers missing tokens with the
`WWW-Authenticate` header that OAuth and MCP clients expect. See
[`mcp.md`](mcp.md#oauth-protected-resource-support).

### `[store]`

Where `rate_limit` and `cache` keep their state.

```toml
[store]
backend  = "redis"              # "memory" (the default) | "redis"
url      = "{env.REDIS_URL}"    # redis://, rediss:// (TLS) or redis+unix://
token    = "{env.REDIS_TOKEN}"  # only with an https:// url (see below)
prefix   = "billing:"           # optional, prepended to every key
on_error = "open"               # "open" (the default) | "closed"
```

`memory` keeps state in the process, which is fine for a single instance.
With several instances, each would count its own rate limits and fill its
own cache, so use `redis` (or anything that speaks its protocol, such as
Valkey, Dragonfly or a managed Redis) to share them.

Clients choose the keys (each query string is a new cache entry, each
address a new bucket), so the `memory` store is bounded: up to 10,000
cached responses and 64 MB of them, 10,000 remembered API-key verdicts, and
200,000 rate-limit buckets. When one fills up, the cache entry that expires
soonest, or the bucket that refills soonest, makes way. To cache more, use
`redis`, and set a `maxmemory` limit and eviction policy on it.

- **`url` and `token`:** the server speaks the Redis protocol, so it takes a
  `redis://` or `rediss://` URL. [Serverless functions](serverless.md#the-store)
  can't keep connections open, so they reach Redis through a REST API
  instead: give the API's `https://` URL and its `token`. Any service that speaks
  Upstash's REST protocol works. Rate limits use the same keys either way,
  so a server and a function on the same Redis share them; cached responses
  aren't shared between the two.
- **`prefix`:** set it when separate projects or environments share one
  Redis. Without it, two projects with the same routes share rate limits and
  can serve each other's cached responses.
- **`on_error`:** if Redis can't be reached, rate-limited routes let requests
  through uncounted (`"open"`), or answer `503` (`"closed"`). Caching is
  skipped either way. Unsworn reconnects on its own, logs the outage, and
  reports it in the admin listener's `/_unsworn/health` and the
  `unsworn_store_up` metric without marking the instance unhealthy, since
  every instance shares the store.
- **Treat the store as part of your auth.** It holds remembered API-key
  verdicts (`cache` on an `api_key` scheme) and cached responses, so anyone
  who can write to it can make a key valid or change what a route answers.
  Its key names don't give credentials away: cache keys, verdict keys and
  the values of `header.<name>` rate-limit keys are SHA-256 hashes.
  Give Redis a password, reach it over `rediss://` unless it's on a network
  you control, and don't share it with software you trust less.

### `[gen]`

```toml
[gen]
ts     = "src/unsworn.gen.ts"
python = "unsworn_gen.py"
go     = { file = "routes/unsworn_gen.go", package = "main" }   # or just the path
rust   = "src/unsworn_gen.rs"
```

Where to write each language's types for handlers: one file per language,
inside the project. `unsworn dev` keeps the files current and `unsworn gen`
writes them. What's in them is described in [`types.md`](types.md).

### `[[function]]`

```toml
[[function]]
name    = "embed"
handler = "src/embed.py#embed"
input   = "schemas/embed-in.json"    # optional; checked before the function runs
output  = "schemas/embed-out.json"   # optional
timeout = "10s"                      # optional; default: [server].timeout
```

A handler that other handlers call with `ctx.call("embed", input)`, in
any language. It has no path, so nothing outside reaches it. See
[`functions.md`](functions.md).

### `[outbound.<name>]`

```toml
[outbound.stripe]
url     = "https://api.stripe.com"
headers = { Authorization = "Bearer {env.STRIPE_KEY}" }
methods = ["GET", "POST"]                        # optional
paths   = ["/v1/charges", "/v1/charges/{id}"]    # optional
```

A service handlers reach with `ctx.fetch("stripe", path)`. Unsworn makes
the request and adds `headers`, so no handler holds the credential. See
[`outbound.md`](outbound.md).

### `{env.NAME}`

Values in these places can include `{env.NAME}`, replaced with the
environment variable `NAME` when the config loads: auth scheme secrets, JWKS
and verifier URLs, verifier `headers`, issuers, audiences and API keys, proxy upstreams, `[store].url` and `token`,
an outbound service's `url` and `headers`, and runtime `env` values. A missing variable is an error.

Workers inherit Unsworn's environment, except for the variables it reads
its own credentials from: a JWT `secret`, API `keys`, verifier `headers`,
`[store].url` and `token`, and an outbound service's `headers`. A handler that could read the signing secret
could mint its own tokens, so those stay with Unsworn. If a handler needs
one of them (the same Redis, say), pass it on by name:

```toml
[runtimes.ts]
env = { REDIS_URL = "{env.REDIS_URL}" }
```

Unsworn logs the variables it holds back when a runtime starts.

Workers run as the same user as Unsworn, and Linux lets a process read
the environment and memory of another one with the same user
(`/proc/<pid>/environ`). So Unsworn closes its own process to that when it
starts: a handler can't read a held-back variable from Unsworn's
environment either, and Unsworn writes no core dump. What that leaves:

- **Files that user can read,** such as `[server.tls].key` or a
  `public_key` file. A handler can read them too. Keeping them from
  workers needs the platform for now (see the
  [roadmap](roadmap.md#beyond-the-front-door)).
- **A variable passed on to one runtime** can be read by the workers of
  the others, since workers aren't closed to each other.
- **A server running as root.** Its workers are root as well, and root
  can usually read any process. The official image runs as the user
  `unsworn` for this reason.

## Routes

Each `[[route]]` has a **matcher**, optional **policies** and exactly one
**action**.

### Matcher

```toml
path   = "/users/{id}"            # {name} = one segment, {*name} = the rest of the path
method = "GET"                    # or ["GET", "HEAD"]; omitted = all methods
host   = "api.example.com"        # optional
```

Unsworn normalizes a request's path before matching it, as RFC 3986
describes: `.` and `..` segments are resolved and percent-encoded letters,
digits, `-`, `.`, `_` and `~` are decoded. `/public/../admin` and `/%61dmin`
are both `/admin`, for routes, policies, handlers and upstreams alike, so a
request can't reach one route's upstream under another route's policies. An
encoded slash (`%2F`) stays encoded and never separates segments.

### Actions

| Action | Example | Runs a worker? |
|---|---|---|
| `respond` | `respond = { status = 200, json = { ok = true } }` | No |
| `redirect` | `redirect = { to = "/v2/users/{id}", status = 308 }` | No |
| `static` | `static = { dir = "public", spa_fallback = "index.html" }` | No |
| `proxy` | `proxy = "http://old-api:8080"` or `{ upstreams = [...], strip_prefix = "/legacy", sticky = "header:x-tenant-id" }` | No |
| `handler` | `handler = "src/users.ts#getUser"` | Yes |

An upstream on the same machine can listen on a Unix socket instead of a
port: `proxy = "unix:/run/app.sock"`, with the socket's full path. Requests
skip the TCP stack that way, which costs less for each one. The upstream
gets `Host: localhost` unless the route sets `preserve_host`. Only the
server target can reach one.

A `proxy` route tells its upstream about the request it forwards: it adds
this hop to `Forwarded` (RFC 7239), `X-Forwarded-For` and `Via`, and sets
`X-Forwarded-Proto`, `X-Forwarded-Host`, `X-Real-IP` (the client's address)
and `X-Request-Id`. Hop-by-hop headers stop at Unsworn, in both directions,
and the response gets a `Via` entry too.

Only a proxy in `[server].trusted_proxies` gets its forwarding headers
passed on and added to. From anyone else, `Forwarded`, `X-Real-IP` and the
`X-Forwarded-*` headers are the client's own words, so they're replaced
rather than extended: otherwise an upstream that reads the first
`X-Forwarded-For` entry would take the client's word for its address. On
Cloudflare and serverless targets, the platform gives the client's
address, and the upstream gets fresh headers with it. `X-Original-URL` and
`X-Rewrite-URL` are always removed: some frameworks serve the path they
name, which would sidestep the policies Unsworn applied to the real one.

A `static` route never serves a dotfile or anything inside a dot directory
(`.env`, `.git/config`): those answer `404`. `.well-known` is the exception.

A `static` route sends `ETag` and `Last-Modified`, and honors
`If-None-Match`, `If-Modified-Since`, `Range` and `If-Range`. In a
serverless function there's no `Last-Modified`; see
[`serverless.md`](serverless.md#differences-from-the-server).

### Handler references

A handler reference has the form **`<file>[#<name>]`**:

```toml
handler = "src/users.ts#getUser"     # the getUser export in users.ts
handler = "src/users.ts"             # the file's default handler
handler = "billing.go#Checkout"      # a registered Go function
handler = "worker.nl#users.get"      # whatever the newlang worker understands
```

Unsworn reads only the **file part**: it checks that the file exists and
picks the runtime from its extension. It never interprets the `#name` part.
The whole reference is passed to the worker unchanged (see
[`protocol.md`](protocol.md)), and the worker decides what it means:

- the TS SDK loads the file and calls that export, or the default export
  when there's no `#name`
- the Go SDK looks the name up among the functions it was given, and checks
  that it's defined in that file
- the Rust SDK looks the name up among the functions passed to
  `handlers![...]`
- a hand-written Tier 1 worker can simply `switch` on the string

Because Unsworn never needs to understand modules, exports or packages in any
language, a project can keep every handler in one file, in a few files per
resource, or in one file per route.

When an extension is ambiguous, or a file has none, set the runtime
explicitly:

```toml
handler = "bin/legacy-worker#orders"
runtime = "newlang"
```

`before` lists take handler references too.

Unsworn itself can't confirm that `#name` exists, because that depends on
the language. Workers that support it answer a
[`verify` request](protocol.md#verify-unsworn--worker) instead, so a typo
such as `#getUsr` is reported before any request reaches it:

```
✗ src/users.ts#getUsr: src/users.ts has no export "getUsr"
```

| Command | When handler names are verified |
|---|---|
| `unsworn dev` | At startup, for every runtime. Missing handlers are reported, and their routes return `500` with the error until fixed. |
| `unsworn serve` | When a runtime's first worker starts. For `eager = true` runtimes that's at boot, and a missing handler stops startup. For lazy runtimes, the affected routes return `500` and the error is logged. |
| `unsworn check --deep` | Starts each runtime, verifies, then stops it. Needs the language runtimes installed, so it's opt-in; plain `unsworn check` never starts workers. Run it in CI to keep typos out of production. |

Workers without `verify` support (such as hand-written Tier 1 workers) are
skipped, and `unsworn check --deep` notes which runtimes it couldn't verify.

### Validation

```toml
params = { id = { type = "integer", min = 1 } }
query  = { limit = { type = "integer", default = 20, max = 100 }, q = { type = "string" } }
headers = { "X-Tenant" = { type = "string", required = true } }
body   = "schemas/checkout.json"         # JSON Schema file, or an inline table
response = "schemas/user.json"           # optional response schema
```

Inline shorthand covers simple cases; anything larger uses a JSON Schema
file. Invalid requests get a `400` (or `422` for bodies) with a standard
error body and never reach the handler. Validated values are converted to
their declared types before the handler sees them.

Requests that programs read in different ways are turned away with `400`,
because a `proxy` route forwards the request as it was sent, and its
upstream could act on a value that wasn't checked:

- **A declared parameter or header that isn't an array may appear once.**
  With `?limit=10&limit=999999`, Unsworn would check one value, while Go's
  `Query().Get` and Flask's `args.get` read the first. Declare it as an
  array (`type = "array"`) to accept repeats.
- **A JSON body Unsworn reads may not have two members with the same
  name** in one object, such as `{"role": "user", "role": "admin"}`.

Unsworn reads a JSON body when the route has a `body` schema or `before`
handlers: the schema, each `before` handler and the route's handler would
otherwise each be free to read it differently. A body with neither goes to
the route's handler as it arrived, and the handler's SDK parses it when the
handler first asks for it. A body that isn't JSON then gets its `400` from
the SDK, with the same kind of error body, and a handler that never reads
the body never finds out. A worker without an SDK, or with one from before
this, still gets JSON that Unsworn has read first.

### Policies

```toml
auth       = "jwt"                       # or ["jwt", "service"] = either one; "none" = public
                                         # or { scheme = "jwt", scopes = ["billing:write"] }
cors       = { origins = ["https://app.example.com"], credentials = true }  # credentials need listed origins
origin     = { allow = ["https://app.example.com"] }   # reject other Origin headers with 403
ip         = { allow = ["10.0.0.0/8"], deny = ["10.0.0.5"] }  # IPs or CIDRs; deny wins
rate_limit = { requests = 100, per = "1m", key = "auth.sub" }   # or "ip", "header.<name>"
cache      = { ttl = "30s", vary = ["Authorization"] }  # one entry per value of these request headers
timeout    = "5s"
max_body   = "10MB"
stream_body = true                    # handler reads the body as it arrives
shadow     = "src/users.rs#get"       # a second handler to compare with; see shadow.md
response_headers = { set = { "Cache-Control" = "no-store" }, remove = ["Server"] }
before     = ["src/auth.ts#tenant"]   # handlers that run first, in order
require    = { "params.org" = "auth.org" }   # two values that have to be the same; see below
use        = ["public_cached"]        # named policy sets, applied in order
```

Any inherited policy except `auth` can be switched off for a route with
`"off"`, for example `cache = "off"`. For `auth`, use `"none"`.

`cors` with `origins = "*"` answers any site, but never with credentials:
`credentials = true` needs the origins listed, and `unsworn check` rejects
the two together. Browsers refuse `*` with credentials, and answering each
site by name instead would let any website read what a signed-in user's
browser fetches.

Most keys replace what wider statements set. `before` and `require` are
different: lists from every one of them are joined, widest first, so a
folder's `_before` handler and a route's own `before` both run, and a
route's `require` adds to a prefix's. `"off"` clears the inherited ones.

`response_headers` sets and removes headers on the response. `headers`, on
a route, is something else: rules for the request's headers (see
[Validation](#validation)).

`stream_body = true` passes the request body to the handler as it arrives
instead of reading it first, for large uploads. A slow handler slows the
upload rather than having Unsworn hold the body in memory. `max_body` still
applies (`413`), a body that stalls for 30 seconds gets `408`, and the route's
`timeout` covers the whole upload, so raise it for large files. A streamed
body can't also have `body` validation, and `before` handlers don't see it.
Each upload holds a worker slot while it arrives, so consider a separate
runtime for upload routes.

Rate limits use a token bucket, so `requests` can arrive in a burst and
then refill evenly over `per`. A request over the limit gets `429` with
`Retry-After`. The client IP is the connection's address, or the
`X-Forwarded-For` address when the connection comes from one of
`[server].trusted_proxies`.

#### `require`: whose things a caller may reach

`auth` says who may call a route, and its `scopes` what kind of thing they
may do. Neither says *whose* things: a user with a valid token for one
organization can ask for `/orgs/another/invoices`, and the token passes.
Whether they get the invoices is then up to the handler remembering to
compare the two.

`require` makes that comparison a rule:

```toml
[[route]]
path    = "/orgs/{org}/{*rest}"
require = { "params.org" = "auth.org" }
```

Each pair is two values that have to be the same for the request to go
on. A request where they differ, or that lacks either one, gets `403`
before validation, `before` handlers and the handler run. Its answer
names the rule and neither value. The values a rule can name are ones the
request and its caller already have:

| Value | What it is |
|---|---|
| `auth.<claim>` | a claim of the verified caller: a string, a number or a boolean. A list or an object equals nothing |
| `params.<name>` | a parameter of the path the rule is written for, with its escapes resolved. A `{*rest}` is all of the rest |
| `query.<name>` | a query parameter, sent once |
| `header.<name>` | a request header, sent once |

- **Write it once for everything under a path.** On a prefix entry, as
  above, it covers every route below, an app behind a catch-all included,
  and a route added later. `params.org` is the `{org}` of the entry's own
  path, whatever a route under it calls that segment.
- **Rules add up.** A narrower statement adds its pairs to the wider
  ones', as `before` lists do, and `require = "off"` clears them. In a
  named set, `params.<name>` gets its place from the path of each entry
  that uses the set, so `[defaults]`, which has no path, can't hold one.
- **TOML reads an unquoted `params.org` as a table,** which Unsworn takes
  too: `require = { params.org = "auth.org" }` is the same rule.
- **A rule with `auth.<claim>` needs a caller,** so it's an error on a
  route that's public.
- **It compares, and nothing else.** There are no other operators, and a
  value can't be compared with one written in the config. "Is invoice 812
  this user's" needs a database, and stays in a handler or a `before`
  handler.

A route offered as an [MCP tool](mcp.md#level-3-routes-as-tools) keeps its
rules: a tool call is checked against the request its arguments make.

**A limit is counted where it's written.** Every request it applies to
takes from one count for each caller, whichever route serves the request:

| Written in | One count for |
|---|---|
| `[defaults]` | every route that takes its limit from there: the whole API, less the routes with a limit of their own |
| an entry for a prefix, `/api/{*rest}` | everything under `/api` |
| a route, or an entry for its path | that route |
| a named set, `[policies.free]` | every route that uses the set |

```toml
[defaults]
rate_limit = { requests = 1000, per = "1h", key = "auth.sub" }   # 1,000 an hour for each user, across the API

[[route]]
path       = "/login"
method     = "POST"
auth       = "none"
handler    = "src/login.ts"
rate_limit = { requests = 5, per = "1m", key = "ip" }            # its own count, in place of the one above
```

A narrower limit replaces a wider one, as any policy does: a request to
`/login` counts against its 5 a minute and not against the 1,000 an hour.
For a count several routes share that isn't a path, put the limit in a
named set and `use` it. [`unsworn plan`](plan.md) says what each route's
limit is counted for.

- **Requests that fail `auth` count too,** so guessing credentials is
  limited like anything else. A limit keyed on `ip` or a header is counted
  before credentials are checked, which also keeps a remote API-key verifier
  from being asked more often than the limit allows. A limit keyed on a
  claim (`auth.sub`) has no claim to count a failed request by, so it counts
  that request against the client's IP.
- **An IPv6 client is counted by its /64,** the smallest block a network
  gives one subscriber. Counting whole addresses would give such a client a
  new bucket for every address in it.
- **A `header.<name>` key is only as trustworthy as the header.** Any client
  can send any value, and each new value starts a new count. Use one only
  when the route's `auth` reads that header, or a proxy you trust sets it;
  `unsworn check` warns otherwise.

### Applying policies to many routes

There are three ways to set policies once for many routes:

| Scope | How | Section |
|---|---|---|
| Every route | `[defaults]` | below |
| Every request under a path | A prefix entry such as `path = "/api/{*rest}"` | [Policies for routes from any source](#policies-for-routes-from-any-source) |
| Any routes you choose, wherever they are | Named policy sets with `use` | below |

All of them are in the config file. A folder of route files gets its
policies from a prefix entry for its path, like any other part of the API.

How they combine is described under [Precedence](#precedence).

#### `[defaults]`

Takes the same policy keys and applies them to every route:

```toml
[defaults]
timeout = "10s"
cors    = { origins = ["https://app.example.com"] }
```

#### Named policy sets

For groups of routes that don't share a path, define a set once and refer
to it by name:

```toml
[policies.public_cached]
auth  = "none"
cache = { ttl = "5m" }

[policies.admin]
auth       = "jwt"
rate_limit = { requests = 20, per = "1m", key = "auth.sub" }

[[route]]
path = "/products/{id}"
use  = ["public_cached"]

[[route]]
path = "/categories"
use  = ["public_cached"]

[[route]]
path  = "/internal/{*rest}"
use   = ["admin"]
cache = { ttl = "10s" }        # the entry's own values override its sets
```

- A set takes any policy keys, but not `use`: sets can't include other sets,
  so there are no inheritance chains to trace.
- `use` applies the sets in the order listed, then the entry's own values on
  top.
- `use` works in `[[route]]` entries (with or without an action) and in
  `[defaults]`.
- Naming a set that doesn't exist is an error from `unsworn check`.

### Caching rules

A cached response is stored under a cache key made from the route, method,
path and query string. Two settings add to that key:

- **`vary`** lists request headers, named as in HTTP's `Vary` header.
  Requests with different values for any of them get separate entries, so
  `vary = ["Accept-Language"]` caches each language apart.
- **`key`** adds one value, with the same sources as `rate_limit`'s `key`:
  `"auth.sub"` (or another claim), `"ip"` or `"header.<name>"`. A claim is
  taken from the scheme that verified the caller, so two schemes' users
  with the same `sub` get separate entries.

They can be used together. The key is stored as a SHA-256 hash, so a store
never holds the credentials a `vary` on `Authorization` puts in it.

Caching the wrong response can serve one user's data to another, so `cache`
follows these rules wherever it is set, including in `[defaults]`:

- **Only `GET` responses are cached,** and only with status `200`, `203`,
  `204`, `301`, `404` or `410`. A `HEAD` request is answered from a cached
  `GET` response, but its own response is never stored. WebSocket upgrades
  are never answered from the cache.
- **Responses to authenticated requests aren't cached** unless `vary`
  includes `Authorization` or the header an API-key scheme reads, or the
  `key` setting puts the user in the cache key (`key = "auth.sub"`). `unsworn check` warns about routes where `cache`
  and `auth` are both set but neither is true, so the gap is visible.
- **A caller without the claim the key names isn't cached.** Keyed on
  `auth.sub`, a request whose credentials have no `sub` is neither answered
  from the cache nor stored, rather than sharing one entry with every other
  such caller. API keys listed in `keys` verify as `{type, key}` only, so
  `unsworn check` warns when a route they can call is keyed on another
  claim; vary on the header they're sent in instead.
- **A request with credentials Unsworn didn't check isn't cached on a
  public route:** an `Authorization` header unless `vary` includes
  `Authorization`, and cookies unless it includes `Cookie`. The upstream or
  handler behind the route may have checked them (an app with sessions
  keeps them in a cookie), and its answer may be one user's.
- **A response is only stored if the key covers its `Vary`.** A response
  that varies on a request header the rule's `vary` doesn't list, such as
  `Vary: Cookie` from an app with sessions, could be another user's, so it
  isn't stored. An upstream that compresses its responses sends
  `Vary: Accept-Encoding`; add `"Accept-Encoding"` to `vary` to cache them.
- **`before` handlers run before the cache is read,** so one that turns a
  caller away does so even when a response is cached. That costs a handler
  call on every hit; what the handler adds to the context isn't part of the
  key, so a response that depends on it needs the same request header in
  `vary`.
- **Handlers can always opt out** by returning `Cache-Control: no-store`,
  `no-cache` or `private`, and a response with `Set-Cookie` or `Vary: *` is
  never cached.
- **The response decides how long it stays fresh; `ttl` is the fallback
  and the ceiling.** Unsworn follows RFC 9111 as a shared cache: it reads
  `Cache-Control: s-maxage`, then `max-age`, then `Expires`. A response
  that states none of them is cached for `ttl`. One that states a shorter
  time is cached for that time, and one that states a longer time still
  gets only `ttl`. Time the response already spent in a cache upstream (its
  `Age`) comes off.

  The caveat: **`ttl` can't make a response cacheable for longer than it
  says.** A handler or upstream that sends `Cache-Control: max-age=0` (many
  frameworks do by default) or an `Expires` in the past is never cached,
  whatever `ttl` is. If a route isn't caching, look at its response headers
  first; `unsworn dev` shows no `X-Unsworn-Cache` header on a response that
  wasn't stored. To cache such a response, change what the handler or
  upstream sends.
  `Cache-Control` in a *request* doesn't change what Unsworn serves, so a
  client can't force its way past the cache.
- **A client that already has the cached response gets `304 Not
  Modified`** with no body, when its `If-None-Match` matches the cached
  response's `ETag`, or its `If-Modified-Since` isn't before the cached
  `Last-Modified`. Unsworn doesn't make up validators: a response gets
  this only if the handler or upstream sent an `ETag` or `Last-Modified`.
- **Unsworn adds an `Age` header** to responses served from its cache, and
  `unsworn dev` adds `X-Unsworn-Cache: hit` or `miss` for debugging.

```toml
cache = { ttl = "30s", vary = ["Authorization", "Accept-Language"] }
cache = { ttl = "5m", key = "auth.sub" }       # one cache entry per user
cache = "off"
```

### A full example

```toml
[[route]]
path    = "/checkout"
method  = "POST"
auth    = "jwt"
body    = "schemas/checkout.json"
before  = ["src/auth.ts#tenant"]
timeout = "5s"
handler = "src/billing.go#Checkout"
```

## Route sources

Every source produces entries for the same **route table**: a matcher, an
action and policies. Sources can be combined freely. `unsworn routes` prints
the final table with the source of each route:

```
$ unsworn routes
METHOD  PATH            ACTION                         SOURCE
GET     /health         respond 200                    unsworn.toml:14
GET     /users/{id}     src/users.ts#getUser           openapi.yaml:/users/{id}
PUT     /users/{id}     routes/users.[id].put.go       routes/
GET     /files/{*path}  routes/files/[...path]/get.rs  routes/
*       /{*rest}        src/app.ts                     unsworn.toml:40
```

### 1. Config routes

`[[route]]` entries with an action, as shown throughout this document. With
[handler references](#handler-references), a project needs no route folders:

```toml
[[route]]
path    = "/users"
method  = "GET"
handler = "src/users.ts#listUsers"

[[route]]
path    = "/users/{id}"
method  = "GET"
handler = "src/users.ts#getUser"
```

### 2. Route files (nested or flat)

```toml
[routes]
dir = "routes"
```

Unsworn scans the directory. **A route is built from the directory names
followed by the parts of the file name split on `.`**, where the last part
before the extension is the HTTP method. The same rule covers nested folders,
a single flat folder and any mix of the two:

| Nested | Flat | Route |
|---|---|---|
| `routes/get.ts` | `routes/get.ts` | `GET /` |
| `routes/users/get.ts` | `routes/users.get.ts` | `GET /users` |
| `routes/users/post.ts` | `routes/users.post.ts` | `POST /users` |
| `routes/users/[id]/get.ts` | `routes/users.[id].get.ts` | `GET /users/{id}` |
| `routes/users/[id]/put.go` | `routes/users.[id].put.go` | `PUT /users/{id}` |
| `routes/files/[...path]/get.rs` | `routes/files.[...path].get.rs` | `GET /files/{*path}` |
| `routes/(admin)/stats/get.ts` | `routes/(admin).stats.get.ts` | `GET /stats` |
| `routes/users/_before.ts` | `routes/users._before.ts` | `before` handler for `/users` and below |

- `[name]` is a parameter, `[...name]` matches the rest of the path, and
  `(name)` is a group that doesn't appear in the URL.
- **Literal dots** in file names are written `[.]`:
  `routes/sitemap[.]xml.get.ts` is `GET /sitemap.xml`. Directory names are
  never split, so `routes/.well-known/...` works as written.
- **One file per method.** This works the same in every language, including
  Tier 1 languages with no module system to export from. The file's handler
  reference has no `#name`.
- **The extension picks the runtime,** using `[runtimes.*].extensions`.
- **Go** packages are directories and can't have brackets in their import
  paths, so Go route files use the flat layout in one folder. See the
  [Go SDK](../sdk/go/README.md#route-files).
- **Two files for the same method and path** (such as `get.ts` and `get.go`,
  or a nested and a flat file for the same route) is an error from
  `unsworn check`.

#### Policies for a folder

Route files carry no policies of their own. Give a folder's routes theirs
with an entry for the folder's path, in the config:

```toml
[[route]]
path       = "/admin/{*rest}"        # routes/admin/ and everything in it
auth       = "jwt"
rate_limit = { requests = 20, per = "1m", key = "auth.sub" }
```

That entry covers every request under `/admin`, whichever source its route
comes from. A group folder such as `(public)` has no path of its own, so
its routes take a named set through an entry each, or share a path prefix.

Unsworn doesn't read a `_policies.toml` file in a routes folder, and says
so if it finds one. Such a file would cover the route files beside it and
not a `[[route]]` or an OpenAPI operation under the same path.

### 3. OpenAPI spec

```toml
[openapi]
spec      = "openapi.yaml"                  # OpenAPI 3.0 or 3.1, YAML or JSON
handler   = "src/{tag}.ts#{operationId}"    # optional default for operations
mcp_tools = { tags = ["public"] }           # optional: offer operations as MCP tools
```

Each operation in the spec becomes a route:

- **Path and method** come from the spec. OpenAPI also uses `{name}` for
  path parameters.
- **Validation** comes from the operation's `parameters` and `requestBody`
  schemas, and its `responses` schemas when response validation is on. They
  don't need repeating in `unsworn.toml`.
- **Auth:** names in an operation's `security` are matched to
  `[auth.<name>]` schemes with the same name.
- **Handler:** the operation's `x-unsworn-handler` field, or else the
  `[openapi] handler` template. The template only substitutes values the spec
  already has (`{operationId}`, `{tag}` for the first tag), in line with
  [the config rule](#the-rule-config-never-computes).

```yaml
paths:
  /users/{id}:
    get:
      operationId: getUser
      tags: [users]
      x-unsworn-handler: src/users.ts#getUser   # optional with the template above
      parameters:
        - { name: id, in: path, required: true, schema: { type: integer, minimum: 1 } }
```

An operation with no handler is an error from `unsworn check`, unless a
`[[route]]` entry gives it a non-handler action such as `respond` or `proxy`.

**`x-unsworn-handler` is the only Unsworn field allowed in the spec.** It
links an operation to its code. Policies such as `cache`, `rate_limit` and
`timeout` go in `unsworn.toml` as [action-less `[[route]]`
entries](#policies-for-routes-from-any-source), the same as for every other
route source. This keeps the spec a clean contract that's safe to publish
(internal limits don't leak into public API docs), and keeps all of
Unsworn's behavior in one file. Any other `x-unsworn-*` field is an error
from `unsworn check`, so a setting can't be silently ignored.

`unsworn openapi` outputs the input spec plus any routes from other sources,
so the published document always matches what Unsworn serves.

### 4. Catch-all to an existing app

A catch-all route sends everything not matched by a more specific route to
one handler, which does its own routing:

```toml
[[route]]
path    = "/{*rest}"
handler = "src/app.ts"         # e.g. a Hono or Express app; Go: an http.Handler
```

Unsworn still handles the specific routes declared elsewhere, and can add
policies such as `auth`, `rate_limit` or `cache` to paths inside the app
with action-less `[[route]]` entries. This is mainly a migration path:
move routes out of the app one at a time, while Unsworn sits in front from
day one.

The handler receives the original path. Language SDKs can make this
convenient by accepting a framework's app directly, for example a TS default
export with a `fetch` method, or a Go `http.Handler` through
`unsworn.HTTPHandler`. An app that already runs
as its own HTTP server can use the `proxy` action instead.

### Policies for routes from any source

A `[[route]]` entry with a matcher and **no action** adds policies to a route
defined by another source:

```toml
[[route]]
path   = "/users/{id}"
method = "GET"
auth   = "jwt"
cache  = { ttl = "30s" }
```

A policy entry is a statement about some requests: those its path and
`method` match, whichever route serves them. There are two kinds:

- **An entry for a path** has no `{*rest}`, like the one above. Its path
  has to be some route's path, or a path a catch-all route serves:
  otherwise it's an error from `unsworn check`, which catches typos. It
  covers a narrower route that its path matches too. With the entry
  above, a route added later at `/users/export` also needs a token,
  unless that route says otherwise.
- **An entry for a prefix** has a path ending in `{*rest}`, and covers
  every request under it. This is how you add policies to a folder of
  route files, to part of an OpenAPI spec, or to paths inside a
  catch-all app:

  ```toml
  [[route]]
  path       = "/api/{*rest}"
  auth       = "jwt"
  rate_limit = { requests = 100, per = "1m", key = "auth.sub" }
  ```

`/api/{*rest}` covers `/api` itself as well as everything under it.

An entry with `method = "GET"` is for `GET` requests only, and `HEAD`,
since a `GET` route answers `HEAD`. On a route that takes several
methods, the others keep the policies they had.

An entry for a path can also carry validation (`params`, `query`,
`headers`, `body`, `response`), `mcp` or `shadow` for the route with that
path. Validation belongs to the route as a whole, so such an entry has to
be for all of the route's methods.

#### Locking down part of a catch-all app

A policy entry matches paths as they're written: `/admin/{*rest}` covers
`/admin/users`, but not `/Admin/users` or `/admin%2Fusers`. Unsworn treats
those as different paths, and for its own routes they are. An app behind a
catch-all does its own routing, though, and may not agree: Express matches
paths without regard to case by default, and some servers decode `%2F`
before routing. Such an app would serve `/Admin/users` as its admin page,
without the policy.

So don't add `auth` (or `ip`) to part of a catch-all that is otherwise open.
Require it for the whole catch-all, or in `[defaults]`, and open the public
paths:

```toml
[[route]]
path  = "/{*rest}"
proxy = "http://127.0.0.1:3000"
auth  = "jwt"                 # every path the app serves needs a token

[[route]]
path = "/assets/{*rest}"
auth = "none"                 # except these
```

The entry for `/assets/{*rest}` is for part of what the catch-all serves, so
its `auth` wins there: `/assets` and everything under it are open, and
every other path still needs a token. A path that slips past the entry that
opens it now meets the stricter policy, not the looser one. `unsworn check`
warns about an entry that adds `auth` or `ip` inside an open catch-all.

For OpenAPI routes, policy values here override what the spec implies.

### Precedence

1. **Most specific path wins:** static segments beat parameters, and
   parameters beat `{*rest}`. This is what keeps a catch-all from shadowing
   everything else.
2. **One action per method and path.** Two sources defining an action for
   the same method and path is an error, never a silent override.
3. **The narrowest statement about a request decides each policy.** A
   request gets `[defaults]`, then what the OpenAPI spec says about its
   operation, then every `[[route]]` entry that covers it, from the one
   that covers the most requests to the one that covers the fewest. Each
   sets only the keys it mentions, and a later one replaces what an
   earlier one set. The route's own policies count as an entry for the
   route's path and methods. From wider to narrower:

   | Wider | Narrower |
   |---|---|
   | a prefix: `/api/{*rest}` | a full path: `/api/users/{id}` |
   | a shorter path: `/api/{*rest}` | a longer one: `/api/admin/{*rest}` |
   | a parameter: `/users/{id}` | a literal in its place: `/users/me` |
   | every method | `method = ["GET", "POST"]`, and then `method = "GET"` |

   They're compared in that order, so a longer path is narrower than a
   shorter one whatever their methods. Where two paths of the same length
   differ in more than one segment, the first difference decides, as it
   does when routes are matched.

   So `/api/{*rest}` can require `jwt` for everything, and an entry for
   `/api/public/status` can set `auth = "none"`. A catch-all route needs
   no rule of its own: `/assets/{*rest}` covers part of what a `/{*rest}`
   route serves, so its values win there over the route's, while the
   route `/users/{id}` is narrower than `/{*rest}` and keeps its own.

   Within one table, the sets named in `use` apply first, in order, then
   the table's own values.
4. **Two statements about the same requests can't set the same policy.**
   Neither is the narrower, so `unsworn check` reports it as an error
   instead of picking one: two entries with the same path and methods,
   an entry and a route with the same path and methods, or two with the
   same path whose methods overlap without one list holding the other
   (`["GET", "POST"]` and `["POST", "PUT"]`). `before` and `require`
   are the exceptions, since their lists add up.
5. **`unsworn routes --policies`** prints the final, merged policies for
   each route and which statement set each value, in the order they
   apply. [`unsworn plan`](plan.md) writes the result for every route,
   with what differs for part of a route's paths or for one method.

## MCP

MCP support is described in [`mcp.md`](mcp.md). In config it adds:

| Key | Where |
|---|---|
| `protected_resource` | `[auth.<name>]` |
| `origin` policy, `sticky` proxying, SSE pass-through | routes |
| `[mcp]` and `[[mcp.tool]]`, including `list_tools` and `list_ttl` ([reference](mcp.md#config-reference)) | top level |
| `[mcp] proxy`, with handler-less `[[mcp.tool]]` entries for per-tool policies | top level |
| `mcp = { tool = ... }` ([routes as tools](mcp.md#level-3-routes-as-tools)) | `[[route]]` with a handler, or a policy-only `[[route]]` entry |
| `mcp_tools` | `[openapi]` |

## Deployment targets

`unsworn check --target <t>` checks a config against what a target supports:

```
$ unsworn check --target vercel
✗ unsworn.toml:20 [[route]].rate_limit: needs a shared store on this target: each instance of the function would count on its own
  → set [store] backend = "redis" with a REST API's `url` and `token`, or remove it
✗ 1 error(s)
```

The capability table for each target is part of Unsworn's source, so a config
that passes `check` for a target is expected to work there.

`unsworn build --target cloudflare` bundles a project into a Cloudflare
Worker; see [`cloudflare.md`](cloudflare.md). `--target vercel`,
`--target netlify` and `--target lambda` bundle it into a serverless
function; see [`serverless.md`](serverless.md).

## Open questions

- Should route files also allow one file with several method exports
  (`route.ts` exporting `GET` and `POST`, as in xink and SvelteKit) for SDK
  languages?
- Environment-specific overrides: `unsworn.production.toml` layered on top, or
  `{env.*}` substitution only?
