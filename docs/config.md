# Unsworn configuration

> Draft, 2026-11-13. Field names and defaults are proposals.

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
- `unsworn check` validates the config and the routes directory and prints
  errors with a path and a fix, for example:

  ```
  ✗ route[3].handler: no runtime handles ".rb"
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
[mcp]          # optional: serve MCP tools (v2; see mcp.md)
[runtimes.*]   # how to run handlers for each language
[auth.*]       # named authentication schemes
[store]        # shared state for rate limits and caches
[defaults]     # policies applied to every route unless overridden
[policies.*]   # named policy sets that routes can `use`
[[route]]      # explicit routes, and policies for routes from other sources
```

### `[server]`

```toml
[server]
listen        = "0.0.0.0:8080"
max_body      = "1MB"
timeout       = "30s"
trailing_slash = "redirect"       # "redirect" | "strict" | "ignore"

[server.tls]
cert = "certs/fullchain.pem"
key  = "certs/privkey.pem"
```

### `[runtimes.<name>]`

```toml
[runtimes.ts]
extensions    = [".ts", ".js"]
command       = "bun run node_modules/@unsworn/bun/worker.ts"
protocol      = "binary"          # "jsonl" (Tier 1) | "binary" (Tier 2)
workers       = 4
eager         = false             # start workers at boot instead of first request
env           = { NODE_ENV = "production" }

[runtimes.go]
extensions = [".go"]
build      = "go build -o .unsworn/go-worker ./routes/..."
command    = ".unsworn/go-worker"
protocol   = "binary"
workers    = 2

[runtimes.newlang]
extensions = [".nl"]
command    = "newlang run worker.nl"
protocol   = "jsonl"              # no SDK needed
```

Runtime names are arbitrary. Unsworn knows nothing about any specific language.

### `[auth.<name>]`

```toml
[auth.jwt]
type     = "jwt"
jwks_url = "https://auth.example.com/.well-known/jwks.json"
audience = "api"
issuer   = "https://auth.example.com/"

[auth.service]
type   = "api_key"
header = "X-API-Key"
keys   = "{env.SERVICE_KEYS}"     # comma-separated
```

Verified claims are available to handlers as `ctx.auth`.

A `jwt` scheme can also describe the OAuth resource it protects, with
`[auth.<name>].protected_resource`. Unsworn then serves the OAuth
protected-resource metadata and answers missing tokens with the
`WWW-Authenticate` header that OAuth and MCP clients expect. See
[`mcp.md`](mcp.md#oauth-protected-resource-support).

### `[store]`

```toml
[store]
backend = "memory"                # "memory" | "redis" | platform stores (per target)
url     = "{env.REDIS_URL}"
```

`memory` is fine on a single node. Stateless targets need a shared backend
for `rate_limit` and `cache`; `unsworn check --target <t>` reports this.

## Routes

Each `[[route]]` has a **matcher**, optional **policies** and exactly one
**action**.

### Matcher

```toml
path   = "/users/{id}"            # {name} = one segment, {*name} = the rest of the path
method = "GET"                    # or ["GET", "HEAD"]; omitted = all methods
host   = "api.example.com"        # optional
```

### Actions

| Action | Example | Runs a worker? |
|---|---|---|
| `respond` | `respond = { status = 200, json = { ok = true } }` | No |
| `redirect` | `redirect = { to = "/v2/users/{id}", status = 308 }` | No |
| `static` | `static = { dir = "public", spa_fallback = "index.html" }` | No |
| `proxy` | `proxy = "http://old-api:8080"` or `{ upstreams = [...], strip_prefix = "/legacy", sticky = "header:mcp-session-id" }` | No |
| `handler` | `handler = "src/users.ts#getUser"` | Yes |

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
- the Go SDK looks the name up in its handler registry
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
✗ route[2].handler: src/users.ts has no export "getUsr"
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

### Policies

```toml
auth       = "jwt"                       # or ["jwt", "service"] = either one; "none" = public
                                         # or { scheme = "jwt", scopes = ["billing:write"] }
cors       = { origins = ["https://app.example.com"], credentials = true }
origin     = { allow = ["https://app.example.com"] }   # reject other Origin headers with 403
rate_limit = { requests = 100, per = "1m", key = "auth.sub" }   # or "ip"
cache      = { ttl = "30s", vary = ["Authorization"] }
timeout    = "5s"
max_body   = "10MB"
headers    = { set = { "Cache-Control" = "no-store" }, remove = ["Server"] }
before     = ["src/auth.ts#tenant"]   # handlers that run first, in order
use        = ["public_cached"]        # named policy sets, applied in order
```

Any inherited policy except `auth` can be switched off for a route with
`"off"`, for example `cache = "off"`. For `auth`, use `"none"`.

### Applying policies to many routes

There are four ways to set policies once for many routes, from broadest to
narrowest:

| Scope | How | Section |
|---|---|---|
| Every route | `[defaults]` | below |
| Any routes you choose, wherever they are | Named policy sets with `use` | below |
| A folder of route files | `_policies.toml` in that folder | [Route files](#2-route-files-nested-or-flat) |
| Every route under a path | A prefix entry such as `path = "/api/{*rest}"` | [Policies for routes from any source](#policies-for-routes-from-any-source) |

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
- `use` works in `[[route]]` entries (with or without an action), in
  `_policies.toml` files and in `[defaults]`.
- Naming a set that doesn't exist is an error from `unsworn check`.

### Caching rules

Caching the wrong response can serve one user's data to another, so `cache`
follows these rules wherever it is set, including in `[defaults]`:

- **Only `GET` and `HEAD` responses are cached,** and only with status
  `200`, `203`, `204`, `301`, `404` or `410`.
- **Responses to authenticated requests aren't cached** unless `vary`
  includes `Authorization`, or the `key` setting puts the user in the cache
  key (`key = "auth.sub"`). `unsworn check` warns about routes where `cache`
  and `auth` are both set but neither is true, so the gap is visible.
- **Handlers can always opt out** by returning `Cache-Control: no-store` or
  `private`, and a response with `Set-Cookie` is never cached.
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
| `routes/admin/_policies.toml` | `routes/admin._policies.toml` | policies for every route file under `/admin` |

- `[name]` is a parameter, `[...name]` matches the rest of the path, and
  `(name)` is a group that doesn't appear in the URL.
- **Literal dots** in file names are written `[.]`:
  `routes/sitemap[.]xml.get.ts` is `GET /sitemap.xml`. Directory names are
  never split, so `routes/.well-known/...` works as written.
- **One file per method.** This works the same in every language, including
  Tier 1 languages with no module system to export from. The file's handler
  reference has no `#name`.
- **The extension picks the runtime,** using `[runtimes.*].extensions`.
- **Two files for the same method and path** (such as `get.ts` and `get.go`,
  or a nested and a flat file for the same route) is an error from
  `unsworn check`.

#### Folder policies: `_policies.toml`

A `_policies.toml` file applies policies to every route file in its folder
and the folders below it, so a group's settings live next to the group:

```
routes/
  admin/
    _policies.toml        # auth = "jwt", rate_limit = ...
    users.get.ts
    stats.get.ts
  (public)/
    _policies.toml        # use = ["public_cached"]
    products.[id].get.ts
    categories.get.ts
```

```toml
# routes/admin/_policies.toml
auth       = "jwt"
rate_limit = { requests = 20, per = "1m", key = "auth.sub" }
```

- It takes the same keys as a policy-only `[[route]]` entry, including
  `use`, but no path or method: the folder is the scope.
- In a group folder such as `(public)`, it gives routes a shared policy
  without adding anything to their URLs.
- It applies only to route files. Routes from config or an OpenAPI spec that
  happen to share the path aren't affected; use a prefix entry for those.

### 3. OpenAPI spec

```toml
[openapi]
spec      = "openapi.yaml"                  # OpenAPI 3.0 or 3.1, YAML or JSON
handler   = "src/{tag}.ts#{operationId}"    # optional default for operations
mcp_tools = { tags = ["public"] }           # optional, v3: offer operations as MCP tools
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
export with a `fetch` method or a Go `http.Handler`. An app that already runs
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

There are two kinds of policy entries:

- **Exact entries** have a path with no `{*rest}`, like the one above. They
  must match exactly one route from some source, or `unsworn check` reports
  it, which catches typos in paths.
- **Prefix entries** have a path ending in `{*rest}`. They apply to every
  request under that prefix, whichever route serves it. This is how you add
  policies to paths inside a catch-all app:

  ```toml
  [[route]]
  path       = "/api/{*rest}"
  auth       = "jwt"
  rate_limit = { requests = 100, per = "1m", key = "auth.sub" }
  ```

For OpenAPI routes, policy values here override what the spec implies.

### Precedence

1. **Most specific path wins:** static segments beat parameters, and
   parameters beat `{*rest}`. This is what keeps a catch-all from shadowing
   everything else.
2. **One action per method and path.** Two sources defining an action for
   the same method and path is an error, never a silent override.
3. **Policies merge from broadest to most specific,** later winning:
   1. `[defaults]`
   2. the OpenAPI spec
   3. `_policies.toml` files, outermost folder first
   4. prefix entries, shorter prefixes first
   5. exact entries

   Within any one of these, the sets named in `use` apply first, in order,
   then that layer's own values. So `/api/{*rest}` can require `jwt` for
   everything, and an exact entry for `/api/public/status` can set
   `auth = "none"`.
4. **`unsworn routes --policies`** prints the final, merged policies for
   each route and which layer set each value, so nothing about the merge
   has to be worked out by hand.

## MCP

MCP support is described in [`mcp.md`](mcp.md). In config it adds:

| Key | Where | Planned |
|---|---|---|
| `protected_resource` | `[auth.<name>]` | v1 |
| `origin` policy, `sticky` proxying, SSE pass-through | routes | v1 |
| `[mcp]` and `[[mcp.tool]]`, including `list_tools` | top level | v2 |
| `[mcp] proxy`, with handler-less `[[mcp.tool]]` entries for per-tool policies | top level | v2 |
| `mcp = { tool = ... }` | `[[route]]` with a handler | v3 |
| `mcp_tools` | `[openapi]` | v3 |

## Deployment targets

`unsworn check --target <t>` checks a config against what a target supports:

```
$ unsworn check --target vercel
✗ route[4].rate_limit: needs a shared store on this target
  → set [store] backend = "upstash", or remove rate_limit
✓ 12 routes OK
```

The capability table for each target is part of Unsworn's source, so a config
that passes `check` for a target is expected to work there.

## Open questions

- Should route files also allow one file with several method exports
  (`route.ts` exporting `GET` and `POST`, as in xink and SvelteKit) for SDK
  languages?
- Environment-specific overrides: `unsworn.production.toml` layered on top, or
  `{env.*}` substitution only?
