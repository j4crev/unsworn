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
