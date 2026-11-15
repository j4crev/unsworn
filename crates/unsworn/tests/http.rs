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

fn r_etag(r: &common::Response) -> String {
    r.header("etag").expect("an ETag").to_string()
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
        ("public/.env", "SECRET=1"),
        (
            "public/.well-known/security.txt",
            "Contact: mailto:security@example.com",
        ),
        ("public/app.css", "body{}"),
        ("public/big.txt", &"unsworn ".repeat(400)),
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
    assert_eq!(r.header("allow"), Some("GET, HEAD"));
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
    let r = s.request("POST", "/assets/app.css", &[], b"");
    assert_eq!((r.status, r.header("allow")), (405, Some("GET, HEAD")));

    // A range applies only while `If-Range` still names the file.
    let etag = r_etag(&s.get("/assets/app.css"));
    assert!(etag.starts_with('"'), "a strong ETag: {etag}");
    let range = |condition: &[(&str, &str)]| {
        let mut headers = vec![("range", "bytes=0-3")];
        headers.extend_from_slice(condition);
        let r = s.request("GET", "/assets/app.css", &headers, b"");
        (r.status, r.text())
    };
    assert_eq!(range(&[]), (206, "body".into()));
    assert_eq!(range(&[("if-range", &etag)]), (206, "body".into()));
    assert_eq!(range(&[("if-range", "\"stale\"")]), (200, "body{}".into()));
    let weak = format!("W/{etag}");
    assert_eq!(range(&[("if-range", &weak)]), (200, "body{}".into()));
    assert_eq!(
        range(&[("if-range", "Thu, 01 Jan 1970 00:00:00 GMT")]),
        (200, "body{}".into())
    );

    // Compressed on the way out, a file no longer has its strong ETag.
    let plain = r_etag(&s.get("/assets/big.txt"));
    let r = s.request(
        "GET",
        "/assets/big.txt",
        &[("accept-encoding", "gzip")],
        b"",
    );
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    assert_eq!(r_etag(&r), format!("W/{plain}"));

    let r = s.get("/assets/some/client/route");
    assert_eq!((r.status, r.text().as_str()), (200, "<h1>app</h1>"));
    for path in ["/assets/../unsworn.toml", "/assets/%2e%2e/unsworn.toml"] {
        let r = s.get(path);
        assert!(
            !r.text().contains("[[route]]"),
            "{path} leaked a file outside the dir"
        );
    }

    // Dotfiles in a served directory stay private; `.well-known` doesn't.
    assert_eq!(s.get("/assets/.env").status, 404);
    assert_eq!(s.get("/assets/.well-known/security.txt").status, 200);

    // The public health check says whether the instance is healthy, and
    // nothing about what it's made of.
    let r = s.get("/_unsworn/health");
    assert_eq!(r.json(), serde_json::json!({ "status": "ok" }));
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

[[route]]
path = "/office"
ip = { allow = ["10.0.0.0/8"] }
respond = { status = 200 }

[[route]]
path = "/local"
ip = { allow = ["127.0.0.0/8"], deny = ["10.1.2.3"] }
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

    assert_eq!(s.get("/office").status, 403);
    assert_eq!(s.get("/local").status, 200);
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

[[route]]
path = "/huge"
method = "GET"
handler = "src/h.py#huge"
cache = {{ ttl = "1m" }}

[[route]]
path = "/huge-stream"
method = "GET"
handler = "src/h.py#huge_stream"
cache = {{ ttl = "1m" }}
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

    // Too large to cache: passed through whole, and not cached.
    for path in ["/huge", "/huge-stream"] {
        for _ in 0..2 {
            let r = s.get(path);
            assert_eq!(r.status, 200, "{path}");
            assert_eq!(r.body.len(), 1_500_000, "{path}");
            assert!(r.header("age").is_none(), "{path} isn't cached");
        }
    }
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
fn serve_no_build_skips_build_commands() {
    if !needs_python() {
        return;
    }
    let cfg = r#"
[runtimes.py]
extensions = [".py"]
build = "touch built"
command = "python3 -u worker.py"
workers = 1
eager = true

[[route]]
path = "/a"
handler = "src/a.py#a"
"#;
    let p = Project::new(&[
        ("unsworn.toml", cfg),
        ("worker.py", ECHO_WORKER),
        ("src/a.py", ""),
    ]);
    let built = p.path().join("built");
    {
        let s = p.start_with(&["serve", "--no-build"]);
        assert_eq!(s.get("/a").status, 200, "{}", s.logs());
        assert!(!built.exists(), "--no-build ran the build command");
    }
    let s = p.serve();
    assert_eq!(s.get("/a").status, 200, "{}", s.logs());
    assert!(built.exists(), "serve didn't run the build command");
}

#[test]
fn a_skipped_build_is_named_when_its_worker_is_missing() {
    let cfg = r#"
[runtimes.go]
extensions = [".go"]
build = "go build -o .unsworn/go-worker ./src"
command = ".unsworn/go-worker"
protocol = "framed"
workers = 1
eager = true

[[route]]
path = "/a"
handler = "src/a.go#A"
"#;
    let p = Project::new(&[("unsworn.toml", cfg), ("src/a.go", "")]);
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_unsworn"))
        .arg("-C")
        .arg(p.path())
        .args(["serve", "--listen", "127.0.0.1:0"])
        .env("UNSWORN_NO_BUILD", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("[runtimes.go].build (`go build -o .unsworn/go-worker ./src`) didn't run"),
        "{stderr}"
    );
    // `0` turns building back on: the build runs (and fails on an empty file).
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_unsworn"))
        .arg("-C")
        .arg(p.path())
        .args(["serve", "--listen", "127.0.0.1:0"])
        .env("UNSWORN_NO_BUILD", "0")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("build failed"), "{stderr}");
    assert!(!stderr.contains("didn't run"), "{stderr}");
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
fn workers_dont_inherit_credentials() {
    if !needs_python() {
        return;
    }
    // A worker that answers with the variables it can see.
    let worker = r#"
import json, os, sys
out = sys.stdout.buffer
def send(message):
    out.write(json.dumps(message).encode() + b"\n")
    out.flush()
send({"type": "ready", "protocol": 1})
for line in sys.stdin.buffer:
    msg = json.loads(line)
    if msg["type"] == "request":
        names = ["JWT_SECRET", "API_KEYS", "VERIFIER_TOKEN", "PASSED_ON", "ISSUER", "UNRELATED"]
        send({"type": "response", "id": msg["id"], "status": 200,
              "json": {n: os.environ.get(n) for n in names}})
"#;
    let cfg = r#"
[runtimes.py]
extensions = [".py"]
command = "python3 worker.py"
env = { PASSED_ON = "{env.PASSED_ON}" }

[auth.users]
type = "jwt"
secret = "{env.JWT_SECRET}"
issuer = "{env.ISSUER}"

[auth.keys]
type = "api_key"
keys = "{env.API_KEYS}"

[auth.remote]
type = "api_key"
header = "X-Remote-Key"
url = "http://127.0.0.1:9/verify"
headers = { Authorization = "Bearer {env.VERIFIER_TOKEN}" }

[auth.partners]
type = "jwt"
secret = "{env.PASSED_ON}"

[[route]]
path = "/env"
method = "GET"
handler = "worker.py#env"
"#;
    let p = Project::new(&[("unsworn.toml", cfg), ("worker.py", worker)]);
    let s = p.start_with_env(
        &["serve"],
        &[
            ("JWT_SECRET", "0123456789abcdef0123456789abcdef"),
            ("API_KEYS", "k1,k2"),
            ("VERIFIER_TOKEN", "service-token"),
            ("PASSED_ON", "fedcba9876543210fedcba9876543210"),
            ("ISSUER", "https://auth.example/"),
            ("UNRELATED", "kept"),
        ],
    );
    let r = s.get("/env");
    assert_eq!(r.status, 200, "{}\n{}", r.text(), s.logs());
    assert_eq!(
        r.json(),
        serde_json::json!({
            // What Unsworn checks credentials with stays with Unsworn,
            "JWT_SECRET": null,
            "API_KEYS": null,
            "VERIFIER_TOKEN": null,
            // unless the runtime's `env` asks for it.
            "PASSED_ON": "fedcba9876543210fedcba9876543210",
            // Everything else is inherited as before.
            "ISSUER": "https://auth.example/",
            "UNRELATED": "kept",
        })
    );
    let logs = s.logs();
    assert!(
        logs.contains("not passing API_KEYS, JWT_SECRET, VERIFIER_TOKEN to workers"),
        "{logs}"
    );
}

#[test]
fn connections_are_limited() {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    let config = |server: &str| {
        format!(
            "[server]\n{server}\n\n[[route]]\npath = \"/a\"\nrespond = {{ status = 200, text = \"ok\" }}\n"
        )
    };
    let connect = |s: &common::Server| {
        let stream = TcpStream::connect(s.addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
    };

    let p = Project::new(&[("unsworn.toml", &config("idle_timeout = \"300ms\""))]);
    let s = p.serve();
    // A connection that never sends a request is closed.
    let start = Instant::now();
    let mut silent = connect(&s);
    let mut raw = Vec::new();
    silent.read_to_end(&mut raw).unwrap();
    assert!(raw.is_empty());
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    // So is one that stalls in the middle of its headers,
    let start = Instant::now();
    let mut slow = connect(&s);
    slow.write_all(b"GET /a HTTP/1.1\r\nHost: x\r\n").unwrap();
    let mut raw = Vec::new();
    let _ = slow.read_to_end(&mut raw);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    // and a kept-alive one that sends nothing more.
    let start = Instant::now();
    let mut idle = connect(&s);
    idle.write_all(b"GET /a HTTP/1.1\r\nHost: x\r\n\r\n")
        .unwrap();
    let mut raw = Vec::new();
    let _ = idle.read_to_end(&mut raw);
    assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 200"));
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );

    // At `max_connections`, the next connection waits for one to close.
    let p = Project::new(&[(
        "unsworn.toml",
        &config("max_connections = 1\nidle_timeout = \"1500ms\""),
    )]);
    let s = p.serve();
    let _held = connect(&s);
    std::thread::sleep(Duration::from_millis(100));
    let start = Instant::now();
    assert_eq!(s.get("/a").text(), "ok");
    assert!(
        start.elapsed() > Duration::from_secs(1),
        "{:?}",
        start.elapsed()
    );
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
protocol = "framed"
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
        &format!(
            "[server]\nmax_connections = 1\nidle_timeout = \"1s\"\n\n[[route]]\npath = \"/ws\"\nproxy = \"http://{up_addr}\"\n"
        ),
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

    // The open WebSocket still counts toward `max_connections`, so the next
    // connection waits until it closes for being idle.
    let start = std::time::Instant::now();
    s.get("/ws");
    let waited = start.elapsed();
    assert!(waited > std::time::Duration::from_millis(700), "{waited:?}");
    assert!(waited < std::time::Duration::from_secs(4), "{waited:?}");
    assert_eq!(stream.read(&mut buf).unwrap_or(0), 0, "closed when idle");
    let _ = upstream.kill();
    let _ = upstream.wait();
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

#[test]
fn published_schema_is_up_to_date() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_unsworn"))
        .arg("schema")
        .output()
        .unwrap();
    let generated: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let path = common::repo_root().join("schema/unsworn.schema.json");
    let committed: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(
        generated, committed,
        "schema/unsworn.schema.json is stale: run `unsworn schema > schema/unsworn.schema.json`"
    );
}

#[test]
fn worker_log_records() {
    if !needs_python() {
        return;
    }
    // A worker with no SDK: one `log` message, one JSON line on stderr in the
    // shape JSON loggers write, and one plain stderr line.
    let worker = r##"
import json, os, sys
out = sys.stdout.buffer
def send(m):
    out.write(json.dumps(m).encode() + b"\n"); out.flush()
send({"type": "ready", "protocol": 1})
for line in sys.stdin:
    msg = json.loads(line)
    if msg.get("type") != "request":
        continue
    rid = msg["id"]
    send({"type": "log", "id": rid, "level": "warn", "message": "via protocol", "fields": {"amount": 1200}})
    print(json.dumps({"level": 30, "msg": "via json stderr", "id": rid, "user": "ada"}), file=sys.stderr, flush=True)
    print("plain stderr line", file=sys.stderr, flush=True)
    send({"type": "response", "id": rid, "status": 200, "json": {"level": os.environ.get("UNSWORN_LOG_LEVEL")}})
"##;
    let cfg = format!(
        r#"{PY}
[[route]]
path = "/work"
handler = "src/work.py"
"#
    );
    let p = Project::new(&[
        ("unsworn.toml", &cfg),
        ("worker.py", worker),
        ("src/work.py", ""),
    ]);
    let s = p.start_with(&["serve", "--log-format", "json"]);
    let r = s.request("GET", "/work", &[("x-request-id", "req-abc")], b"");
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        r.json()["level"],
        "info",
        "workers learn Unsworn's log level"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let find = |message: &str| loop {
        let found = s
            .logs()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|v| v["message"] == message);
        if let Some(v) = found {
            return v;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no log record {message:?} in:\n{}",
            s.logs()
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    let v = find("via protocol");
    assert_eq!(v["level"], "WARN");
    assert_eq!(v["id"], "req-abc");
    assert_eq!(v["route"], "/work");
    assert_eq!(v["runtime"], "py");
    assert_eq!(v["fields"], r#"{"amount":1200}"#);

    let v = find("via json stderr");
    assert_eq!(v["level"], "INFO");
    assert_eq!(v["id"], "req-abc");
    assert_eq!(v["fields"], r#"{"user":"ada"}"#);

    let v = find("plain stderr line");
    assert_eq!(v["stream"], "stderr");
    assert!(v.get("id").is_none());
}

#[test]
fn stream_body_uploads() {
    if !needs_python() {
        return;
    }
    let cfg = r#"
[runtimes.py]
extensions = [".py"]
command = "python3 -u -m unsworn.worker"
protocol = "framed"
workers = 1

[runtimes.legacy]
extensions = [".legacy"]
command = "python3 -u worker.py"
workers = 1

[[route]]
path = "/upload"
method = "POST"
handler = "src/upload.py#upload"
stream_body = true
max_body = "4MB"

[[route]]
path = "/legacy"
method = "POST"
handler = "src/old.legacy"
stream_body = true
"#;
    let handler = r#"
from unsworn import json

async def upload(req, ctx):
    n = total = 0
    async for chunk in req.stream:
        n += len(chunk)
        total = (total + sum(chunk)) % (1 << 32)
    return json({"bytes": n, "sum": total})
"#;
    let p = Project::new(&[
        ("unsworn.toml", cfg),
        ("src/upload.py", handler),
        ("worker.py", ECHO_WORKER),
        ("src/old.legacy", ""),
    ]);
    let s = p.serve();

    // Larger than the credit window, so the worker's acks pace the upload.
    let body: Vec<u8> = (0..3 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    let sum = body
        .iter()
        .fold(0u64, |acc, b| (acc + *b as u64) % (1 << 32));
    let r = s.request("POST", "/upload", &[], &body);
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["bytes"], body.len());
    assert_eq!(r.json()["sum"], sum);

    // A declared length over max_body is refused up front...
    let big = vec![0u8; 5 * 1024 * 1024];
    assert_eq!(upload(&s, "/upload", &big, false).status, 413);
    // ...and a chunked body is cut off once it passes max_body.
    assert_eq!(upload(&s, "/upload", &big, true).status, 413);

    // Workers that don't support streamed bodies are reported, not hung.
    let r = s.request("POST", "/legacy", &[], b"hello");
    assert_eq!(r.status, 500);
    assert!(
        s.logs().contains("doesn't accept streamed request bodies"),
        "{}",
        s.logs()
    );
}

/// POSTs `body`, chunked (so Unsworn can't know its length up front) or
/// with a Content-Length, and reads the answer even if Unsworn stops
/// reading the body early.
fn upload(s: &common::Server, path: &str, body: &[u8], chunked: bool) -> common::Response {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(s.addr).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .unwrap();
    let length = if chunked {
        "Transfer-Encoding: chunked".to_string()
    } else {
        format!("Content-Length: {}", body.len())
    };
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{length}\r\n\r\n",
        s.addr
    );
    stream.write_all(head.as_bytes()).unwrap();
    for chunk in body.chunks(64 * 1024) {
        let sent = if chunked {
            stream
                .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                .and_then(|_| stream.write_all(chunk))
                .and_then(|_| stream.write_all(b"\r\n"))
        } else {
            stream.write_all(chunk)
        };
        if sent.is_err() {
            break; // Unsworn answered and closed the connection
        }
    }
    if chunked {
        let _ = stream.write_all(b"0\r\n\r\n");
    }
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    common::parse_response(&raw)
}

#[test]
fn redis_store_is_shared_and_fails_open() {
    // Health details, such as the store's state, are on the admin listener.
    let admin = common::free_port();
    let details = || {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], admin));
        common::http(addr, "GET", "/_unsworn/health", &[], b"")
    };
    let config = |store: &str, server: &str| {
        format!(
            r#"
{server}

{store}

[[route]]
path = "/limited"
respond = {{ status = 200, text = "ok" }}
rate_limit = {{ requests = 2, per = "1m", key = "ip" }}
"#
        )
    };
    let with_admin = format!("[server]\nadmin_listen = \"127.0.0.1:{admin}\"");
    let rate_limited = |store: &str| config(store, &with_admin);

    // Unreachable: rate limits let requests through (on_error = "open" by
    // default), health stays ok but reports it, and metrics count errors.
    let down = r#"[store]
backend = "redis"
url = "redis://127.0.0.1:1""#;
    let p = Project::new(&[("unsworn.toml", &rate_limited(down))]);
    let s = p.serve();
    for _ in 0..3 {
        assert_eq!(s.get("/limited").status, 200);
    }
    assert_eq!(s.get("/_unsworn/health").status, 200);
    let health = details();
    assert_eq!(health.status, 200, "{}", health.text());
    assert_eq!(health.json()["store"]["reachable"], false);
    assert!(health.json()["version"].is_string());
    assert!(
        s.logs().contains("letting requests through uncounted"),
        "{}",
        s.logs()
    );
    drop(s);

    // on_error = "closed" answers 503 instead.
    let closed = format!("{down}\non_error = \"closed\"");
    let p = Project::new(&[("unsworn.toml", &rate_limited(&closed))]);
    let s = p.serve();
    assert_eq!(s.get("/limited").status, 503);
    drop(s);

    // Two instances sharing a real Redis share one rate limit.
    let Ok(url) = std::env::var("UNSWORN_TEST_REDIS_URL") else {
        eprintln!("skipping the shared part: set UNSWORN_TEST_REDIS_URL");
        return;
    };
    let shared = format!(
        "[store]\nbackend = \"redis\"\nurl = \"{url}\"\nprefix = \"e2e-{}:\"",
        std::process::id()
    );
    let p = Project::new(&[("unsworn.toml", &rate_limited(&shared))]);
    let a = p.serve();
    // The second instance leaves the admin port to the first.
    let p = Project::new(&[("unsworn.toml", &config(&shared, ""))]);
    let b = p.serve();
    assert_eq!(a.get("/limited").status, 200);
    assert_eq!(b.get("/limited").status, 200);
    assert_eq!(a.get("/limited").status, 429, "the limit is shared");
    assert_eq!(b.get("/limited").status, 429);
    assert_eq!(details().json()["store"]["reachable"], true);
}

/// A stand-in API-key verifier: answers by key, and records each request's
/// `Authorization` header.
fn mock_verifier() -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let log = log.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut len, mut auth) = (0, String::new());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                    let (name, value) = line.split_once(':').unwrap_or_default();
                    match name.to_ascii_lowercase().as_str() {
                        "content-length" => len = value.trim().parse().unwrap_or(0),
                        "authorization" => auth = value.trim().to_string(),
                        _ => {}
                    }
                }
                let mut body = vec![0; len];
                reader.read_exact(&mut body).unwrap();
                log.lock().unwrap().push(auth);
                // An RFC 7662 introspection request: `token=<key>`.
                let key = String::from_utf8(body)
                    .unwrap()
                    .strip_prefix("token=")
                    .expect("a form-encoded token")
                    .to_string();
                let (status, reply) = match key.as_str() {
                    "good" => (
                        "200 OK",
                        r#"{"active":true,"sub":"cust_1","scope":"reports:read"}"#,
                    ),
                    "boom" => ("500 Internal Server Error", "{}"),
                    "slow" => {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                        ("200 OK", r#"{"active":true}"#)
                    }
                    _ => ("200 OK", r#"{"active":false}"#),
                };
                let mut stream = stream;
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
            });
        }
    });
    (addr, seen)
}

#[test]
fn remote_api_key_verification() {
    let (verifier, seen) = mock_verifier();
    let cfg = format!(
        r#"
[auth.customers]
type    = "api_key"
header  = "X-API-Key"
url     = "http://{verifier}/verify-key"
timeout = "300ms"
headers = {{ Authorization = "Bearer service-token" }}

[auth.down]
type = "api_key"
url  = "http://127.0.0.1:{}/verify-key"

[auth.cached]
type   = "api_key"
header = "X-Cached-Key"
url    = "http://{verifier}/verify-key"
cache  = "1m"

[[route]]
path = "/reports"
auth = {{ scheme = "customers", scopes = ["reports:read"] }}
rate_limit = {{ requests = 2, per = "1m", key = "auth.sub" }}
respond = {{ status = 200, text = "ok" }}

[[route]]
path = "/admin"
auth = {{ scheme = "customers", scopes = ["admin"] }}
respond = {{ status = 200 }}

[[route]]
path = "/down"
auth = "down"
respond = {{ status = 200 }}

[[route]]
path = "/cached"
auth = "cached"
respond = {{ status = 200 }}
"#,
        common::free_port()
    );
    let p = Project::new(&[("unsworn.toml", &cfg)]);
    let s = p.serve();
    let with_key = |path: &str, key: &str| s.request("GET", path, &[("x-api-key", key)], b"");

    assert_eq!(s.get("/reports").status, 401, "no key");
    assert!(seen.lock().unwrap().is_empty(), "no key, no verifier call");

    let r = with_key("/reports", "good");
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(seen.lock().unwrap()[0], "Bearer service-token");

    assert_eq!(with_key("/reports", "bad").status, 401);
    assert_eq!(
        with_key("/admin", "good").status,
        403,
        "claims carry scopes"
    );
    // Rate limits key on the verifier's claims: the third `cust_1` request is
    // over the limit.
    assert_eq!(with_key("/reports", "good").status, 200);
    assert_eq!(with_key("/reports", "good").status, 429);

    for (path, key) in [("/reports", "boom"), ("/reports", "slow"), ("/down", "any")] {
        let r = with_key(path, key);
        assert_eq!(r.status, 503, "{path} {key}: {}", r.text());
        assert!(r.header("www-authenticate").is_none());
    }
    assert!(s.logs().contains("can't verify API key"), "{}", s.logs());

    // With `cache`, each key reaches the verifier once, valid or not; an
    // outage isn't cached.
    let cached = |key: &str| s.request("GET", "/cached", &[("x-cached-key", key)], b"");
    let calls = || seen.lock().unwrap().len();
    let before = calls();
    for (key, status) in [("good", 200), ("good", 200), ("bad", 401), ("bad", 401)] {
        assert_eq!(cached(key).status, status, "{key}");
    }
    assert_eq!(calls() - before, 2, "one call per key");
    assert_eq!(cached("boom").status, 503);
    assert_eq!(cached("boom").status, 503);
    assert_eq!(calls() - before, 4, "outages are asked about again");
}

/// A JWT for the `oauth` scheme used by the MCP tests.
fn mcp_token(scope: &str) -> String {
    use jsonwebtoken::{EncodingKey, Header, encode};
    let token = encode(
        &Header::default(),
        &json!({"sub": "u1", "aud": "https://api.example.com/mcp", "scope": scope, "exp": 4_000_000_000u64}),
        &EncodingKey::from_secret(b"test-secret").unwrap(),
    )
    .unwrap();
    format!("Bearer {token}")
}

/// An MCP 2026-07-28 request body.
fn mcp_body(method: &str, params: serde_json::Value) -> Vec<u8> {
    let mut params = params;
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": {},
        "io.modelcontextprotocol/clientInfo": {"name": "test-client", "version": "1"}
    });
    serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
        .unwrap()
}

#[test]
fn mcp_proxy_with_per_tool_policies() {
    if !needs_python() {
        return;
    }
    // The "existing MCP server": another Unsworn instance echoing requests.
    let app_cfg = format!("{PY}\n[[route]]\npath = \"/mcp\"\nhandler = \"src/app.py#echo\"\n");
    let app = Project::new(&[
        ("unsworn.toml", &app_cfg),
        ("worker.py", ECHO_WORKER),
        ("src/app.py", ""),
    ]);
    let app_server = app.serve();

    let cfg = format!(
        r#"{PY}
[auth.oauth]
type = "jwt"
secret = "test-secret"
audience = "https://api.example.com/mcp"

[mcp]
proxy = "http://{}"
auth = {{ scheme = "oauth", scopes = ["tools:read"] }}

[[mcp.tool]]
name = "get_forecast"
input = {{ type = "object", required = ["city"], properties = {{ city = {{ type = "string" }}, tenant = {{ type = "string", x-mcp-header = "Tenant" }} }} }}
rate_limit = {{ requests = 1, per = "1m", key = "header.mcp-param-tenant" }}

[[mcp.tool]]
name = "create_invoice"
auth = {{ scheme = "oauth", scopes = ["billing:write"] }}
before = ["src/mcp.py#tenant"]
"#,
        app_server.addr
    );
    let front = Project::new(&[
        ("unsworn.toml", &cfg),
        ("worker.py", ECHO_WORKER),
        ("src/mcp.py", ""),
    ]);
    let s = front.serve();

    let read = mcp_token("tools:read");
    let both = mcp_token("tools:read billing:write");
    let post =
        |token: &str, method: &str, name: Option<&str>, extra: &[(&str, &str)], body: &[u8]| {
            let mut headers = vec![
                ("authorization", token),
                ("content-type", "application/json"),
                ("accept", "application/json, text/event-stream"),
                ("mcp-protocol-version", "2026-07-28"),
                ("mcp-method", method),
            ];
            if let Some(n) = name {
                headers.push(("mcp-name", n));
            }
            headers.extend_from_slice(extra);
            s.request("POST", "/mcp", &headers, body)
        };
    let call = |token: &str, name: &str, args: serde_json::Value, extra: &[(&str, &str)]| {
        let body = mcp_body("tools/call", json!({"name": name, "arguments": args}));
        post(token, "tools/call", Some(name), extra, &body)
    };
    let tool_error = |r: &common::Response| -> String {
        assert_eq!(r.status, 200, "{}", r.text());
        let v = r.json();
        assert_eq!(v["result"]["isError"], true, "{v}");
        assert_eq!(v["result"]["resultType"], "complete");
        v["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let rpc_code = |r: &common::Response| -> i64 {
        assert_eq!(r.status, 400, "{}", r.text());
        r.json()["error"]["code"].as_i64().unwrap()
    };

    // Other methods pass through after the [mcp] checks.
    let r = post(
        &read,
        "tools/list",
        None,
        &[],
        &mcp_body("tools/list", json!({})),
    );
    assert_eq!(r.status, 200, "{}", r.text());
    let echoed = r.json();
    assert_eq!(
        echoed["json"]["method"], "tools/list",
        "the body is forwarded as is"
    );
    assert_eq!(echoed["headers"]["mcp-method"], "tools/list");

    let r = s.request("POST", "/mcp", &[], &mcp_body("tools/list", json!({})));
    assert_eq!(r.status, 401, "[mcp] auth runs first");
    assert_eq!(s.get("/mcp").status, 405);

    // Protocol checks.
    assert_eq!(
        rpc_code(&post(&read, "tools/list", None, &[], b"{")),
        -32700
    );
    let init = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25"}}"#;
    let r = post(&read, "initialize", None, &[], init);
    assert_eq!(rpc_code(&r), -32022);
    assert_eq!(
        r.json()["error"]["data"]["supported"],
        json!(["2026-07-28"])
    );
    let body = mcp_body(
        "tools/call",
        json!({"name": "get_forecast", "arguments": {"city": "Lisbon"}}),
    );
    assert_eq!(
        rpc_code(&post(&read, "tools/call", Some("other"), &[], &body)),
        -32020
    );
    assert_eq!(
        rpc_code(&post(&read, "tools/list", Some("get_forecast"), &[], &body)),
        -32020
    );

    // Input validation is a tool error the model can fix.
    let text = tool_error(&call(&read, "get_forecast", json!({}), &[]));
    assert!(
        text.contains("Invalid arguments") && text.contains("city"),
        "{text}"
    );

    // `Mcp-Param-*` headers must match the arguments, and then key the tool's rate limit.
    let r = call(
        &read,
        "get_forecast",
        json!({"city": "Lisbon", "tenant": "a"}),
        &[("mcp-param-tenant", "b")],
    );
    assert_eq!(rpc_code(&r), -32020);
    let a = [("mcp-param-tenant", "a")];
    let r = call(
        &read,
        "get_forecast",
        json!({"city": "Lisbon", "tenant": "a"}),
        &a,
    );
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(
        r.json()["json"]["params"]["name"],
        "get_forecast",
        "forwarded"
    );
    let text = tool_error(&call(
        &read,
        "get_forecast",
        json!({"city": "Porto", "tenant": "a"}),
        &a,
    ));
    assert!(text.contains("Rate limit exceeded"), "{text}");
    let b = [("mcp-param-tenant", "b")];
    let r = call(
        &read,
        "get_forecast",
        json!({"city": "Porto", "tenant": "b"}),
        &b,
    );
    assert_eq!(r.status, 200, "another tenant has its own count");

    // The tool's own scopes are checked on top of [mcp]'s.
    let r = call(&read, "create_invoice", json!({"amount": 5}), &[]);
    assert_eq!(r.status, 403);
    assert!(
        r.header("www-authenticate")
            .unwrap()
            .contains("insufficient_scope")
    );
    let text = tool_error(&call(
        &both,
        "create_invoice",
        json!({}),
        &[("x-tenant", "blocked")],
    ));
    assert!(text.contains("rejected (403 Forbidden)"), "{text}");
    let r = call(&both, "create_invoice", json!({"amount": 5}), &[]);
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["json"]["params"]["arguments"]["amount"], 5);

    // Tools not in the config get only the [mcp] policies.
    let r = call(&read, "unlisted", json!({"x": 1}), &[]);
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["json"]["params"]["name"], "unlisted");
}

#[test]
fn mcp_tools_as_handlers() {
    if !needs_python() {
        return;
    }
    let cfg = format!(
        r#"{PY}
[server]
response_validation = "enforce"

[auth.oauth]
type = "jwt"
secret = "test-secret"
audience = "https://api.example.com/mcp"

[mcp]
name = "acme-tools"
version = "2.0.0"
instructions = "Tools for tests."
auth = {{ scheme = "oauth", scopes = ["tools:read"] }}

[[mcp.tool]]
name = "get_forecast"
description = "Echoes the call"
input = {{ type = "object", required = ["city"], properties = {{ city = {{ type = "string" }} }} }}
output = {{ type = "object", required = ["handler"] }}
handler = "tools/t.py#echo"
annotations = {{ read_only = true }}

[[mcp.tool]]
name = "greet"
description = "Says hello"
input = {{ type = "object", properties = {{ name = {{ type = "string" }} }} }}
handler = "tools/t.py#text"

[[mcp.tool]]
name = "picture"
description = "Returns an image"
handler = "tools/t.py#content"

[[mcp.tool]]
name = "lookup"
description = "Fails with a 404"
handler = "tools/t.py#fail"

[[mcp.tool]]
name = "crash"
description = "Sends an error message"
handler = "tools/t.py#crash"

[[mcp.tool]]
name = "bad_output"
description = "Returns text despite declaring output"
output = {{ type = "object" }}
handler = "tools/t.py#text"

[[mcp.tool]]
name = "create_invoice"
description = "Needs billing:write"
input = {{ type = "object" }}
handler = "tools/t.py#echo"
auth = {{ scheme = "oauth", scopes = ["billing:write"] }}
before = ["tools/t.py#tenant"]

[[mcp.tool]]
name = "slow"
description = "Takes too long"
handler = "tools/t.py#slow"
timeout = "500ms"
"#
    );
    let p = Project::new(&[
        ("unsworn.toml", &cfg),
        ("worker.py", ECHO_WORKER),
        ("tools/t.py", ""),
    ]);
    let s = p.serve();

    let read = mcp_token("tools:read");
    let both = mcp_token("tools:read billing:write");
    let send = |token: &str,
                method: &str,
                name: Option<&str>,
                params: serde_json::Value,
                extra: &[(&str, &str)]| {
        let mut headers = vec![
            ("authorization", token),
            ("content-type", "application/json"),
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", method),
        ];
        if let Some(n) = name {
            headers.push(("mcp-name", n));
        }
        headers.extend_from_slice(extra);
        s.request("POST", "/mcp", &headers, &mcp_body(method, params))
    };
    let result = |r: &common::Response| -> serde_json::Value {
        assert_eq!(r.status, 200, "{}", r.text());
        assert_eq!(r.header("content-type"), Some("application/json"));
        let v = r.json();
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["resultType"], "complete", "{v}");
        assert_eq!(
            v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"],
            json!({"name": "acme-tools", "version": "2.0.0"})
        );
        v["result"].clone()
    };
    let call = |token: &str, name: &str, args: serde_json::Value, extra: &[(&str, &str)]| {
        send(
            token,
            "tools/call",
            Some(name),
            json!({"name": name, "arguments": args}),
            extra,
        )
    };
    let error_text = |r: &common::Response| -> String {
        let res = result(r);
        assert_eq!(res["isError"], true, "{res}");
        res["content"][0]["text"].as_str().unwrap().to_string()
    };

    // Discovery.
    let d = result(&send(&read, "server/discover", None, json!({}), &[]));
    assert_eq!(d["supportedVersions"], json!(["2026-07-28"]));
    assert_eq!(d["capabilities"], json!({"tools": {}}));
    assert_eq!(d["instructions"], "Tools for tests.");
    assert_eq!(
        (d["ttlMs"].as_u64(), d["cacheScope"].as_str()),
        (Some(300_000), Some("public"))
    );

    // The list only shows tools the token's scopes allow, sorted by name.
    let names = |token: &str| -> Vec<String> {
        let l = result(&send(token, "tools/list", None, json!({}), &[]));
        assert_eq!(l["cacheScope"], "private");
        l["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect()
    };
    let visible = names(&read);
    assert_eq!(
        visible,
        [
            "bad_output",
            "crash",
            "get_forecast",
            "greet",
            "lookup",
            "picture",
            "slow"
        ]
    );
    assert!(names(&both).contains(&"create_invoice".to_string()));
    let l = result(&send(&read, "tools/list", None, json!({}), &[]));
    let forecast = l["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "get_forecast")
        .unwrap()
        .clone();
    assert_eq!(forecast["description"], "Echoes the call");
    assert_eq!(forecast["inputSchema"]["required"], json!(["city"]));
    assert_eq!(forecast["outputSchema"]["required"], json!(["handler"]));
    assert_eq!(forecast["annotations"], json!({"readOnlyHint": true}));
    let r = send(&read, "tools/list", None, json!({"cursor": "x"}), &[]);
    assert_eq!(
        (r.status, r.json()["error"]["code"].as_i64()),
        (400, Some(-32602))
    );

    // `json` becomes structured content plus a text copy; the handler sees `context.mcp`.
    let res = result(&call(&read, "get_forecast", json!({"city": "Lisbon"}), &[]));
    assert!(res.get("isError").is_none());
    let sc = &res["structuredContent"];
    assert_eq!(sc["json"], json!({"city": "Lisbon"}));
    assert_eq!(sc["handler"], "tools/t.py#echo");
    assert_eq!(sc["context"]["mcp"]["tool"], "get_forecast");
    assert_eq!(sc["context"]["mcp"]["protocol_version"], "2026-07-28");
    assert_eq!(sc["context"]["mcp"]["client"]["name"], "test-client");
    assert_eq!(sc["context"]["auth"]["sub"], "u1");
    let copy: serde_json::Value =
        serde_json::from_str(res["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(&copy, sc);

    // `text` and `mcp_content`.
    let res = result(&call(&read, "greet", json!({"name": "Ada"}), &[]));
    assert_eq!(
        res["content"],
        json!([{"type": "text", "text": "Hello, Ada"}])
    );
    let res = result(&call(&read, "picture", json!({}), &[]));
    assert_eq!(res["content"][0]["type"], "image");

    // Failures are tool errors the model can see.
    assert!(error_text(&call(&read, "get_forecast", json!({}), &[])).contains("Invalid arguments"));
    assert_eq!(
        error_text(&call(&read, "lookup", json!({}), &[])),
        "city not found"
    );
    assert_eq!(
        error_text(&call(&read, "crash", json!({}), &[])),
        "The tool failed.",
        "no internal details"
    );
    assert!(error_text(&call(&read, "bad_output", json!({}), &[])).contains("output schema"));

    // A tool's own scopes, then its `before` handler, whose context reaches the handler.
    let r = call(&read, "create_invoice", json!({}), &[]);
    assert_eq!(r.status, 403);
    assert!(
        error_text(&call(
            &both,
            "create_invoice",
            json!({}),
            &[("x-tenant", "blocked")]
        ))
        .contains("rejected (403 Forbidden)")
    );
    let res = result(&call(
        &both,
        "create_invoice",
        json!({"amount": 5}),
        &[("x-tenant", "acme")],
    ));
    assert_eq!(res["structuredContent"]["context"]["tenant"], "acme");
    assert_eq!(
        res["structuredContent"]["context"]["mcp"]["tool"],
        "create_invoice"
    );

    // Protocol errors.
    let r = call(&read, "nope", json!({}), &[]);
    assert_eq!(
        (r.status, r.json()["error"]["code"].as_i64()),
        (400, Some(-32602))
    );
    for method in ["resources/list", "ping"] {
        let r = send(&read, method, None, json!({}), &[]);
        assert_eq!(
            (r.status, r.json()["error"]["code"].as_i64()),
            (404, Some(-32601)),
            "{method}"
        );
    }
    let note = br#"{"jsonrpc":"2.0","method":"notifications/x"}"#;
    let r = s.request(
        "POST",
        "/mcp",
        &[
            ("authorization", read.as_str()),
            ("mcp-method", "notifications/x"),
        ],
        note,
    );
    assert_eq!(r.status, 202);

    // Last, since the slow handler keeps the only worker busy for a while.
    assert_eq!(
        error_text(&call(&read, "slow", json!({}), &[])),
        "The tool didn't finish within 0.5s."
    );
}

#[test]
fn mcp_routes_as_tools() {
    if !needs_python() {
        return;
    }
    let cfg = format!(
        r#"{PY}
[auth.oauth]
type = "jwt"
secret = "test-secret"
audience = "https://api.example.com/mcp"

[mcp]
auth = {{ scheme = "oauth", scopes = ["tools:read"] }}

[[route]]
path = "/users/{{id}}"
method = "GET"
handler = "src/app.py#echo"
params = {{ id = {{ type = "integer", min = 1 }} }}
query = {{ verbose = {{ type = "boolean" }}, limit = {{ type = "integer", default = 20 }} }}
auth = "oauth"
rate_limit = {{ requests = 2, per = "1m", key = "auth.sub" }}
mcp = {{ tool = "get_user", description = "Look up a user" }}

[[route]]
path = "/users"
method = "POST"
handler = "src/app.py#echo"
body = {{ type = "object", required = ["name"], properties = {{ name = {{ type = "string" }} }} }}
auth = "oauth"
mcp = {{ tool = "create_user", description = "Create a user" }}
"#
    );
    let p = Project::new(&[
        ("unsworn.toml", &cfg),
        ("worker.py", ECHO_WORKER),
        ("src/app.py", ""),
    ]);
    let s = p.serve();
    let token = mcp_token("tools:read");
    let call = |name: &str, args: serde_json::Value| {
        let headers = [
            ("authorization", token.as_str()),
            ("content-type", "application/json"),
            ("mcp-protocol-version", "2026-07-28"),
            ("mcp-method", "tools/call"),
            ("mcp-name", name),
        ];
        let body = mcp_body("tools/call", json!({"name": name, "arguments": args}));
        let r = s.request("POST", "/mcp", &headers, &body);
        assert_eq!(r.status, 200, "{}", r.text());
        r.json()["result"].clone()
    };
    let same = |v: &serde_json::Value| json!({"method": v["method"], "path": v["path"], "params": v["params"], "query": v["query"], "json": v["json"]});

    // The handler gets the same request either way.
    let rest = s.request(
        "GET",
        "/users/42?verbose=true",
        &[("authorization", token.as_str())],
        b"",
    );
    assert_eq!(rest.status, 200, "{}", rest.text());
    let rest = rest.json();
    let tool = call("get_user", json!({"id": 42, "verbose": true}));
    let sc = &tool["structuredContent"];
    assert_eq!(same(sc), same(&rest));
    assert_eq!(
        same(sc),
        json!({"method": "GET", "path": "/users/42", "params": {"id": 42}, "query": {"verbose": true, "limit": 20}, "json": null})
    );
    assert_eq!(
        sc["context"]["mcp"]["tool"], "get_user",
        "a tool call can still tell"
    );
    assert!(rest["context"].get("mcp").is_none());

    // REST requests and tool calls share the route's rate limit.
    let limited = call("get_user", json!({"id": 7}));
    assert_eq!(limited["isError"], true, "{limited}");
    assert!(
        limited["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Rate limit exceeded")
    );
    let r = s.request("GET", "/users/7", &[("authorization", token.as_str())], b"");
    assert_eq!(r.status, 429);

    // Body fields are top-level arguments, and arguments are validated.
    let created = call("create_user", json!({"name": "Ada"}));
    assert_eq!(
        same(&created["structuredContent"]),
        json!({"method": "POST", "path": "/users", "params": {}, "query": {}, "json": {"name": "Ada"}})
    );
    let bad = call("create_user", json!({}));
    assert_eq!(bad["isError"], true);
    assert!(bad["content"][0]["text"].as_str().unwrap().contains("name"));

    // The tools are listed with the schemas built from the routes.
    let headers = [
        ("authorization", token.as_str()),
        ("content-type", "application/json"),
        ("mcp-protocol-version", "2026-07-28"),
        ("mcp-method", "tools/list"),
    ];
    let list = s
        .request("POST", "/mcp", &headers, &mcp_body("tools/list", json!({})))
        .json();
    let tools = list["result"]["tools"].as_array().unwrap();
    let get_user = tools.iter().find(|t| t["name"] == "get_user").unwrap();
    assert_eq!(get_user["inputSchema"]["required"], json!(["id"]));
    assert_eq!(get_user["description"], "Look up a user");

    // `unsworn routes` shows the tools and where their arguments go.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_unsworn"))
        .args(["routes", "--policies"])
        .current_dir(p.path())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("src/app.py#echo (tool get_user)"), "{text}");
    assert!(
        text.contains("mcp tool get_user: id (path), limit (query), verbose (query)"),
        "{text}"
    );
    assert!(text.contains("mcp tool create_user: name (body)"), "{text}");
}
