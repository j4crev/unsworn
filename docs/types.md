# Typed handlers: `unsworn gen`

A route's `params`, `query`, `body` and `response` are checked by Unsworn
before its handler runs. So the handler can rely on `req.params.id` being
an integer, but only the config says so. `unsworn gen` writes that down as
types in the handler's own language:

```toml
[[route]]
path    = "/users/{id}"
method  = "GET"
handler = "src/users.ts#getUser"
params  = { id = { type = "integer", min = 1 } }
query   = { limit = { type = "integer", default = 20 }, q = { type = "string" } }
```

```ts
// src/unsworn.gen.ts, written by `unsworn gen`
/** GET /users/{id} */
export type GetUser = Handler<{
  params: {
    id: number;
  };
  query: {
    limit: number;
    q?: string;
  } & Record<string, unknown>;
}>;
```

```ts
// src/users.ts
import type { GetUser } from "./unsworn.gen";

export const getUser: GetUser = async (req) => {
  return { json: await db.users.find(req.params.id, req.query.limit) };
};
```

Change `id` to a string in the config and this file stops compiling,
before a request finds out.

## Setting it up

List a file for each language you write handlers in:

```toml
[gen]
ts     = "src/unsworn.gen.ts"
python = "unsworn_gen.py"
go     = { file = "routes/unsworn_gen.go", package = "main" }   # or just the path, for package main
rust   = "src/unsworn_gen.rs"
```

- **`unsworn dev` keeps them current.** It writes them at startup and on
  every reload, before it rebuilds compiled workers, so a Go or Rust
  worker is built against its new types.
- **`unsworn gen` writes them** outside dev, and `unsworn gen --check`
  fails when one is out of date. Commit the files and run `--check` in CI,
  or leave them out of git and run `unsworn gen` before you build.
- **`unsworn gen <lang>`** prints one language's types without a `[gen]`
  table. `-o <file>` writes them, and `--package` names the Go package.

Each file covers the handlers written in its language, picked by file
extension. Neither command reads the environment, so no variable has to be
set.

## What each language gets

| | For each handler | Use it as |
|---|---|---|
| TypeScript | `GetUser`, a `Handler` type, and `GetUserResponse` | `export const getUser: GetUser = ...`, or `satisfies GetUser` |
| Python | `GetUserParams`, `GetUserQuery`, `GetUserBody` and `GetUserResponse` as `TypedDict`s, and `GetUserRequest` | `def get_user(req: GetUserRequest, ctx):` |
| Go | `GetUserParams`, `GetUserQuery`, `GetUserBody`, `GetUserResponse` structs | `req.BindParams(&p)`, `req.BindQuery(&q)`, `req.Bind(&body)` |
| Rust | the same four, as `serde` structs | `req.params()?`, `req.query()?`, `req.body_json()?` |

A type is only written for what the route declares: a route with no
`query` has no `GetUserQuery`.

```python
# src/users.py
from unsworn import json
from unsworn_gen import GetUserRequest, GetUserResponse

def get_user(req: GetUserRequest, ctx):
    user: GetUserResponse = {"id": req.params["id"], "name": "Ada"}
    return json(user)
```

```go
// routes/users.go
func GetUser(req *unsworn.Request) (unsworn.Reply, error) {
	var p GetUserParams
	if err := req.BindParams(&p); err != nil {
		return unsworn.Reply{}, err
	}
	return unsworn.JSON(GetUserResponse{ID: p.ID, Name: "Ada"}), nil
}
```

```rust
// src/users.rs
use crate::unsworn_gen::{GetUserParams, GetUserResponse};

pub async fn get_user(req: Request) -> Result {
    let p: GetUserParams = req.params()?;
    Ok(Reply::json(GetUserResponse { id: p.id, name: "Ada".into(), email: None }))
}
```

The Rust file needs `serde` (with its `derive` feature) and `serde_json`
in your `Cargo.toml`, and `mod unsworn_gen;` in your `main.rs`. The Python
types need SDK 0.4.0 or newer, where `Request` takes type arguments.

### Names

A handler's types are named after it: `src/users.ts#getUser` and
`src/users.py#get_user` both give `GetUser`. A reference without a name,
as route files have, uses the file's: `routes/api.users.[id].get.ts` gives
`ApiUsersIdGet`. Two handlers that would share a name get their file's
name in front (`UsersList`, `OrdersList`).

A handler that serves routes with different contracts gets each of them:
in TypeScript as one `Handler` type that accepts any of them, and in the
other languages as a set of types per route, named for it
(`ItemGETItemsIdParams`, `ItemPUTItemsIdBody`).

An MCP tool's handler gets its `input` schema as the body and its `output`
schema as the response. So does a [function](functions.md)'s.

### Functions

Every language's file also has each `[[function]]`'s input and output,
since any handler may call it. In TypeScript they type `ctx.call` itself:

```ts
const vector = await ctx.call("embed", { text: "hello" });   // number[]
await ctx.call("embed", { text: 5 });    // error: number isn't a string
await ctx.call("embd", {});              // error: "embd" isn't a function
```

In Python, Go and Rust they are `EmbedInput` and `EmbedOutput`, named
after the function.

The TypeScript file also lists each [outbound service](outbound.md), so
`ctx.fetch("strpe", ...)` is a compile error.

## How schemas become types

| JSON Schema | TypeScript | Python | Go | Rust |
|---|---|---|---|---|
| `string` | `string` | `str` | `string` | `String` |
| `integer` | `number` | `int` | `int64` | `i64` |
| `number` | `number` | `float` | `float64` | `f64` |
| `boolean` | `boolean` | `bool` | `bool` | `bool` |
| `array` with `items` | `T[]` | `list[T]` | `[]T` | `Vec<T>` |
| `object` with `properties` | an object type | a `TypedDict` | a struct | a struct |
| `object` with `additionalProperties` only | `Record<string, T>` | `dict[str, T]` | `map[string]T` | `BTreeMap<String, T>` |
| `enum`, `const` | the values | `Literal[...]` | the values' type | the values' type |
| `["string", "null"]`, `nullable` | `string \| null` | `str \| None` | `*string` | `Option<String>` |
| `oneOf`, `anyOf` | a union | a union | `any` | `serde_json::Value` |
| `allOf` of objects | one object | one `TypedDict` | one struct | one struct |
| a property not in `required` | `name?:` | an optional key | a pointer, `omitempty` | `Option<T>` |
| a local `$ref` | what it points to | the same | the same | the same |
| anything else | `unknown` | `Any` | `any` | `serde_json::Value` |

- **A query parameter with a `default` is always there,** so it isn't
  optional in the types.
- **A path parameter without a rule is a string,** as it is in the
  request.
- **Query parameters the route doesn't declare still arrive,** as strings.
  The TypeScript type allows reading them (as `unknown`); the others
  describe the declared ones.

## Limits

- **`before` handlers aren't typed.** One runs for many routes.
- **Formats and ranges aren't types.** `min = 1` is checked by Unsworn,
  and the type is still a number.
- **A schema that refers to itself** becomes "anything" where it recurses.
- **In Python, an object with a key that isn't a name** (`x-labels`) is
  written in `TypedDict`'s other form, which can't mark single keys as
  optional. If any key is optional, all of them are.
- **Response types aren't enforced on what a handler returns,** since a
  handler may also answer with an error of another shape. Use the type
  for the value you build; `response_validation` checks what is sent.
