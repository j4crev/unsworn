//! A route's shadow handler: it gets a copy of each request, and its
//! answers are compared with the route's own, which is all a client sees.

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::{Project, Server};
use serde_json::{Value, json};

const OLD: &str = r#"
from unsworn import json

def search(req, ctx):
    q = req.query.get("q", "")
    return json({"q": q, "hits": [q.upper()], "score": 1})

def save(req, ctx):
    return json({"saved": True})
"#;

/// The rewrite: the same answers written another way, except that it
/// gets `q=wrong` wrong and fails on `q=boom`.
const NEW: &str = r#"
from unsworn import json

def search(req, ctx):
    q = req.query.get("q", "")
    if q == "boom":
        raise RuntimeError("the rewrite has a bug")
    hits = [q.upper()] if q != "wrong" else [q]
    return json({"score": 1.0, "hits": hits, "q": q, "tenant": ctx.get("tenant")} if q == "extra" else {"score": 1.0, "hits": hits, "q": q})
"#;

fn config(admin: SocketAddr, shadow: &str) -> String {
    format!(
        r#"
[server]
admin_listen = "{admin}"

[runtimes.py]
extensions = [".py"]
command = "python3 -u -m unsworn.worker"

[[route]]
path = "/search"
method = "GET"
handler = "old.py#search"
shadow = {shadow}
"#
    )
}

fn start(shadow: &str) -> Option<(Project, Server, SocketAddr)> {
    if !common::has("python3") {
        eprintln!("skipped: python3 isn't installed");
        return None;
    }
    let admin = SocketAddr::from(([127, 0, 0, 1], common::free_port()));
    let p = Project::new(&[
        ("unsworn.toml", &config(admin, shadow)),
        ("old.py", OLD),
        ("new.py", NEW),
    ]);
    let s = p.serve();
    Some((p, s, admin))
}

/// The route's shadow figures, once `compared + failed` has reached `n`.
fn shadow_after(s: &Server, admin: SocketAddr, n: u64) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let stats = common::http(admin, "GET", "/_unsworn/stats", &[], b"").json();
        let row = stats["shadows"].get(0).cloned().unwrap_or(Value::Null);
        let done = row["compared"].as_u64().unwrap_or(0) + row["failed"].as_u64().unwrap_or(0);
        if done >= n {
            return row;
        }
        assert!(
            Instant::now() < deadline,
            "only {done} of {n}: {stats}\n{}",
            s.logs()
        );
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[test]
fn a_shadow_gets_a_copy_and_only_the_routes_own_answer_is_sent() {
    let Some((p, s, admin)) = start("\"new.py#search\"") else {
        return;
    };
    // The same answer, written with other key order and `1.0` for `1`.
    for q in ["a", "b", "c"] {
        let r = s.get(&format!("/search?q={q}"));
        assert_eq!(
            r.json(),
            json!({ "q": q, "hits": [q.to_uppercase()], "score": 1 })
        );
    }
    let row = shadow_after(&s, admin, 3);
    assert_eq!(
        (row["compared"].as_u64(), row["same"].as_u64()),
        (Some(3), Some(3)),
        "{row}"
    );
    assert_eq!(row["handler"], "new.py#search");
    assert_eq!(row["runtime"], "py");
    assert_eq!(row["last_difference"], Value::Null);

    // A wrong answer from the shadow: the client still gets the right one.
    let r = s.get("/search?q=wrong");
    assert_eq!(r.json()["hits"], json!(["WRONG"]));
    let row = shadow_after(&s, admin, 4);
    assert_eq!(
        (row["compared"].as_u64(), row["same"].as_u64()),
        (Some(4), Some(3)),
        "{row}"
    );
    assert_eq!(
        row["last_difference"],
        "$.hits[0] is \"WRONG\", and \"wrong\" in the shadow's answer"
    );
    // A shadow that fails: the client still gets its 200.
    assert_eq!(s.get("/search?q=boom").status, 200);
    let row = shadow_after(&s, admin, 5);
    assert_eq!(
        (row["compared"].as_u64(), row["failed"].as_u64()),
        (Some(4), Some(1)),
        "{row}"
    );

    // Both are logged with the request they belong to.
    let logs = s.logs();
    assert!(logs.contains("shadow: $.hits[0] is \"WRONG\""), "{logs}");
    assert!(
        logs.contains("the shadow gave no answer: handler error: the rewrite has a bug"),
        "{logs}"
    );
    assert!(
        logs.contains("route=/search shadow=new.py#search id="),
        "{logs}"
    );

    // `unsworn top` puts the two side by side.
    let out = p.command(&["top", "--once"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains("SHADOW of GET /search: new.py#search (py)"),
        "{text}"
    );
    assert!(
        text.contains("4 compared, 75.0% the same, 1 the shadow didn't answer"),
        "{text}"
    );
    assert!(text.contains("last difference: $.hits[0]"), "{text}");
}

#[test]
fn a_share_of_requests_can_be_copied() {
    let Some((_p, s, admin)) = start("{ handler = \"new.py#search\", sample = 0.25 }") else {
        return;
    };
    for i in 0..20 {
        assert_eq!(s.get(&format!("/search?q={i}")).status, 200);
    }
    let row = shadow_after(&s, admin, 5);
    std::thread::sleep(Duration::from_millis(200));
    let row_later = shadow_after(&s, admin, 5);
    assert_eq!(row["compared"], 5, "{row}");
    assert_eq!(row_later["compared"], 5, "{row_later}");
}

#[test]
fn shadows_are_checked_and_planned() {
    let admin = SocketAddr::from(([127, 0, 0, 1], 9));
    let p = Project::new(&[
        ("unsworn.toml", &config(admin, "\"new.py#search\"")),
        ("old.py", ""),
        ("new.py", ""),
    ]);
    let out = p.command(&["plan", "-o", "-"]);
    let plan = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        plan.contains("\nGET /search\n  handler            old.py#search (py)\n  shadow             new.py#search (py)\n"),
        "{plan}"
    );
    let check = |config: &str| {
        p.write("unsworn.toml", config);
        let out = p.command(&["check"]);
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    // A route that writes: allowed, with a warning about doing it twice.
    let post = config(admin, "\"new.py#search\"").replace("method = \"GET\"", "method = \"POST\"");
    let (ok, err) = check(&post);
    assert!(ok, "{err}");
    assert!(
        err.contains("anything it writes or sends is done twice"),
        "{err}"
    );
    // Mistakes.
    let (ok, err) = check(&config(admin, "\"old.py#search\""));
    assert!(
        !ok && err.contains("the shadow is the route's own handler"),
        "{err}"
    );
    let (ok, err) = check(&config(admin, "\"missing.py#search\""));
    assert!(
        !ok && err.contains("handler file \"missing.py\" not found"),
        "{err}"
    );
    let (ok, err) = check(&config(
        admin,
        "{ handler = \"new.py#search\", sample = 2 }",
    ));
    assert!(
        !ok && err.contains("sample is a share of requests"),
        "{err}"
    );
    let streamed = format!("{}stream_body = true\n", config(admin, "\"new.py#search\""));
    let (ok, err) = check(&streamed);
    assert!(
        !ok && err.contains("a streamed body can only be read once"),
        "{err}"
    );
    let on_proxy = "[runtimes.py]\nextensions = [\".py\"]\ncommand = \"x\"\n\n[[route]]\npath = \"/p\"\nproxy = \"http://app\"\nshadow = \"new.py#search\"\n";
    let (ok, err) = check(on_proxy);
    assert!(!ok && err.contains("this route has none"), "{err}");
}
