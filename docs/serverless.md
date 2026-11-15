# Serverless functions: Vercel, Netlify and AWS Lambda

`unsworn build --target vercel`, `--target netlify` and `--target lambda`
turn a project into a serverless function. The core runs inside the function
as WebAssembly, so routing, validation, auth, CORS, rate limits, caching and
MCP behave as they do on the server. Your JavaScript and TypeScript handlers
are bundled into the same function and called directly, with no worker
processes.

> These targets are tested by running the built functions in Node, called
> the way each platform calls them. They haven't been run on Vercel, Netlify
> or AWS themselves yet.

## One function per project

Every request goes to one function, which runs the whole project. The
platform's own routing is reduced to "send everything here", so a route
means the same thing on every target, and its policies apply whatever it
serves: handlers, static files, proxied requests or MCP tools.

The cost is that settings a platform keeps per function are shared by every
route: memory, the platform's maximum duration, permissions and the bundle
that's loaded at a cold start. A route's own `timeout` still applies, because
the core enforces it.

## Quick start

In a project with JS or TS handlers:

```sh
npm install @unswornjs/sdk
npm install --save-dev esbuild        # the build bundles your handlers with it
unsworn check --target vercel         # what won't work there, if anything
unsworn build --target vercel
```

The `@unswornjs/sdk` version must match your `unsworn` binary; the build
says so if it doesn't. Rebuild after any change, to handlers included: the
function is a single bundled file.

The config files are bundled as written and loaded when the function starts,
so `{env.NAME}` is read from the function's environment variables, not from
your shell at build time. The build lists any it can't see.

## Vercel

```sh
unsworn build --target vercel
npx vercel deploy --prebuilt          # add --prod for production
```

The build writes a deployment in Vercel's
[Build Output API](https://vercel.com/docs/build-output-api) layout, which
`vercel deploy --prebuilt` uploads as it is:

| Path | What it is |
|---|---|
| `.vercel/output/config.json` | Sends every path to the function. |
| `.vercel/output/functions/unsworn.func/` | The function: `index.mjs`, the core's `unsworn_bg.wasm`, `assets/` and `.vc-config.json`. |

Function settings come from your `vercel.json`. Vercel's own build is what
normally applies them, and a prebuilt deployment skips it, so `unsworn
build` copies them into the function's `.vc-config.json`:

```json
{
  "regions": ["fra1"],
  "functions": {
    "unsworn": { "memory": 1024, "maxDuration": 60 }
  }
}
```

It reads `regions`, and `memory` and `maxDuration` from the entry in
`functions`. The project is one function, so the entry's key doesn't matter
and there can be only one; other keys in it are ignored with a note. The
function runs on `nodejs22.x`. Set environment variables with `vercel env
add`, or in the project's settings. Add `.vercel/` to `.gitignore`.

Responses stream. The client IP comes from the `X-Real-IP` header that
Vercel sets, and the request id from `X-Vercel-Id`.

## Netlify

```toml
# netlify.toml
[build]
  command = "unsworn build --target netlify"
  publish = ".netlify/static"
```

```sh
npx netlify deploy --prod
```

The build writes a function in Netlify's
[Frameworks API](https://docs.netlify.com/build/frameworks/frameworks-api/)
layout:

| Path | What it is |
|---|---|
| `.netlify/v1/functions/unsworn/unsworn.mjs` | What Netlify reads: the function's `config`, which routes every path to it and tells Netlify it's already bundled. |
| `.netlify/v1/functions/unsworn/` | The rest of the function: `index.mjs`, `unsworn_bg.wasm` and `assets/`. |
| `.netlify/static/` | An empty directory to publish. The function answers every path, so nothing else needs publishing. |

Netlify runs the build command wherever it builds, so the `unsworn` binary
and `node_modules` must be there: either build and deploy from your machine
or CI with the Netlify CLI, or install `unsworn` as the first step of the
build command. Function settings such as the region go in `netlify.toml`
as usual. Add `.netlify/` to `.gitignore`.

Responses stream. The client IP, the request id and `waitUntil` come from
Netlify's context, which a handler that's an app with `fetch()` also gets
as its second argument.

## AWS Lambda

```sh
unsworn build --target lambda         # writes dist/lambda
```

`dist/lambda` holds the function's code and nothing else: `index.mjs`,
`unsworn_bg.wasm` and `assets/`. Zip it, or point your infrastructure code
at the directory. The function, its URL or API and its settings are yours to
define. With SAM:

```yaml
Resources:
  Api:
    Type: AWS::Serverless::Function
    Properties:
      CodeUri: dist/lambda
      Handler: index.handler
      Runtime: nodejs22.x
      MemorySize: 512
      Timeout: 30
      FunctionUrlConfig:
        AuthType: NONE
      Environment:
        Variables:
          JWT_SECRET: "{{resolve:secretsmanager:api/jwt}}"
```

The bundle exports two handlers. Pick the one that matches how the function
is invoked:

| Handler | Answers | Use with |
|---|---|---|
| `index.handler` | Whole, once the response is complete | API Gateway HTTP APIs and REST APIs, and function URLs in the default `BUFFERED` mode |
| `index.streamHandler` | As the response is produced | Function URLs with `InvokeMode: RESPONSE_STREAM` |

Send every path to the function: a function URL does, as does the
`$default` route of an HTTP API, or `/{proxy+}` plus `/` on a REST API. On
an API's own `execute-api` domain with a named stage, the stage is taken off
the front of the path.

With `index.handler`, a streamed response such as server-sent events is
collected and sent at the end. Lambda limits requests and whole responses to
6 MB. Text responses go out as text and anything else as base64, which a
REST API only decodes if it has binary media types configured.

The client IP is the event's source IP, and the request id is the
invocation's `awsRequestId`. A handler that's an app with `fetch()` gets the
event and the Lambda context as its second and third arguments.

## Handlers

Handlers have the same shape as on the server (see
[`sdk/js`](../sdk/js/README.md)): `(req, ctx)` returning `{ json }`,
`{ text }`, `{ body }`, a `Response`, or `next(...)` from a `before`
handler. Only JavaScript and TypeScript handlers run in a function;
`unsworn check --target <name>` reports any others. For other languages,
run the server in a container.

All of Node's APIs are available, and environment variables are in
`process.env`. esbuild bundles every dependency into `index.mjs`, so a
package that loads native code or reads its own files from disk at runtime
may not work bundled.

## The store

Functions come and go, and several run at once, so `rate_limit` and `cache`
need a store outside them. Functions also can't keep a Redis connection
open, so they reach Redis through a REST API:

```toml
[store]
backend = "redis"
url     = "{env.UPSTASH_REDIS_REST_URL}"
token   = "{env.UPSTASH_REDIS_REST_TOKEN}"
```

Any service that speaks Upstash's REST protocol works. `unsworn check
--target <name>` reports a `rate_limit` or `cache` without such a store, and
a `redis://` URL, which is for the server.

Rate limits run the same script on the same keys as the server's Redis
store, so a server and a function on one Redis share their limits. Cached
responses aren't shared between the two. `[store].on_error` applies as on
the server: if the API doesn't answer within a second, `"open"` lets the
request through and `"closed"` answers `503`.

Without `[store]`, what the core remembers on its own (signing keys, API-key
verdicts) is kept in each instance's memory, which is fine.

## Differences from the server

- **TLS, HTTP versions and compression** are the platform's job.
  `[server.tls]` is ignored.
- **The client IP** comes from the platform; `trusted_proxies` is ignored.
  If the platform doesn't give one, a route with an `ip` policy answers
  `403`: an unknown client can't be shown to be allowed.
- **Request ids** come from an incoming `X-Request-Id`, else the platform's
  id for the request.
- **There's no `/_unsworn/health`, metrics endpoint or admin listener.** Use
  the platform's logs and metrics.
- **Logs** go to the console, which the platform collects. Handler logs,
  from `log` and `req.log`, are one JSON object per line with the request
  id.
- **Error details:** set the environment variable `UNSWORN_DEV` to `1` to
  put error details in responses, as `unsworn dev` does.
- **Static files** are bundled into the function and served by it, so a
  `static` route's auth, CORS and rate limits apply to its files. They
  count towards the function's size, and aren't served from the platform's
  CDN.
- **Proxy routes** forward with `fetch()`. WebSocket upgrades can't pass
  through a function, and answer `501`. Compressed upstream responses are
  decompressed on the way through.
- **Work after the response,** such as a cache write, uses the platform's
  `waitUntil` where there is one. On Lambda it finishes before the handler
  returns.
- **Timeouts and sizes:** `timeout` and `max_body` work as on the server,
  but the platform's own limits on duration and on request and response
  size apply too.
- **Cold starts:** a new instance loads the core (about 10 ms) and the
  config before its first request. Validation schemas are compiled when a
  request first needs them, so the cost doesn't grow with the number of
  routes.

## Size

The core adds about 4.4 MB to the function (1.4 MB gzipped), plus your
handlers, their dependencies and your static files.

## How it works

`crates/unsworn-wasm` compiles the core to WebAssembly, as for
[Cloudflare Workers](cloudflare.md). `sdk/js/src/node.ts` is the bridge for
Node: handlers are called in the process, files are read from disk, and the
store is `sdk/js/src/store.ts`. `vercel.ts`, `netlify.ts` and `lambda.ts`
adapt each platform's request and response to it. See
[`architecture.md`](architecture.md).
