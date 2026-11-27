//! `[telemetry]`: a span for each sampled request, and for what the server
//! asks of something else for it, sent over OTLP/HTTP to an outbound
//! service. What's sent is decoded with OpenTelemetry's own message
//! definitions.

mod common;

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{Project, Server};
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::trace::v1::Span;
use opentelemetry_proto::tonic::trace::v1::span::SpanKind;
use opentelemetry_proto::tonic::trace::v1::status::StatusCode;
use prost::Message;

const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT: &str = "00f067aa0ba902b7";

/// A request a [`Fake`] received.
#[derive(Debug, Clone)]
struct Received {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

/// A server that keeps every request it gets and gives them all the same
/// answer: a collector, an app behind a `proxy` route, a service.
struct Fake {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Received>>>,
}

impl Fake {
    fn start() -> Fake {
        Fake::answering("200 OK", "", "")
    }

    /// `headers` are whole lines, each ending in `\r\n`.
    fn answering(status: &str, headers: &str, body: &str) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Received>>> = Arc::default();
        let log = seen.clone();
        let answer = format!(
            "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\n\r\n{body}",
            body.len()
        );
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (log, answer) = (log.clone(), answer.clone());
                std::thread::spawn(move || serve(stream, &log, &answer));
            }
        });
        Fake { addr, seen }
    }

    fn seen(&self) -> Vec<Received> {
        self.seen.lock().unwrap().clone()
    }

    /// The last request for `path`.
    fn request(&self, path: &str) -> Received {
        let seen = self.seen();
        seen.iter()
            .rfind(|r| r.path == path)
            .unwrap_or_else(|| panic!("nothing asked for {path}: {seen:?}"))
            .clone()
    }

    /// Every span exported here, in the order they were sent.
    fn spans(&self) -> Vec<Span> {
        self.seen()
            .iter()
            .flat_map(|r| decode(r).resource_spans)
            .flat_map(|r| r.scope_spans)
            .flat_map(|s| s.spans)
            .collect()
    }
}

fn serve(mut stream: TcpStream, log: &Mutex<Vec<Received>>, answer: &str) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        // One request: headers, then `content-length` bytes of body.
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut lines = head.lines();
        let mut first = lines.next().unwrap_or_default().split(' ');
        let method = first.next().unwrap_or_default().to_string();
        let path = first.next().unwrap_or_default().to_string();
        let headers: BTreeMap<String, String> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
            .collect();
        let length: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        while buf.len() < head_end + length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let body = buf[head_end..head_end + length].to_vec();
        buf.drain(..head_end + length);
        log.lock().unwrap().push(Received {
            method,
            path,
            headers,
            body,
        });
        if stream.write_all(answer.as_bytes()).is_err() {
            return;
        }
    }
}

fn decode(request: &Received) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest::decode(request.body.as_slice()).expect("an OTLP trace request")
}

fn values(attributes: &[KeyValue]) -> BTreeMap<String, Value> {
    attributes
        .iter()
        .map(|kv| {
            let value = kv.value.as_ref().and_then(|v| v.value.clone());
            (kv.key.clone(), value.expect("a value"))
        })
        .collect()
}

fn string(value: &str) -> Value {
    Value::StringValue(value.into())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `traceparent` that makes `span` the parent of what gets it.
fn traceparent(span: &Span) -> String {
    format!("00-{}-{}-01", hex(&span.trace_id), hex(&span.span_id))
}

fn of_kind(spans: &[Span], kind: SpanKind) -> Vec<&Span> {
    spans.iter().filter(|s| s.kind == kind as i32).collect()
}

/// The one span named `name` right below `parent`.
fn below<'a>(spans: &'a [Span], parent: &Span, name: &str) -> &'a Span {
    let found: Vec<&Span> = spans
        .iter()
        .filter(|s| s.parent_span_id == parent.span_id && s.name == name)
        .collect();
    let all: Vec<&str> = spans.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(found.len(), 1, "{name} below {}: {all:?}", parent.name);
    assert_eq!(found[0].trace_id, parent.trace_id);
    found[0]
}

/// Why a span failed.
fn failure(span: &Span) -> &str {
    let status = span.status.as_ref().expect("a failed span");
    assert_eq!(status.code, StatusCode::Error as i32);
    &status.message
}

/// A project with a `respond` route and a `proxy` route, which sends spans
/// to `collector`.
fn project(collector: &Fake, upstream: &Fake, sample: &str) -> Project {
    let config = format!(
        r#"
[outbound.otel]
url = "http://{}"
headers = {{ Authorization = "Bearer t" }}

[telemetry]
outbound = "otel"
service_name = "api"
sample = {sample}

[[route]]
path = "/hello"
respond = {{ status = 200, json = {{ ok = true }} }}

[[route]]
path = "/up/{{*rest}}"
proxy = "http://{}"
"#,
        collector.addr, upstream.addr
    );
    Project::new(&[("unsworn.toml", &config)])
}

fn get(server: &Server, path: &str, headers: &[(&str, &str)]) {
    let response = server.request("GET", path, headers, b"");
    assert_eq!(response.status, 200, "{}", server.logs());
}

#[test]
fn sampled_requests_are_exported_as_server_spans() {
    let (collector, upstream) = (Fake::start(), Fake::start());
    let project = project(&collector, &upstream, "1.0");
    let before = SystemTime::now();
    let mut server = project.serve();
    get(&server, "/hello?token=secret", &[("x-request-id", "first")]);
    // A trace that starts here doesn't keep a `tracestate` that came alone.
    get(&server, "/up/new", &[("tracestate", "vendor=1")]);
    assert_eq!(server.get("/nowhere").status, 404);
    server.terminate();

    let sent = collector.seen();
    assert!(!sent.is_empty(), "{}", server.logs());
    for request in &sent {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/v1/traces");
        assert_eq!(request.headers["authorization"], "Bearer t");
        assert_eq!(request.headers["content-type"], "application/x-protobuf");
    }
    let exported = decode(&sent[0]);
    let resource = values(
        &exported.resource_spans[0]
            .resource
            .as_ref()
            .unwrap()
            .attributes,
    );
    assert_eq!(resource["service.name"], string("api"));
    assert_eq!(resource["telemetry.sdk.name"], string("unsworn"));
    let scope = exported.resource_spans[0].scope_spans[0].scope.as_ref();
    assert_eq!(scope.unwrap().name, "unsworn");

    let spans = collector.spans();
    let servers = of_kind(&spans, SpanKind::Server);
    let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["GET /hello", "GET /up/{*rest}", "GET"]);
    // Only the proxied one asked anything of anyone.
    assert_eq!(of_kind(&spans, SpanKind::Client).len(), 1);

    let hello = servers[0];
    assert_eq!(hello.trace_id.len(), 16);
    assert_eq!(hello.span_id.len(), 8);
    assert!(hello.parent_span_id.is_empty());
    assert!(hello.status.is_none());
    let nanos = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64;
    // A second's leeway for a clock that's adjusted while this runs.
    assert!(hello.start_time_unix_nano >= nanos(before - Duration::from_secs(1)));
    assert!(hello.end_time_unix_nano >= hello.start_time_unix_nano);
    assert!(hello.end_time_unix_nano <= nanos(SystemTime::now() + Duration::from_secs(1)));
    let mut got = values(&hello.attributes);
    let Some(Value::DoubleValue(waited)) = got.remove("unsworn.in_handlers_ms") else {
        panic!("{got:?}");
    };
    assert_eq!(waited, 0.0);
    let expected = [
        ("client.address", string("127.0.0.1")),
        ("http.request.method", string("GET")),
        ("http.response.status_code", Value::IntValue(200)),
        ("http.route", string("/hello")),
        ("network.protocol.version", string("1.1")),
        ("unsworn.request_id", string("first")),
        // Without the query string.
        ("url.path", string("/hello")),
        ("url.scheme", string("http")),
    ];
    assert_eq!(
        got,
        expected
            .map(|(k, v)| (k.to_string(), v))
            .into_iter()
            .collect()
    );

    let forwarded = below(&spans, servers[1], "GET");
    assert!(forwarded.status.is_none());
    let expected = [
        ("http.request.method", string("GET")),
        ("http.response.status_code", Value::IntValue(200)),
        (
            "unsworn.upstream",
            string(&format!("http://{}", upstream.addr)),
        ),
        ("url.path", string("/up/new")),
    ];
    assert_eq!(
        values(&forwarded.attributes),
        expected
            .map(|(k, v)| (k.to_string(), v))
            .into_iter()
            .collect()
    );
    let proxied = upstream.request("/up/new");
    assert_eq!(proxied.headers["traceparent"], traceparent(forwarded));
    assert!(!proxied.headers.contains_key("tracestate"));

    // No route matched, and a client's error doesn't fail the span.
    let missing = values(&servers[2].attributes);
    assert!(!missing.contains_key("http.route"));
    assert_eq!(missing["http.response.status_code"], Value::IntValue(404));
    assert!(servers[2].status.is_none());
}

#[test]
fn a_failed_request_fails_its_span() {
    let collector = Fake::start();
    let config = format!(
        r#"
[outbound.otel]
url = "http://{}"

[telemetry]
outbound = "otel"
sample = 1

[[route]]
path = "/down"
proxy = "http://127.0.0.1:{}"
"#,
        collector.addr,
        common::free_port()
    );
    let project = Project::new(&[("unsworn.toml", &config)]);
    let mut server = project.serve();
    assert_eq!(server.get("/down").status, 502);
    server.terminate();
    let spans = collector.spans();
    assert_eq!(spans.len(), 2, "{}", server.logs());
    let servers = of_kind(&spans, SpanKind::Server);
    assert_eq!(failure(servers[0]), "answered 502");
    let forwarded = below(&spans, servers[0], "GET");
    assert!(!failure(forwarded).is_empty());
    assert!(!values(&forwarded.attributes).contains_key("http.response.status_code"));
}

#[test]
fn what_runs_out_of_time_is_still_recorded() {
    let collector = Fake::start();
    // Accepts connections and never answers.
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = format!(
        r#"
[outbound.otel]
url = "http://{}"

[telemetry]
outbound = "otel"
sample = 1

[[route]]
path = "/slow"
proxy = "http://{}"
timeout = "200ms"
"#,
        collector.addr,
        silent.local_addr().unwrap()
    );
    let project = Project::new(&[("unsworn.toml", &config)]);
    let mut server = project.serve();
    assert_eq!(server.get("/slow").status, 504);
    server.terminate();
    let spans = collector.spans();
    assert_eq!(spans.len(), 2, "{}", server.logs());
    let servers = of_kind(&spans, SpanKind::Server);
    let forwarded = below(&spans, servers[0], "GET");
    assert!(failure(forwarded).starts_with("cancelled"));
}

#[test]
fn a_request_the_client_gives_up_on_is_recorded() {
    let collector = Fake::start();
    // Accepts a connection and never answers.
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let config = format!(
        r#"
[outbound.otel]
url = "http://{}"

[telemetry]
outbound = "otel"
sample = 1

[[route]]
path = "/slow"
proxy = "http://{}"
"#,
        collector.addr,
        silent.local_addr().unwrap()
    );
    let project = Project::new(&[("unsworn.toml", &config)]);
    let mut server = project.serve();
    let mut client = TcpStream::connect(server.addr).unwrap();
    client
        .write_all(b"GET /slow HTTP/1.1\r\nHost: unsworn\r\n\r\n")
        .unwrap();
    let (mut upstream, _) = silent.accept().unwrap();
    upstream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    // The request reaches the upstream, and then the client leaves.
    let mut chunk = [0; 1024];
    assert!(upstream.read(&mut chunk).unwrap() > 0);
    drop(client);
    // The server gives up too, and closes its connection to the upstream.
    while upstream.read(&mut chunk).expect("a closed connection") > 0 {}
    server.terminate();

    let spans = collector.spans();
    assert_eq!(spans.len(), 2, "{}", server.logs());
    let servers = of_kind(&spans, SpanKind::Server);
    assert_eq!(servers.len(), 1);
    assert!(failure(servers[0]).starts_with("cancelled"));
    assert!(!values(&servers[0].attributes).contains_key("http.response.status_code"));
    let forwarded = below(&spans, servers[0], "GET");
    assert!(failure(forwarded).starts_with("cancelled"));
}

#[test]
fn a_verifier_request_is_recorded() {
    let collector = Fake::start();
    let verifier = Fake::answering(
        "200 OK",
        "content-type: application/json\r\n",
        r#"{"active":true}"#,
    );
    let config = format!(
        r#"
[outbound.otel]
url = "http://{}"

[telemetry]
outbound = "otel"
sample = 1

[auth.customers]
type = "api_key"
url = "http://{}/verify?tenant=a"

[[route]]
path = "/reports"
auth = "customers"
respond = {{ status = 200 }}
"#,
        collector.addr, verifier.addr
    );
    let project = Project::new(&[("unsworn.toml", &config)]);
    let mut server = project.serve();
    get(&server, "/reports", &[("x-api-key", "good")]);
    server.terminate();

    let spans = collector.spans();
    let servers = of_kind(&spans, SpanKind::Server);
    assert_eq!(servers.len(), 1, "{}", server.logs());
    let asked = below(&spans, servers[0], "POST");
    assert_eq!(asked.kind, SpanKind::Client as i32);
    let attributes = values(&asked.attributes);
    assert_eq!(attributes["server.address"], string("127.0.0.1"));
    // Without the query string.
    assert_eq!(attributes["url.path"], string("/verify"));
    assert_eq!(
        attributes["http.response.status_code"],
        Value::IntValue(200)
    );
    let seen = verifier.request("/verify?tenant=a");
    assert_eq!(seen.headers["traceparent"], traceparent(asked));
}

/// A `before` handler, a handler that calls a function and fetches from a
/// service, and the function.
const HANDLERS: &str = r#"
from unsworn import json, next

def gate(req, ctx):
    return next()

def search(req, ctx):
    inner = ctx.call("inner", {})
    ctx.fetch("echo", "/seen?q=1")
    return json(inner)

def inner(req, ctx):
    return json([req.headers.get("traceparent"), req.headers.get("tracestate")])
"#;

#[test]
fn a_handlers_work_nests_under_it() {
    if !common::has("python3") {
        eprintln!("skipping: python3 not found");
        return;
    }
    let (collector, echo) = (Fake::start(), Fake::start());
    let config = format!(
        r#"
[runtimes.py]
extensions = [".py"]
command = "python3 -u -m unsworn.worker"
protocol = "framed"

[outbound.otel]
url = "http://{}"

[outbound.echo]
url = "http://{}"

[telemetry]
outbound = "otel"
sample = 1

[[route]]
path = "/search"
handler = "h.py#search"
before = ["h.py#gate"]

[[function]]
name = "inner"
handler = "h.py#inner"
"#,
        collector.addr, echo.addr
    );
    let project = Project::new(&[("unsworn.toml", &config), ("h.py", HANDLERS)]);
    let mut server = project.serve();
    let incoming = format!("00-{TRACE}-{PARENT}-01");
    let response = server.request(
        "GET",
        "/search",
        &[("traceparent", &incoming), ("tracestate", "vendor=1")],
        b"",
    );
    assert_eq!(response.status, 200, "{}", server.logs());
    server.terminate();

    let spans = collector.spans();
    assert_eq!(spans.len(), 5, "{}", server.logs());
    let servers = of_kind(&spans, SpanKind::Server);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "GET /search");
    assert_eq!(hex(&servers[0].trace_id), TRACE);
    let gate = below(&spans, servers[0], "before h.py#gate");
    let search = below(&spans, servers[0], "handler h.py#search");
    let call = below(&spans, search, "call inner");
    let fetch = below(&spans, search, "GET");
    for span in [gate, search, call, fetch] {
        assert_eq!(span.kind, SpanKind::Client as i32, "{}", span.name);
        assert!(span.status.is_none(), "{}", span.name);
    }

    let attributes = values(&search.attributes);
    assert_eq!(attributes["unsworn.handler"], string("h.py#search"));
    assert_eq!(attributes["unsworn.runtime"], string("py"));
    assert_eq!(
        attributes["http.response.status_code"],
        Value::IntValue(200)
    );
    assert!(matches!(
        attributes["unsworn.wait_ms"],
        Value::DoubleValue(ms) if ms >= 0.0
    ));

    let attributes = values(&call.attributes);
    assert_eq!(attributes["unsworn.function"], string("inner"));
    assert_eq!(attributes["unsworn.caller"], string("h.py#search"));
    assert_eq!(attributes["unsworn.handler"], string("h.py#inner"));
    assert_eq!(attributes["unsworn.runtime"], string("py"));
    assert_eq!(attributes["unsworn.status"], Value::IntValue(200));
    assert!(attributes.contains_key("unsworn.wait_ms"));
    // The function was told the call is its parent.
    assert_eq!(
        response.json(),
        serde_json::json!([traceparent(call), "vendor=1"])
    );

    let attributes = values(&fetch.attributes);
    assert_eq!(attributes["unsworn.service"], string("echo"));
    assert_eq!(attributes["server.address"], string("127.0.0.1"));
    assert_eq!(attributes["url.path"], string("/seen"));
    assert_eq!(
        attributes["http.response.status_code"],
        Value::IntValue(200)
    );
    let seen = echo.request("/seen?q=1");
    assert_eq!(seen.headers["traceparent"], traceparent(fetch));
    assert_eq!(seen.headers["tracestate"], "vendor=1");
}

#[test]
fn a_sampled_traceparent_is_the_spans_parent() {
    let (collector, upstream) = (Fake::start(), Fake::start());
    // `sample` is for requests that arrive without one.
    let project = project(&collector, &upstream, "0.0");
    let mut server = project.serve();
    let incoming = format!("00-{TRACE}-{PARENT}-01");
    get(
        &server,
        "/up/traced",
        &[("traceparent", &incoming), ("tracestate", "vendor=1")],
    );
    get(&server, "/up/untraced", &[]);
    server.terminate();

    let spans = collector.spans();
    assert_eq!(spans.len(), 2, "{}", server.logs());
    let servers = of_kind(&spans, SpanKind::Server);
    assert_eq!(servers.len(), 1);
    assert_eq!(hex(&servers[0].trace_id), TRACE);
    assert_eq!(hex(&servers[0].parent_span_id), PARENT);
    assert_ne!(hex(&servers[0].span_id), PARENT);
    let forwarded = below(&spans, servers[0], "GET");
    let proxied = upstream.request("/up/traced");
    assert_eq!(proxied.headers["traceparent"], traceparent(forwarded));
    assert_eq!(proxied.headers["tracestate"], "vendor=1");

    // The access log names the trace of a request that was recorded.
    let logs = server.logs();
    let line = |path: &str| {
        let found = format!("path={path} ");
        logs.lines()
            .find(|l| l.contains(&found))
            .unwrap_or_else(|| panic!("no line for {path}:\n{logs}"))
    };
    assert!(
        line("/up/traced").ends_with(&format!(" trace_id={TRACE}")),
        "{logs}"
    );
    assert!(!line("/up/untraced").contains("trace_id"), "{logs}");
}

#[test]
fn an_unsampled_request_is_left_as_it_came() {
    let (collector, upstream) = (Fake::start(), Fake::start());
    let project = project(&collector, &upstream, "1.0");
    let mut server = project.serve();
    let incoming = format!("00-{TRACE}-{PARENT}-00");
    get(
        &server,
        "/up/unsampled",
        &[("traceparent", &incoming), ("tracestate", "vendor=1")],
    );
    server.terminate();
    assert!(collector.seen().is_empty(), "{:?}", collector.seen());
    let proxied = upstream.request("/up/unsampled");
    assert_eq!(proxied.headers["traceparent"], incoming);
    assert_eq!(proxied.headers["tracestate"], "vendor=1");
}

#[test]
fn nothing_is_made_up_for_a_request_that_isnt_sampled() {
    let (collector, upstream) = (Fake::start(), Fake::start());
    let project = project(&collector, &upstream, "0.0");
    let mut server = project.serve();
    get(&server, "/up/plain", &[]);
    server.terminate();
    assert!(collector.seen().is_empty(), "{:?}", collector.seen());
    assert!(
        !upstream
            .request("/up/plain")
            .headers
            .contains_key("traceparent")
    );
}

#[test]
fn a_redirect_isnt_followed() {
    let (elsewhere, upstream) = (Fake::start(), Fake::start());
    let collector = Fake::answering(
        "307 Temporary Redirect",
        &format!("location: http://{}/v1/traces\r\n", elsewhere.addr),
        "",
    );
    let project = project(&collector, &upstream, "1.0");
    let mut server = project.serve();
    // A full batch, which is sent without waiting.
    for _ in 0..512 {
        get(&server, "/hello", &[]);
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while !server.logs().contains("answered 307") {
        assert!(Instant::now() < deadline, "{}", server.logs());
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(collector.spans().len(), 512);
    assert!(elsewhere.seen().is_empty(), "{:?}", elsewhere.seen());
    get(&server, "/hello", &[]);
    server.terminate();
    assert!(elsewhere.seen().is_empty(), "{:?}", elsewhere.seen());
}
