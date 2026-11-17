//! How many workers a runtime runs: one to start with, more when they're
//! busy, fewer again when they aren't, and none left behind by a reload.

mod common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::{Project, Server};

fn raw_worker() -> String {
    std::fs::read_to_string(common::repo_root().join("examples/raw-python/worker.py")).unwrap()
}

/// A project whose `raw` runtime is the worker written without an SDK,
/// which takes one request at a time. Health details are on `admin`.
fn project(workers: &str, admin: SocketAddr) -> Project {
    let config = format!(
        r#"
[server]
admin_listen = "{admin}"

[runtimes.raw]
extensions = [".py"]
command = "python3 -u worker.py"
{workers}

[[route]]
path = "/slow"
handler = "worker.py#slow"

[[route]]
path = "/text"
handler = "worker.py#text"
"#
    );
    Project::new(&[("unsworn.toml", &config), ("worker.py", &raw_worker())])
}

fn admin_addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], common::free_port()))
}

/// The number of workers the health report gives for a runtime.
fn workers(admin: SocketAddr, runtime: &str) -> u64 {
    let health = common::http(admin, "GET", "/_unsworn/health", &[], b"").json();
    health["runtimes"]
        .as_array()
        .and_then(|list| list.iter().find(|r| r["name"] == runtime))
        .and_then(|r| r["workers"].as_u64())
        .unwrap_or_else(|| panic!("no runtime {runtime} in {health}"))
}

fn wait_for(what: &str, server: &Server, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(
            Instant::now() < deadline,
            "never happened: {what}\n{}",
            server.logs()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Sends `n` requests at once and waits for all of them.
fn together(server: &Server, path: &'static str, n: usize) -> Vec<u16> {
    let addr = server.addr;
    let threads: Vec<_> = (0..n)
        .map(|_| std::thread::spawn(move || common::http(addr, "GET", path, &[], b"").status))
        .collect();
    threads.into_iter().map(|t| t.join().unwrap()).collect()
}

/// The processes a server started that are still running.
fn children(server: &Server) -> Vec<u32> {
    let parent = server.pid().to_string();
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        // "pid (command) state ppid ...", and a zombie isn't running.
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
            continue;
        };
        let mut fields = rest.split_ascii_whitespace();
        if fields.next() != Some("Z") && fields.next() == Some(parent.as_str()) {
            out.push(pid);
        }
    }
    out
}

#[test]
fn starts_with_one_worker_and_adds_more_when_calls_wait() {
    if !common::has("python3") {
        eprintln!("skipped: python3 isn't installed");
        return;
    }
    let admin = admin_addr();
    let p = project("workers = { min = 1, max = 3 }", admin);
    let s = p.serve();
    assert_eq!(s.get("/text").status, 201);
    assert_eq!(workers(admin, "raw"), 1);

    // Three requests of half a second each, for a worker that takes one
    // at a time.
    let began = Instant::now();
    assert_eq!(together(&s, "/slow", 3), [200, 200, 200]);
    assert_eq!(workers(admin, "raw"), 3, "{}", s.logs());
    assert!(
        s.logs()
            .contains("added a worker: calls were waiting for a worker"),
        "{}",
        s.logs()
    );
    // One after the other would have taken a second and a half.
    assert!(
        began.elapsed() < Duration::from_millis(1400),
        "{:?}",
        began.elapsed()
    );

    // And never more than the most allowed.
    assert_eq!(together(&s, "/slow", 6), [200; 6]);
    assert_eq!(workers(admin, "raw"), 3);
}

#[test]
fn a_number_is_exactly_that_many() {
    if !common::has("python3") {
        eprintln!("skipped: python3 isn't installed");
        return;
    }
    let admin = admin_addr();
    let p = project("workers = 2", admin);
    let s = p.serve();
    assert_eq!(s.get("/text").status, 201);
    assert_eq!(workers(admin, "raw"), 2);
    assert_eq!(together(&s, "/slow", 4), [200; 4]);
    assert_eq!(workers(admin, "raw"), 2);
}

#[test]
fn retires_the_workers_it_can_do_without() {
    if !common::has("python3") {
        eprintln!("skipped: python3 isn't installed");
        return;
    }
    let admin = admin_addr();
    let p = project("workers = { min = 1, max = 3, idle = \"1s\" }", admin);
    let s = p.serve();
    assert_eq!(together(&s, "/slow", 3), [200, 200, 200]);
    assert_eq!(workers(admin, "raw"), 3, "{}", s.logs());

    wait_for("back to one worker", &s, || workers(admin, "raw") == 1);
    // The retired ones have exited, and weren't taken for crashes.
    wait_for("their processes gone", &s, || children(&s).len() == 1);
    assert!(!s.logs().contains("restarting"), "{}", s.logs());
    assert!(s.logs().contains("retiring a worker"), "{}", s.logs());
    // The one that's left still answers, and the pool grows again.
    assert_eq!(s.get("/text").status, 201);
    assert_eq!(together(&s, "/slow", 2), [200, 200]);
    assert_eq!(workers(admin, "raw"), 2);
}

#[test]
fn a_reload_stops_the_workers_it_replaces() {
    if !common::has("python3") {
        eprintln!("skipped: python3 isn't installed");
        return;
    }
    let admin = admin_addr();
    let p = project("workers = 2", admin);
    let s = p.serve();
    assert_eq!(s.get("/text").status, 201);
    let before = children(&s);
    assert_eq!(before.len(), 2, "{before:?}");

    std::process::Command::new("kill")
        .arg("-HUP")
        .arg(s.pid().to_string())
        .status()
        .unwrap();
    wait_for("the reload", &s, || s.logs().contains("reloaded"));
    assert_eq!(s.get("/text").status, 201);
    wait_for("only the new workers left", &s, || {
        let now = children(&s);
        now.len() == 2 && now.iter().all(|pid| !before.contains(pid))
    });
}

/// A worker that takes many requests at once never makes a call wait, so
/// it's its CPU time that says it's busy.
#[test]
fn adds_a_worker_when_one_uses_a_whole_core() {
    if !common::has("python3") {
        eprintln!("skipped: python3 isn't installed");
        return;
    }
    let admin = admin_addr();
    let config = format!(
        r#"
[server]
admin_listen = "{admin}"

[runtimes.py]
extensions = [".py"]
command = "python3 -u -m unsworn.worker"
workers = {{ min = 1, max = 2 }}

[[route]]
path = "/burn"
handler = "burn.py#burn"

[[route]]
path = "/idle"
handler = "burn.py#idle"
"#
    );
    let handlers = r#"
import time
from unsworn import json

def burn(req, ctx):
    end = time.perf_counter() + 0.02
    while time.perf_counter() < end:
        pass
    return json({"ok": True})

def idle(req, ctx):
    return json({"ok": True})
"#;
    let p = Project::new(&[("unsworn.toml", &config), ("burn.py", handlers)]);
    let s = p.serve();

    // A few requests a second don't keep it busy.
    for _ in 0..10 {
        assert_eq!(s.get("/idle").status, 200);
        std::thread::sleep(Duration::from_millis(60));
    }
    assert_eq!(workers(admin, "py"), 1, "{}", s.logs());

    // Four callers at once do.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let addr = s.addr;
    let callers: Vec<_> = (0..4)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    common::http(addr, "GET", "/burn", &[], b"");
                }
            })
        })
        .collect();
    wait_for("a second worker", &s, || workers(admin, "py") == 2);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for c in callers {
        c.join().unwrap();
    }
    assert!(
        s.logs().contains("added a worker: its workers were busy"),
        "{}",
        s.logs()
    );
}
