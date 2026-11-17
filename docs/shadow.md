# Trying a rewrite on real requests: `shadow`

Replacing `search.py` with `search.rs` is meant to change nothing but the
speed. `shadow` checks that before the new handler answers anyone:

```toml
[[route]]
path    = "/search"
method  = "GET"
handler = "src/search.py#search"
shadow  = "src/search.rs#search"
```

Every request to `/search` still goes to the Python handler, and the
client gets its answer. A copy goes to the Rust handler too, and Unsworn
compares what the two answered.

```
$ unsworn top --once
...
SHADOW of GET /search: src/search.rs#search (rs)
  12480 compared, 99.9% the same
  time in handler      24ms  shadow   0.61ms
  CPU a request        23ms  shadow   0.58ms
  last difference: $.hits[3].score is 0.5, and 0.25 in the shadow's answer
```

When it reads `100.0% the same` for long enough, swap the two lines:
`handler` becomes the Rust one, and `shadow` goes (or becomes the Python
one for a while, to be sure).

## What the shadow gets

The same request the route's handler gets: the same params, query,
headers, body and context, after the same policies, validation and
`before` handlers, which run once. It runs in its own runtime's workers,
beside the route's handler, so it doesn't delay the response. If the
shadow is slow, fails or times out, the client never knows.

A request the route's handler doesn't see isn't copied either: one that
was rejected, or answered from the cache.

## What "the same" means

- **The status** has to be equal.
- **Bodies that are both JSON** are compared as values, so two languages
  writing one answer differently is still the same answer: another order
  of keys, other spacing, `1.0` for `1`.
- **Any other bodies** have to be equal byte for byte.
- **A streamed body** isn't compared, only its status.

Headers aren't compared.

Each difference is logged with the request's id and where the answers
first differ (the first ten for a route, then every hundredth):

```
WARN shadow: $.hits[3].score is 0.5, and 0.25 in the shadow's answer route=/search shadow=src/search.rs#search id=1f3a-2c
```

## Routes that write

The shadow runs for every request the route's handler runs for. If the
handler charges a card, sends an email or inserts a row, a shadow that
does the same does it a second time. `unsworn check` warns about `shadow`
on a route that takes anything but `GET` and `HEAD`.

Shadow routes that only read. For one that writes, give the shadow
handler a way to know it's the shadow and skip the write, or point it at
a scratch database.

## Options

```toml
shadow = "src/search.rs#search"                                # every request
shadow = { handler = "src/search.rs#search", sample = 0.1 }    # one in ten
shadow = { handler = "bin/search#run", runtime = "native" }    # when the extension doesn't name the runtime
```

`shadow` goes on a route with a handler. For a route that comes from a
route file or an OpenAPI spec, put it on a config entry for the same path
and method, as with policies:

```toml
[[route]]
path   = "/api/users/{id}"
method = "GET"
shadow = "src/users.rs#get"
```

`unsworn plan` lists a route's shadow, so adding or removing one shows up
in review.

## Limits

- **Servers and containers only.** Edge and serverless targets ignore
  `shadow`.
- **Not with `stream_body`:** a streamed request body can only be read
  once.
- **MCP tools aren't shadowed.**
- **The counts are since the server started,** and aren't shared between
  instances. Each one compares the requests it serves.
- **A shadow costs what it costs.** With `sample = 1`, the shadow's
  runtime does the route's whole traffic again.
