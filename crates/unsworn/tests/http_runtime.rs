//! An `http` runtime: Unsworn starts an ordinary HTTP server, written
//! with no SDK, and forwards requests to it once they've passed their
//! route's rules.

mod common;

use std::time::{Duration, Instant};

use common::Project;
use serde_json::{Value, json};

/// A server with nothing of Unsworn's in it but the place to listen: a
/// Unix socket (`UNSWORN_SOCKET`) or a port (`PORT`). It answers with what
/// it was sent.
const APP: &str = r#"
import base64, json, os, socketserver, sys
from http.server import BaseHTTPRequestHandler

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        if self.path == "/api/exit":
            os._exit(3)
        raw = self.headers.get("unsworn-context")
        context = json.loads(base64.urlsafe_b64decode(raw + "=" * (-len(raw) % 4))) if raw else None
        body = json.dumps({
            "path": self.path,
            "context": context,
            "pid": os.getpid(),
            "on_a_socket": bool(os.environ.get("UNSWORN_SOCKET")),
            "request_id": self.headers.get("x-request-id"),
            "has_secret": "API_KEYS" in os.environ,
            "call_token": self.headers.get("unsworn-call"),
            "calls_url": os.environ.get("UNSWORN_CALLS_URL"),
        }).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass

    def address_string(self):
        return "unsworn"

class Unix(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True

class Tcp(socketserver.ThreadingMixIn, socketserver.TCPServer):
    daemon_threads = True
    allow_reuse_address = True

print("starting", file=sys.stderr, flush=True)
path = os.environ.get("UNSWORN_SOCKET")
server = Unix(path, Handler) if path else Tcp(("127.0.0.1", int(os.environ["PORT"])), Handler)
server.serve_forever()
"#;

fn config(runtime: &str) -> String {
    format!(
        r#"
# With `&& true` the shell has to stay in front of the server, as some
# shells do for any command: stopping has to reach the server behind it.
[runtimes.api]
command = "python3 app.py && true"
protocol = "http"
{runtime}

[auth.keys]
type = "api_key"
keys = "{{env.API_KEYS}}"

[[route]]
path = "/health"
auth = "none"
respond = {{ status = 200 }}

[[route]]
path = "/api/{{*rest}}"
auth = "keys"
proxy = {{ runtime = "api" }}
"#
    )
}

fn python() -> bool {
    let found = common::has("python3");
    if !found {
        eprintln!("skipped: python3 isn't installed");
    }
    found
}

fn serve(p: &Project) -> common::Server {
    p.start_with_env(&["serve"], &[("API_KEYS", "k1")])
}

fn alive(pid: u64) -> bool {
    // Signal 0 asks whether the process is there, and sends nothing.
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Waits until `check` holds, for a few seconds.
fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !check() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_server_with_no_sdk_is_a_runtime() {
    if !python() {
        return;
    }
    let p = Project::new(&[("unsworn.toml", &config("")), ("app.py", APP)]);
    let s = serve(&p);
    let key = [("x-api-key", "k1")];

    // The route's rules come first: without a key, the server isn't asked,
    // and isn't even started.
    assert_eq!(s.get("/api/x").status, 401);
    assert!(!s.logs().contains("runtime started"), "{}", s.logs());

    // The first request starts it. It gets the path as sent, who the
    // caller is, and the request's id.
    let r = s.request("GET", "/api/users/7?full=1", &key, b"");
    assert_eq!(r.status, 200, "{}\n{}", r.text(), s.logs());
    let seen = r.json();
    assert_eq!(seen["path"], "/api/users/7?full=1");
    assert_eq!(
        seen["context"],
        json!({ "auth": { "type": "api_key", "key": 0, "scheme": "keys" } })
    );
    assert_eq!(seen["on_a_socket"], true);
    assert!(seen["request_id"].is_string(), "{seen}");
    // With no functions or outbound services, there's nothing to call
    // back for: no token, and no endpoint.
    assert_eq!(seen["call_token"], Value::Null);
    assert_eq!(seen["calls_url"], Value::Null);
    // It's a worker like any other: Unsworn's own credentials stay out of
    // its environment, and what it prints is in Unsworn's log.
    assert_eq!(seen["has_secret"], false);
    // (Its output is read as it comes, so the line may be a moment behind
    // the response.)
    eventually("the server's output is logged", || {
        s.logs().contains("starting")
    });

    // A caller can't pass itself off through the server's own header.
    let forged = [
        ("x-api-key", "k1"),
        ("unsworn-context", "eyJhdXRoIjp7InN1YiI6ImFkbWluIn19"),
    ];
    let r = s.request("GET", "/api/x", &forged, b"");
    assert_eq!(r.json()["context"]["auth"]["scheme"], "keys");

    // It's restarted when it exits.
    let pid = seen["pid"].as_u64().unwrap();
    let r = s.request("GET", "/api/exit", &key, b"");
    assert_eq!(r.status, 502, "{}", r.text());
    eventually("the server is started again", || {
        let r = s.request("GET", "/api/x", &key, b"");
        r.status == 200 && r.json()["pid"].as_u64() != Some(pid)
    });
    assert!(
        s.logs().contains("worker exited; restarting"),
        "{}",
        s.logs()
    );

    // And it doesn't outlive Unsworn, even one that's killed.
    let pid = s.request("GET", "/api/x", &key, b"").json()["pid"]
        .as_u64()
        .unwrap();
    assert!(alive(pid));
    let killed = std::process::Command::new("kill")
        .args(["-KILL", &s.pid().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    eventually("the server stops with Unsworn", || !alive(pid));
}

#[test]
fn a_server_that_cant_use_a_socket_gets_a_port() {
    if !python() {
        return;
    }
    let p = Project::new(&[
        ("unsworn.toml", &config("listen = \"port\"\neager = true")),
        ("app.py", APP),
    ]);
    let s = serve(&p);
    // `eager` starts it with Unsworn.
    assert!(s.logs().contains("runtime started"), "{}", s.logs());
    let r = s.request("GET", "/api/x", &[("x-api-key", "k1")], b"");
    assert_eq!(r.status, 200, "{}\n{}", r.text(), s.logs());
    assert_eq!(r.json()["on_a_socket"], false);
    assert_eq!(r.json()["context"]["auth"]["scheme"], "keys");
}

#[test]
fn a_server_that_doesnt_start_is_a_503_that_says_why() {
    let cfg =
        config("startup_timeout = \"1s\"").replace("python3 app.py && true", "echo no server here");
    let p = Project::new(&[("unsworn.toml", &cfg)]);
    let s = serve(&p);
    let r = s.request("GET", "/api/x", &[("x-api-key", "k1")], b"");
    assert_eq!(r.status, 503, "{}", r.text());
    let logs = s.logs();
    assert!(
        logs.contains("exited before listening on the socket in UNSWORN_SOCKET"),
        "{logs}"
    );
    // What the command printed is read as it comes, so it may be a
    // moment behind the response.
    eventually("the command's output is logged", || {
        s.logs().contains("no server here")
    });
    // The rest of the project is unaffected.
    assert_eq!(s.get("/health").status, 200);
}

#[test]
fn check_says_what_an_http_runtime_is_for() {
    let errors = |config: &str, files: &[(&str, &str)]| -> String {
        let mut all = vec![("unsworn.toml", config)];
        all.extend_from_slice(files);
        let p = Project::new(&all);
        let out = p.command(&["check"]);
        assert!(!out.status.success());
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let base = "[runtimes.api]\ncommand = \"python3 app.py\"\nprotocol = \"http\"\n";

    // It has no handler files,
    let e = errors(
        &format!("{base}\n[[route]]\npath = \"/x\"\nhandler = \"app.py\"\nruntime = \"api\"\n"),
        &[("app.py", "")],
    );
    assert!(
        e.contains("is an HTTP server, which has no handler files"),
        "{e}"
    );
    assert!(e.contains("proxy = { runtime = \"api\" }"), "{e}");
    let e = errors(&format!("{base}extensions = [\".py\"]\n"), &[]);
    assert!(
        e.contains("an `http` runtime doesn't run handler files"),
        "{e}"
    );
    // a route names it, and it has to be there and be one,
    let e = errors(
        &format!("{base}\n[[route]]\npath = \"/x\"\nproxy = {{ runtime = \"app\" }}\n"),
        &[],
    );
    assert!(e.contains("no runtime named \"app\""), "{e}");
    let e = errors(
        "[runtimes.ts]\nextensions = [\".ts\"]\ncommand = \"bun run worker.ts\"\n\n[[route]]\npath = \"/x\"\nproxy = { runtime = \"ts\" }\n",
        &[],
    );
    assert!(e.contains("runs handlers, not an HTTP server"), "{e}");
    // and it's one place to forward to.
    let e = errors(
        &format!(
            "{base}\n[[route]]\npath = \"/x\"\nproxy = {{ runtime = \"api\", upstreams = [\"http://a\"] }}\n"
        ),
        &[],
    );
    assert!(e.contains("pick one"), "{e}");
    // Other runtimes still need extensions, and don't listen.
    let e = errors("[runtimes.ts]\ncommand = \"bun run worker.ts\"\n", &[]);
    assert!(e.contains("no extensions"), "{e}");

    // The plan says how the server is reached.
    let ok =
        format!("{base}\n[[route]]\npath = \"/x/{{*rest}}\"\nproxy = {{ runtime = \"api\" }}\n");
    let p = Project::new(&[("unsworn.toml", &ok)]);
    let out = p.command(&["plan", "-o", "-"]);
    let plan = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        plan.contains("  protocol           http, on a socket\n"),
        "{plan}"
    );
    assert!(
        plan.contains("  proxy              runtime api\n"),
        "{plan}"
    );
}

/// A server that calls functions and outbound services through Unsworn's
/// local endpoint, with nothing but the standard library: on the port in
/// `UNSWORN_CALLS_URL`, or with `?via=socket` on the socket in
/// `UNSWORN_CALLS_SOCKET`. It answers with what the endpoint answered.
const CALLING_APP: &str = r#"
import http.client, json, os, socket, socketserver, threading, time
from http.server import BaseHTTPRequestHandler
from urllib.parse import urlsplit, parse_qs

saved = {}

class UnixConnection(http.client.HTTPConnection):
    def __init__(self, path):
        super().__init__("unsworn")
        self.unix_path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(self.unix_path)

def unsworn(via, method, path, token, body=None, headers=None):
    headers = dict(headers or {})
    if token is not None:
        headers["unsworn-call"] = token
    if via == "socket":
        conn = UnixConnection(os.environ["UNSWORN_CALLS_SOCKET"])
    else:
        conn = http.client.HTTPConnection(urlsplit(os.environ["UNSWORN_CALLS_URL"]).netloc)
    conn.request(method, path, body=body, headers=headers)
    r = conn.getresponse()
    data = r.read()
    try:
        body = json.loads(data) if data else None
    except ValueError:
        body = data.decode()
    return {
        "status": r.status,
        "error": r.getheader("unsworn-error"),
        "type": r.getheader("content-type"),
        "body": body,
        "headers": {k.lower(): v for k, v in r.getheaders()},
    }

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        url = urlsplit(self.path)
        query = parse_qs(url.query)
        via = query.get("via", ["url"])[0]
        token = self.headers.get("unsworn-call")
        parts = url.path.split("/")[2:]
        what = parts[0]
        if what == "call":
            input = query.get("input", [None])[0]
            out = unsworn(via, "POST", "/call/" + parts[1], token, body=input)
        elif what == "fetch":
            out = unsworn(via, "POST", "/fetch/svc/" + "/".join(parts[1:]) + "?x=1", token,
                          body=b'{"a": 1}', headers={"content-type": "application/json", "x-mine": "yes"})
        elif what == "get":
            out = unsworn(via, "GET", "/call/whoami", token)
        elif what == "nowhere":
            out = unsworn(via, "POST", "/other", token)
        elif what == "untokened":
            out = unsworn(via, "POST", "/call/whoami", None, body=b"{}")
        elif what == "save":
            saved["token"] = token
            out = {"saved": True}
        elif what == "stale":
            out = unsworn(via, "POST", "/call/whoami", saved["token"], body=b"{}")
        elif what == "token":
            out = {"token": token, "env": {k: os.environ.get(k) for k in ("UNSWORN_CALLS_URL", "UNSWORN_CALLS_SOCKET")}}
        elif what == "late":
            # A call made after the response has gone, for a request
            # that's over.
            def later():
                time.sleep(0.3)
                saved["late"] = unsworn(via, "POST", "/call/whoami", token, body=b"{}")
            threading.Thread(target=later).start()
            out = {"later": True}
        elif what == "late-result":
            out = saved.get("late")
        else:
            out = {"unknown": what}
        body = json.dumps(out).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass

    def address_string(self):
        return "unsworn"

class Unix(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True

Unix(os.environ["UNSWORN_SOCKET"], Handler).serve_forever()
"#;

const FUNCTIONS_PY: &str = r#"
from unsworn import json

def whoami(req, ctx):
    return json({"auth": ctx.get("auth"), "caller": ctx.get("caller"), "input": req.json, "headers": dict(req.headers)})

def refuse(req, ctx):
    return json({"error": "not now"}, status=409)
"#;

/// A service that answers with what it was sent, as JSON.
fn echo_service() -> std::net::SocketAddr {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
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
                let first: Vec<&str> = lines.next().unwrap_or_default().split(' ').collect();
                let mut headers = serde_json::Map::new();
                for line in lines {
                    if let Some((name, value)) = line.split_once(':') {
                        headers.insert(name.trim().to_ascii_lowercase(), json!(value.trim()));
                    }
                }
                let length: usize = headers
                    .get("content-length")
                    .and_then(|v| v.as_str()?.parse().ok())
                    .unwrap_or(0);
                while buf.len() < head_end + length {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let body = String::from_utf8_lossy(&buf[head_end..head_end + length]);
                let text = json!({
                    "method": first.first(),
                    "path": first.get(1),
                    "headers": headers,
                    "body": body,
                })
                .to_string();
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 201 Created\r\ncontent-type: application/json\r\nx-service: yes\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{text}",
                        text.len()
                    )
                    .as_bytes(),
                );
            });
        }
    });
    addr
}

fn calling_project() -> Project {
    let service = echo_service();
    let config = format!(
        r#"
[runtimes.api]
command = "python3 app.py"
protocol = "http"

[runtimes.py]
extensions = [".py"]
command = "python3 -u -m unsworn.worker"
protocol = "framed"

[auth.keys]
type = "api_key"
keys = "{{env.API_KEYS}}"

[[function]]
name = "whoami"
handler = "fn/functions.py#whoami"

[[function]]
name = "refuse"
handler = "fn/functions.py#refuse"

[[function]]
name = "typed"
handler = "fn/functions.py#whoami"
input = {{ type = "object", required = ["text"], properties = {{ text = {{ type = "string" }} }} }}

[outbound.svc]
url = "http://{service}/base"
headers = {{ Authorization = "Bearer s3cret" }}

[[route]]
path = "/api/{{*rest}}"
auth = "keys"
proxy = {{ runtime = "api" }}
"#
    );
    Project::new(&[
        ("unsworn.toml", &config),
        ("app.py", CALLING_APP),
        ("fn/functions.py", FUNCTIONS_PY),
    ])
}

#[test]
fn a_server_calls_functions_and_services_through_unsworn() {
    if !python() {
        return;
    }
    let p = calling_project();
    let s = serve(&p);
    let get = |path: &str| {
        let r = s.request("GET", path, &[("x-api-key", "k1")], b"");
        assert_eq!(r.status, 200, "{path}: {}\n{}", r.text(), s.logs());
        r.json()
    };

    // The server is told where the endpoint is, and gets a token with each
    // request: a new one each time.
    let first = get("/api/token");
    assert!(
        first["env"]["UNSWORN_CALLS_URL"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:"),
        "{first}"
    );
    assert!(
        first["env"]["UNSWORN_CALLS_SOCKET"]
            .as_str()
            .unwrap()
            .ends_with(".sock"),
        "{first}"
    );
    let token = first["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 48, "{first}");
    assert_ne!(get("/api/token")["token"].as_str(), Some(token.as_str()));
    // A client can't send one of its own.
    let r = s.request(
        "GET",
        "/api/token",
        &[("x-api-key", "k1"), ("unsworn-call", "mine")],
        b"",
    );
    assert_ne!(r.json()["token"], "mine");

    // A function gets the request's context, and knows who called it, on
    // the port and on the socket.
    for via in ["url", "socket"] {
        let out = get(&format!(
            "/api/call/whoami?via={via}&input=%7B%22n%22%3A1%7D"
        ));
        assert_eq!(out["status"], 200, "{via}: {out}\n{}", s.logs());
        assert_eq!(out["type"], "application/json");
        assert_eq!(out["error"], Value::Null);
        assert_eq!(out["body"]["input"], json!({ "n": 1 }));
        assert_eq!(out["body"]["auth"]["scheme"], "keys", "{out}");
        assert_eq!(
            out["body"]["caller"],
            json!({ "function": "whoami", "from": "runtime:api", "depth": 1 })
        );
    }
    // No body is no input.
    let out = get("/api/call/whoami");
    assert_eq!(out["body"]["input"], Value::Null, "{out}");

    // A function's own failure is its answer, as it sent it.
    let out = get("/api/call/refuse?input=%7B%7D");
    assert_eq!(out["status"], 409, "{out}");
    assert_eq!(out["body"], json!({ "error": "not now" }));
    assert_eq!(out["error"], Value::Null);

    // Unsworn's own refusals are marked as Unsworn's.
    let out = get("/api/call/typed?input=%7B%7D");
    assert_eq!(out["status"], 422, "{out}");
    assert_eq!(out["type"], "application/problem+json");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("doesn't match the schema"),
        "{out}"
    );
    assert_eq!(out["body"]["errors"][0]["location"], "input", "{out}");
    let out = get("/api/call/nope?input=%7B%7D");
    assert_eq!(out["status"], 404, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("no [[function]] named \"nope\""),
        "{out}"
    );
    let out = get("/api/call/whoami?input=nope");
    assert_eq!(out["status"], 400, "{out}");
    let out = get("/api/get");
    assert_eq!(out["status"], 405, "{out}");
    assert_eq!(out["headers"]["allow"], "POST");
    let out = get("/api/nowhere");
    assert_eq!(out["status"], 404, "{out}");
    assert!(out["error"].is_string(), "{out}");

    // An outbound service gets the request with its credential added, and
    // without the token; its answer comes back as it is.
    for via in ["url", "socket"] {
        let out = get(&format!("/api/fetch/things/7?via={via}"));
        assert_eq!(out["status"], 201, "{via}: {out}\n{}", s.logs());
        assert_eq!(out["headers"]["x-service"], "yes");
        assert_eq!(out["error"], Value::Null);
        let seen = &out["body"];
        assert_eq!(seen["method"], "POST");
        assert_eq!(seen["path"], "/base/things/7?x=1");
        assert_eq!(seen["body"], "{\"a\": 1}");
        assert_eq!(seen["headers"]["authorization"], "Bearer s3cret");
        assert_eq!(seen["headers"]["x-mine"], "yes");
        assert_eq!(seen["headers"]["unsworn-call"], Value::Null, "{seen}");
    }

    // Without a token, or with one whose request is over, nothing is called.
    let out = get("/api/untokened");
    assert_eq!(out["status"], 401, "{out}");
    get("/api/save");
    let out = get("/api/stale");
    assert_eq!(out["status"], 409, "{out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap()
            .contains("isn't one for a request in flight"),
        "{out}"
    );
    get("/api/late");
    eventually("the late call is answered", || {
        !get("/api/late-result").is_null()
    });
    assert_eq!(get("/api/late-result")["status"], 409);
}
