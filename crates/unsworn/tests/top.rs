//! `unsworn top`, against a running server.

mod common;

use std::net::SocketAddr;

use common::Project;

const HANDLERS: &str = r#"
import time
from unsworn import json

def burn(req, ctx):
    end = time.perf_counter() + 0.03
    while time.perf_counter() < end:
        pass
    return json({"ok": True})

def quick(req, ctx):
    return json({"ok": True})

def broken(req, ctx):
    raise RuntimeError("no")
"#;

#[test]
fn top_ranks_routes_by_what_their_handlers_cost() {
    if !common::has("python3") {
        eprintln!("skipped: python3 isn't installed");
        return;
    }
    let admin = SocketAddr::from(([127, 0, 0, 1], common::free_port()));
    let config = format!(
        r#"
[server]
admin_listen = "{admin}"

[runtimes.py]
extensions = [".py"]
command = "python3 -u -m unsworn.worker"
workers = 1

[[route]]
path = "/burn/{{n}}"
method = "GET"
handler = "h.py#burn"

[[route]]
path = "/quick"
method = "GET"
handler = "h.py#quick"

[[route]]
path = "/broken"
method = "GET"
handler = "h.py#broken"

[[route]]
path = "/static"
respond = {{ status = 200, text = "hi" }}
"#
    );
    let p = Project::new(&[("unsworn.toml", &config), ("h.py", HANDLERS)]);
    let s = p.serve();
    for i in 0..10 {
        assert_eq!(s.get(&format!("/burn/{i}")).status, 200);
        assert_eq!(s.get("/quick").status, 200);
        assert_eq!(s.get("/static").status, 200);
    }
    assert_eq!(s.get("/broken").status, 500);

    // The address comes from the config's admin listener.
    let out = p.command(&["top", "--once"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let row = |name: &str| -> Vec<String> {
        text.lines()
            .find(|l| l.starts_with(name))
            .unwrap_or_else(|| panic!("no {name} in\n{text}"))
            .split_whitespace()
            .map(String::from)
            .collect()
    };
    // The route's pattern, not each path that matched it.
    let burn = row("GET /burn/{n}");
    let quick = row("GET /quick");
    assert_eq!(burn.last().unwrap(), "py", "{text}");
    // Nearly all of a burn request's time is the handler's, and it costs
    // about the 30 ms of CPU it burns.
    let share: f64 = burn[5].trim_end_matches('%').parse().unwrap();
    assert!(share >= 90.0, "{text}");
    let cpu: f64 = burn[6].trim_end_matches("ms").parse().unwrap();
    assert!((20.0..=100.0).contains(&cpu), "{text}");
    let quick_cpu: f64 = quick[6].trim_end_matches("ms").parse().unwrap();
    assert!(quick_cpu < cpu / 5.0, "{text}");
    // Costliest handler first; a route without one has no handler columns.
    assert!(
        text.find("GET /burn").unwrap() < text.find("GET /quick").unwrap(),
        "{text}"
    );
    let fixed = row("GET /static");
    assert_eq!(&fixed[5..], ["-", "-", "-", "-", "-"], "{text}");
    assert_eq!(row("GET /broken")[8], "100.0%", "{text}");
    // The runtime and Unsworn itself, with their memory.
    let py = row("py ");
    assert_eq!(py[1], "1", "{text}");
    assert!(
        text.lines().last().unwrap().starts_with("unsworn"),
        "{text}"
    );

    let out = p.command(&["top", "--json", "--addr", &admin.to_string()]);
    let stats: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let burn = stats["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["route"] == "/burn/{n}")
        .unwrap();
    assert_eq!(burn["count"], 10);
    assert_eq!(burn["runtime"], "py");
    assert!(burn["handler_seconds"].as_f64().unwrap() > 0.25, "{burn}");
    assert!(
        stats["runtimes"][0]["cpu_seconds"].as_f64().unwrap() > 0.2,
        "{stats}"
    );

    // The public listener doesn't hand this out.
    assert_eq!(s.get("/_unsworn/stats").status, 404);
    // And the Prometheus endpoint has the handler's share too.
    let metrics = common::http(admin, "GET", "/_unsworn/metrics", &[], b"").text();
    assert!(
        metrics.contains("unsworn_handler_duration_seconds_sum{method=\"GET\",route=\"/burn/{n}\",status=\"200\"}"),
        "{metrics}"
    );
}

#[test]
fn top_says_where_it_looked_when_nothing_answers() {
    let p = Project::new(&[(
        "unsworn.toml",
        "[server]\nadmin_listen = \"127.0.0.1:1\"\n\n[[route]]\npath = \"/a\"\nrespond = { status = 200 }\n",
    )]);
    let out = p.command(&["top", "--once"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(err.contains("can't reach Unsworn at 127.0.0.1:1"), "{err}");
}
