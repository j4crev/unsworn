//! Outbound services: Unsworn makes a handler's request to a declared
//! service and adds the credential, which the handler never holds.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};

use common::{Project, Server};
use serde_json::{Value, json};

/// A service that answers every request with what it received, as JSON,
/// and keeps the requests for the test to look at. `/slow` takes a second.
struct Service {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Value>>>,
}

fn service() -> Service {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let log = log.clone();
            std::thread::spawn(move || serve(stream, &log));
        }
    });
    Service { addr, seen }
}

fn serve(mut stream: std::net::TcpStream, log: &Mutex<Vec<Value>>) {
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
        let (method, path) = (
            first.next().unwrap_or_default(),
            first.next().unwrap_or_default(),
        );
        let mut headers = serde_json::Map::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), json!(value.trim()));
            }
        }
        let length: usize = headers
            .get("content-length")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        while buf.len() < head_end + length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let body = String::from_utf8_lossy(&buf[head_end..head_end + length]).into_owned();
        buf.drain(..head_end + length);
        let request = json!({ "method": method, "path": path, "headers": headers, "body": body });
        log.lock().unwrap().push(request.clone());
        if path.contains("/slow") {
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        let (status, text) = if path.contains("/teapot") {
            ("418 I'm a teapot", "short and stout".to_string())
        } else if path.contains("/big") {
            ("200 OK", "x".repeat(4096))
        } else {
            ("200 OK", request.to_string())
        };
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\nx-service: yes\r\ncontent-length: {}\r\n\r\n{text}",
            text.len()
        );
        if stream.write_all(response.as_bytes()).is_err() {
            return;
        }
    }
}

fn config(service: SocketAddr) -> String {
    format!(
        r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun run {root}/sdk/js/src/worker.ts"
protocol = "framed"

[runtimes.py]
extensions = [".py"]
command = "python3 -u -m unsworn.worker"
protocol = "framed"

[outbound.billing]
url = "http://{service}/api"
headers = {{ Authorization = "Bearer {{env.BILLING_KEY}}", "X-Account" = "acme" }}
methods = ["GET", "POST"]
paths = ["/charges", "/charges/{{id}}", "/public/{{*rest}}", "/slow", "/teapot", "/big"]
timeout = "300ms"
max_response = "1KB"

[outbound.open]
url = "http://{service}"

[[route]]
path = "/ts"
method = "POST"
handler = "src/routes.ts#route"

[[route]]
path = "/py"
method = "POST"
handler = "src/routes.py#route"

[[route]]
path = "/py-async"
method = "POST"
handler = "src/routes.py#route_async"

[[route]]
path = "/env"
handler = "src/routes.py#env"
"#,
        root = common::repo_root().display()
    )
}

/// Each route takes `{{ service, path, method?, headers?, json? }}` and
/// answers with what `ctx.fetch` gave back, or how it failed.
const ROUTES_TS: &str = r#"
export const route = async (req: any, ctx: any) => {
  const { service, path, ...init } = req.json;
  try {
    const r = await ctx.fetch(service, path, init);
    return { json: { status: r.status, ok: r.ok, service: r.headers["x-service"], text: r.text() } };
  } catch (err: any) {
    return { json: { failed: err.status, name: err.name, message: err.message } };
  }
};
"#;

const ROUTES_PY: &str = r#"
import os
from unsworn import FetchError, json, _UNSET

def _args(req):
    a = dict(req.json)
    return a.pop("service"), a.pop("path"), {
        "method": a.get("method"), "headers": a.get("headers"), "json": a.get("json", _UNSET),
    }

def route(req, ctx):
    service, path, more = _args(req)
    try:
        r = ctx.fetch(service, path, **more)
    except FetchError as e:
        return json({"failed": e.status, "message": str(e)})
    return json({"status": r.status, "ok": r.ok, "service": r.headers.get("x-service"), "text": r.text})

async def route_async(req, ctx):
    service, path, more = _args(req)
    r = await ctx.fetch(service, path, **more)
    return json({"status": r.status, "text": r.text})

def env(req, ctx):
    return json({"key": os.environ.get("BILLING_KEY")})
"#;

fn start() -> Option<(Project, Server, Service)> {
    if !common::has("bun") || !common::has("python3") {
        eprintln!("skipped: needs bun and python3");
        return None;
    }
    let service = service();
    let p = Project::new(&[
        ("unsworn.toml", &config(service.addr)),
        ("src/routes.ts", ROUTES_TS),
        ("src/routes.py", ROUTES_PY),
    ]);
    let s = p.start_with_env(&["serve"], &[("BILLING_KEY", "sk_live_s3cret")]);
    Some((p, s, service))
}

fn ask(s: &Server, route: &str, body: Value) -> Value {
    let r = s.request(
        "POST",
        route,
        &[("content-type", "application/json")],
        body.to_string().as_bytes(),
    );
    assert_eq!(r.status, 200, "{}\n{}", r.text(), s.logs());
    r.json()
}

#[test]
fn unsworn_adds_the_credential_the_handler_never_holds() {
    let Some((_p, s, service)) = start() else {
        return;
    };
    for route in ["/ts", "/py"] {
        let got = ask(
            &s,
            route,
            json!({
                "service": "billing",
                "path": "/charges?limit=2",
                "json": { "amount": 1200 },
                // A handler can't replace what the service sets.
                "headers": { "Authorization": "Bearer mine", "X-Trace": "t1", "Host": "evil.example" },
            }),
        );
        assert_eq!(got["status"], 200, "{route}: {got}");
        assert_eq!(got["ok"], true);
        assert_eq!(got["service"], "yes");
        let seen: Value = serde_json::from_str(got["text"].as_str().unwrap()).unwrap();
        assert_eq!(seen["method"], "POST", "{route}: {seen}");
        assert_eq!(seen["path"], "/api/charges?limit=2");
        assert_eq!(seen["body"], r#"{"amount":1200}"#);
        assert_eq!(seen["headers"]["authorization"], "Bearer sk_live_s3cret");
        assert_eq!(seen["headers"]["x-account"], "acme");
        assert_eq!(seen["headers"]["x-trace"], "t1");
        assert_eq!(seen["headers"]["content-type"], "application/json");
        assert_eq!(seen["headers"]["host"], service.addr.to_string());
    }
    // The variable the credential comes from isn't in the worker's environment.
    assert_eq!(s.get("/env").json(), json!({ "key": null }));
    // From an `async def` handler too, and a plain GET.
    let r = s.request(
        "POST",
        "/py-async",
        &[("content-type", "application/json")],
        json!({ "service": "billing", "path": "/charges/42" })
            .to_string()
            .as_bytes(),
    );
    assert_eq!(r.status, 200, "{}\n{}", r.text(), s.logs());
    let seen: Value = serde_json::from_str(r.json()["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        (seen["method"].as_str(), seen["path"].as_str()),
        (Some("GET"), Some("/api/charges/42"))
    );
    assert_eq!(service.seen.lock().unwrap().len(), 3);

    // What was reached is logged, without its query.
    let logs = s.logs();
    assert!(logs.contains("outbound"), "{logs}");
    assert!(
        logs.contains("service=billing method=POST path=/charges status=200"),
        "{logs}"
    );
    assert!(!logs.contains("limit=2"), "{logs}");
    assert!(!logs.contains("sk_live_s3cret"), "{logs}");
}

#[test]
fn only_what_the_config_allows_is_sent() {
    let Some((_p, s, service)) = start() else {
        return;
    };
    let refused = |body: Value| {
        let got = ask(&s, "/ts", body.clone());
        assert_eq!(got["failed"], 403, "{body}: {got}");
        assert_eq!(got["name"], "FetchError");
        got["message"].as_str().unwrap().to_string()
    };
    assert!(
        refused(json!({ "service": "billing", "path": "/charges", "method": "DELETE" }))
            .contains("doesn't allow DELETE")
    );
    assert!(
        refused(json!({ "service": "billing", "path": "/refunds" }))
            .contains("doesn't allow the path")
    );
    // Nor a path that steps out of an allowed one, however it's spelled.
    for path in [
        "/public/../refunds",
        "/public/%2e%2e/refunds",
        "/public/x/%2E%2E/%2E%2E/refunds",
    ] {
        refused(json!({ "service": "billing", "path": path }));
    }
    refused(json!({ "service": "open", "path": "//other.example/x/../y" }));
    let got = ask(
        &s,
        "/py",
        json!({ "service": "billing", "path": "/refunds" }),
    );
    assert_eq!(got["failed"], 403, "{got}");
    // None of them reached the service.
    assert!(
        service.seen.lock().unwrap().is_empty(),
        "{:?}",
        service.seen.lock().unwrap()
    );

    let got = ask(&s, "/ts", json!({ "service": "nope", "path": "/" }));
    assert_eq!(got["failed"], 404, "{got}");
    assert!(
        got["message"]
            .as_str()
            .unwrap()
            .contains("there is: billing, open"),
        "{got}"
    );

    // What is allowed goes through: a pattern with a parameter, and one
    // with the rest of the path.
    assert_eq!(
        ask(
            &s,
            "/ts",
            json!({ "service": "billing", "path": "/charges/7" })
        )["status"],
        200
    );
    assert_eq!(
        ask(
            &s,
            "/ts",
            json!({ "service": "billing", "path": "/public/a/b" })
        )["status"],
        200
    );
    // A service with no lists takes any method and path.
    assert_eq!(
        ask(
            &s,
            "/ts",
            json!({ "service": "open", "path": "/anything", "method": "DELETE" })
        )["status"],
        200
    );
}

#[test]
fn a_services_own_errors_and_limits() {
    let Some((_p, s, _service)) = start() else {
        return;
    };
    // A status from the service is an answer, not a failure.
    let got = ask(
        &s,
        "/ts",
        json!({ "service": "billing", "path": "/teapot" }),
    );
    assert_eq!(
        (got["status"].as_u64(), got["ok"].as_bool()),
        (Some(418), Some(false)),
        "{got}"
    );
    assert_eq!(got["text"], "short and stout");
    // Too slow for the service's `timeout`.
    let began = std::time::Instant::now();
    let got = ask(&s, "/ts", json!({ "service": "billing", "path": "/slow" }));
    assert_eq!(got["failed"], 504, "{got}");
    assert!(
        began.elapsed() < std::time::Duration::from_millis(900),
        "{:?}",
        began.elapsed()
    );
    // Larger than `max_response`.
    let got = ask(&s, "/ts", json!({ "service": "billing", "path": "/big" }));
    assert_eq!(got["failed"], 502, "{got}");
    assert!(
        got["message"].as_str().unwrap().contains("max_response"),
        "{got}"
    );
}

#[test]
fn services_are_planned_without_their_secrets() {
    let service = service();
    let p = Project::new(&[
        ("unsworn.toml", &config(service.addr)),
        ("src/routes.ts", ""),
        ("src/routes.py", ""),
    ]);
    // No BILLING_KEY in the environment: a plan doesn't need it.
    let out = p.command(&["plan", "-o", "-"]);
    let plan = String::from_utf8_lossy(&out.stdout).into_owned();
    let want = format!(
        "\noutbound billing\n  url                http://{}/api\n  adds               authorization, x-account\n  methods            GET, POST\n  paths              /charges, /charges/{{id}}, /public/{{*rest}}, /slow, /teapot, /big\n  timeout            300ms\n  max_response       1KB\n",
        service.addr
    );
    assert!(plan.contains(&want), "{plan}");
    assert!(!plan.contains("BILLING_KEY"), "{plan}");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_unsworn"))
        .arg("-C")
        .arg(p.path())
        .args(["check", "--target", "lambda"])
        .env("BILLING_KEY", "k")
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("only work on the server"), "{err}");
}
