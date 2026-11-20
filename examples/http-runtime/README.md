# HTTP runtime example

A plain HTTP server, with no Unsworn SDK in it, run as a runtime. Needs
`python3`; the server is 40 lines of its standard library, and could be an
ASP.NET, Spring, Rails or Phoenix app just as well.

```sh
cd examples/http-runtime
DEMO_KEY=secret unsworn dev
```

```sh
curl localhost:8080/health
curl localhost:8080/api/users/7                           # 401: /api needs a key
curl -H 'x-api-key: secret' localhost:8080/api/users/7    # the server answers, and names the caller
```

See [`docs/http-runtime.md`](../../docs/http-runtime.md).
