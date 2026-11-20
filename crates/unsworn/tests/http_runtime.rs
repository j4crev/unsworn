//! An `http` runtime: Unsworn starts an ordinary HTTP server, written
//! with no SDK, and forwards requests to it once they've passed their
//! route's rules.

mod common;

use std::time::{Duration, Instant};

use common::Project;
use serde_json::json;

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
[runtimes.api]
command = "python3 app.py"
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
    std::path::Path::new(&format!("/proc/{pid}")).exists()
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
    // It's a worker like any other: Unsworn's own credentials stay out of
    // its environment, and what it prints is in Unsworn's log.
    assert_eq!(seen["has_secret"], false);
    assert!(s.logs().contains("starting"), "{}", s.logs());

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
    let cfg = config("startup_timeout = \"1s\"").replace("python3 app.py", "echo no server here");
    let p = Project::new(&[("unsworn.toml", &cfg)]);
    let s = serve(&p);
    let r = s.request("GET", "/api/x", &[("x-api-key", "k1")], b"");
    assert_eq!(r.status, 503, "{}", r.text());
    let logs = s.logs();
    assert!(
        logs.contains("exited before listening on the socket in UNSWORN_SOCKET"),
        "{logs}"
    );
    assert!(logs.contains("no server here"), "{logs}");
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
