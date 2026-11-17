# Polyglot example

TypeScript and Python handlers in one API, with an existing app behind a
catch-all. Needs `bun` and `python3`.

```sh
cd examples/polyglot
DEMO_KEY=secret unsworn dev
```

```sh
curl localhost:8080/health
curl localhost:8080/anything/else                          # the legacy app
curl localhost:8080/api/users/7                            # 401: /api needs a key
curl -H 'x-api-key: secret' localhost:8080/api/users/7     # 400: the before handler wants a tenant
curl -H 'x-api-key: secret' -H 'x-tenant: acme' localhost:8080/api/users/7
curl -H 'x-api-key: secret' -H 'x-tenant: acme' localhost:8080/api/users/0      # 400: id must be >= 1
curl -H 'x-api-key: secret' -H 'x-tenant: acme' -H 'content-type: application/json' \
     -d '{"text": "Unsworn routes requests to handlers in any language"}' localhost:8080/api/summarize
unsworn routes --policies
```

The users route is TypeScript and asks a Python handler for the user's
initials with `ctx.call("initials", ...)`: a `[[function]]` in the config,
with no service between the two. See
[`docs/functions.md`](../../docs/functions.md).

[`unsworn.plan`](unsworn.plan) is what the config and route files add up
to, for each route. `unsworn plan` writes it, and `unsworn diff` shows what
your changes do to it. See [`docs/plan.md`](../../docs/plan.md).
