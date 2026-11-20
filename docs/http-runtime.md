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

## What it doesn't have

Compared with a handler in an SDK language:

- **No handler references.** The server has its own router, so a path
  is written in the config and again in the app. `unsworn check --deep`
  starts the server and sees that it listens, but can't tell whether it
  serves a given path.
- **No typed request.** Validated parameters arrive as the text in the
  URL, and `unsworn gen` writes nothing for it.
- **No `ctx.call` or `ctx.fetch` yet.** Those go over a handler worker's
  connection to Unsworn, which a server doesn't have.
- **Servers only.** Edge and serverless targets can't start a process, so
  `unsworn check --target` reports an HTTP runtime there. Deploy the
  server on its own and `proxy` to its URL, or use the server target.
- **In a container,** `unsworn build --target image` installs the tools
  it knows (Bun, Node, Deno, Python, Go, Rust). For another one, add its
  install to the Dockerfile it writes.

The SDK languages keep everything they had; this is one more protocol
beside [`jsonl` and `framed`](protocol.md), not a replacement.
