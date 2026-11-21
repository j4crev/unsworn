# Commands

```sh
unsworn init        # writes a starter unsworn.toml
unsworn dev         # serves it, reloads on changes, verifies handler names
```

| Command | What it does |
|---|---|
| `unsworn init` | Write a starter config |
| `unsworn serve` | Run the router (`--listen`, `--log-format json`, `--no-build` for prebuilt workers); `SIGHUP` reloads |
| `unsworn dev` | Serve with file watching, every runtime started up front, and detailed errors |
| `unsworn check [--deep] [--target <t>]` | Validate config, routes and handler files; `--deep` asks workers whether every handler exists; `--target <t>` checks a deployment target |
| `unsworn routes [--policies]` | Print the route table and each route's source, including MCP tools; `--policies` shows merged policies and each tool's arguments |
| `unsworn plan [--check]` | Write what every route and tool enforces to `unsworn.plan`, to commit with the config; `--check` fails when it's out of date. See [`plan.md`](plan.md) |
| `unsworn diff [<base>] [--fail-on <looser\|any>]` | Show what changed in the rules since a git revision (or another directory), marking what got looser; `--fail-on looser` for CI |
| `unsworn top [--once]` | Show each route's requests, latency and its handler's share of time and CPU on a running server, costliest first. See [`top.md`](top.md) |
| `unsworn gen [<lang>] [--check]` | Write typed request models for handlers (TypeScript, Python, Go, Rust) from each route's schemas; `--check` fails when one is out of date. See [`types.md`](types.md) |
| `unsworn openapi` | Print the OpenAPI document for what Unsworn serves |
| `unsworn build --target <t>` | Bundle the project for a deployment target: `image` writes a Dockerfile for the server and its workers, `lambda` writes one for AWS Lambda, `cloudflare` makes a Cloudflare Worker, and `vercel` and `netlify` make a function for each |
| `unsworn conformance --runtime <r> --handlers <file>` | Test a runtime's workers against the protocol. See [`conformance.md`](conformance.md) |
| `unsworn schema` | Print the config's JSON Schema (also at [`schema/unsworn.schema.json`](../schema/unsworn.schema.json)) |
