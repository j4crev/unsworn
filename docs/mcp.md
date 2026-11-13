# MCP servers with Unsworn

> Level 1 and the shared building blocks are implemented in v1. Levels 2
> and 3 are planned.

A remote [Model Context Protocol](https://modelcontextprotocol.io) (MCP)
server lets AI agents call tools over HTTP. Running one in production needs
most of what Unsworn already does: token checks, rate limits, input
validation, streaming responses and logs. Unsworn supports MCP at three
levels:

| Level | What you get | Planned |
|---|---|---|
| [1. In front of an MCP server](#level-1-in-front-of-an-existing-mcp-server) | OAuth checks, rate limits, origin checks and logs for an MCP server built with any MCP SDK. Per-tool policies for it follow in v2 | v1 |
| [2. MCP tools as handlers](#level-2-mcp-tools-as-handlers) | Unsworn speaks MCP itself. Each tool is a plain function in any language, with its own policies | v2 |
| [3. Routes as tools](#level-3-routes-as-tools) | Any REST route, or OpenAPI operation, can also be an MCP tool, from the same handler | v3 |

Unsworn only covers **remote** MCP servers (the Streamable HTTP transport).
Local servers that a client starts and talks to over stdio don't benefit.

## What a remote MCP server needs

- **One HTTP endpoint,** such as `/mcp`, that accepts JSON-RPC messages over
  `POST` and can answer with a stream of server-sent events (SSE).
- **OAuth-based authorization.** The MCP server acts as an OAuth *resource
  server*: it publishes metadata at `/.well-known/oauth-protected-resource`,
  answers missing or invalid tokens with `401` and a `WWW-Authenticate`
  header pointing at that metadata, and only accepts tokens issued for itself
  (the audience check).
- **An `Origin` header check,** to block DNS-rebinding attacks from web
  pages.
- **Sessions,** when the server uses them, tracked through the
  `Mcp-Session-Id` header.
- In practice: rate limits, timeouts, audit logs and metrics.

Unsworn is never the OAuth *authorization server*. Tokens come from an
identity provider such as Auth0, Clerk, WorkOS, Keycloak or your own.
Unsworn verifies them.

## Shared building blocks (v1)

These are general Unsworn features that MCP relies on. They work for any
route, at every level.

### OAuth protected-resource support

An auth scheme can describe the resource it protects:

```toml
[auth.oauth]
type     = "jwt"
issuer   = "https://auth.example.com/"
jwks_url = "https://auth.example.com/.well-known/jwks.json"
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

When a proxy has several upstreams, requests in the same MCP session must
reach the same one:

```toml
proxy = { upstreams = ["http://10.0.0.5:4000", "http://10.0.0.6:4000"], sticky = "header:mcp-session-id" }
```

## Level 1: in front of an existing MCP server

Planned for **v1**. An MCP server built with any MCP SDK runs as it is, and
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

In v1, Unsworn sees every MCP request as `POST /mcp`. It can't tell tools
apart, so policies apply to the whole server.

### Per-tool policies for a proxied server (v2)

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

- Unsworn reads the method and tool name from each JSON-RPC request before
  forwarding it. A `tools/call` for `create_invoice` gets that tool's
  policies on top of the `[mcp]` ones. Other tools get the `[mcp]` ones.
- Responses, including the tool list, pass through unchanged. The
  [tool list visibility](#tool-list-visibility) rules below apply only to
  tools Unsworn hosts itself, because filtering a proxied server's list would
  mean rewriting its responses.
- `input` can be set too, to validate a proxied tool's arguments before they
  reach the server.

## Level 2: MCP tools as handlers

Planned for **v2**. Unsworn implements the MCP protocol itself, and each tool
is a handler:

```toml
[mcp]
path   = "/mcp"
name   = "acme-tools"
auth   = { scheme = "oauth", scopes = ["tools:read"] }
origin = { allow = ["https://claude.ai"] }

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

### What Unsworn handles

- The protocol: initialization and version negotiation, capabilities,
  `tools/list`, `tools/call`, `ping`, and cancellation notifications.
- The transport: JSON-RPC over `POST`, SSE streams, and the
  `Mcp-Session-Id` header.
- **Input validation.** Arguments are checked against the tool's `input`
  schema before the handler runs. Invalid arguments come back to the agent as
  a tool error with the validation details, so the model can correct them and
  retry.
- **Per-tool policies.** Each `[[mcp.tool]]` takes the usual policy keys
  (`auth`, `rate_limit`, `timeout`, `before`, `use`), on top of the `[mcp]`
  ones. `cache` isn't allowed on tools.
- **Output validation.** If a tool declares `output`, results are checked
  against it, following the route's response-validation setting.
- **Change notifications.** When a config reload changes the tool list,
  connected clients are told the list has changed.

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

Filtering only uses scopes, which Unsworn can check without running any
code. **`before` handlers don't run for `tools/list`.** They run on each
tool call, where they can still reject the call, for example for a tenant
whose plan doesn't include that tool. Running them for the list would add a
worker call to every `tools/list` request, which clients send often. A
handler-based list filter is listed under [Later](#later) in case the need
shows up.

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
    "mcp": {"tool": "get_forecast", "session": "s-4f2a", "client": {"name": "claude-ai", "version": "0.1.0"}}
  },
  "deadline_ms": 19950
}
```

In an SDK, that's an ordinary function:

```python
def get_forecast(args, ctx):
    return {"city": args["city"], "days": forecast(args["city"], args["days"])}
```

### How results are mapped

| Handler returns | The agent receives |
|---|---|
| `2xx` with `json` | `structuredContent` with that value, plus a text copy for older clients |
| `2xx` with `text` | One text content block |
| `2xx` with `mcp_content` | Those MCP content blocks as they are (images, audio, embedded resources) |
| `4xx` or `5xx`, or an `error` message | A tool result with `isError: true` and the message, so the model can react |
| Timeout or crashed worker | A tool error saying the tool failed, without internal details |

`mcp_content` is a response field only used for MCP tools. See
[`protocol.md`](protocol.md#mcp-tool-calls).

### Sessions

Tools-only servers don't need sessions, so by default Unsworn runs the MCP
endpoint **without sessions** and needs no shared state. That also makes it
work on edge and serverless targets. Features that need sessions (see
[Later](#later)) will use the `[store]`, the same as rate limits.

### MCP versions

The MCP spec changes often. Each Unsworn release supports the newest MCP
version available when it ships, plus older versions it can still
negotiate with clients. New MCP features therefore arrive with Unsworn
releases. Level 1 isn't affected, since the MCP server behind Unsworn
handles the protocol itself.

## Level 3: routes as tools

Planned for **v3**. Any route with a handler can also be offered as an MCP
tool:

```toml
[mcp]
path = "/mcp"
auth = { scheme = "oauth" }

[[route]]
path    = "/users/{id}"
method  = "GET"
params  = { id = { type = "integer", min = 1 } }
auth    = "oauth"
handler = "src/users.ts#getUser"
mcp     = { tool = "get_user", description = "Look up a user by id" }
```

- **The tool's input schema is built from the route's contract:** path
  params, query params and the body schema become one arguments object.
- **The handler doesn't change.** Unsworn maps the tool's arguments back to
  `params`, `query` and `json`, so the handler receives the same request
  either way and can't tell whether a person or an agent called it.
- **The route's policies apply to the tool too.** One auth rule, one rate
  limit and one validation schema cover both ways in.
- Routes that stream responses or take raw binary bodies can't be tools, and
  `unsworn check` says so.

With an [OpenAPI source](config.md#3-openapi-spec), operations can become
tools in bulk. Tool names come from `operationId` and descriptions from the
operation's summary and description:

```toml
[openapi]
spec      = "openapi.yaml"
mcp_tools = { tags = ["public"] }      # or "all"
```

## Later

Not planned for a specific version yet:

- **Resources and prompts.** Read-only data and prompt templates the client
  can list and fetch.
- **Requests from the server to the client mid-call,** such as asking the
  client's model for a completion (sampling) or asking the user a question
  (elicitation). A handler would need a way to send such a request through
  Unsworn and wait for the answer, which needs a new protocol message and
  sessions.
- **Progress notifications.** A handler would report progress on a long
  tool call with a new `progress` message (see the open questions in
  [`protocol.md`](protocol.md#open-questions)).
- **A handler-based tool list filter,** such as
  `[mcp] list_filter = "src/tools.ts#visibleTools"`, which receives the
  tools allowed by scope and returns the subset to show. Only if scope-based
  filtering turns out not to be enough.
