//! End-to-end tests: the real binary, real HTTP, real worker processes.

mod common;

use common::{ECHO_WORKER, Project, has};
use serde_json::json;

const PY: &str = r#"
[runtimes.py]
extensions = [".py"]
command = "python3 -u worker.py"
workers = 1
"#;

fn needs_python() -> bool {
    if has("python3") {
        true
    } else {
        eprintln!("skipping: python3 not found");
        false
    }
}

#[test]
fn declarative_routes_and_errors() {
    let p = Project::new(&[
        (
            "unsworn.toml",
            r#"
[[route]]
path = "/health"
respond = { status = 200, json = { ok = true } }

[[route]]
path = "/hello"
method = "GET"
respond = { status = 200, text = "hi", headers = { "x-greeting" = "yes" } }

[[route]]
path = "/old/{id}"
redirect = { to = "/new/{id}", status = 301 }

[[route]]
path = "/assets/{*file}"
static = { dir = "public", spa_fallback = "index.html", cache_control = "max-age=60" }
"#,
        ),
        ("public/index.html", "<h1>app</h1>"),
        ("public/app.css", "body{}"),
    ]);
    let s = p.serve();

    let r = s.get("/health");
    assert_eq!(r.status, 200);
    assert_eq!(r.json(), json!({"ok": true}));
    assert!(r.header("x-request-id").is_some());

    let r = s.get("/hello");
    assert_eq!((r.status, r.text().as_str()), (200, "hi"));
    assert_eq!(r.header("x-greeting"), Some("yes"));

    let r = s.request("POST", "/hello", &[], b"");
    assert_eq!(r.status, 405);
    assert_eq!(r.header("allow"), Some("GET"));
    assert_eq!(r.header("content-type"), Some("application/problem+json"));

    let r = s.get("/nope");
    assert_eq!(r.status, 404);
    assert_eq!(r.json()["status"], 404);

    let r = s.get("/hello/");
    assert_eq!((r.status, r.header("location")), (308, Some("/hello")));

    let r = s.get("/old/7");
    assert_eq!((r.status, r.header("location")), (301, Some("/new/7")));

    let r = s.get("/assets/app.css");
    assert_eq!((r.status, r.text().as_str()), (200, "body{}"));
    assert_eq!(r.header("cache-control"), Some("max-age=60"));
    let r = s.get("/assets/some/client/route");
    assert_eq!((r.status, r.text().as_str()), (200, "<h1>app</h1>"));
    for path in ["/assets/../unsworn.toml", "/assets/%2e%2e/unsworn.toml"] {
        let r = s.get(path);
        assert!(
            !r.text().contains("[[route]]"),
            "{path} leaked a file outside the dir"
        );
    }

    let r = s.get("/_unsworn/health");
    assert_eq!(r.json()["status"], "ok");
}

#[test]
fn handlers_validation_and_before() {
    if !needs_python() {
        return;
    }
    let cfg = format!(
        r#"{PY}
[routes]
dir = "routes"

[[route]]
path = "/users/{{id}}"
method = "GET"
params = {{ id = {{ type = "integer", min = 1 }} }}
query = {{ verbose = {{ type = "boolean" }}, limit = {{ type = "integer", default = 20, max = 100 }} }}

[[route]]
path = "/users"
method = "POST"
handler = "src/users.py#create"
body = {{ type = "object", required = ["name"], properties = {{ name = {{ type = "string" }} }} }}
max_body = "64B"
before = ["src/users.py#tenant"]
"#
    );
    let p = Project::new(&[
        ("unsworn.toml", &cfg),
        ("worker.py", ECHO_WORKER),
        ("src/users.py", ""),
        ("routes/users/[id]/get.py", ""),
    ]);
    let s = p.serve();

    let r = s.get("/users/42?verbose=true&extra=x");
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["params"]["id"], 42);
    assert_eq!(v["query"]["verbose"], true);
    assert_eq!(v["query"]["limit"], 20);
    assert_eq!(v["query"]["extra"], "x");
    assert_eq!(v["handler"], "routes/users/[id]/get.py");

    let r = s.get("/users/0");
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["errors"][0]["location"], "params.id");
    let r = s.get("/users/1?limit=500");
    assert_eq!(r.status, 400);

    let json_ct = [("content-type", "application/json"), ("x-tenant", "acme")];
    let r = s.request("POST", "/users", &json_ct, br#"{"name":"Ada"}"#);
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["json"]["name"], "Ada");
    assert_eq!(
        v["context"]["tenant"], "acme",
        "before handler adds context"
    );

    let r = s.request("POST", "/users", &json_ct, br#"{"nom":"Ada"}"#);
    assert_eq!(r.status, 422);
    let r = s.request("POST", "/users", &json_ct, b"{not json");
    assert_eq!(r.status, 400);
    let r = s.request("POST", "/users", &[("content-type", "text/plain")], b"Ada");
    assert_eq!(r.status, 415);
    let r = s.request("POST", "/users", &json_ct, &[b'x'; 200]);
    assert_eq!(r.status, 413);
    let blocked = [
        ("content-type", "application/json"),
        ("x-tenant", "blocked"),
    ];
    let r = s.request("POST", "/users", &blocked, br#"{"name":"Ada"}"#);
    assert_eq!(r.status, 403, "before handler can reject");
}

#[test]
fn auth_scopes_and_protected_resource() {
    use jsonwebtoken::{EncodingKey, Header, encode};
    let p = Project::new(&[(
        "unsworn.toml",
        r#"
[auth.oauth]
type = "jwt"
secret = "test-secret"
audience = "https://api.example.com/mcp"

[auth.oauth.protected_resource]
resource = "https://api.example.com/mcp"
authorization_servers = ["https://auth.example.com"]

[auth.service]
type = "api_key"
keys = "k1, k2"

[[route]]
path = "/mcp"
auth = { scheme = "oauth", scopes = ["tools:read"] }
respond = { status = 200, json = { ok = true } }

[[route]]
path = "/internal"
auth = ["oauth", "service"]
respond = { status = 200 }
"#,
    )]);
    let s = p.serve();
    let token = |scope: &str, aud: &str| {
        encode(
            &Header::default(),
            &json!({"sub": "u1", "aud": aud, "scope": scope, "exp": 4_000_000_000u64}),
            &EncodingKey::from_secret(b"test-secret").unwrap(),
        )
        .unwrap()
    };

    let r = s.get("/mcp");
    assert_eq!(r.status, 401);
    let www = r.header("www-authenticate").unwrap();
    assert!(
        www.contains(
            "resource_metadata=\"https://api.example.com/.well-known/oauth-protected-resource/mcp\""
        ),
        "{www}"
    );

    let good = format!(
        "Bearer {}",
        token("tools:read", "https://api.example.com/mcp")
    );
    let r = s.request("GET", "/mcp", &[("authorization", &good)], b"");
    assert_eq!(r.status, 200);

    let no_scope = format!("Bearer {}", token("other", "https://api.example.com/mcp"));
    let r = s.request("GET", "/mcp", &[("authorization", &no_scope)], b"");
    assert_eq!(r.status, 403);
    assert!(
        r.header("www-authenticate")
            .unwrap()
            .contains("insufficient_scope")
    );

    let wrong_aud = format!(
        "Bearer {}",
        token("tools:read", "https://other.example.com")
    );
    let r = s.request("GET", "/mcp", &[("authorization", &wrong_aud)], b"");
    assert_eq!(r.status, 401);

    let r = s.get("/.well-known/oauth-protected-resource/mcp");
    assert_eq!(r.json()["resource"], "https://api.example.com/mcp");

    let r = s.request("GET", "/internal", &[("x-api-key", "k2")], b"");
    assert_eq!(r.status, 200);
    let r = s.request("GET", "/internal", &[("x-api-key", "nope")], b"");
    assert_eq!(r.status, 401);
}

#[test]
fn rate_limits_cors_and_origin() {
    let p = Project::new(&[(
        "unsworn.toml",
        r#"
[[route]]
path = "/limited"
rate_limit = { requests = 2, per = "1m" }
respond = { status = 200 }

[[route]]
path = "/api/{*rest}"
cors = { origins = ["https://app.example.com"], credentials = true, max_age = "10m" }

[[route]]
path = "/api/data"
method = ["GET", "POST"]
respond = { status = 200, json = { data = 1 } }

[[route]]
path = "/mcp"
origin = { allow = ["https://claude.ai"] }
respond = { status = 200 }
"#,
    )]);
    let s = p.serve();

    assert_eq!(s.get("/limited").status, 200);
    assert_eq!(s.get("/limited").status, 200);
    let r = s.get("/limited");
    assert_eq!(r.status, 429);
    assert!(r.header("retry-after").unwrap().parse::<u64>().unwrap() >= 1);

    let r = s.request(
        "OPTIONS",
        "/api/data",
        &[
            ("origin", "https://app.example.com"),
            ("access-control-request-method", "POST"),
            ("access-control-request-headers", "content-type"),
        ],
        b"",
    );
    assert_eq!(r.status, 204);
    assert_eq!(
        r.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(r.header("access-control-allow-methods"), Some("GET, POST"));
    assert_eq!(r.header("access-control-max-age"), Some("600"));

    let r = s.request(
        "GET",
        "/api/data",
        &[("origin", "https://app.example.com")],
        b"",
    );
    assert_eq!(r.header("access-control-allow-credentials"), Some("true"));
    let r = s.request(
        "GET",
        "/api/data",
        &[("origin", "https://evil.example.com")],
        b"",
    );
    assert_eq!(r.header("access-control-allow-origin"), None);

    let r = s.request(
        "GET",
        "/mcp",
        &[("origin", "https://evil.example.com")],
        b"",
    );
    assert_eq!(r.status, 403);
    assert_eq!(
        s.request("GET", "/mcp", &[("origin", "https://claude.ai")], b"")
            .status,
        200
    );
    assert_eq!(s.get("/mcp").status, 200, "no Origin header is allowed");
}

#[test]
fn caching_streaming_and_compression() {
    if !needs_python() {
        return;
    }
    let cfg = format!(
        r#"{PY}
[[route]]
path = "/counter"
method = "GET"
handler = "src/h.py#count"
cache = {{ ttl = "1m" }}

[[route]]
path = "/events"
method = "GET"
handler = "src/h.py#events"

[[route]]
path = "/big"
method = "GET"
handler = "src/h.py#big"
"#
    );
    let p = Project::new(&[
        ("unsworn.toml", &cfg),
        ("worker.py", ECHO_WORKER),
        ("src/h.py", ""),
    ]);
    let s = p.serve();

    let first = s.get("/counter").json()["count"].clone();
    let r = s.get("/counter");
    assert_eq!(
        r.json()["count"],
        first,
        "second response comes from the cache"
    );
    assert!(r.header("age").is_some());
    assert_ne!(
        s.get("/counter?x=1").json()["count"],
        first,
        "the query is part of the key"
    );

    let r = s.get("/events");
    assert_eq!(r.header("content-type"), Some("text/event-stream"));
    assert_eq!(r.text(), "data: 0\n\ndata: 1\n\ndata: 2\n\n");
    let r = s.request("GET", "/events", &[("accept-encoding", "gzip")], b"");
    assert_eq!(
        r.header("content-encoding"),
        None,
        "SSE is never compressed"
    );

    let r = s.request("GET", "/big", &[("accept-encoding", "gzip")], b"");
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert!(r.body.len() < 1000, "compressed size {}", r.body.len());
    let r = s.get("/big");
    assert_eq!(r.header("content-encoding"), None);
    assert_eq!(r.json()["items"].as_array().unwrap().len(), 500);
}

#[test]
fn proxy_and_catch_all_migration() {
    if !needs_python() {
        return;
    }
    // The "existing app": another Unsworn instance echoing requests.
    let app_cfg = format!(
        r#"{PY}
[[route]]
path = "/{{*rest}}"
handler = "src/app.py#echo"
"#
    );
    let app = Project::new(&[
        ("unsworn.toml", &app_cfg),
        ("worker.py", ECHO_WORKER),
        ("src/app.py", ""),
    ]);
    let app_server = app.serve();

    let cfg = format!(
        r#"
[auth.service]
type = "api_key"
keys = "secret"

[[route]]
path = "/{{*rest}}"
proxy = "http://{}"

[[route]]
path = "/admin/{{*rest}}"
auth = "service"

[[route]]
path = "/legacy/{{*rest}}"
proxy = {{ upstreams = ["http://{}"], strip_prefix = "/legacy" }}

[[route]]
path = "/health"
respond = {{ status = 200, text = "front" }}
"#,
        app_server.addr, app_server.addr
    );
    let front = Project::new(&[("unsworn.toml", &cfg)]);
    let s = front.serve();

    assert_eq!(
        s.get("/health").text(),
        "front",
        "specific routes win over the catch-all"
    );

    let r = s.request(
        "POST",
        "/orders/9?x=1",
        &[("content-type", "application/json")],
        br#"{"a":1}"#,
    );
    assert_eq!(r.status, 200, "{}", r.text());
    let v = r.json();
    assert_eq!(v["path"], "/orders/9");
    assert_eq!(v["method"], "POST");
    assert_eq!(v["query"]["x"], "1");
    assert_eq!(v["headers"]["x-forwarded-for"], "127.0.0.1");
    assert!(v["headers"]["x-request-id"].is_string());

    assert_eq!(
        s.get("/admin/users").status,
        401,
        "prefix policy inside the catch-all"
    );
    let r = s.request("GET", "/admin/users", &[("x-api-key", "secret")], b"");
    assert_eq!(r.json()["path"], "/admin/users");

    assert_eq!(s.get("/legacy/thing").json()["path"], "/thing");

    // An upstream that's down gives a 502.
    let dead = Project::new(&[(
        "unsworn.toml",
        &format!(
            "[[route]]\npath = \"/{{*rest}}\"\nproxy = \"http://127.0.0.1:{}\"\n",
            common::free_port()
        ),
    )]);
    let d = dead.serve();
    assert_eq!(d.get("/x").status, 502);
    drop(app_server);
}

#[test]
fn check_routes_and_openapi_commands() {
    let p = Project::new(&[(
        "unsworn.toml",
        r#"
[openapi]
title = "Test API"

[[route]]
path = "/items/{id}"
method = "GET"
params = { id = { type = "integer" } }
respond = { status = 200 }

[[route]]
path = "/me"
respond = { status = 200 }
cache = { ttl = "1m" }
rate_limit = { requests = 1, per = "1s" }
"#,
    )]);
    let out = p.command(&["check"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("2 routes OK"));

    let out = p.command(&["check", "--target", "vercel"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs a shared store"));

    let out = p.command(&["routes"]);
    let table = String::from_utf8_lossy(&out.stdout);
    assert!(
        table.contains("/items/{id}") && table.contains("unsworn.toml:"),
        "{table}"
    );

    let out = p.command(&["openapi"]);
    let doc: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["info"]["title"], "Test API");
    assert_eq!(
        doc["paths"]["/items/{id}"]["get"]["parameters"][0]["schema"]["type"],
        "integer"
    );

    let bad = Project::new(&[(
        "unsworn.toml",
        "[[route]]\npath = \"/x\"\nhandler = \"a.rb\"\n",
    )]);
    let out = bad.command(&["check"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no runtime handles \".rb\""));

    let empty = Project::new(&[]);
    assert!(empty.command(&["init"]).status.success());
    assert!(empty.command(&["check"]).status.success());
}

#[test]
fn reload_on_sighup() {
    let p = Project::new(&[(
        "unsworn.toml",
        "[[route]]\npath = \"/a\"\nrespond = { status = 200, text = \"one\" }\n",
    )]);
    let s = p.serve();
    assert_eq!(s.get("/a").text(), "one");
    p.write(
        "unsworn.toml",
        "[[route]]\npath = \"/a\"\nrespond = { status = 200, text = \"two\" }\n",
    );
    std::process::Command::new("kill")
        .arg("-HUP")
        .arg(s.pid().to_string())
        .status()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if s.get("/a").text() == "two" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "reload didn't happen:\n{}",
            s.logs()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // A broken config keeps the previous version running.
    p.write("unsworn.toml", "[[route]]\npath = \"nope\"\n");
    std::process::Command::new("kill")
        .arg("-HUP")
        .arg(s.pid().to_string())
        .status()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(s.get("/a").text(), "two");
}

#[test]
fn js_sdk_on_bun() {
    if !has("bun") {
        eprintln!("skipping: bun not found");
        return;
    }
    let root = common::repo_root();
    let cfg = format!(
        r#"
[runtimes.ts]
extensions = [".ts"]
command = "bun run {}/sdk/js/src/worker.ts"
protocol = "binary"
workers = 1

[[route]]
path = "/hello/{{name}}"
method = "GET"
handler = "src/hello.ts#hello"

[[route]]
path = "/app/{{*rest}}"
handler = "src/app.ts"
"#,
        root.display()
    );
    let hello = r#"
export const hello = (req, ctx) => ({ json: { hi: req.params.name } });
"#;
    let app = r#"
export default {
  fetch(req) {
    const url = new URL(req.url);
    return new Response(JSON.stringify({ fetched: url.pathname }), {
      headers: { "content-type": "application/json", "set-cookie": "a=1" },
    });
  },
};
"#;
    let p = Project::new(&[
        ("unsworn.toml", &cfg),
        ("src/hello.ts", hello),
        ("src/app.ts", app),
    ]);
    let s = p.start("dev");
    let r = s.get("/hello/ada");
    assert_eq!(r.status, 200, "{}\n{}", r.text(), s.logs());
    assert_eq!(r.json()["hi"], "ada");
    let r = s.get("/app/inner/path");
    assert_eq!(r.json()["fetched"], "/app/inner/path");
    assert_eq!(r.header("set-cookie"), Some("a=1"));
}

#[test]
fn websocket_pass_through() {
    use std::io::{Read, Write};
    if !has("bun") {
        eprintln!("skipping: bun not found");
        return;
    }
    let port = common::free_port();
    let server_js = format!(
        r#"Bun.serve({{ port: {port}, fetch(req, srv) {{ if (srv.upgrade(req)) return; return new Response("no"); }},
          websocket: {{ message(ws, msg) {{ ws.send("echo:" + msg); }} }} }});"#
    );
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ws.js"), server_js).unwrap();
    let mut upstream = std::process::Command::new("bun")
        .arg("run")
        .arg(dir.path().join("ws.js"))
        .spawn()
        .unwrap();
    let up_addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    for _ in 0..200 {
        if std::net::TcpStream::connect(up_addr).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let p = Project::new(&[(
        "unsworn.toml",
        &format!("[[route]]\npath = \"/ws\"\nproxy = \"http://{up_addr}\"\n"),
    )]);
    let s = p.serve();

    let mut stream = std::net::TcpStream::connect(s.addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /ws HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        s.addr
    )
    .unwrap();
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).unwrap();
    let head = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");

    // A masked text frame: "hi".
    let mask = [1u8, 2, 3, 4];
    let payload = b"hi";
    let mut frame = vec![0x81, 0x80 | payload.len() as u8];
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    stream.write_all(&frame).unwrap();
    let n = stream.read(&mut buf).unwrap();
    assert_eq!(buf[0], 0x81, "text frame back");
    let len = (buf[1] & 0x7f) as usize;
    assert_eq!(&buf[2..2 + len], b"echo:hi", "{:?}", &buf[..n]);
    let _ = upstream.kill();
}

#[test]
fn tls() {
    if !has("openssl") || !has("curl") {
        eprintln!("skipping: openssl or curl not found");
        return;
    }
    let p = Project::new(&[(
        "unsworn.toml",
        "[server.tls]\ncert = \"cert.pem\"\nkey = \"key.pem\"\n\n[[route]]\npath = \"/\"\nrespond = { status = 200, text = \"secure\" }\n",
    )]);
    let ok = std::process::Command::new("openssl")
        .current_dir(p.path())
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            "key.pem",
            "-out",
            "cert.pem",
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
        ])
        .output()
        .unwrap();
    assert!(ok.status.success());
    let s = p.serve();
    let out = std::process::Command::new("curl")
        .args([
            "-sk",
            "--http2",
            "-w",
            " %{http_version}",
            &format!("https://{}/", s.addr),
        ])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "secure 2");
}
