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
protocol        = "binary"
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

## The request

`req` has `method`, `path`, `route`, `params`, `query`, `raw_query`,
`headers` (lowercase names), `ctx` (verified claims under `"auth"`, plus
anything before handlers added), `json`, `text`, `body`, `handler` and
`phase`.

- `req.param::<T>("id")` and `req.query_value::<T>("limit")` read and convert
  single values.
- `req.params::<T>()`, `req.query::<T>()` and `req.body_json::<T>()`
  deserialize into a struct.
- When Unsworn cancels a request (the client went away or the route's
  timeout passed), the handler's future is dropped. `req.cancelled()` and
  `req.is_cancelled()` are there for work it spawned.

Each request runs as its own Tokio task. `run` starts a multi-threaded
runtime; inside an existing one, call `unsworn_sdk::serve(handlers).await`.

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

## Tiers

The worker reads `UNSWORN_TRANSPORT` and speaks either tier. Tier 1 (stdio)
points file descriptor 1 at stderr, so `println!` output ends up in
Unsworn's logs. Tier 2 (`protocol = "binary"`) is recommended.

## Status

Not published to crates.io yet. Until it is, depend on the repository:

```toml
[dependencies]
unsworn-sdk = { git = "https://github.com/j4crev/unsworn" }
```
