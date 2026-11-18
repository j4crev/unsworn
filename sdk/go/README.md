# unsworn (Go SDK)

Write [Unsworn](https://github.com/j4crev/unsworn) handlers in Go. A handler
is a function that takes a request and returns a reply:

```go
// users.go
package main

import "github.com/j4crev/unsworn/sdk/go/unsworn"

func GetUser(req *unsworn.Request) (unsworn.Reply, error) {
	// The id param is already validated and converted by Unsworn,
	// and req.Ctx["auth"] holds the verified token claims.
	id, _ := req.ParamInt("id")
	return unsworn.JSON(map[string]any{"id": id, "name": "Ada"}), nil
}
```

Go can't load a function from a file by name at runtime, so the handlers are
compiled into one worker program that lists them:

```go
// main.go
package main

import "github.com/j4crev/unsworn/sdk/go/unsworn"

func main() {
	unsworn.Serve(GetUser, ListUsers)
}
```

Unsworn builds and runs it:

```toml
# unsworn.toml
[runtimes.go]
extensions = [".go"]
build      = "go build -o .unsworn/go-worker ."
command    = ".unsworn/go-worker"
protocol   = "framed"

[[route]]
path    = "/users/{id}"
method  = "GET"
handler = "users.go#GetUser"
params  = { id = { type = "integer", min = 1 } }
```

`unsworn dev` rebuilds the worker when files change.

## How references find handlers

`unsworn.Serve` and `unsworn.Register` record each function's name and the
file it's defined in, so `users.go#GetUser` finds the function `GetUser` in
`users.go`. A typo, or a function registered from another file, is reported
by `unsworn check --deep` and `unsworn dev` before any request reaches it.

A reference with no `#name`, as route files use, finds the only handler
registered from that file, or the one named `Handler`.

For anonymous functions, or to pick the reference yourself, use
`RegisterAs`:

```go
unsworn.RegisterAs("users.go#GetUser", getUser)  // a full reference
unsworn.RegisterAs("Ping", ping)                 // "<any file>#Ping"
```

## Replies

| Return | Response |
|---|---|
| `unsworn.JSON(v)` | JSON body |
| `unsworn.Text(s)` | Text body |
| `unsworn.Bytes(b)` | Raw bytes |
| `unsworn.Stream(func(w io.Writer) error {...})` | Streamed body: each `Write` is sent right away |
| `unsworn.Status(204)` | No body |
| `unsworn.Next(map[string]any{"tenant": "acme"})` | From a `before` handler: continue, adding context |

Add `.WithStatus(code)` and `.WithHeader(name, value)` to any of them:
`unsworn.JSON(err).WithStatus(404)`. Returning an error sends a `500`; the
message is logged and shown to the client only under `unsworn dev`. Panics
are recovered and treated the same way.

## MCP tools

A handler listed in `[[mcp.tool]]` is an ordinary handler. Its arguments
are the JSON body, already validated (read them with `req.Bind`), and
`req.MCP()` returns the tool name, protocol version and client (`nil`
outside a tool call). Unsworn turns the reply into an MCP tool result (see
[`docs/mcp.md`](../../docs/mcp.md#how-results-are-mapped)):

```go
func GetForecast(req *unsworn.Request) (unsworn.Reply, error) {
	var args struct{ City string `json:"city"` }
	if err := req.Bind(&args); err != nil {
		return unsworn.Reply{}, err
	}
	if !known(args.City) {
		return unsworn.ToolError("unknown city " + args.City), nil // the model sees this
	}
	return unsworn.JSON(forecast(args.City)), nil
}
```

| Return | Tool result |
|---|---|
| `unsworn.JSON(v)` | Structured content, plus a text copy |
| `unsworn.Text(s)` | One text block |
| `unsworn.Content(blocks)`, or `unsworn.JSON(v).WithContent(blocks)` | MCP content blocks as given; with `JSON`, structured content too |
| `unsworn.ToolError(message)` | `isError: true` with `message` |

## The request

`req` has `Method`, `Path`, `Route`, `Params`, `Query`, `RawQuery`,
`Headers` (lowercase names), `Ctx` (verified claims under `"auth"`, plus
anything before handlers added), `JSON`, `Text`, `Body`, `Handler` and
`Phase`.

- `req.ParamInt("id")`, `req.Param("slug")`, `req.QueryInt("limit")` and
  `req.QueryValue("q")` read single values. Numbers in `Params` and `Query`
  are `json.Number`.
- `req.Bind(&v)` decodes the JSON body; `req.BindParams(&v)` and
  `req.BindQuery(&v)` decode params and query into a struct.
- `req.Context()` is cancelled when the client goes away or the route's
  timeout passes. Pass it to database and HTTP calls.
- On routes with `stream_body = true`, `req.BodyReader` reads the body as it
  arrives. `HTTPHandler` apps get it as `r.Body`.

Each request runs in its own goroutine.

### Types from the config

`unsworn gen` writes a struct for each part of a route's contract, to
bind into:

```toml
[gen]
go = "unsworn_gen.go"        # or { file = "...", package = "..." }
```

```go
var p GetUserParams          // written by `unsworn gen` from the route
if err := req.BindParams(&p); err != nil {
	return unsworn.Reply{}, err
}
```

`unsworn dev` rewrites the file before it rebuilds the worker. See
[`docs/types.md`](../../docs/types.md).

## Calling other handlers

`req.Call` calls a [function](../../docs/functions.md): a handler declared
as `[[function]]` in the config, in any language.

```go
var vector []float32
err := req.Call("embed", map[string]any{"text": text}, &vector)   // a Python handler
var failed *unsworn.CallError
if errors.As(err, &failed) && failed.Status == 422 {
	return unsworn.JSON(failed.JSON).WithStatus(400), nil
}
if err != nil {
	return unsworn.Reply{}, err
}
```

Unsworn checks the input against the function's schema, and the function
gets this request's context and what is left of its time. `Call` returns
the request's context error if the request ends first.

## Logging

The worker makes `slog`'s default logger send records to Unsworn, unless the
program has already set its own. Pass the request's context to tag a record
with the request:

```go
slog.InfoContext(req.Context(), "charged card", "amount", 1200)
```

`log.Printf` output goes through `slog` too, at the info level. To build your
own logger, use `slog.New(unsworn.LogHandler())`.

## Wrapping an existing app

`unsworn.HTTPHandler` turns any `http.Handler` (`http.ServeMux`, chi, gin,
echo) into a handler, for a catch-all route in front of an existing app:

```go
unsworn.RegisterAs("app.go", unsworn.HTTPHandler(mux))
```

```toml
[[route]]
path    = "/{*rest}"
handler = "app.go"
```

The app sees the original method, path, query, headers and body. Responses
are buffered unless the app calls `Flush`, which starts streaming, as
server-sent events need.

## Route files

Go packages are directories, and Go import paths can't contain `[`, `]`,
`(` or `)`. Use the flat layout for Go route files, with one package for
the whole folder:

```
routes/
  main.go                  # unsworn.Serve(GetUsers, GetUser, ...)
  users.get.go             # GET /users
  users.[id].get.go        # GET /users/{id}
  users._before.go         # before handler for /users and below
```

```toml
[runtimes.go]
extensions = [".go"]
build      = "go build -o .unsworn/go-worker ./routes"
command    = ".unsworn/go-worker"
protocol   = "framed"
```

`main.go` has no HTTP method in its name, so Unsworn doesn't treat it as a
route. The Go toolchain skips files whose names start with `_`, so a root
`_before.go` is never compiled; list Go before handlers with `before = [...]`
in `unsworn.toml` instead.

## Production

`unsworn serve` runs `build` too, which would need Go and the source on the
server. Build the worker ahead of time instead, in CI or a Docker build stage,
ship the binary, and skip the build:

```sh
go build -o .unsworn/go-worker .
unsworn serve --no-build    # or UNSWORN_NO_BUILD=1
```

Set `eager = true` on the runtime so a missing binary or handler stops
startup instead of failing the first request.

## Tiers

The worker reads `UNSWORN_TRANSPORT` and speaks either tier. Tier 1 (stdio)
points `os.Stdout` at stderr, so `fmt.Println` output ends up in Unsworn's
logs. Tier 2 (`protocol = "framed"`) leaves stdout alone. For Go the two
perform about the same, with Tier 1 slightly ahead for small requests.

