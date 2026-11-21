# HTTP runtimes: a server in any language, with no SDK

A handler is a function Unsworn calls through one of its SDKs. An **HTTP
runtime** is the other way to put code behind Unsworn's rules: an ordinary
HTTP server, in any language and any framework, that Unsworn starts,
supervises and forwards requests to.

```toml
[runtimes.api]
command  = "dotnet Api.dll --urls http://unix:$UNSWORN_SOCKET"
protocol = "http"

[[route]]
path       = "/invoices/{*rest}"
auth       = { scheme = "users", scopes = ["invoices:read"] }
rate_limit = { requests = 100, per = "1m", key = "auth.sub" }
proxy      = { runtime = "api" }
```

Every language used for APIs has a production HTTP server, so every one
of them works this way today: C#, Java, Kotlin, PHP, Ruby, Elixir, Swift,
and the four with SDKs too. There is nothing of Unsworn's to install in
the app.

[`examples/http-runtime`](../examples/http-runtime) is a small one to run.

## What Unsworn does

- **Starts the server** with the runtime's `command`, on the runtime's
  first request, or with Unsworn when the runtime has `eager = true`.
  `build`, `env`, `startup_timeout` and `workers` work as for
  [any runtime](config.md#runtimesname).
- **Waits for it to listen,** up to `startup_timeout` (30 seconds by
  default). A server that exits first, or never listens, is a `503` for
  its routes and an error in the log that says which.
- **Checks each request first.** A request reaches the server after its
  route's `auth`, `require`, rate limit, validation and `before` handlers
  have passed. One that fails any of them never does.
- **Forwards it as HTTP,** as a [`proxy`](config.md#actions) route does:
  the path and query as sent, the forwarding headers, `X-Request-Id`, and
  streamed bodies in both directions. WebSocket upgrades pass through.
- **Says who the caller is** in
  [`Unsworn-Context`](config.md#who-the-caller-is-unsworn-context): the
  verified claims and what `before` handlers added.
- **Restarts it** when it exits, waiting longer each time it happens
  again, and answers `502` for the requests it had.
- **Collects what it prints.** Its stdout and stderr go to Unsworn's log,
  tagged with the runtime, and a line of JSON with `level` and `message`
  is kept as a structured record.
- **Stops it** with `SIGTERM` when Unsworn shuts down or reloads, then
  `SIGKILL` if it's still there two seconds later. It also gets `SIGTERM`
  if Unsworn itself is killed.
- **Keeps Unsworn's credentials from it,** as from any worker: the
  variables the config reads a signing secret or an API key from aren't
  in its environment.

## Where the server listens

The server listens and Unsworn connects. Unsworn says where in the
server's environment:

| `listen` | The server gets | Who can reach it |
|---|---|---|
| `"socket"` (the default) | `UNSWORN_SOCKET`: the path of a Unix socket to listen on | Unsworn only: the socket is in a directory only its user can open |
| `"port"` | `PORT` (and `UNSWORN_PORT`): a free port on this machine, to listen on at `127.0.0.1` | anything on the machine |

Use the socket when the server can. Then nothing gets to it without
passing Unsworn's rules, and `Unsworn-Context` can be taken at its word.
On a port, another process on the machine can call the server directly,
with any header it likes; that's for a server that can't listen on a
socket, on a machine where that's acceptable.

The command runs through `sh -c`, so it can use the variables:

| Server | Command |
|---|---|
| Node | `node server.js`, with `server.listen(process.env.UNSWORN_SOCKET)` |
| Python, uvicorn | `uvicorn app:app --uds $UNSWORN_SOCKET` |
| Python, gunicorn | `gunicorn -b unix:$UNSWORN_SOCKET app:app` |
| Ruby, Puma | `bundle exec puma -b unix://$UNSWORN_SOCKET` |
| ASP.NET Core | `dotnet Api.dll --urls http://unix:$UNSWORN_SOCKET` |
| Go | your binary, with `net.Listen("unix", os.Getenv("UNSWORN_SOCKET"))` |
| Spring Boot | `java -jar api.jar --server.port=$PORT --server.address=127.0.0.1`, with `listen = "port"` |
| Phoenix | `mix phx.server` or the release's `start`, which read `PORT`, with `listen = "port"` |

These are the usual flags; check your server's documentation for its own.
PHP needs an HTTP server in front of it (FrankenPHP, RoadRunner, or
`php -S 127.0.0.1:$PORT` while developing), since PHP-FPM speaks FastCGI.

## Routes

A route reaches the runtime with `proxy = { runtime = "<name>" }`, and
takes what any `proxy` route does: `strip_prefix` and `preserve_host`.
The server does its own routing below that, so a catch-all for a path
prefix is the usual shape:

```toml
[[route]]
path  = "/api/{*rest}"
auth  = "users"
proxy = { runtime = "api" }

[[route]]                          # rules for part of it
path    = "/api/orgs/{org}/{*rest}"
require = { "params.org" = "auth.org" }
```

Entries for a path or a prefix add policies to the paths the server
serves, as they do for [any catch-all](config.md#policies-for-routes-from-any-source).
For validation, give the paths you want checked routes of their own, or
load the server's OpenAPI document as a [route source](config.md#3-openapi-spec)
and point its operations at the runtime with action-less entries.

## How many

An HTTP runtime runs **one** server unless `workers` says otherwise,
since a server takes many requests at once on its own:

```toml
workers = 4                    # four copies, each on a socket of its own
workers = { min = 1, max = 4 } # more when the ones running are busy
```

Requests go to the copy with the fewest in hand.

## Calling functions and outbound services

A handler calls a [function](functions.md) with `ctx.call` and an
[outbound service](outbound.md) with `ctx.fetch`. A server does the same
with plain HTTP requests to a local endpoint Unsworn runs for it, so
nothing of Unsworn's has to be installed in the app.

In a project with a `[[function]]` or an `[outbound.<name>]`, the server
gets two more variables. Both lead to the same endpoint:

| Variable | What it is |
|---|---|
| `UNSWORN_CALLS_SOCKET` | a Unix socket, in the directory only Unsworn's user can open |
| `UNSWORN_CALLS_URL` | `http://127.0.0.1:<port>`, for an HTTP client that can't use a socket |

Each request Unsworn forwards to the server comes with a token in the
`Unsworn-Call` header. Send it back with each call. It tells Unsworn which
request the call is for, and so what the function gets:

| Request | What it does |
|---|---|
| `POST /call/<function>`, with the input as the JSON body | Calls the function. The answer is its status and JSON. |
| `<any method> /fetch/<service>/<path>?<query>` | Sends the request to the service's `url` with that path and query, adding the service's `headers`. The answer is the service's own: status, headers and body. |

```js
// Node, Express: the embedding comes from a Python function.
app.get("/search", async (req, res) => {
  const r = await fetch(`${process.env.UNSWORN_CALLS_URL}/call/embed`, {
    method: "POST",
    headers: { "unsworn-call": req.get("unsworn-call"), "content-type": "application/json" },
    body: JSON.stringify({ text: req.query.q }),
  });
  if (!r.ok) return res.status(502).json(await r.json());
  res.json(await index.nearest(await r.json()));
});
```

```python
# Python, FastAPI, on the socket.
calls = httpx.AsyncClient(transport=httpx.AsyncHTTPTransport(uds=os.environ["UNSWORN_CALLS_SOCKET"]))

@app.post("/charge")
async def charge(request: Request, amount: int):
    token = {"unsworn-call": request.headers["unsworn-call"]}
    r = await calls.post("http://unsworn/fetch/stripe/v1/charges", json={"amount": amount}, headers=token)
    return r.json()
```

Most apps put this in a short helper that takes the incoming request, so
the token and the address are written once.

Calls and fetches work as they do from a handler. A function gets the
request's verified context, and its `ctx.caller.from` is
`runtime:<name>`. It has no more time than the route's `timeout` leaves.
The request's `traceparent` goes along. The function's `input` schema and
the service's allowed `methods` and `paths` are checked. The statuses are
the ones in [`functions.md`](functions.md#calling) and
[`outbound.md`](outbound.md#fetching), with these differences:

- **Unsworn's own refusals carry an `Unsworn-Error` header** with the
  reason, and a problem document (`application/problem+json`) as the body.
  A function's or a service's own error status has no such header, so a
  `404` from Stripe and "no `[outbound.stripe]` in the config" can be
  told apart.
- **A missing token is a `401`, and an unknown or expired one a `409`.**
- **A body to a service is streamed** as the server sends it. The answer
  is read whole, up to `max_response`, as it is for a handler.

The token works until the response to its request has been sent,
including a streamed one, and then stops: calls still running for the
request are cancelled, with a `409`. A WebSocket's request gets a token,
but it ends when the connection is upgraded, so calls can't be made
during a WebSocket. A new request gets a new token.

Anything on the machine can reach the port, so the token is what lets a
call in. It's 128 random bits and lasts only as long as its request, but
it's in the request's headers: an app that logs all its headers, or passes
them on to another service, gives it away for that time. Leave
`Unsworn-Call` out of what's logged or forwarded.

## What it doesn't have

Compared with a handler in an SDK language:

- **No handler references.** The server has its own router, so a path
  is written in the config and again in the app. `unsworn check --deep`
  starts the server and sees that it listens, but can't tell whether it
  serves a given path.
- **No typed request.** Validated parameters arrive as the text in the
  URL, and `unsworn gen` writes nothing for it.
- **No `ctx` object.** A server calls functions and outbound services
  with its own HTTP client ([above](#calling-functions-and-outbound-services)),
  and nothing checks the names it uses until a request is made. `unsworn
  gen` writes no types for them.
- **Servers only.** Edge and serverless targets can't start a process, so
  `unsworn check --target` reports an HTTP runtime there. Deploy the
  server on its own and `proxy` to its URL, or use the server target.
- **In a container,** `unsworn build --target image` installs the tools
  it knows (Bun, Node, Python, Go, Rust). For another one, add its
  install to the Dockerfile it writes.

The SDK languages keep everything they had; this is one more protocol
beside [`jsonl` and `framed`](protocol.md), not a replacement.
