//! The pipeline against an in-memory host: no processes, sockets or
//! runtime. Handlers are closures, the store is a map, the clocks are set
//! by the test, and outgoing requests get canned answers.
//!
//! The binary's HTTP tests cover the same flow end to end. These pin down
//! what the core asks of a host, which every host (the binary, a Worker)
//! relies on.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::FutureExt;
use http::{HeaderMap, StatusCode};
use jsonwebtoken::{EncodingKey, Header, encode};
use serde_json::{Map, Value, json};
use unsworn_proto::{Phase, Request as ProtoRequest};

use super::host::{
    Body, CallError, Deadline, FetchError, Host, ProxyRequest, Reply, Response, StaticRequest,
};
use super::{KeyCache, Problem, Router};
use crate::policy::HandlerRef;
use crate::project::tests::MemHost;
use crate::store::{Bucket, CachedResponse, RateLimiter, ResponseCache, StoreError, take_token};

/// The host's clocks, which tests move by hand.
#[derive(Default)]
struct Clock {
    ms: AtomicU64,
}

impl Clock {
    fn now(&self) -> u64 {
        // Start well after the epoch, as a real wall clock would.
        1_700_000_000_000 + self.ms.load(Ordering::SeqCst)
    }
    fn advance(&self, by: Duration) {
        self.ms.fetch_add(by.as_millis() as u64, Ordering::SeqCst);
    }
}

struct TestStore {
    clock: Arc<Clock>,
    fail: AtomicBool,
    buckets: Mutex<HashMap<String, Bucket>>,
    cache: Mutex<HashMap<String, CachedResponse>>,
}

impl TestStore {
    fn check(&self) -> Result<(), StoreError> {
        if self.fail.load(Ordering::SeqCst) {
            Err(StoreError("down".into()))
        } else {
            Ok(())
        }
    }
}

impl RateLimiter for TestStore {
    async fn take(
        &self,
        key: &str,
        requests: u32,
        per: Duration,
    ) -> Result<Option<Duration>, StoreError> {
        self.check()?;
        let now = Duration::from_millis(self.clock.now());
        let mut buckets = self.buckets.lock().unwrap();
        let (bucket, wait) = take_token(buckets.get(key).copied(), now, requests, per);
        buckets.insert(key.to_string(), bucket);
        Ok(wait)
    }
}

impl ResponseCache for TestStore {
    async fn get(&self, key: &str) -> Result<Option<CachedResponse>, StoreError> {
        self.check()?;
        Ok(self.cache.lock().unwrap().get(key).cloned())
    }

    async fn put(&self, key: &str, value: CachedResponse) -> Result<(), StoreError> {
        self.check()?;
        self.cache.lock().unwrap().insert(key.to_string(), value);
        Ok(())
    }
}

type Handler =
    Box<dyn Fn(&ProtoRequest, &Body<Bytes>) -> Result<Reply<Bytes>, CallError> + Send + Sync>;

/// One handler call, as the host saw it.
struct Call {
    handler: String,
    request: ProtoRequest,
}

struct TestHost {
    clock: Arc<Clock>,
    store: Arc<TestStore>,
    dev: bool,
    handlers: HashMap<String, Handler>,
    calls: Mutex<Vec<Call>>,
    /// Canned answers to outgoing requests, by URL.
    fetches: HashMap<String, (u16, String)>,
    fetched: Mutex<Vec<String>>,
    /// Outgoing requests never get an answer: a stalled identity provider.
    stall_fetches: AtomicBool,
}

impl TestHost {
    fn new() -> TestHost {
        let clock = Arc::new(Clock::default());
        TestHost {
            store: Arc::new(TestStore {
                clock: clock.clone(),
                fail: AtomicBool::new(false),
                buckets: Mutex::default(),
                cache: Mutex::default(),
            }),
            clock,
            dev: false,
            handlers: HashMap::new(),
            calls: Mutex::default(),
            fetches: HashMap::new(),
            fetched: Mutex::default(),
            stall_fetches: AtomicBool::new(false),
        }
    }

    /// Registers a handler by its reference (`file#name`).
    fn handler(
        mut self,
        name: &str,
        f: impl Fn(&ProtoRequest, &Body<Bytes>) -> Result<Reply<Bytes>, CallError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.handlers.insert(name.to_string(), Box::new(f));
        self
    }

    fn fetch_answer(mut self, url: &str, status: u16, body: impl Into<String>) -> Self {
        self.fetches.insert(url.to_string(), (status, body.into()));
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|c| c.handler.clone())
            .collect()
    }

    fn last_call<T>(&self, f: impl FnOnce(&Call) -> T) -> T {
        f(self.calls.lock().unwrap().last().expect("a handler call"))
    }

    fn fetch_count(&self) -> usize {
        self.fetched.lock().unwrap().len()
    }
}

fn bytes_of(body: Body<Bytes>) -> Bytes {
    match body {
        Body::Empty => Bytes::new(),
        Body::Bytes(b) | Body::Host(b) => b,
    }
}

impl Host for TestHost {
    type RequestBody = Bytes;
    type ResponseBody = Bytes;
    type Store = Arc<TestStore>;

    fn store(&self) -> Arc<TestStore> {
        self.store.clone()
    }

    fn now_ms(&self) -> u64 {
        self.clock.now()
    }

    fn monotonic(&self) -> Duration {
        Duration::from_millis(self.clock.now())
    }

    fn dev(&self) -> bool {
        self.dev
    }

    async fn read_body(&self, body: Bytes, max: u64) -> Result<Bytes, Problem> {
        if body.len() as u64 > max {
            return Err(Problem::new(StatusCode::PAYLOAD_TOO_LARGE, "too large"));
        }
        Ok(body)
    }

    fn limit_body(&self, body: Bytes, _max: u64) -> Bytes {
        body
    }

    async fn collect(&self, body: Bytes, limit: usize) -> Result<Bytes, Bytes> {
        if body.len() <= limit {
            Ok(body)
        } else {
            Err(body)
        }
    }

    async fn call(
        &self,
        handler: &HandlerRef,
        request: ProtoRequest,
        body: Body<Bytes>,
        _deadline: Deadline,
    ) -> Result<Reply<Bytes>, CallError> {
        let Some(f) = self.handlers.get(&handler.raw) else {
            return Err(CallError::MissingHandler(handler.raw.clone()));
        };
        let reply = f(&request, &body);
        self.calls.lock().unwrap().push(Call {
            handler: handler.raw.clone(),
            request,
        });
        reply
    }

    async fn proxy(&self, request: ProxyRequest<Bytes>) -> Response<Bytes> {
        let (parts, body) = request.request.into_parts();
        let echo = json!({
            "upstream": request.proxy.upstreams,
            "path": parts.uri.path(),
            "client_ip": request.client_ip.to_string(),
            "body": String::from_utf8_lossy(&bytes_of(body)),
        });
        super::json_response(StatusCode::OK, &echo)
    }

    async fn serve_static(&self, request: StaticRequest) -> Response<Bytes> {
        let echo = json!({ "dir": request.dir, "rest": request.rest, "index": request.index });
        super::json_response(StatusCode::OK, &echo)
    }

    async fn fetch(
        &self,
        request: http::Request<Bytes>,
        _timeout: Duration,
        _limit: usize,
    ) -> Result<http::Response<Bytes>, FetchError> {
        let url = request.uri().to_string();
        self.fetched.lock().unwrap().push(url.clone());
        if self.stall_fetches.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        let Some((status, body)) = self.fetches.get(&url) else {
            return Err(FetchError::Connect("no such host".into()));
        };
        let mut resp = http::Response::new(Bytes::from(body.clone()));
        *resp.status_mut() = StatusCode::from_u16(*status).unwrap();
        Ok(resp)
    }

    async fn timeout<F>(&self, _timeout: Duration, future: F) -> Option<F::Output>
    where
        F: Future + crate::MaybeSend,
        F::Output: crate::MaybeSend,
    {
        Some(future.await)
    }

    async fn background(&self, future: impl Future<Output = ()> + crate::MaybeSend + 'static) {
        future.await;
    }
}

/// A response with its body read.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    route: String,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&self.body)))
    }
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
}

struct Test {
    router: Router,
    host: TestHost,
}

const IP: &str = "198.51.100.7";

impl Test {
    /// `files` are paths the config refers to, such as handler files.
    fn new(config: &str, files: &[&str], host: TestHost) -> Test {
        Test::with_keys(config, files, host, Arc::new(KeyCache::default()))
    }

    /// As [`Test::new`], with the key cache a host would keep across reloads.
    fn with_keys(config: &str, files: &[&str], host: TestHost, keys: Arc<KeyCache>) -> Test {
        let mut all = vec![("unsworn.toml", config)];
        all.extend(files.iter().map(|f| (*f, "")));
        let files = MemHost::with(&all);
        let compiled = crate::compile(&files, "unsworn.toml");
        let project = compiled
            .project
            .unwrap_or_else(|| panic!("{:#?}", compiled.diagnostics));
        Test {
            router: Router::new(Arc::new(project), keys),
            host,
        }
    }

    fn send(&self, method: &str, uri: &str, headers: &[(&str, &str)], body: &[u8]) -> Answer {
        let mut req = http::Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let req = req.body(Bytes::copy_from_slice(body)).unwrap();
        let ip: IpAddr = IP.parse().unwrap();
        let handled = self
            .router
            .handle(&self.host, req, ip)
            .now_or_never()
            .expect("the test host never waits");
        let (parts, body) = handled.response.into_parts();
        Answer {
            status: parts.status,
            headers: parts.headers,
            body: bytes_of(body),
            route: handled.route,
        }
    }

    fn get(&self, uri: &str) -> Answer {
        self.send("GET", uri, &[], b"")
    }
}

const TS: &str = r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun run worker.ts"
"#;

fn ok_json(value: Value) -> Result<Reply<Bytes>, CallError> {
    let mut response = unsworn_proto::Response::new("", 200);
    response.json = Some(value.into());
    Ok(Reply::Response {
        response,
        body: Body::Empty,
    })
}

/// A handler that answers with what it was sent.
fn echo(req: &ProtoRequest, body: &Body<Bytes>) -> Result<Reply<Bytes>, CallError> {
    let raw = match body {
        Body::Bytes(b) | Body::Host(b) => String::from_utf8_lossy(b).into_owned(),
        Body::Empty => String::new(),
    };
    ok_json(json!({
        "method": req.method,
        "path": req.path,
        "params": req.params,
        "query": req.query,
        "json": req.json,
        "context": req.context,
        "raw": raw,
    }))
}

#[test]
fn matching_and_declarative_routes() {
    let t = Test::new(
        r#"
[server]
trailing_slash = "redirect"

[[route]]
path = "/health"
method = "GET"
respond = { status = 200, json = { ok = true }, headers = { "X-Kind" = "static" } }

[[route]]
path = "/old/{id}"
redirect = { to = "/new/{id}", status = 301 }

[[route]]
path = "/files/{*rest}"
method = "GET"
static = { dir = "public" }

[[route]]
path = "/legacy/{*rest}"
proxy = "http://old-api:8080"
"#,
        &["public/index.html"],
        TestHost::new(),
    );

    let r = t.get("/health");
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json(), json!({ "ok": true }));
    assert_eq!(r.header("x-kind"), Some("static"));
    assert_eq!(r.route, "/health");

    let r = t.get("/old/42");
    assert_eq!(r.status, StatusCode::MOVED_PERMANENTLY);
    assert_eq!(r.header("location"), Some("/new/42"));

    let r = t.get("/health/?x=1");
    assert_eq!(r.status, StatusCode::PERMANENT_REDIRECT);
    assert_eq!(r.header("location"), Some("/health?x=1"));

    let r = t.get("/nope");
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.header("content-type"), Some(Problem::CONTENT_TYPE));
    assert_eq!(r.route, "(none)");

    let r = t.send("DELETE", "/health", &[], b"");
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(r.header("allow"), Some("GET, HEAD"));

    // Static files and proxies go to the host.
    let r = t.get("/files/css/site.css");
    assert_eq!(r.json()["dir"], "public");
    assert_eq!(r.json()["rest"], "css/site.css");
    let r = t.send("POST", "/legacy/x", &[], b"payload");
    assert_eq!(r.json()["upstream"], json!(["http://old-api:8080"]));
    assert_eq!(r.json()["body"], "payload");
    assert_eq!(r.json()["client_ip"], IP);
}

#[test]
fn paths_are_normalized_before_matching() {
    let t = Test::new(
        r#"
[auth.keys]
type = "api_key"
keys = ["k1"]

[[route]]
path = "/public/{*rest}"
proxy = "http://up:8080"

[[route]]
path = "/admin/{*rest}"
proxy = "http://up:8080"
auth = "keys"
"#,
        &[],
        TestHost::new(),
    );
    let r = t.get("/public/a/./b");
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["path"], "/public/a/b");
    // A path that only looks public gets the policies of what it resolves
    // to, and the upstream is sent the resolved path.
    for path in [
        "/public/../admin/x",
        "/public/%2e%2e/admin/x",
        "/%61dmin/x",
        "/public/..//../admin/x",
    ] {
        let r = t.get(path);
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(r.route, "/admin/{*rest}", "{path}");
    }
    let r = t.send(
        "GET",
        "/public/../admin/x?a=..",
        &[("x-api-key", "k1")],
        b"",
    );
    assert_eq!(r.json()["path"], "/admin/x");
    // An encoded slash isn't a separator, so it can't form a dot segment.
    let r = t.get("/public/a%2f..%2fb");
    assert_eq!(r.json()["path"], "/public/a%2F..%2Fb");
}

#[test]
fn hosts_are_matched_by_name() {
    let t = Test::new(
        r#"
[[route]]
path = "/"
host = "api.example.com"
respond = { text = "api" }

[[route]]
path = "/"
host = "[::1]"
respond = { text = "v6" }

[[route]]
path = "/"
respond = { text = "any" }
"#,
        &[],
        TestHost::new(),
    );
    let body = |uri: &str, host: &str| {
        let r = t.send("GET", uri, &[("host", host)], b"");
        String::from_utf8(r.body.to_vec()).unwrap()
    };
    assert_eq!(body("/", "API.example.com:8443"), "api");
    assert_eq!(body("/", "[::1]:8080"), "v6");
    assert_eq!(body("/", "[::1]"), "v6");
    assert_eq!(body("/", "other.example.com"), "any");
    // An absolute request target outranks `Host`.
    assert_eq!(body("http://api.example.com/", "other.example.com"), "api");
    assert_eq!(body("http://other.example.com/", "api.example.com"), "any");
}

#[test]
fn cors_headers_and_access_rules() {
    let t = Test::new(
        r#"
[[route]]
path = "/a"
method = ["GET", "POST"]
respond = { status = 200, text = "hi", headers = { Server = "x" } }
cors = { origins = ["https://app.example"], credentials = true, max_age = "10m" }
response_headers = { set = { "Cache-Control" = "no-store" }, remove = ["Server"] }

[[route]]
path = "/internal"
respond = { status = 200 }
ip = { allow = ["10.0.0.0/8"] }

[[route]]
path = "/browser"
respond = { status = 200 }
origin = { allow = ["https://app.example"] }
"#,
        &[],
        TestHost::new(),
    );

    let r = t.send(
        "OPTIONS",
        "/a",
        &[
            ("origin", "https://app.example"),
            ("access-control-request-method", "POST"),
        ],
        b"",
    );
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(r.header("access-control-allow-methods"), Some("GET, POST"));
    assert_eq!(r.header("access-control-max-age"), Some("600"));

    let r = t.send("GET", "/a", &[("origin", "https://app.example")], b"");
    assert_eq!(
        r.header("access-control-allow-origin"),
        Some("https://app.example")
    );
    assert_eq!(r.header("access-control-allow-credentials"), Some("true"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("server"), None);
    assert_eq!(r.body, "hi");

    assert_eq!(t.get("/internal").status, StatusCode::FORBIDDEN);
    let r = t.send(
        "GET",
        "/browser",
        &[("origin", "https://evil.example")],
        b"",
    );
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(t.get("/browser").status, StatusCode::OK, "no Origin header");
}

/// Only handlers read the request's copied headers, so routes without one
/// skip the copy. A `before` handler on such a route still needs them.
#[test]
fn before_handlers_see_headers_on_routes_without_a_handler() {
    let host = TestHost::new().handler("src/auth.ts#tenant", |req, _| {
        match req.headers.get("x-tenant").map(String::as_str) {
            Some("acme") => Ok(Reply::Continue(Map::new())),
            _ => Ok(Reply::Response {
                response: unsworn_proto::Response::new("", 403),
                body: Body::Empty,
            }),
        }
    });
    let t = Test::new(
        &format!(
            r#"{TS}
[[route]]
path = "/gated"
respond = {{ status = 200, json = {{ ok = true }} }}
before = ["src/auth.ts#tenant"]

[[route]]
path = "/open"
respond = {{ status = 200, json = {{ ok = true }} }}
"#
        ),
        &["src/auth.ts"],
        host,
    );
    let r = t.send("GET", "/gated", &[("x-tenant", "acme")], b"");
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(t.get("/gated").status, StatusCode::FORBIDDEN);
    assert_eq!(t.get("/open").status, StatusCode::OK);
}

#[test]
fn handlers_get_validated_requests() {
    let host = TestHost::new()
        .handler("src/users.ts#get", echo)
        .handler("src/users.ts#create", echo)
        .handler("src/auth.ts#tenant", |req, _| {
            match req.headers.get("x-tenant").map(String::as_str) {
                Some("blocked") => {
                    let mut response = unsworn_proto::Response::new("", 403);
                    response.json = Some(json!({ "detail": "blocked" }).into());
                    Ok(Reply::Response {
                        response,
                        body: Body::Empty,
                    })
                }
                tenant => {
                    let mut more = Map::new();
                    more.insert("tenant".into(), json!(tenant.unwrap_or("none")));
                    // `auth` is the pipeline's to set.
                    more.insert("auth".into(), json!("forged"));
                    Ok(Reply::Continue(more))
                }
            }
        });
    let t = Test::new(
        &format!(
            r#"{TS}
[[route]]
path = "/users/{{id}}"
method = "GET"
handler = "src/users.ts#get"
params = {{ id = {{ type = "integer", min = 1 }} }}
query = {{ limit = {{ type = "integer", default = 20, max = 100 }} }}
before = ["src/auth.ts#tenant"]

[[route]]
path = "/users"
method = "POST"
handler = "src/users.ts#create"
body = {{ type = "object", required = ["name"], properties = {{ name = {{ type = "string" }} }} }}
"#
        ),
        &["src/users.ts", "src/auth.ts"],
        host,
    );

    let r = t.send("GET", "/users/7?limit=5", &[("x-tenant", "acme")], b"");
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let v = r.json();
    assert_eq!(v["params"]["id"], 7, "converted to its declared type");
    assert_eq!(v["query"]["limit"], 5);
    assert_eq!(v["context"], json!({ "tenant": "acme" }));
    assert_eq!(
        t.host.calls(),
        ["src/auth.ts#tenant", "src/users.ts#get"],
        "before handlers run first"
    );
    t.host.last_call(|c| {
        assert_eq!(c.request.phase, Phase::Handle);
        assert_eq!(c.request.route, "/users/{id}");
    });

    assert_eq!(
        t.send("GET", "/users/1", &[("x-tenant", "blocked")], b"")
            .status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(t.get("/users/0").status, StatusCode::BAD_REQUEST);
    assert_eq!(t.get("/users/1?limit=500").status, StatusCode::BAD_REQUEST);

    let json = [("content-type", "application/json")];
    let r = t.send("POST", "/users", &json, br#"{"name":"Ada"}"#);
    assert_eq!(r.json()["json"], json!({ "name": "Ada" }));
    let r = t.send("POST", "/users", &json, br#"{"name":1}"#);
    assert_eq!(r.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(r.json()["errors"][0]["location"], "body.name");
    let r = t.send("POST", "/users", &json, b"{");
    assert_eq!(r.status, StatusCode::BAD_REQUEST);

    // Two values where one is declared, or two members with one name: an
    // upstream could read the one that wasn't checked.
    let r = t.get("/users/1?limit=500&limit=5");
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json()["errors"][0]["location"], "query.limit");
    let r = t.send("POST", "/users", &json, br#"{"name":1,"name":"Ada"}"#);
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(
        r.json()["detail"]
            .as_str()
            .unwrap()
            .contains("\"name\" appears more than once")
    );

    // JSON is `application/json` or a `+json` type, whatever its
    // parameters; a type that merely contains those words isn't.
    let post = |content_type: &str| {
        let headers = [("content-type", content_type)];
        t.send("POST", "/users", &headers, br#"{"name":"Ada"}"#)
            .status
    };
    for json in [
        "Application/JSON; charset=utf-8",
        "application/merge-patch+json",
        "application/vnd.api+json ; ext=x",
    ] {
        assert_eq!(post(json), StatusCode::OK, "{json}");
    }
    for other in [
        "application/json-seq",
        "application/jsonp",
        "text/plain; note=+json",
    ] {
        assert_eq!(post(other), StatusCode::UNSUPPORTED_MEDIA_TYPE, "{other}");
    }
    let r = t.send("POST", "/users", &[("content-type", "text/plain")], b"x");
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[test]
fn handler_failures_and_response_validation() {
    let host = TestHost::new()
        .handler("src/h.ts#slow", |_, _| Err(CallError::Timeout))
        .handler("src/h.ts#gone", |_, _| {
            Err(CallError::Unavailable("no workers".into()))
        })
        .handler("src/h.ts#cont", |_, _| Ok(Reply::Continue(Map::new())))
        .handler("src/h.ts#wrong", |_, _| ok_json(json!({ "id": "x" })))
        .handler("src/h.ts#teapot", |_, _| {
            let mut response = unsworn_proto::Response::new("", 418);
            response.text = Some("short and stout".into());
            Ok(Reply::Response {
                response,
                body: Body::Bytes(Bytes::from_static(b"short and stout")),
            })
        });
    let t = Test::new(
        &format!(
            r#"{TS}
[server]
response_validation = "enforce"

[[route]]
path = "/slow"
handler = "src/h.ts#slow"

[[route]]
path = "/gone"
handler = "src/h.ts#gone"

[[route]]
path = "/cont"
handler = "src/h.ts#cont"

[[route]]
path = "/wrong"
handler = "src/h.ts#wrong"
response = {{ type = "object", properties = {{ id = {{ type = "integer" }} }} }}

[[route]]
path = "/teapot"
handler = "src/h.ts#teapot"
"#
        ),
        &["src/h.ts"],
        host,
    );
    assert_eq!(t.get("/slow").status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(t.get("/gone").status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(t.get("/cont").status, StatusCode::INTERNAL_SERVER_ERROR);
    let r = t.get("/wrong");
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(r.json().get("errors").is_none(), "details only in dev mode");
    let r = t.get("/teapot");
    assert_eq!(r.status.as_u16(), 418);
    assert_eq!(r.header("content-type"), Some("text/plain; charset=utf-8"));
    assert_eq!(r.body, "short and stout");
}

#[test]
fn rate_limits_and_store_failures() {
    let t = Test::new(
        r#"
[[route]]
path = "/a"
respond = { status = 200 }
rate_limit = { requests = 2, per = "1m" }

[[route]]
path = "/b"
respond = { status = 200 }
rate_limit = { requests = 2, per = "1m", key = "header.x-tenant" }
"#,
        &[],
        TestHost::new(),
    );
    assert_eq!(t.get("/a").status, StatusCode::OK);
    assert_eq!(t.get("/a").status, StatusCode::OK);
    let r = t.get("/a");
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.header("retry-after"), Some("30"));
    assert_eq!(
        t.get("/b").status,
        StatusCode::OK,
        "routes count separately"
    );
    t.host.clock.advance(Duration::from_secs(30));
    assert_eq!(t.get("/a").status, StatusCode::OK, "a token refilled");

    for _ in 0..2 {
        t.send("GET", "/b", &[("x-tenant", "a")], b"");
    }
    let tenant = |name| t.send("GET", "/b", &[("x-tenant", name)], b"").status;
    assert_eq!(tenant("a"), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(tenant("b"), StatusCode::OK, "keys count separately");

    // `on_error = "open"` (the default) lets requests through.
    t.host.store.fail.store(true, Ordering::SeqCst);
    assert_eq!(tenant("a"), StatusCode::OK);

    let closed = Test::new(
        r#"
[store]
on_error = "closed"

[[route]]
path = "/a"
respond = { status = 200 }
rate_limit = { requests = 2, per = "1m" }
"#,
        &[],
        TestHost::new(),
    );
    closed.host.store.fail.store(true, Ordering::SeqCst);
    assert_eq!(closed.get("/a").status, StatusCode::SERVICE_UNAVAILABLE);
}

#[test]
fn caching() {
    let counter = Arc::new(AtomicU64::new(0));
    let n = counter.clone();
    let host = TestHost::new()
        .handler("src/h.ts#count", move |_, _| {
            ok_json(json!({ "n": n.fetch_add(1, Ordering::SeqCst) }))
        })
        .handler("src/h.ts#huge", |_, _| {
            let mut response = unsworn_proto::Response::new("", 200);
            response.body_len = Some(2_000_000);
            Ok(Reply::Response {
                response,
                body: Body::Host(Bytes::from(vec![b'x'; 2_000_000])),
            })
        })
        .handler("src/h.ts#private", |_, _| {
            let mut response = unsworn_proto::Response::new("", 200);
            response.headers.insert(
                "cache-control".into(),
                unsworn_proto::HeaderValues::One("private".into()),
            );
            response.json = Some(json!({}).into());
            Ok(Reply::Response {
                response,
                body: Body::Empty,
            })
        });
    let mut host = host;
    host.dev = true;
    let t = Test::new(
        &format!(
            r#"{TS}
[auth.keys]
type = "api_key"
keys = ["k1", "k2"]

[[route]]
path = "/count"
method = "GET"
handler = "src/h.ts#count"
cache = {{ ttl = "30s" }}

[[route]]
path = "/mine"
method = "GET"
handler = "src/h.ts#count"
auth = "keys"
cache = {{ ttl = "30s" }}

[[route]]
path = "/per-key"
method = "GET"
handler = "src/h.ts#count"
auth = "keys"
cache = {{ ttl = "30s", vary = ["X-API-Key"] }}

[[route]]
path = "/huge"
method = "GET"
handler = "src/h.ts#huge"
cache = {{ ttl = "30s" }}

[[route]]
path = "/private"
method = "GET"
handler = "src/h.ts#private"
cache = {{ ttl = "30s" }}

[[route]]
path = "/head-first"
method = "GET"
handler = "src/h.ts#count"
cache = {{ ttl = "30s" }}

[[route]]
path = "/upstream-auth"
method = "GET"
handler = "src/h.ts#count"
cache = {{ ttl = "30s" }}

[[route]]
path = "/upstream-auth-varied"
method = "GET"
handler = "src/h.ts#count"
cache = {{ ttl = "30s", vary = ["Authorization"] }}
"#
        ),
        &["src/h.ts"],
        host,
    );

    let first = t.get("/count");
    assert_eq!(first.header("x-unsworn-cache"), Some("miss"));
    t.host.clock.advance(Duration::from_secs(5));
    let hit = t.get("/count");
    assert_eq!(hit.json(), first.json());
    assert_eq!(hit.header("x-unsworn-cache"), Some("hit"));
    assert_eq!(hit.header("age"), Some("5"));
    let head = t.send("HEAD", "/count", &[], b"");
    assert_eq!(head.header("x-unsworn-cache"), Some("hit"));
    assert!(head.body.is_empty());
    assert_ne!(
        t.get("/count?x=1").json(),
        first.json(),
        "the query is in the key"
    );
    t.host.clock.advance(Duration::from_secs(30));
    assert_ne!(t.get("/count").json(), first.json(), "expired");

    // An authenticated response is cached only when the key separates users.
    let key = |path, k| t.send("GET", path, &[("x-api-key", k)], b"");
    let a = key("/mine", "k1").json();
    assert_ne!(key("/mine", "k1").json(), a, "not cached");
    let a = key("/per-key", "k1").json();
    assert_eq!(key("/per-key", "k1").json(), a);
    assert_ne!(key("/per-key", "k2").json(), a);

    // A `HEAD` response is never stored: it has no body for a later `GET`.
    let head = t.send("HEAD", "/head-first", &[], b"");
    assert_eq!(head.header("x-unsworn-cache"), None);
    assert_eq!(t.get("/head-first").header("x-unsworn-cache"), Some("miss"));
    assert_eq!(
        t.send("HEAD", "/head-first", &[], b"")
            .header("x-unsworn-cache"),
        Some("hit")
    );

    // A public route may front an upstream that checks `Authorization`
    // itself, so such requests share nothing unless the rule varies on it.
    let bearer = |path, token: &str| {
        let value = format!("Bearer {token}");
        t.send("GET", path, &[("authorization", &value)], b"")
    };
    let a = bearer("/upstream-auth", "a");
    assert_eq!(a.header("x-unsworn-cache"), None);
    assert_ne!(bearer("/upstream-auth", "a").json(), a.json());
    assert_eq!(
        t.get("/upstream-auth").header("x-unsworn-cache"),
        Some("miss"),
        "anonymous requests still cache"
    );
    let a = bearer("/upstream-auth-varied", "a").json();
    assert_eq!(bearer("/upstream-auth-varied", "a").json(), a);
    assert_ne!(bearer("/upstream-auth-varied", "b").json(), a);

    // Too large or private: passed through whole, never stored.
    for _ in 0..2 {
        let r = t.get("/huge");
        assert_eq!(r.body.len(), 2_000_000);
        assert_eq!(r.header("x-unsworn-cache"), None);
    }
    t.get("/private");
    assert_eq!(t.get("/private").header("x-unsworn-cache"), None);
}

#[test]
fn cache_keeps_sessions_and_before_handlers() {
    let counter = Arc::new(AtomicU64::new(0));
    let (n1, n2) = (counter.clone(), counter.clone());
    let host = TestHost::new()
        .handler("src/h.ts#count", move |_, _| {
            ok_json(json!({ "n": n1.fetch_add(1, Ordering::SeqCst) }))
        })
        // An app with sessions: its page depends on the cookie.
        .handler("src/h.ts#session", move |_, _| {
            let mut response = unsworn_proto::Response::new("", 200);
            response.headers.insert(
                "vary".into(),
                unsworn_proto::HeaderValues::One("Accept-Encoding, Cookie".into()),
            );
            response.json = Some(json!({ "n": n2.fetch_add(1, Ordering::SeqCst) }).into());
            Ok(Reply::Response {
                response,
                body: Body::Empty,
            })
        })
        .handler("src/h.ts#members", |req, _| {
            if req.headers.get("x-member").map(String::as_str) == Some("yes") {
                Ok(Reply::Continue(Map::new()))
            } else {
                let mut response = unsworn_proto::Response::new("", 403);
                response.json = Some(json!({ "error": "members only" }).into());
                Ok(Reply::Response {
                    response,
                    body: Body::Empty,
                })
            }
        });
    let mut host = host;
    host.dev = true;
    let t = Test::new(
        &format!(
            r#"{TS}
[[route]]
path = "/session"
method = "GET"
handler = "src/h.ts#session"
cache = {{ ttl = "30s" }}

[[route]]
path = "/cookie-varied"
method = "GET"
handler = "src/h.ts#session"
cache = {{ ttl = "30s", vary = ["Cookie", "Accept-Encoding"] }}

[[route]]
path = "/public"
method = "GET"
handler = "src/h.ts#count"
cache = {{ ttl = "30s" }}

[[route]]
path = "/members"
method = "GET"
handler = "src/h.ts#count"
before = ["src/h.ts#members"]
cache = {{ ttl = "30s" }}
"#
        ),
        &["src/h.ts"],
        host,
    );

    // A response that varies on a header the key doesn't cover isn't
    // stored, even for a request that sent no cookie.
    t.get("/session");
    assert_eq!(t.get("/session").header("x-unsworn-cache"), None);
    // Unless the rule varies on it too.
    let cookie = |path, value| t.send("GET", path, &[("cookie", value)], b"");
    let a = cookie("/cookie-varied", "sid=a").json();
    assert_eq!(cookie("/cookie-varied", "sid=a").json(), a);
    assert_ne!(cookie("/cookie-varied", "sid=b").json(), a);

    // A request with cookies on a public route isn't answered from, or
    // stored in, the cache: the app behind it may read a session from them.
    let anon = t.get("/public");
    assert_eq!(t.get("/public").json(), anon.json());
    let r = cookie("/public", "sid=a");
    assert_eq!(r.header("x-unsworn-cache"), None);
    assert_ne!(r.json(), anon.json());

    // `before` handlers run on a cache hit too, so one that turns a caller
    // away still does.
    let member = t.send("GET", "/members", &[("x-member", "yes")], b"");
    assert_eq!(member.status, 200);
    let hit = t.send("GET", "/members", &[("x-member", "yes")], b"");
    assert_eq!(hit.header("x-unsworn-cache"), Some("hit"));
    let stranger = t.get("/members");
    assert_eq!(stranger.status, 403);
    assert_eq!(stranger.header("x-unsworn-cache"), None);
}

#[test]
fn cache_freshness_and_conditional_requests() {
    // A handler whose response carries the headers its name lists, and a
    // count of the calls so far as its body.
    let counter = Arc::new(AtomicU64::new(0));
    let answering = |headers: &'static [(&'static str, &'static str)]| {
        let n = counter.clone();
        move |_: &ProtoRequest, _: &Body<Bytes>| {
            let mut response = unsworn_proto::Response::new("", 200);
            for (k, v) in headers {
                let value = unsworn_proto::HeaderValues::One(v.to_string());
                response.headers.insert(k.to_string(), value);
            }
            response.text = Some(n.fetch_add(1, Ordering::SeqCst).to_string());
            Ok(Reply::Response {
                response,
                body: Body::Empty,
            })
        }
    };
    let mut host = TestHost::new()
        .handler(
            "src/h.ts#short",
            answering(&[("cache-control", "max-age=5"), ("etag", "\"v1\"")]),
        )
        .handler(
            "src/h.ts#long",
            answering(&[("cache-control", "max-age=3600")]),
        )
        .handler(
            "src/h.ts#stale",
            answering(&[("cache-control", "max-age=0")]),
        )
        .handler(
            "src/h.ts#aged",
            answering(&[
                ("age", "10"),
                ("last-modified", "Tue, 14 Nov 2023 22:00:00 GMT"),
            ]),
        );
    host.dev = true;
    let route = |name: &str| {
        format!(
            "[[route]]\npath = \"/{name}\"\nmethod = \"GET\"\nhandler = \"src/h.ts#{name}\"\ncache = {{ ttl = \"30s\" }}\n"
        )
    };
    let config: String = ["short", "long", "stale", "aged"].map(route).concat();
    let t = Test::new(&format!("{TS}\n{config}"), &["src/h.ts"], host);
    let cache = |path| t.get(path).header("x-unsworn-cache").map(String::from);
    let (hit, miss) = (Some("hit".to_string()), Some("miss".to_string()));

    // The response's own lifetime is shorter than the rule's `ttl`: it wins.
    assert_eq!(cache("/short"), miss);
    t.host.clock.advance(Duration::from_secs(4));
    assert_eq!(cache("/short"), hit);
    t.host.clock.advance(Duration::from_secs(1));
    assert_eq!(cache("/short"), miss, "stale after its max-age");
    // A longer one doesn't: `ttl` is the most any response gets.
    assert_eq!(cache("/long"), miss);
    t.host.clock.advance(Duration::from_secs(29));
    assert_eq!(cache("/long"), hit);
    t.host.clock.advance(Duration::from_secs(1));
    assert_eq!(cache("/long"), miss);
    // Stale on arrival: never stored.
    assert_eq!(cache("/stale"), None);
    assert_eq!(cache("/stale"), None);
    // Ten seconds old on arrival: twenty left, and `Age` keeps counting.
    assert_eq!(cache("/aged"), miss);
    t.host.clock.advance(Duration::from_secs(19));
    let r = t.get("/aged");
    assert_eq!(r.header("x-unsworn-cache"), Some("hit"));
    assert_eq!(r.header("age"), Some("29"));
    t.host.clock.advance(Duration::from_secs(1));
    assert_eq!(cache("/aged"), miss);

    // A client that already has the cached response gets `304`, no body.
    let fresh = t.get("/short");
    assert_eq!(fresh.header("x-unsworn-cache"), Some("miss"));
    let conditional = |path, name, value| t.send("GET", path, &[(name, value)], b"");
    let r = conditional("/short", "if-none-match", "\"v0\", W/\"v1\"");
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    assert!(r.body.is_empty());
    assert_eq!(r.header("etag"), Some("\"v1\""));
    assert_eq!(r.header("cache-control"), Some("max-age=5"));
    assert_eq!(r.header("content-type"), None);
    assert_eq!(r.header("age"), Some("0"));
    let r = conditional("/short", "if-none-match", "\"v2\"");
    assert_eq!((r.status, &r.body), (StatusCode::OK, &fresh.body));
    let r = t.send("HEAD", "/short", &[("if-none-match", "*")], b"");
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);

    let r = conditional(
        "/aged",
        "if-modified-since",
        "Tue, 14 Nov 2023 22:00:00 GMT",
    );
    assert_eq!(r.status, StatusCode::NOT_MODIFIED);
    assert_eq!(r.header("age"), Some("10"));
    let r = conditional(
        "/aged",
        "if-modified-since",
        "Tue, 14 Nov 2023 21:59:59 GMT",
    );
    assert_eq!(r.status, StatusCode::OK);
    // Only a cache hit is answered this way: a miss goes to the handler.
    t.host.clock.advance(Duration::from_secs(60));
    let r = conditional("/short", "if-none-match", "\"v1\"");
    assert_eq!(r.status, StatusCode::OK);
}

fn jwt(secret: &[u8], kid: Option<&str>, claims: Value) -> String {
    let mut header = Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = kid.map(String::from);
    let token = encode(&header, &claims, &EncodingKey::from_secret(secret).unwrap()).unwrap();
    format!("Bearer {token}")
}

#[test]
fn jwt_auth_with_a_fetched_jwks() {
    // An HS256 key as a JWK: the secret, base64url-encoded.
    let jwks =
        json!({ "keys": [{ "kty": "oct", "kid": "k1", "alg": "HS256", "k": "c2VjcmV0LW9uZQ" }] });
    let host = TestHost::new().handler("src/h.ts#me", echo).fetch_answer(
        "https://auth.example/jwks.json",
        200,
        jwks.to_string(),
    );
    let t = Test::new(
        &format!(
            r#"{TS}
[auth.users]
type = "jwt"
url = "https://auth.example/jwks.json"
algorithms = ["HS256"]
issuer = "https://auth.example/"

[[route]]
path = "/me"
method = "GET"
handler = "src/h.ts#me"
auth = {{ scheme = "users", scopes = ["profile"] }}
rate_limit = {{ requests = 1, per = "1m", key = "auth.sub" }}
"#
        ),
        &["src/h.ts"],
        host,
    );
    let exp = t.host.clock.now() / 1000 + 3600;
    let claims = |sub: &str, scope: &str| json!({ "sub": sub, "scope": scope, "iss": "https://auth.example/", "exp": exp });
    let me = |token: &str| t.send("GET", "/me", &[("authorization", token)], b"");

    let r = me(&jwt(b"secret-one", Some("k1"), claims("u1", "profile")));
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["context"]["auth"]["sub"], "u1");
    assert_eq!(r.json()["context"]["auth"]["scheme"], "users");
    assert_eq!(t.host.fetch_count(), 1);

    // The keys are kept; the rate limit is per subject.
    let r = me(&jwt(b"secret-one", Some("k1"), claims("u1", "profile")));
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    let r = me(&jwt(b"secret-one", Some("k1"), claims("u2", "profile")));
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(t.host.fetch_count(), 1);

    // A refetch that stalls holds up no one who already has keys.
    t.host.clock.advance(Duration::from_secs(601));
    t.host.stall_fetches.store(true, Ordering::SeqCst);
    let token = jwt(b"secret-one", Some("k1"), claims("u7", "profile"));
    let req = http::Request::get("/me")
        .header("authorization", &token)
        .body(Bytes::new())
        .unwrap();
    let mut stuck = Box::pin(t.router.handle(&t.host, req, IP.parse().unwrap()));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        stuck.as_mut().poll(&mut cx).is_pending(),
        "refreshing the JWKS"
    );
    let r = me(&jwt(b"secret-one", Some("k1"), claims("u8", "profile")));
    assert_eq!(r.status, StatusCode::OK, "served with the keys it had");
    drop(stuck);
    t.host.stall_fetches.store(false, Ordering::SeqCst);
    let fetches = t.host.fetch_count();

    // An unknown key id refetches, but at most once a minute.
    me(&jwt(b"secret-two", Some("k2"), claims("u5", "profile")));
    assert_eq!(
        t.host.fetch_count(),
        fetches,
        "fetched less than a minute ago"
    );
    t.host.clock.advance(Duration::from_secs(61));
    me(&jwt(b"secret-two", Some("k2"), claims("u5", "profile")));
    assert_eq!(t.host.fetch_count(), fetches + 1);

    // Requests that fail authentication have no subject to count by, so
    // they count against the client's address.
    let minute = Duration::from_secs(60);
    t.host.clock.advance(minute);
    let r = t.get("/me");
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(r.header("www-authenticate"), Some("Bearer"));
    let r = me(&jwt(b"wrong", Some("k1"), claims("u4", "profile")));
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "a second guess");
    let r = me(&jwt(b"secret-one", Some("k1"), claims("u6", "profile")));
    assert_eq!(r.status, StatusCode::OK, "a valid token has its own count");

    t.host.clock.advance(minute);
    let r = me(&jwt(b"secret-one", Some("k1"), claims("u3", "email")));
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(
        r.header("www-authenticate")
            .unwrap()
            .contains("insufficient_scope")
    );
    t.host.clock.advance(minute);
    let r = me(&jwt(b"wrong", Some("k1"), claims("u4", "profile")));
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
}

#[test]
fn a_rotated_jwt_secret_takes_effect_on_reload() {
    let config = |secret: &str| {
        format!(
            r#"
[auth.users]
type = "jwt"
secret = "{secret}"

[[route]]
path = "/me"
respond = {{ status = 200 }}
auth = "users"
"#
        )
    };
    // A host keeps one key cache across reloads.
    let keys = Arc::new(KeyCache::default());
    let before = Test::with_keys(&config("leaked"), &[], TestHost::new(), keys.clone());
    let exp = before.host.clock.now() / 1000 + 3600;
    let token = |secret: &[u8]| jwt(secret, None, json!({ "sub": "u1", "exp": exp }));
    let me = |t: &Test, token: &str| {
        t.send("GET", "/me", &[("authorization", token)], b"")
            .status
    };
    assert_eq!(me(&before, &token(b"leaked")), StatusCode::OK);

    let after = Test::with_keys(&config("fresh"), &[], TestHost::new(), keys);
    assert_eq!(me(&after, &token(b"leaked")), StatusCode::UNAUTHORIZED);
    assert_eq!(me(&after, &token(b"fresh")), StatusCode::OK);
    // Requests still on the old project keep its key.
    assert_eq!(me(&before, &token(b"leaked")), StatusCode::OK);
}

#[test]
fn rate_limits_count_requests_that_fail_auth() {
    let host = TestHost::new().fetch_answer(
        "https://keys.example/verify",
        200,
        json!({ "active": false }).to_string(),
    );
    let t = Test::new(
        r#"
[auth.customers]
type = "api_key"
url = "https://keys.example/verify"

[[route]]
path = "/report"
respond = { status = 200 }
auth = "customers"
rate_limit = { requests = 2, per = "1m" }
"#,
        &[],
        host,
    );
    let guess = |key: &str| t.send("GET", "/report", &[("x-api-key", key)], b"").status;
    assert_eq!(guess("sk_1"), StatusCode::UNAUTHORIZED);
    assert_eq!(guess("sk_2"), StatusCode::UNAUTHORIZED);
    assert_eq!(guess("sk_3"), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        t.host.fetch_count(),
        2,
        "the verifier isn't asked past the limit"
    );
}

#[test]
fn a_get_policy_covers_head() {
    let t = Test::new(
        r#"
[auth.key]
type = "api_key"
keys = ["k1"]

[[route]]
path = "/{*rest}"
respond = { status = 200 }

[[route]]
path = "/admin/{*rest}"
method = "GET"
auth = "key"

[[route]]
path = "/reports/daily"
method = "GET"
auth = "key"
"#,
        &[],
        TestHost::new(),
    );
    for path in ["/admin/users", "/reports/daily"] {
        for method in ["GET", "HEAD"] {
            let r = t.send(method, path, &[], b"");
            assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{method} {path}");
            let r = t.send(method, path, &[("x-api-key", "k1")], b"");
            assert_eq!(r.status, StatusCode::OK, "{method} {path}");
        }
        // The entry names `GET`, so other methods are outside it.
        assert_eq!(t.send("POST", path, &[], b"").status, StatusCode::OK);
    }
}

#[test]
fn remote_api_keys_with_a_verdict_cache() {
    let host = TestHost::new()
        .handler("src/h.ts#report", echo)
        .fetch_answer(
            "https://keys.example/verify",
            200,
            json!({ "active": true, "sub": "acct_1" }).to_string(),
        );
    let t = Test::new(
        &format!(
            r#"{TS}
[auth.customers]
type = "api_key"
header = "X-API-Key"
url = "https://keys.example/verify"
cache = "30s"

[auth.down]
type = "api_key"
url = "https://unreachable.example/verify"

[[route]]
path = "/report"
method = "GET"
handler = "src/h.ts#report"
auth = "customers"

[[route]]
path = "/down"
method = "GET"
handler = "src/h.ts#report"
auth = "down"
"#
        ),
        &["src/h.ts"],
        host,
    );
    let key = |path| t.send("GET", path, &[("x-api-key", "sk_1")], b"");
    let r = key("/report");
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    assert_eq!(r.json()["context"]["auth"]["sub"], "acct_1");
    assert_eq!(key("/report").status, StatusCode::OK);
    assert_eq!(t.host.fetch_count(), 1, "the verdict is cached");
    t.host.clock.advance(Duration::from_secs(31));
    key("/report");
    assert_eq!(t.host.fetch_count(), 2, "and expires");

    assert_eq!(key("/down").status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(t.get("/report").status, StatusCode::UNAUTHORIZED);
}

const MCP: &str = r#"
[mcp]
name = "test-tools"
version = "1.0.0"

[[mcp.tool]]
name = "weather"
description = "Weather for a city"
input = { type = "object", required = ["city"], properties = { city = { type = "string" } } }
handler = "tools/t.ts#weather"
rate_limit = { requests = 1, per = "1m" }

[[mcp.tool]]
name = "guarded"
description = "Behind a before handler"
handler = "tools/t.ts#weather"
before = ["tools/t.ts#deny"]
"#;

fn mcp_send(t: &Test, method: &str, name: Option<&str>, params: Value) -> Answer {
    let mut params = params;
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": { "name": "test-client", "version": "1" }
    });
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
    let mut headers = vec![
        ("content-type", "application/json"),
        ("mcp-protocol-version", "2026-07-28"),
        ("mcp-method", method),
    ];
    if let Some(n) = name {
        headers.push(("mcp-name", n));
    }
    t.send("POST", "/mcp", &headers, body.to_string().as_bytes())
}

#[test]
fn mcp_tools_as_handlers() {
    let host = TestHost::new()
        .handler("tools/t.ts#weather", |req, _| {
            ok_json(
                json!({ "city": req.json.as_ref().unwrap().to_value()["city"], "mcp": req.context["mcp"] }),
            )
        })
        .handler("tools/t.ts#deny", |_, _| {
            let mut response = unsworn_proto::Response::new("", 403);
            response.json = Some(json!({ "detail": "not on weekends" }).into());
            Ok(Reply::Response {
                response,
                body: Body::Empty,
            })
        });
    let t = Test::new(&format!("{TS}{MCP}"), &["tools/t.ts"], host);

    let r = mcp_send(&t, "tools/list", None, json!({}));
    assert_eq!(r.status, StatusCode::OK, "{:?}", r.body);
    let names: Vec<Value> = r.json()["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].clone())
        .collect();
    assert_eq!(names, [json!("guarded"), json!("weather")]);

    let call = |name: &str, args: Value| {
        mcp_send(
            &t,
            "tools/call",
            Some(name),
            json!({ "name": name, "arguments": args }),
        )
        .json()["result"]
            .clone()
    };
    let result = call("weather", json!({ "city": "Oslo" }));
    assert_eq!(result["isError"], json!(null), "{result}");
    assert_eq!(result["structuredContent"]["city"], "Oslo");
    assert_eq!(result["structuredContent"]["mcp"]["tool"], "weather");

    // Policy failures are tool errors the model can read.
    let text = |r: &Value| {
        r["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    };
    let limited = call("weather", json!({ "city": "Oslo" }));
    assert_eq!(limited["isError"], true);
    assert!(
        text(&limited).contains("Rate limit exceeded for weather"),
        "{limited}"
    );
    let bad = call("weather", json!({}));
    assert_eq!(bad["isError"], true);
    let denied = call("guarded", json!({}));
    assert_eq!(
        text(&denied),
        "The call was rejected (403 Forbidden): not on weekends"
    );

    let r = mcp_send(
        &t,
        "tools/call",
        Some("nope"),
        json!({ "name": "nope", "arguments": {} }),
    );
    assert_eq!(r.json()["error"]["code"], -32602);
    let r = mcp_send(&t, "resources/list", None, json!({}));
    assert_eq!(r.json()["error"]["code"], -32601);
}
