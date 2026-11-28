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
use opentelemetry_proto::tonic::collector::logs::v1::ExportLogsServiceRequest;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::logs::v1::{LogRecord, SeverityNumber};
use opentelemetry_proto::tonic::metrics::v1::{
    AggregationTemporality, Metric, metric::Data as MetricData, number_data_point::Value as NumVal,
};
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
            .filter(|r| r.path == "/v1/traces")
            .flat_map(|r| decode(r).resource_spans)
            .flat_map(|r| r.scope_spans)
            .flat_map(|s| s.spans)
            .collect()
    }

    /// Every metric exported here, in the order they were sent.
    fn metrics(&self) -> Vec<Metric> {
        self.seen()
            .iter()
            .filter(|r| r.path == "/v1/metrics")
            .flat_map(|r| {
                ExportMetricsServiceRequest::decode(r.body.as_slice())
                    .expect("an OTLP metrics request")
                    .resource_metrics
            })
            .flat_map(|r| r.scope_metrics)
            .flat_map(|s| s.metrics)
            .collect()
    }

    /// Every log record exported here, in the order they were sent.
    fn log_records(&self) -> Vec<LogRecord> {
        self.seen()
            .iter()
            .filter(|r| r.path == "/v1/logs")
            .flat_map(|r| {
                ExportLogsServiceRequest::decode(r.body.as_slice())
                    .expect("an OTLP logs request")
                    .resource_logs
            })
            .flat_map(|r| r.scope_logs)
            .flat_map(|s| s.log_records)
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

/// The names of a span's events, and the attributes of each.
fn events(span: &Span) -> Vec<(String, BTreeMap<String, Value>)> {
    let mut last = span.start_time_unix_nano;
    span.events
        .iter()
        .map(|e| {
            assert!(e.time_unix_nano >= last, "{} out of order", e.name);
            assert!(e.time_unix_nano <= span.end_time_unix_nano, "{}", e.name);
            last = e.time_unix_nano;
            (e.name.clone(), values(&e.attributes))
        })
        .collect()
}

fn names(events: &[(String, BTreeMap<String, Value>)]) -> Vec<&str> {
    events.iter().map(|(name, _)| name.as_str()).collect()
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
    project_with("", collector, upstream, sample)
}

/// [`project`], taking its clients' word for whether they're sampled.
fn trusting(collector: &Fake, upstream: &Fake, sample: &str) -> Project {
    let server = "[server]\ntrusted_proxies = [\"127.0.0.1\"]";
    project_with(server, collector, upstream, sample)
}

fn project_with(server: &str, collector: &Fake, upstream: &Fake, sample: &str) -> Project {
    let config = format!(
        r#"
{server}

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
    let attributes = values(&servers[0].attributes);
    assert_eq!(attributes["unsworn.rejected_by"], string("timeout"));
}

#[test]
fn the_requests_span_says_what_the_pipeline_decided() {
    let (collector, upstream) = (Fake::start(), Fake::start());
    let config = format!(
        r#"
[outbound.otel]
url = "http://{}"

[telemetry]
outbound = "otel"
sample = 1

[[route]]
path = "/limited"
rate_limit = {{ requests = 1, per = "1h", key = "ip" }}
respond = {{ status = 204 }}

[[route]]
path = "/items/{{id}}"
params = {{ id = {{ type = "integer" }} }}
respond = {{ status = 204 }}

[[route]]
path = "/cached"
method = "GET"
cache = {{ ttl = "1m" }}
proxy = "http://{}"
"#,
        collector.addr, upstream.addr
    );
    let project = Project::new(&[("unsworn.toml", &config)]);
    let mut server = project.serve();
    for (path, status) in [
        ("/limited", 204),
        ("/limited", 429),
        ("/items/x", 400),
        ("/cached", 200),
        ("/cached", 200),
    ] {
        assert_eq!(server.get(path).status, status, "{path}");
    }
    server.terminate();

    let spans = collector.spans();
    let servers = of_kind(&spans, SpanKind::Server);
    assert_eq!(servers.len(), 5, "{}", server.logs());
    let decided = |span: &Span, key: &str| values(&span.attributes).get(key).cloned();

    let allowed = events(servers[0]);
    assert_eq!(names(&allowed), ["unsworn.checked"]);
    assert_eq!(decided(servers[0], "unsworn.rejected_by"), None);

    let limited = events(servers[1]);
    assert_eq!(names(&limited), ["unsworn.rejected"]);
    assert_eq!(limited[0].1["unsworn.rule"], string("rate_limit"));
    assert_eq!(
        decided(servers[1], "unsworn.rejected_by"),
        Some(string("rate_limit"))
    );
    // A client's error, not the server's.
    assert!(servers[1].status.is_none());

    assert_eq!(
        decided(servers[2], "unsworn.rejected_by"),
        Some(string("validation"))
    );

    let missed = events(servers[3]);
    assert_eq!(
        names(&missed),
        ["unsworn.checked", "unsworn.cache", "unsworn.cache"]
    );
    assert_eq!(missed[1].1["unsworn.cache"], string("miss"));
    assert_eq!(decided(servers[3], "unsworn.cache"), Some(string("stored")));
    assert_eq!(decided(servers[4], "unsworn.cache"), Some(string("hit")));
    // A hit asked nothing of the upstream.
    assert_eq!(upstream.seen().len(), 1);
    assert_eq!(of_kind(&spans, SpanKind::Client).len(), 1);
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
    let attributes = values(&servers[0].attributes);
    assert_eq!(attributes["unsworn.auth.scheme"], string("customers"));
    let steps = events(servers[0]);
    assert_eq!(names(&steps), ["unsworn.authenticated", "unsworn.checked"]);
    // The verifier was asked before auth was done.
    assert!(asked.end_time_unix_nano <= servers[0].events[0].time_unix_nano);
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
    // `sample` isn't for what a trusted proxy marks.
    let project = trusting(&collector, &upstream, "0.0");
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
    let project = trusting(&collector, &upstream, "1.0");
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
fn a_client_cant_choose_to_be_recorded() {
    let (collector, upstream) = (Fake::start(), Fake::start());
    let none = project(&collector, &upstream, "0.0");
    let mut server = none.serve();
    let marked = format!("00-{TRACE}-{PARENT}-01");
    get(
        &server,
        "/up/marked",
        &[("traceparent", &marked), ("tracestate", "vendor=1")],
    );
    server.terminate();
    assert!(collector.seen().is_empty(), "{:?}", collector.seen());
    // What's behind the server isn't told to record it either.
    let proxied = upstream.request("/up/marked");
    assert_eq!(
        proxied.headers["traceparent"],
        format!("00-{TRACE}-{PARENT}-00")
    );
    assert_eq!(proxied.headers["tracestate"], "vendor=1");

    // Nor can a client keep its request from being recorded.
    let all = project(&collector, &upstream, "1.0");
    let mut server = all.serve();
    let unmarked = format!("00-{TRACE}-{PARENT}-00");
    get(&server, "/up/unmarked", &[("traceparent", &unmarked)]);
    server.terminate();
    let spans = collector.spans();
    let servers = of_kind(&spans, SpanKind::Server);
    assert_eq!(servers.len(), 1, "{}", server.logs());
    // Its span is still in the client's trace.
    assert_eq!(hex(&servers[0].trace_id), TRACE);
    assert_eq!(hex(&servers[0].parent_span_id), PARENT);
    let forwarded = below(&spans, servers[0], "GET");
    let proxied = upstream.request("/up/unmarked");
    assert_eq!(proxied.headers["traceparent"], traceparent(forwarded));
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

/// A handler that names a span of its own for what it calls and fetches.
const OWN_SPAN: &str = r#"
from unsworn import json

def search(req, ctx):
    trace = req.headers["traceparent"].split("-")[1]
    own = f"00-{trace}-1111111111111111-01"
    mine = ctx.call("inner", {}, traceparent=own, tracestate="mine=1")
    ctx.fetch("echo", "/own", headers={"traceparent": own, "tracestate": "mine=1"})
    # Not marked sampled.
    unsampled = f"00-{trace}-2222222222222222-00"
    quiet = ctx.call("inner", {}, traceparent=unsampled, tracestate="mine=2")
    ctx.fetch("echo", "/quiet", headers={"Traceparent": unsampled})
    # Not one a receiver would accept.
    ignored = ctx.call("inner", {}, traceparent="nonsense", tracestate="mine=3")
    return json({"own": own, "unsampled": unsampled, "mine": mine, "quiet": quiet, "ignored": ignored})

def inner(req, ctx):
    return json([req.headers.get("traceparent"), req.headers.get("tracestate")])
"#;

#[test]
fn a_handlers_own_span_can_parent_its_calls() {
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

[[function]]
name = "inner"
handler = "h.py#inner"
"#,
        collector.addr, echo.addr
    );
    let project = Project::new(&[("unsworn.toml", &config), ("h.py", OWN_SPAN)]);
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
    let said = response.json();

    // The request, its handler, two calls and one fetch: nothing for what
    // was asked under a span that isn't sampled.
    let spans = collector.spans();
    assert_eq!(spans.len(), 5, "{}", server.logs());
    let servers = of_kind(&spans, SpanKind::Server);
    let search = below(&spans, servers[0], "handler h.py#search");
    // The handler's own span, which it records itself.
    let own = |name: &str| {
        let found: Vec<&Span> = spans
            .iter()
            .filter(|s| s.name == name && hex(&s.parent_span_id) == "1111111111111111")
            .collect();
        assert_eq!(found.len(), 1, "{name}");
        assert_eq!(hex(&found[0].trace_id), TRACE);
        found[0]
    };

    let call = own("call inner");
    assert_eq!(
        said["mine"],
        serde_json::json!([traceparent(call), "mine=1"])
    );
    let fetch = own("GET");
    let seen = echo.request("/own");
    assert_eq!(seen.headers["traceparent"], traceparent(fetch));
    assert_eq!(seen.headers["tracestate"], "mine=1");

    // Passed on as they came.
    assert_eq!(
        said["quiet"],
        serde_json::json!([said["unsampled"], "mine=2"])
    );
    let seen = echo.request("/quiet");
    assert_eq!(seen.headers["traceparent"], said["unsampled"]);
    assert!(!seen.headers.contains_key("tracestate"));

    // Ignored: the call is under the handler's call, as any other.
    let call = below(&spans, search, "call inner");
    assert_eq!(
        said["ignored"],
        serde_json::json!([traceparent(call), "vendor=1"])
    );
}

#[test]
fn metrics_are_exported_over_otlp() {
    let collector = Fake::start();
    let py = common::has("python3");
    let runtime = if py {
        r#"
[runtimes.py]
extensions = [".py"]
command = "python3 -u -m unsworn.worker"
protocol = "framed"

[[route]]
path = "/work"
handler = "h.py#work"
"#
    } else {
        ""
    };
    let config = format!(
        r#"
[outbound.otel]
url = "http://{}"
headers = {{ Authorization = "Bearer m" }}
methods = ["POST"]
paths = ["/v1/traces", "/v1/metrics"]

[telemetry]
outbound = "otel"
service_name = "api"
metrics = true

[[route]]
path = "/hello"
respond = {{ status = 200, json = {{ ok = true }} }}
{runtime}
"#,
        collector.addr
    );
    let mut files: Vec<(&str, &str)> = vec![("unsworn.toml", &config)];
    if py {
        files.push((
            "h.py",
            "from unsworn import json\ndef work(req, ctx):\n    return json({'ok': True})\n",
        ));
    }
    let project = Project::new(&files);
    let mut server = project.serve();
    get(&server, "/hello", &[]);
    assert_eq!(server.get("/missing").status, 404);
    if py {
        get(&server, "/work", &[]);
    }
    server.terminate();

    let requests: Vec<Received> = collector
        .seen()
        .into_iter()
        .filter(|r| r.path == "/v1/metrics")
        .collect();
    assert!(!requests.is_empty(), "{}", server.logs());
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].headers["authorization"], "Bearer m");
    assert_eq!(
        requests[0].headers["content-type"],
        "application/x-protobuf"
    );

    let exported = ExportMetricsServiceRequest::decode(requests[0].body.as_slice())
        .expect("an OTLP metrics request");
    let resource = values(
        &exported.resource_metrics[0]
            .resource
            .as_ref()
            .unwrap()
            .attributes,
    );
    assert_eq!(resource["service.name"], string("api"));

    let metrics = collector.metrics();
    let by_name = |name: &str| {
        metrics
            .iter()
            .find(|m| m.name == name)
            .unwrap_or_else(|| panic!("no metric {name}: {:?}", metrics.iter().map(|m| &m.name).collect::<Vec<_>>()))
    };

    // http.server.request.duration histogram
    let dur = by_name("http.server.request.duration");
    assert_eq!(dur.unit, "s");
    let Some(MetricData::Histogram(hist)) = &dur.data else {
        panic!("expected histogram: {dur:?}");
    };
    assert_eq!(
        hist.aggregation_temporality,
        AggregationTemporality::Cumulative as i32
    );
    let hello_dp = hist
        .data_points
        .iter()
        .find(|dp| values(&dp.attributes).get("http.route") == Some(&string("/hello")))
        .expect("/hello data point");
    assert_eq!(hello_dp.count, 1);
    assert_eq!(hello_dp.bucket_counts.len(), hello_dp.explicit_bounds.len() + 1);
    assert_eq!(hello_dp.bucket_counts.iter().sum::<u64>(), 1);
    let hello_attrs = values(&hello_dp.attributes);
    assert_eq!(hello_attrs["http.request.method"], string("GET"));
    assert_eq!(hello_attrs["http.response.status_code"], Value::IntValue(200));

    // Unmatched route omits http.route
    let missing_dp = hist
        .data_points
        .iter()
        .find(|dp| values(&dp.attributes).get("http.response.status_code") == Some(&Value::IntValue(404)))
        .expect("404 data point");
    assert!(!values(&missing_dp.attributes).contains_key("http.route"));

    // Store metrics
    let store_up = by_name("unsworn.store.up");
    let Some(MetricData::Gauge(gauge)) = &store_up.data else {
        panic!("expected gauge: {store_up:?}");
    };
    assert_eq!(gauge.data_points.len(), 1);
    assert_eq!(
        values(&gauge.data_points[0].attributes)["unsworn.store.backend"],
        string("memory")
    );
    assert_eq!(gauge.data_points[0].value, Some(NumVal::AsInt(1)));

    let store_err = by_name("unsworn.store.errors");
    let Some(MetricData::Sum(sum)) = &store_err.data else {
        panic!("expected sum: {store_err:?}");
    };
    assert!(sum.is_monotonic);
    assert_eq!(
        sum.aggregation_temporality,
        AggregationTemporality::Cumulative as i32
    );
    assert_eq!(sum.data_points[0].value, Some(NumVal::AsInt(0)));

    if py {
        let handler_time = by_name("unsworn.handler.time");
        assert_eq!(handler_time.unit, "s");
        let Some(MetricData::Sum(sum)) = &handler_time.data else {
            panic!("expected sum: {handler_time:?}");
        };
        assert!(sum.is_monotonic);
        // Only /work waited for a handler; /hello and /missing are omitted.
        assert_eq!(sum.data_points.len(), 1);
        assert_eq!(
            values(&sum.data_points[0].attributes)["http.route"],
            string("/work")
        );

        let workers = by_name("unsworn.runtime.workers");
        let Some(MetricData::Gauge(gauge)) = &workers.data else {
            panic!("expected gauge: {workers:?}");
        };
        assert_eq!(
            values(&gauge.data_points[0].attributes)["unsworn.runtime"],
            string("py")
        );
        let _ = by_name("unsworn.runtime.crashes");
    }
}

const LOGGING_HANDLERS: &str = r#"
import logging
from unsworn import json

def work(req, ctx):
    logging.getLogger().info("from handler")
    ctx.call("inner", {})
    return json({"ok": True})

def inner(req, ctx):
    logging.getLogger().warning("from function")
    return json({"ok": True})
"#;

#[test]
fn logs_are_exported_over_otlp_with_worker_span_ids() {
    if !common::has("python3") {
        eprintln!("skipping: python3 not found");
        return;
    }
    let collector = Fake::start();
    let config = format!(
        r#"
[runtimes.py]
extensions = [".py"]
command = "python3 -u -m unsworn.worker"
protocol = "framed"

[outbound.otel]
url = "http://{}"
headers = {{ Authorization = "Bearer l" }}
methods = ["POST"]
paths = ["/v1/traces", "/v1/logs"]

[telemetry]
outbound = "otel"
service_name = "api"
sample = 1
logs = true

[[route]]
path = "/work"
handler = "h.py#work"

[[function]]
name = "inner"
handler = "h.py#inner"
"#,
        collector.addr
    );
    let project = Project::new(&[("unsworn.toml", &config), ("h.py", LOGGING_HANDLERS)]);
    let mut server = project.serve();
    get(&server, "/work", &[]);
    server.terminate();

    let requests: Vec<Received> = collector
        .seen()
        .into_iter()
        .filter(|r| r.path == "/v1/logs")
        .collect();
    assert!(!requests.is_empty(), "{}", server.logs());
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].headers["authorization"], "Bearer l");
    assert_eq!(
        requests[0].headers["content-type"],
        "application/x-protobuf"
    );

    let spans = collector.spans();
    let servers = of_kind(&spans, SpanKind::Server);
    let handler = below(&spans, servers[0], "handler h.py#work");
    let call = below(&spans, handler, "call inner");

    let records = collector.log_records();
    fn body(r: &LogRecord) -> &str {
        match r.body.as_ref().and_then(|b| b.value.as_ref()) {
            Some(Value::StringValue(s)) => s.as_str(),
            _ => "",
        }
    }

    // No `access` or `telemetry` logs are sent.
    for r in &records {
        let attrs = values(&r.attributes);
        assert_ne!(attrs.get("target"), Some(&string("access")));
        assert_ne!(attrs.get("target"), Some(&string("telemetry")));
    }

    // Server lifecycle log arrived.
    assert!(
        records.iter().any(|r| body(r).contains("listening on")),
        "expected server startup log: {records:?}"
    );

    // Handler's log carries the handler span's trace_id and span_id.
    let from_handler = records
        .iter()
        .find(|r| body(r) == "from handler")
        .unwrap_or_else(|| panic!("no handler log in {records:?}"));
    assert_eq!(from_handler.severity_number, SeverityNumber::Info as i32);
    assert_eq!(from_handler.severity_text, "INFO");
    assert_eq!(from_handler.trace_id, handler.trace_id);
    assert_eq!(from_handler.span_id, handler.span_id);

    // Function's log carries the call span's trace_id and span_id.
    let from_fn = records
        .iter()
        .find(|r| body(r) == "from function")
        .unwrap_or_else(|| panic!("no function log in {records:?}"));
    assert_eq!(from_fn.severity_number, SeverityNumber::Warn as i32);
    assert_eq!(from_fn.severity_text, "WARN");
    assert_eq!(from_fn.trace_id, call.trace_id);
    assert_eq!(from_fn.span_id, call.span_id);

    // Both also carry trace_id and span_id on stderr.
    let stderr = server.logs();
    let handler_line = stderr
        .lines()
        .find(|l| l.contains("from handler"))
        .unwrap_or_else(|| panic!("no handler line in stderr:\n{stderr}"));
    assert!(
        handler_line.contains(&format!("trace_id={}", hex(&handler.trace_id)))
            && handler_line.contains(&format!("span_id={}", hex(&handler.span_id))),
        "{handler_line}"
    );
    let fn_line = stderr
        .lines()
        .find(|l| l.contains("from function"))
        .unwrap_or_else(|| panic!("no function line in stderr:\n{stderr}"));
    assert!(
        fn_line.contains(&format!("trace_id={}", hex(&call.trace_id)))
            && fn_line.contains(&format!("span_id={}", hex(&call.span_id))),
        "{fn_line}"
    );
}

#[test]
fn a_node_function_exports_nested_spans() {
    if !common::has("node") {
        eprintln!("skipping: node not found");
        return;
    }
    let (collector, echo) = (Fake::start(), Fake::start());
    let config = format!(
        r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun run worker.ts"

[outbound.otel]
url = "http://{}"
headers = {{ Authorization = "Bearer edge" }}
methods = ["POST"]
paths = ["/v1/traces"]

[outbound.echo]
url = "http://{}"

[telemetry]
outbound = "otel"
service_name = "edge-api"
sample = 1

[[route]]
path = "/search"
handler = "h.ts#search"
before = ["h.ts#gate"]

[[function]]
name = "inner"
handler = "h.ts#inner"
"#,
        collector.addr, echo.addr
    );
    let node_ts = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk/js/src/node.ts")
        .canonicalize()
        .unwrap();
    let index_ts = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../sdk/js/src/index.ts")
        .canonicalize()
        .unwrap();
    let incoming = format!("00-{TRACE}-{PARENT}-01");
    let script = format!(
        r#"
import {{ createHandler }} from {node_ts:?};
import {{ json, next }} from {index_ts:?};

const bundle = {{
  config: "unsworn.toml",
  files: {{ "unsworn.toml": {config:?}, "h.ts": null }},
  modules: {{
    "h.ts": {{
      gate: () => next(),
      search: async (_req, ctx) => {{
        const inner = await ctx.call("inner", {{}});
        await ctx.fetch("echo", "/seen?q=1");
        return json(inner);
      }},
      inner: (req) => json([req.headers["traceparent"] ?? null, req.headers["tracestate"] ?? null]),
    }},
  }},
}};

const app = createHandler(bundle, {{ env: {{}} }});
const res = await app.fetch(
  new Request("https://example.com/search", {{
    headers: {{ traceparent: {incoming:?}, tracestate: "vendor=1" }},
  }}),
  {{ ip: "203.0.113.9", requestId: "edge-1" }},
);
const text = await res.text();
await app.idle();
if (res.status !== 200) {{
  console.error("bad status", res.status, text);
  process.exit(1);
}}
process.stdout.write("\nRESULT:" + text + "\n");
process.exit(0);
"#,
        node_ts = node_ts.display().to_string(),
        index_ts = index_ts.display().to_string(),
    );
    let out = std::process::Command::new("node")
        .args(["--input-type=module", "-e", &script])
        .output()
        .expect("run node");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "node failed:\nstdout: {stdout}\nstderr: {stderr}",
    );
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("RESULT:"))
        .unwrap_or_else(|| panic!("no RESULT line:\nstdout: {stdout}\nstderr: {stderr}"));
    let said: serde_json::Value = serde_json::from_str(line).expect("json body");

    let sent = collector.seen();
    assert_eq!(sent.len(), 1, "expected one export request: {sent:?}");
    assert_eq!(sent[0].method, "POST");
    assert_eq!(sent[0].path, "/v1/traces");
    assert_eq!(sent[0].headers["authorization"], "Bearer edge");
    assert_eq!(sent[0].headers["content-type"], "application/x-protobuf");
    let exported = decode(&sent[0]);
    let resource = values(
        &exported.resource_spans[0]
            .resource
            .as_ref()
            .unwrap()
            .attributes,
    );
    assert_eq!(resource["service.name"], string("edge-api"));

    let spans = collector.spans();
    assert_eq!(spans.len(), 5, "{spans:?}");
    let servers = of_kind(&spans, SpanKind::Server);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "GET /search");
    assert_eq!(hex(&servers[0].trace_id), TRACE);
    assert_eq!(hex(&servers[0].parent_span_id), PARENT);
    let steps = events(servers[0]);
    assert_eq!(names(&steps), ["unsworn.checked"]);

    let gate = below(&spans, servers[0], "before h.ts#gate");
    let search = below(&spans, servers[0], "handler h.ts#search");
    let call = below(&spans, search, "call inner");
    let fetch = below(&spans, search, "GET");
    for span in [gate, search, call, fetch] {
        assert_eq!(span.kind, SpanKind::Client as i32, "{}", span.name);
        assert!(span.status.is_none(), "{}", span.name);
    }
    assert_eq!(said, serde_json::json!([traceparent(call), "vendor=1"]));
    let seen = echo.request("/seen?q=1");
    assert_eq!(seen.headers["traceparent"], traceparent(fetch));
    assert_eq!(seen.headers["tracestate"], "vendor=1");
}
