# The plan: your rules, worked out

A route's rules come from several places: `[defaults]`, named policy sets,
`_policies.toml` files, prefix entries, an OpenAPI spec and the route
itself. That keeps a config short, and it means no single line says what a
request to `/api/users/7` has to pass.

`unsworn plan` works that out for every route and MCP tool and writes it to
`unsworn.plan`. `unsworn diff` compares two versions of a project and says
what changed in the rules, and which changes let more requests through.

```
$ unsworn diff origin/main
~ * /admin/{*rest}
  ! auth               jwt → none  (looser)
~ POST /api/uploads
    max_body           10MB → 2MB  (tighter)
+ GET /api/reports
    handler            src/reports.py#list (py)
    auth               jwt

2 changes (1 looser, 1 tighter), 1 added
```

## The plan file

```sh
unsworn plan            # writes unsworn.plan next to the config
```

Commit `unsworn.plan` with the config. A pull request that changes the
rules then shows the change in its own diff, whatever it edited to cause
it: a `[defaults]` key, a folder's `_policies.toml`, a schema file.

This is [`examples/polyglot`](../examples/polyglot)'s, without its
settings:

```
POST /api/summarize
  handler            routes/api.summarize.post.py (py)
  auth               key
  rate_limit         60 per 1m by ip
  before             src/tenant.ts#tenant
  body               {
                       "properties": {
                         "text": {
                           "maxLength": 10000,
                           "type": "string"
                         }
                       },
                       "required": [
                         "text"
                       ],
                       "type": "object"
                     }

GET /api/users/{id}
  handler            routes/api.users.[id].get.ts (ts)
  auth               key
  rate_limit         60 per 1m by ip
  cache              30s; vary x-api-key
  before             src/tenant.ts#tenant
  params.id          {"minimum":1,"type":"integer"}

* /health
  respond            200 json {"ok":true}
  auth               none

* /{*rest}
  handler            src/legacy.ts (ts)
  auth               none
  at /api/{*rest}
    auth               key
    rate_limit         60 per 1m by ip
    before             src/tenant.ts#tenant
```

Neither route file mentions a key, a rate limit or a tenant check. Those
come from one `/api/{*rest}` entry in the config, and the plan puts them
on each route they reach.

What's in it:

- **`server`, `store`, `auth <name>` and `runtime <name>`:** the settings
  that decide who gets in and what runs: body and time limits, where each
  scheme's keys come from, each runtime's command.
- **One block per route and method,** with what the route does, every
  policy that applies to it and every check on its input. Methods of one
  route with the same rules share a block (`GET, DELETE /users/{id}`), and
  `*` is a route for every method. Routes are in path order, so moving one
  in the config changes nothing.
- **`at <path>` inside a route,** for paths where a narrower entry changes
  the rules, as `/api/{*rest}` does inside the catch-all above. It lists
  only what differs from the route's own rules there, and `off` for a
  policy that's switched off.
- **`tool <name>`** for each `[[mcp.tool]]`, with its description, its
  policies and its input schema. A route offered as a tool has an
  `mcp_tool` line.

A route's `timeout` and `max_body` are written only when they differ from
the server's.

What isn't:

- **Secrets.** A JWT secret is `a shared secret`, API keys are
  `listed in the config`, and a runtime's `env` is names only.
- **The environment.** `{env.NAME}` is written as it is in the config, so
  the plan is the same on a laptop and in CI, and neither `plan` nor
  `diff` needs any variable set.
- **Where a rule came from.** `unsworn routes --policies` shows which
  layer set each key.

`unsworn plan -o <file>` writes somewhere else, and `-o -` prints it.

### Keeping it current

```sh
unsworn plan --check    # fails when unsworn.plan is missing or out of date
```

Run that in CI. Without it, a plan nobody regenerated says the rules
didn't change when they did.

## `unsworn diff`

```sh
unsworn diff                 # the working tree against the last commit
unsworn diff origin/main     # against a branch, tag or commit
unsworn diff ../other-copy   # against another directory
```

`diff` compiles both versions of the project and compares what they
enforce. It reads the old one from git without checking it out, and it
doesn't use `unsworn.plan`, so it works in a project that doesn't commit
one. A change that leaves the rules alone, such as moving a policy from
ten routes into a named set, comes out as `The rules are the same.`

Each changed route, tool or setting is listed with the lines that
changed. A `!` marks a change that lets more requests through:

| Change | Looser when |
|---|---|
| `auth` | it's removed, a scheme is added as another way in, or fewer scopes are asked for |
| `rate_limit` | it's removed, or allows more requests in the same time |
| `origin`, `ip` | removed; for `origin`, more origins allowed |
| `cors` | added, more origins allowed, or credentials allowed |
| `before` | a handler is removed |
| `max_body` | raised |
| `params`, `query`, `headers`, `body`, a tool's `input` | the check is removed |

The opposite is marked `(tighter)`. Anything else is listed without a
mark: a different schema, a different scheme, a handler that moved to
another file. Unsworn doesn't try to decide whether one JSON Schema
accepts more than another, so read those.

An added route is printed whole, since everything about it is new. A
change inside a catch-all is listed under the path it applies to:

```
~ * /{*rest}
    at /assets/{*rest}
  !   auth               jwt → none  (looser)
```

### In CI

```yaml
- uses: actions/checkout@v4
  with:
    fetch-depth: 0             # diff needs the base branch's commits
- run: unsworn plan --check
- run: unsworn diff "origin/${{ github.base_ref }}"
```

`--fail-on looser` makes `diff` exit with an error when any change is
marked looser, and `--fail-on any` when the rules changed at all. Use one
on a job a second reviewer has to approve, or on branches where the rules
aren't meant to move.

The base version is compiled by the Unsworn you're running. If it used
config an older Unsworn accepted and this one doesn't, `diff` reports the
errors and stops.

## Limits

- **The marks are a guide, not a proof.** They cover the cases in the
  table. Everything that changed is listed either way.
- **A plan describes requests by path as written.** An app behind a
  catch-all may read `/Admin/x` as `/admin/x`; see
  [locking down part of a catch-all app](config.md#locking-down-part-of-a-catch-all-app).
- **Entries that overlap without one containing the other,** such as
  `/a/{x}/c/{*rest}` and `/a/b/{*rest}`, are each listed, but the paths
  both match aren't listed separately.
- **`diff` reads the base from git or a directory,** not from a plan file,
  so it can't compare against a plan whose project you don't have.
