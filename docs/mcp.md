# MCP servers with Unsworn

A remote [Model Context Protocol](https://modelcontextprotocol.io) (MCP)
server lets AI agents call tools over HTTP. Running one in production needs
most of what Unsworn already does: token checks, rate limits, input
validation, streaming responses and logs. Unsworn supports MCP at three
levels:

| Level | What you get |
|---|---|
| [1. In front of an MCP server](#level-1-in-front-of-an-existing-mcp-server) | OAuth checks, rate limits, origin checks and logs for an MCP server built with any MCP SDK, plus per-tool policies |
| [2. MCP tools as handlers](#level-2-mcp-tools-as-handlers) | Unsworn speaks MCP itself. Each tool is a plain function in any language, with its own policies |
| [3. Routes as tools](#level-3-routes-as-tools) | Any REST route, or OpenAPI operation, can also be an MCP tool, from the same handler |

Unsworn only covers **remote** MCP servers (the Streamable HTTP transport).
Local servers that a client starts and talks to over stdio don't benefit.

## Protocol version

Where Unsworn speaks MCP itself (levels 2 and 3, and per-tool policies for a
proxied server), it implements MCP **2026-07-28** and nothing older.

That version made MCP stateless. There are no sessions and no `initialize`
handshake. Every request carries its protocol version and client details,
so any Unsworn instance can answer any request with no shared state.
Clients built for earlier versions (2025-11-25 and before) use the
handshake, so they can't talk to an Unsworn MCP endpoint. When they try,
they get `400 Bad Request` with an error that lists the supported versions.

Level 1 doesn't depend on the version, because the MCP server behind
Unsworn handles the protocol itself.

Each Unsworn release lists the MCP versions it supports. New MCP versions
arrive with Unsworn releases.

## What a remote MCP server needs

- **One HTTP endpoint,** such as `/mcp`, that accepts JSON-RPC messages
  over `POST`, one message per request. The server answers each request
  with a JSON object, or with a server-sent events (SSE) stream for that
  request alone.
- **Request headers that match the body.** Each request repeats its
  method and tool name in `Mcp-Method` and `Mcp-Name` headers, so gateways
  can route on them. The server must reject a request whose headers don't
  match its body.
- **OAuth-based authorization.** The MCP server acts as an OAuth *resource
  server*: it publishes metadata at `/.well-known/oauth-protected-resource`,
  answers missing or invalid tokens with `401` and a `WWW-Authenticate`
  header pointing at that metadata, and only accepts tokens issued for itself
  (the audience check).
- **An `Origin` header check,** to block DNS-rebinding attacks from web
  pages.
- In practice: rate limits, timeouts, audit logs and metrics.

Unsworn is never the OAuth *authorization server*. Tokens come from an
identity provider such as Auth0, Clerk, WorkOS, Keycloak or your own.
Unsworn verifies them.

## Shared building blocks

These are general Unsworn features that MCP relies on. They work for any
route, at every level.

### OAuth protected-resource support

An auth scheme can describe the resource it protects:

```toml
[auth.oauth]
type     = "jwt"
issuer   = "https://auth.example.com/"
url      = "https://auth.example.com/.well-known/jwks.json"
audience = "https://api.example.com/mcp"     # required: tokens must be issued for this server

[auth.oauth.protected_resource]
resource              = "https://api.example.com/mcp"
authorization_servers = ["https://auth.example.com/"]
scopes_supported      = ["tools:read", "billing:write"]
```

With `protected_resource` set, Unsworn:

- serves the metadata document at `/.well-known/oauth-protected-resource`
  (and at the path-specific form for the resource), with no route needed
- answers a missing or invalid token with `401` and a `WWW-Authenticate`
  header that includes the metadata URL
- answers a valid token without a required scope with `403` and an
  `insufficient_scope` error

`unsworn check` reports a scheme with `protected_resource` but no
`audience`, because accepting tokens issued for other services is a known
MCP security hole.

Routes can require scopes:

```toml
auth = { scheme = "oauth", scopes = ["billing:write"] }
```

### Origin checks

```toml
origin = { allow = ["https://claude.ai", "https://app.example.com"] }
```

A request whose `Origin` header isn't in the list gets `403`. Requests
without an `Origin` header, which is normal for non-browser MCP clients,
pass.

### Streaming pass-through

Proxied responses with `Content-Type: text/event-stream` are passed through
as they arrive: never buffered, compressed or cached. Long-lived streams are
governed by an idle timeout instead of the route's request timeout.

### Sticky proxying

MCP servers built for versions before 2026-07-28 may keep sessions in
memory. When a proxy has several of them as upstreams, requests in the same
session must reach the same one:

```toml
proxy = { upstreams = ["http://10.0.0.5:4000", "http://10.0.0.6:4000"], sticky = "header:mcp-session-id" }
```

Servers on 2026-07-28 or later have no sessions and don't need this.

## Level 1: in front of an existing MCP server

An MCP server built with any MCP SDK runs as it is, and
Unsworn sits in front of it:

```toml
[[route]]
path       = "/mcp"
proxy      = "http://127.0.0.1:4000"
auth       = { scheme = "oauth", scopes = ["tools:read"] }
origin     = { allow = ["https://claude.ai"] }
rate_limit = { requests = 60, per = "1m", key = "auth.sub" }
```

The MCP server's code needs no auth logic of its own, and the security rules
for an agent-facing endpoint are in one short, reviewable file.

At this level, Unsworn sees every MCP request as `POST /mcp`. It can't tell
tools apart, so policies apply to the whole server. This works with MCP
servers of any protocol version.

### Per-tool policies for a proxied server

Once Unsworn understands MCP (level 2), it can also apply per-tool policies
to a server it only proxies. Use `[mcp]` with `proxy` instead of a `[[route]]`,
and add `[[mcp.tool]]` entries **without a handler**. Like action-less
`[[route]]` entries, they only add policies:

```toml
[mcp]
path   = "/mcp"
proxy  = "http://127.0.0.1:4000"          # the existing MCP server, unchanged
auth   = { scheme = "oauth", scopes = ["tools:read"] }
origin = { allow = ["https://claude.ai"] }

[[mcp.tool]]
name       = "create_invoice"
auth       = { scheme = "oauth", scopes = ["billing:write"] }
rate_limit = { requests = 10, per = "1m", key = "auth.sub" }
```

- Unsworn reads the method and tool name from the body of each request,
  checks that they match the `Mcp-Method` and `Mcp-Name` headers, then
  forwards the request. It doesn't rely on the headers alone, because the
  server behind it might not check them. A `tools/call` for
  `create_invoice` gets that tool's policies on top of the `[mcp]` ones.
  Other tools get the `[mcp]` ones.
- Responses, including the tool list, pass through unchanged. The
  [tool list visibility](#tool-list-visibility) rules below apply only to
  tools Unsworn hosts itself, because filtering a proxied server's list would
  mean rewriting its responses.
- `input` can be set too, to validate a proxied tool's arguments before they
  reach the server. Without it, the server behind the proxy checks them.
- The [policies](#policies) and the way their failures are reported are the
  same as for tools Unsworn hosts. A call that fails one never reaches the
  server.
- The server behind the proxy must speak MCP 2026-07-28, like the clients.
  For an older server, use a plain [level 1](#level-1-in-front-of-an-existing-mcp-server)
  route.

## Level 2: MCP tools as handlers

Unsworn implements the MCP protocol itself, and each tool
is a handler:

```toml
[mcp]
path         = "/mcp"
name         = "acme-tools"
instructions = "Tools for Acme's billing and weather data."
auth         = { scheme = "oauth", scopes = ["tools:read"] }
origin       = { allow = ["https://claude.ai"] }
rate_limit   = { requests = 120, per = "1m", key = "auth.sub" }

[[mcp.tool]]
name        = "get_forecast"
description = "Weather forecast for a city, up to 7 days ahead"
input       = "schemas/forecast.json"         # JSON Schema, or an inline table
output      = "schemas/forecast-result.json"  # optional
handler     = "tools/weather.py#get_forecast"
annotations = { read_only = true }
rate_limit  = { requests = 10, per = "1m", key = "auth.sub" }

[[mcp.tool]]
name        = "create_invoice"
description = "Create a draft invoice for a customer"
input       = "schemas/invoice.json"
handler     = "src/billing.ts#createInvoice"
auth        = { scheme = "oauth", scopes = ["billing:write"] }
timeout     = "20s"
```

`unsworn routes` lists each tool as a `TOOL` row, with `--policies`
showing the policies a call to it gets and where each one came from.

### What Unsworn handles

- **The protocol:** `server/discover`, `tools/list` and `tools/call`.
  Unknown methods get `404` with a JSON-RPC `-32601` error. Notifications
  get `202`. `GET` and `DELETE` get `405`, since the endpoint takes only
  `POST`.
- **Version and header checks.** The `MCP-Protocol-Version` header and the
  protocol version in the request's `_meta` must match each other and a
  version Unsworn supports. The `Mcp-Method`, `Mcp-Name` and `Mcp-Param-*`
  headers must match the body. Failures get `400` with the MCP error for
  the case (`-32022` for an unsupported version, which lists the supported
  ones, and `-32020` for headers that don't match).
- **Responses.** Unsworn answers each request with a single
  `application/json` object. Every result has `resultType: "complete"` and
  the server's name and version in its `_meta`.
- **Cancellation.** In MCP 2026-07-28, a client cancels a request by
  closing its connection. Unsworn then sends the worker a `cancel` message,
  the same as for any HTTP request whose client goes away.
- **Input validation.** Arguments are checked against the tool's `input`
  schema before the handler runs. Invalid arguments come back to the agent as
  a tool error with the validation details, so the model can correct them and
  retry. A tool without `input` takes no arguments.
- **Output validation.** If a tool declares `output`, results are checked
  against it, following the route's response-validation setting.

### Policies

A request to the endpoint is checked in two steps.

1. **Every request** (`server/discover`, `tools/list` and `tools/call`)
   gets the `[mcp]` policies: `origin`, `ip`, `cors`, `headers`,
   `max_body`, `auth` and `rate_limit`. The `[mcp]` rate limit counts every
   request to the endpoint.
2. **A tool call** then gets the tool's own policies: `auth`, `rate_limit`,
   `timeout`, `before` and `use`. These add to the `[mcp]` ones:
   - A tool's `auth` is checked **in addition** to the `[mcp]` auth, so a
     tool can require more scopes but can't allow less. `auth = "none"` on
     a tool is an error.
   - A tool's `rate_limit` is a separate limit for calls to that tool, on
     top of the `[mcp]` one. A tool can't switch the `[mcp]` limit off.
   - `timeout` and `before` from `[mcp]` apply to every tool call. A tool
     can set its own `timeout`, add `before` handlers, or turn the inherited
     ones off with `before = "off"`.

Policies that apply to the endpoint as a whole (`origin`, `ip`, `cors`,
`headers`, `max_body`) can't be set on a tool, not even through a `use`
set. `cache` and `stream_body` don't apply to MCP at all.

When a tool call fails a policy:

| Failure | The client receives |
|---|---|
| A token without the tool's scopes | `403` with an `insufficient_scope` error naming the missing scopes, so a client that supports it can ask the user for more access |
| The tool's rate limit | A tool result with `isError: true`, saying when to retry, so the model can react |
| A `before` handler that rejects the call | A tool result with `isError: true` and the handler's message |

The `[mcp]` rate limit answers `429` like any route. It's counted once the
request's headers have been checked against its body, so its key can be a
checked header.

### Tool list visibility

By default, **`tools/list` only shows tools the caller is allowed to call.**
A tool whose `auth` requires scopes the caller's token doesn't have is left
out of the list.

- Agents pick tools from the list, so listing tools that will fail wastes
  the model's context and leads to failed calls. It also avoids revealing
  capabilities to callers who can't use them.
- Calls are still checked. Calling a hidden tool by name gets the same
  `403` with an `insufficient_scope` error as any other call without the
  right scope. That error names the missing scopes, so a client that supports
  asking the user for more access can still do so.
- Some apps want clients to see every tool and request more access when
  needed. They can turn filtering off:

  ```toml
  [mcp]
  list_tools = "all"         # default: "permitted"
  ```

Tools are always listed in the same order (by name), so clients and model
prompt caches can reuse the list. Results tell clients how long they may
cache them (`list_ttl`, default 5 minutes). A list filtered by scopes is
marked `private`, so shared caches don't reuse one caller's list for
another. A `list_tools = "all"` list is marked `public`.

Filtering only uses scopes, which Unsworn can check without running any
code. **`before` handlers don't run for `tools/list`.** They run on each
tool call, where they can still reject the call, for example for a tenant
whose plan doesn't include that tool. Running them for the list would add a
worker call to every `tools/list` request, which clients send often. A
handler-based list filter is listed under [Later](#later) in case the need
shows up.

### Arguments in headers

A tool's input schema can mark arguments that clients copy into
`Mcp-Param-*` headers, so gateways and load balancers can see them without
reading the body:

```json
{
  "type": "object",
  "properties": {
    "tenant": { "type": "string", "x-mcp-header": "Tenant" },
    "query":  { "type": "string" }
  }
}
```

- `unsworn check` enforces the MCP rules for `x-mcp-header`: the argument
  must be a string, integer or boolean, reached from the schema root only
  through `properties`, with a header name that's unique (ignoring case).
  Clients ignore a tool that breaks these rules, so they're config errors.
- Unsworn checks that each `Mcp-Param-*` header matches its argument in the
  body. A checked header can then be used as a policy key, for example
  `rate_limit = { requests = 100, per = "1m", key = "header.mcp-param-tenant" }`.
  Policy keys never see an `Mcp-Param-*` header that wasn't checked against
  the call's arguments, such as one sent with `tools/list`, so a client can't
  pick its own rate-limit bucket.
- Don't mark secrets or personal data: headers are visible to every proxy
  on the way.

### What a handler sees

A tool handler gets the normal `request` message from
[`protocol.md`](protocol.md). The arguments arrive as `json`, and
`context.mcp` describes the call:

```json
{
  "type": "request",
  "id": "r-90",
  "phase": "handle",
  "handler": "tools/weather.py#get_forecast",
  "route": "mcp:get_forecast",
  "method": "POST",
  "path": "/mcp",
  "json": {"city": "Lisbon", "days": 3},
  "context": {
    "auth": {"sub": "u_123", "scope": "tools:read"},
    "mcp": {"tool": "get_forecast", "protocol_version": "2026-07-28", "client": {"name": "claude-ai", "version": "0.1.0"}}
  },
  "deadline_ms": 19950
}
```

`client` is what the client says about itself, and clients may leave it
out. It's for logs, not for decisions.

A tool handler is an ordinary handler, with the same signature as a route
handler. That's what lets [level 3](#level-3-routes-as-tools) offer a route
as a tool without changing its handler. In the Python SDK:

```python
from unsworn import json, tool_error

def get_forecast(req, ctx):
    city = req.json["city"]
    if city not in CITIES:
        return tool_error(f"unknown city {city}")  # the model sees this
    return json({"city": city, "days": forecast(city, req.json.get("days", 3))})
```

Each SDK has helpers for tools:

| | JS/TS | Python | Go | Rust |
|---|---|---|---|---|
| The call's details | `ctx.mcp` | `ctx["mcp"]` | `req.MCP()` | `req.mcp()` |
| Content blocks | `content(blocks, json?)` | `content(blocks, json=...)` | `Content(blocks)`, `.WithContent(blocks)` | `Reply::content(blocks)`, `.with_content(blocks)` |
| An error for the model | `toolError(message)` | `tool_error(message)` | `ToolError(message)` | `Reply::tool_error(message)` |

A tool error is a `400` with `{"error": message}`, so Unsworn returns
`message` to the model as a tool result with `isError: true`. An exception
or returned error is different: it's logged, and the model only sees that
the tool failed.

### How results are mapped

| Handler returns | The agent receives |
|---|---|
| `2xx` with `json` | `structuredContent` with that value, plus a text copy, as MCP recommends |
| `2xx` with `text` | One text content block |
| `2xx` with `mcp_content` | Those MCP content blocks as they are (images, audio, resource links, embedded resources). With `json` too, `json` still becomes `structuredContent` |
| `2xx` with a raw or streamed body | A tool error: MCP results can't carry it |
| `4xx` or `5xx` | A tool result with `isError: true` and the handler's message: the `error`, `message` or `detail` field of its JSON, or its text, so the model can react |
| An `error` message, a timeout or a crashed worker | A tool error saying the tool failed (or didn't finish in time), without internal details. The details go to the logs, and also into the result in `unsworn dev` |

With `output` set, a result whose `json` doesn't match it (or that has no
`json`) is logged, and with `response_validation = "enforce"` it becomes a
tool error instead.

Every answer is `application/json`. An unknown tool gets a JSON-RPC
`-32602` error, and `tools/list` has a single page, so any `cursor` is
invalid.

`mcp_content` is a response field only used for MCP tools. See
[`protocol.md`](protocol.md#mcp-tool-calls).

### State across calls

MCP has no sessions, so a tool can't rely on an earlier call on the same
connection. A tool that needs state across calls, such as a shopping cart,
returns an ID from one call and takes it as an argument in the next. The
handler stores the state under that ID, in its own database. It checks on
every call that the caller may use that ID.

### Server identity

`name`, `version` and `title` in `[mcp]` identify the server to clients.
`instructions` is guidance for the model on how to use the tools. Clients
get them from `server/discover`, and `name` and `version` also appear in
every result's `_meta`.

### Config reference

`[mcp]`:

| Key | Default | Meaning |
|---|---|---|
| `path` | `/mcp` | The endpoint path. It can't have parameters |
| `name` | `unsworn` | Server name reported to clients |
| `version` | the Unsworn version | Server version reported to clients |
| `title` | none | Display name for the server |
| `instructions` | none | Guidance for the model |
| `proxy` | none | An existing MCP server to forward to; tools then only add policies |
| `list_tools` | `permitted` | `permitted` or `all`; see [Tool list visibility](#tool-list-visibility) |
| `list_ttl` | `5m` | How long clients may cache `tools/list` and `server/discover` results |
| `auth`, `origin`, `ip`, `cors`, `headers`, `rate_limit`, `timeout`, `max_body`, `before`, `use` | | Policies; see [Policies](#policies) |

`[[mcp.tool]]`:

| Key | Meaning |
|---|---|
| `name` | Required. 1 to 128 letters, digits, `_`, `-` and `.`, unique within the endpoint |
| `description` | What the tool does. Models choose tools by it, so `unsworn check` warns when it's missing |
| `title` | Display name |
| `input` | JSON Schema for the arguments, as a file or an inline table. Its root must be `"type": "object"`. Default: no arguments |
| `output` | JSON Schema for `structuredContent` |
| `handler`, `runtime` | The handler, as for routes. Required unless `[mcp]` has `proxy` |
| `annotations` | Hints for clients: `read_only`, `destructive`, `idempotent`, `open_world`. Not enforced |
| `auth`, `rate_limit`, `timeout`, `before`, `use` | Policies; see [Policies](#policies) |

For a proxied server, a tool takes only `name`, `input` and policies, and
a tool without `input` isn't validated. Tools the server has but `[mcp]`
doesn't list get only the `[mcp]` policies.

## Level 3: routes as tools

Any route with a handler can also be offered as an MCP
tool:

```toml
[mcp]
path = "/mcp"
auth = { scheme = "oauth", scopes = ["tools:read"] }

[[route]]
path    = "/users/{id}"
method  = "GET"
params  = { id = { type = "integer", min = 1 } }
query   = { verbose = { type = "boolean" } }
auth    = "oauth"
handler = "src/users.ts#getUser"
mcp     = { tool = "get_user", description = "Look up a user by id" }
```

`mcp` takes `tool` (the name, required), `description`, `title`,
`annotations` (as for [`[[mcp.tool]]`](#config-reference)), and `method`
for a route that accepts several. Without `description`, an OpenAPI
operation's description or summary is used.

A route from a route file or the OpenAPI spec becomes a tool through a
policy-only `[[route]]` entry, which leaves the route itself unchanged:

```toml
[[route]]
path   = "/health"
method = "GET"
mcp    = { tool = "health", description = "Check the service's health" }
```

### Arguments

The tool's input schema is built from the route's contract:

- Path and query parameters become top-level arguments, with the types and
  rules declared for them. Path parameters are required; a `default` on a
  query parameter applies as it does for REST requests.
- A JSON body's fields sit next to them, when the body schema is a plain
  object with `properties` and no field shares a parameter's name. Otherwise
  the whole body is one `body` argument.
- A route without a `body` schema gets a tool with no body. `unsworn check`
  warns about that for `POST`, `PUT` and `PATCH` routes.

`unsworn routes --policies` shows each tool's arguments and where they go,
such as `mcp tool get_user: id (path), verbose (query)`.

### What the handler sees

**The handler doesn't change.** Unsworn maps the arguments back to the
request a REST client would send: the route's method, the path with the
parameters filled in (`/users/42`), `route` set to the pattern, and
`params`, `query`, `raw_query` and `json` as usual. The request also has
`context.mcp`, so a handler can tell it's a tool call when it needs to, but
it doesn't have to. Replies map to tool results as for
[tools as handlers](#how-results-are-mapped).

### Policies

- A call first gets the `[mcp]` checks for every request (see
  [Policies](#policies)).
- Then the **route's own policies, as a REST request to it sees them:** its
  `auth`, its `rate_limit` and its `before` handlers, after `[mcp]`'s
  `before` handlers. Its `timeout` applies, or `[mcp]`'s if the route sets
  none.
- **REST requests and tool calls share the route's rate limit.** One limit
  covers both ways in.
- Policies that describe HTTP requests (`origin`, `ip`, `cors`, `headers`,
  `max_body`, `cache`) don't apply to tool calls; the MCP endpoint has its
  own.
- A tool call carries the credentials sent to the MCP endpoint, so the
  route's `auth` must accept at least one scheme the `[mcp]` auth uses.
  `unsworn check` reports a route whose auth no tool call could pass.

### What can't be a tool

`unsworn check` reports:

- routes without a handler (`respond`, `redirect`, `static`, `proxy`)
- routes that accept several methods, unless `mcp.method` picks one
- routes with `stream_body`, since tool arguments arrive as one JSON object
- routes that require a request header, which MCP clients don't send
- routes when `[mcp]` has `proxy`, since Unsworn must be the MCP server
- tool names already used by another tool

A handler that streams its response or returns raw bytes can't be detected
in advance. A tool call that gets one returns a tool error instead.

### OpenAPI operations as tools

With an [OpenAPI source](config.md#3-openapi-spec), operations can become
tools in bulk:

```toml
[openapi]
spec      = "openapi.yaml"
mcp_tools = { tags = ["public"] }      # or "all"
```

- `"all"` picks every operation, and `{ tags = [...] }` the operations
  with any of those tags.
- The tool's name is the operation's `operationId`, and its description is
  the operation's `description`, or its `summary` when it has none. An
  `operationId` that isn't a valid tool name is an error.
- Each picked operation is a route tool like any other: same arguments,
  request shape and policies as above.
- An operation that can't be a tool is skipped with a warning instead of
  failing the config: one without an `operationId`, one served by a config
  route without a handler, or one that breaks a rule under
  [What can't be a tool](#what-cant-be-a-tool).
- A `[[route]]` entry for the operation's path and method can set `mcp`
  itself, to rename the tool or give it a better description:

  ```toml
  [[route]]
  path   = "/users"
  method = "GET"
  mcp    = { tool = "list_users", description = "List users, newest first" }
  ```

## Later

Not planned for a specific version yet:

- **Resources and prompts.** Read-only data and prompt templates the client
  can list and fetch.
- **Asking the user mid-call (elicitation).** In MCP 2026-07-28 this needs
  no sessions: the tool returns an "input required" result describing what
  it needs, and the client calls the tool again with the answers. A handler
  would need a way to return such a result and to read the answers on the
  retry. Sampling and roots, the other kinds of input request, are
  deprecated in MCP, so Unsworn won't add them.
- **Progress notifications.** A handler would report progress on a long
  tool call with a new `progress` message (see the open questions in
  [`protocol.md`](protocol.md#open-questions)), which Unsworn would send on
  an SSE response for that call.
- **Change notifications.** Clients that open a `subscriptions/listen`
  stream would be told when a config reload changes the tool list. Each
  instance reloads the same config, so each can notify its own listeners
  without shared state. Edge and serverless hosts can't hold long streams,
  so this would be for the binary host only.
- **Tasks,** the MCP extension for long-running tool calls that clients
  poll for results.
- **A handler-based tool list filter,** such as
  `[mcp] list_filter = "src/tools.ts#visibleTools"`, which receives the
  tools allowed by scope and returns the subset to show. Only if scope-based
  filtering turns out not to be enough.
