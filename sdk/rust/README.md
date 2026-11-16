# unsworn-sdk (Rust SDK)

Write [Unsworn](https://github.com/j4crev/unsworn) handlers in Rust. A
handler is an async function that takes a request and returns a reply:

```rust
// src/users.rs
use unsworn_sdk::{Reply, Request, Result, json};

pub async fn get_user(req: Request) -> Result {
    // The id param is already validated and converted by Unsworn,
    // and req.ctx["auth"] holds the verified token claims.
    let id: u64 = req.param("id")?;
    Ok(Reply::json(json!({ "id": id, "name": "Ada" })))
}
```

The handlers are compiled into one worker program that lists them:

```rust
// src/main.rs
mod users;

fn main() {
    unsworn_sdk::run(unsworn_sdk::handlers![users::get_user, users::list_users]);
}
```

Unsworn builds and runs it:

```toml
# unsworn.toml
[runtimes.rust]
extensions      = [".rs"]
build           = "cargo build --release"
command         = "target/release/handlers"   # your binary's name
protocol        = "framed"
startup_timeout = "300s"                       # time for the first build

[[route]]
path    = "/users/{id}"
method  = "GET"
handler = "src/users.rs#get_user"
params  = { id = { type = "integer", min = 1 } }
```

`unsworn dev` rebuilds the worker when files change.

## How references find handlers

`src/users.rs#get_user` finds the function registered as `get_user`. If
several registered functions share a name, the module path decides:
`users::get_user` matches `src/users.rs` and `src/users/mod.rs`. A typo is
reported by `unsworn check --deep` and `unsworn dev` before any request
reaches it.

To pick the reference yourself, use `add_ref`:

```rust
let handlers = unsworn_sdk::handlers![users::get_user]
    .add_ref("src/legacy.rs#orders", legacy::orders);
```

## Replies

| Return | Response |
|---|---|
| `Reply::json(value)` | JSON body, from anything `Serialize` |
| `Reply::text(s)` | Text body |
| `Reply::bytes(b)` | Raw bytes |
| `Reply::stream(stream)` | Streamed body: each `Ok(chunk)` is sent right away |
| `Reply::status(204)` | No body |
| `Reply::next(json!({ "tenant": "acme" }))` | From a `before` handler: continue, adding context |

Add `.with_status(code)` and `.with_header(name, value)` to any of them.
Handlers return `unsworn_sdk::Result` (`Result<Reply, Error>`), and any error
converts with `?`. An error sends a `500`; the message is logged and shown
to the client only under `unsworn dev`. Panics are caught and treated the
same way.

## MCP tools

A handler listed in `[[mcp.tool]]` is an ordinary handler. Its arguments
are the JSON body, already validated (read them with `req.body_json()`),
and `req.mcp()` returns the tool name, protocol version and client (`None`
outside a tool call). Unsworn turns the reply into an MCP tool result (see
[`docs/mcp.md`](../../docs/mcp.md#how-results-are-mapped)):

```rust
#[derive(serde::Deserialize)]
struct Args {
    city: String,
}

async fn get_forecast(req: Request) -> Result {
    let args: Args = req.body_json()?;
    if !known(&args.city) {
        let message = format!("unknown city {}", args.city);
        return Ok(Reply::tool_error(message)); // the model sees this
    }
    Ok(Reply::json(forecast(&args.city)))
}
```

| Return | Tool result |
|---|---|
| `Reply::json(value)` | Structured content, plus a text copy |
| `Reply::text(s)` | One text block |
| `Reply::content(blocks)`, or `Reply::json(v).with_content(blocks)` | MCP content blocks as given; with `json`, structured content too |
| `Reply::tool_error(message)` | `isError: true` with `message` |

## The request

`req` has `method`, `path`, `route`, `params`, `query`, `raw_query`,
`headers` (lowercase names), `ctx` (verified claims under `"auth"`, plus
anything before handlers added), `json`, `text`, `body`, `handler` and
`phase`.

- `req.param::<T>("id")` and `req.query_value::<T>("limit")` read and convert
  single values.
- `req.params::<T>()`, `req.query::<T>()` and `req.body_json::<T>()`
  deserialize into a struct.
- On routes with `stream_body = true`, `req.body_stream()` returns the body
  as a `Stream` of chunks as it arrives.
- When Unsworn cancels a request (the client went away or the route's
  timeout passed), the handler's future is dropped. `req.cancelled()` and
  `req.is_cancelled()` are there for work it spawned.

Each request runs as its own Tokio task, and `run` starts a single-threaded
Tokio runtime for them, so a worker uses one thread, as a Bun, Node or
Python worker does. Unsworn runs several workers per runtime (`workers`,
one per CPU core up to four by default), and those are what use the machine's cores.

On one thread, a handler that blocks or computes for long holds up the
worker's other requests. Move that work off the thread with
`tokio::task::spawn_blocking`, or set `UNSWORN_WORKER_THREADS` in the
runtime's `env` to give every worker a multi-threaded runtime with that many
threads. More threads cost a small handler about a quarter more CPU,
because every request then crosses threads on its way in and out, though
they answer a little sooner when the server is nearly idle (see the
[benchmarks](../../docs/benchmarks.md#handler-calls)).
Inside a runtime of your own, call `unsworn_sdk::serve(handlers).await`.

## Logging

The worker sends `tracing` events, and `log` records, to Unsworn, tagged with
the request being handled:

```rust
tracing::info!(amount = 1200, "charged card");
```

If the program sets its own global subscriber first, add
`unsworn_sdk::LogLayer` to it.

## Route files

Route file names such as `users.[id].get.rs` aren't valid module names, so
include them with `#[path]` and register them by file:

```rust
#[path = "../routes/users.[id].get.rs"]
mod get_user;

fn main() {
    unsworn_sdk::run(
        unsworn_sdk::Handlers::new().add_ref("routes/users.[id].get.rs", get_user::handler),
    );
}
```

## Production

`unsworn serve` runs `build` too, which would need Rust and the source on the
server. Build the worker ahead of time instead, in CI or a Docker build stage,
ship the binary, and skip the build:

```sh
cargo build --release
unsworn serve --no-build    # or UNSWORN_NO_BUILD=1
```

Set `eager = true` on the runtime so a missing binary or handler stops
startup instead of failing the first request.

## Tiers

The worker reads `UNSWORN_TRANSPORT` and speaks either tier. Tier 1 (stdio)
points file descriptor 1 at stderr, so `println!` output ends up in
Unsworn's logs. Tier 2 (`protocol = "framed"`) leaves stdout alone and is
faster for bodies of about 64 KB and more. Tier 1 is slightly faster for
small requests.

