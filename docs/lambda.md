# AWS Lambda

On Lambda, Unsworn runs as it does anywhere else: the `unsworn` binary and
its worker processes, in a container image. Handlers in any language work,
they run in processes of their own, and the rules are enforced by the same
code as on a server, because it is the server.

```sh
unsworn check --target lambda     # what differs there, if anything
unsworn build --target lambda     # writes dist/lambda/Dockerfile
```

> The image has been built and run here the way Lambda's sandbox runs
> one: a read-only filesystem with `/tmp` writable, and a user other than
> the image's own. It hasn't been deployed to Lambda itself yet.

## How it works

Lambda invokes a function; it doesn't send it HTTP. The image carries
AWS's [Lambda Web Adapter](https://github.com/awslabs/aws-lambda-web-adapter),
an extension that Lambda starts beside the server. It waits for the server
to answer, then turns each invocation into an HTTP request to it and the
response into the invocation's result.

The Dockerfile is the one [`unsworn build --target image`](image.md)
writes, with four lines more:

```dockerfile
COPY --from=public.ecr.aws/awsguru/aws-lambda-adapter:0.9.1 /lambda-adapter /opt/extensions/lambda-adapter
ENV AWS_LWA_PORT=8080
ENV AWS_LWA_READINESS_CHECK_PATH=/_unsworn/health
ENV HOME=/tmp
```

- The adapter is only started on Lambda. Run the image anywhere else and
  it's an ordinary container, which is how to try it first:
  `docker run -p 8080:8080 <image>`.
- `HOME=/tmp` is for runtimes that write under it: on Lambda only `/tmp`
  can be written, and the image runs as a user Lambda picks.
- The adapter's version is a line in a file that's yours: check for a
  newer one when you build.

## Deploying

Build the image, push it to Amazon ECR, and create the function from it:

```sh
docker build --provenance=false -f dist/lambda/Dockerfile -t my-api .
```

`--provenance=false` makes Docker write one image. Lambda doesn't take
the index Docker otherwise writes around it.

The function, its URL or API, and its settings are yours to define. With
SAM:

```yaml
Resources:
  Api:
    Type: AWS::Serverless::Function
    Properties:
      PackageType: Image
      ImageUri: 123456789012.dkr.ecr.eu-west-1.amazonaws.com/my-api:latest
      MemorySize: 1024
      Timeout: 30
      FunctionUrlConfig:
        AuthType: NONE
      Environment:
        Variables:
          JWT_SECRET: "{{resolve:secretsmanager:api/jwt}}"
```

Send every path to the function: a function URL does, as does the
`$default` route of an HTTP API. The variables your config reads with
`{env.NAME}` go in the function's environment, which `unsworn build` lists.

## What to set for it

```toml
[server]
trusted_proxies = ["127.0.0.1"]    # the adapter, beside the server

[store]
backend = "redis"
url     = "{env.REDIS_URL}"        # redis:// or rediss://, reachable from the function
```

- **`trusted_proxies`.** Every request reaches the server from the
  adapter, at `127.0.0.1`. Trusting it lets Unsworn read the client's
  address from `X-Forwarded-For`, which Lambda sets, for `ip` rules, rate
  limits by `ip` and the access log.
- **A shared store, for `rate_limit` and `cache`.** Lambda runs as many
  instances as there are requests at once, each with its own memory. The
  server speaks Redis itself, so it's a `redis://` or `rediss://` URL the
  function can reach: one in its VPC, or a hosted one.
- **`[server.tls]` isn't needed:** the function's URL or API terminates
  TLS.
- **`workers`.** An instance handles one request at a time, so one worker
  for each runtime, the default to start with, is what's used.

`unsworn check --target lambda` reports each of these for a config.

## What's different from a server that stays up

- **Cold starts.** A new instance starts the server (a few milliseconds)
  and then each runtime's first worker on its first request: about 7 to
  12 ms for Go and Rust, 140 ms for Bun, and 200 to 400 ms for Python and
  Node (see [benchmarks](benchmarks.md#startup)). `eager = true` moves a
  runtime's start into the instance's own, before its first request.
- **Frozen between requests.** Lambda stops an instance's processes when
  it isn't handling a request. Connections kept open across that, to
  Redis or an upstream, may have closed by the time it's thawed; Unsworn
  reconnects, and `[store].on_error` says what a request does meanwhile.
  Work after the response, such as a cache write, may wait for the next
  request.
- **Sizes and time.** Lambda limits a request and a buffered response to
  6 MB, and a function to 15 minutes. `timeout` and `max_body` apply
  within those.
- **Streaming** needs a function URL with `InvokeMode: RESPONSE_STREAM`
  and `ENV AWS_LWA_INVOKE_MODE=response_stream` in the Dockerfile.
  Otherwise a streamed response, such as server-sent events, is collected
  and sent whole.
- **WebSockets** don't pass through a function URL or an HTTP API.
- **No admin listener to reach.** `admin_listen` works inside the
  instance, where nothing can get to it: use CloudWatch for logs, which
  the access log and handlers' output go to.

## Why a container, and not a function bundle

Earlier versions built a Node function for Lambda, as the
[Vercel and Netlify targets](serverless.md) still do: the core as
WebAssembly, with JavaScript handlers called in the same process. That
ran JavaScript and TypeScript only, and the handlers shared a process with
the rules they're checked by. Lambda runs containers, so there it can be
the real thing.
