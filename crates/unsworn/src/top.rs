//! `top`: what each route costs on a running server, and how much of it
//! is the handler's.
//!
//! The server counts every request by route: how long it took, and how
//! long of that it waited for handlers. It also knows how much CPU each
//! runtime's workers have used. `top` reads both from `/_unsworn/stats`
//! and puts them together: a route's share of its runtime's handler time
//! is taken as its share of the runtime's CPU. That is an estimate, and a
//! good one when the handlers compute; it's off for one that spends its
//! time waiting on something else, which holds a request without using a
//! core.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{Read, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::load::FsHost;

#[derive(Debug, Clone, Default, Deserialize)]
struct Usage {
    cpu_seconds: f64,
    memory_kb: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct Runtime {
    name: String,
    workers: usize,
    cpu_seconds: f64,
    memory_kb: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct Route {
    method: String,
    route: String,
    runtime: Option<String>,
    count: u64,
    errors: u64,
    seconds: f64,
    handler_seconds: f64,
    buckets: Vec<u64>,
}

/// How a route's shadow handler has compared with the route's own.
#[derive(Debug, Clone, Default, Deserialize)]
struct Shadow {
    method: String,
    route: String,
    handler: String,
    runtime: String,
    compared: u64,
    same: u64,
    failed: u64,
    primary_seconds: f64,
    shadow_seconds: f64,
    last_difference: Option<String>,
}

/// What `/_unsworn/stats` answers with: totals since the server started.
#[derive(Debug, Clone, Deserialize)]
struct Snapshot {
    uptime_seconds: f64,
    unsworn: Usage,
    runtimes: Vec<Runtime>,
    routes: Vec<Route>,
    #[serde(default)]
    shadows: Vec<Shadow>,
    bucket_bounds_ms: Vec<f64>,
}

/// One route over a period: the difference between two snapshots.
struct Row {
    name: String,
    runtime: Option<String>,
    count: u64,
    errors: u64,
    seconds: f64,
    handler_seconds: f64,
    buckets: Vec<u64>,
    /// The runtime's CPU time taken to be this route's, in seconds.
    cpu: Option<f64>,
}

fn ms(value: f64) -> String {
    if value < 0.05 {
        "<0.05ms".into()
    } else if value < 10.0 {
        format!("{value:.2}ms")
    } else if value < 1000.0 {
        format!("{value:.0}ms")
    } else {
        format!("{:.1}s", value / 1000.0)
    }
}

fn memory(kb: u64) -> String {
    format!("{:.0} MB", kb as f64 / 1024.0)
}

/// The duration that `share` of the requests were at or under, from how
/// many fell in each bucket. A bucket's middle stands for what's in it.
fn percentile(buckets: &[u64], bounds: &[f64], share: f64) -> Option<f64> {
    let total: u64 = buckets.iter().sum();
    if total == 0 {
        return None;
    }
    let wanted = (total as f64 * share).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (i, n) in buckets.iter().enumerate() {
        seen += n;
        if seen >= wanted {
            let upper = bounds.get(i).copied()?;
            return Some(match i {
                0 => upper / 2.0,
                _ => (upper * bounds[i - 1]).sqrt(),
            });
        }
    }
    None
}

/// The table for the time between two snapshots, or since the server
/// started when there's only one. Busiest handlers first.
fn render(before: Option<&Snapshot>, now: &Snapshot, limit: usize) -> String {
    let period = match before {
        Some(b) => (now.uptime_seconds - b.uptime_seconds).max(0.001),
        None => now.uptime_seconds.max(0.001),
    };
    let earlier: BTreeMap<(&str, &str), &Route> = before
        .iter()
        .flat_map(|b| &b.routes)
        .map(|r| ((r.method.as_str(), r.route.as_str()), r))
        .collect();
    let mut rows: Vec<Row> = Vec::new();
    for r in &now.routes {
        let old = earlier.get(&(r.method.as_str(), r.route.as_str()));
        let count = r.count.saturating_sub(old.map_or(0, |o| o.count));
        if count == 0 {
            continue;
        }
        rows.push(Row {
            name: format!("{} {}", r.method, r.route),
            runtime: r.runtime.clone(),
            count,
            errors: r.errors.saturating_sub(old.map_or(0, |o| o.errors)),
            seconds: r.seconds - old.map_or(0.0, |o| o.seconds),
            handler_seconds: (r.handler_seconds - old.map_or(0.0, |o| o.handler_seconds)).max(0.0),
            buckets: r
                .buckets
                .iter()
                .enumerate()
                .map(|(i, n)| {
                    n.saturating_sub(old.and_then(|o| o.buckets.get(i)).copied().unwrap_or(0))
                })
                .collect(),
            cpu: None,
        });
    }

    // Each runtime's CPU over the period, shared out by handler time.
    let cpu_used = |name: &str, total: f64| {
        let old = before
            .and_then(|b| b.runtimes.iter().find(|r| r.name == name))
            .map_or(0.0, |r| r.cpu_seconds);
        (total - old).max(0.0)
    };
    // What each shadow did in the period. A shadow's time in its runtime
    // takes its share of that runtime's CPU, like a route's.
    let mut shadows: Vec<(Shadow, Option<f64>)> = Vec::new();
    for s in &now.shadows {
        let old = before.and_then(|b| {
            b.shadows
                .iter()
                .find(|o| o.route == s.route && o.method == s.method)
        });
        let zero = Shadow::default();
        let old = old.unwrap_or(&zero);
        let period = Shadow {
            compared: s.compared.saturating_sub(old.compared),
            same: s.same.saturating_sub(old.same),
            failed: s.failed.saturating_sub(old.failed),
            primary_seconds: (s.primary_seconds - old.primary_seconds).max(0.0),
            shadow_seconds: (s.shadow_seconds - old.shadow_seconds).max(0.0),
            ..s.clone()
        };
        if period.compared + period.failed > 0 {
            shadows.push((period, None));
        }
    }
    for runtime in &now.runtimes {
        let used = cpu_used(&runtime.name, runtime.cpu_seconds);
        let shadowed: f64 = shadows
            .iter()
            .filter(|(s, _)| s.runtime == runtime.name)
            .map(|(s, _)| s.shadow_seconds)
            .sum();
        let waited: f64 = rows
            .iter()
            .filter(|r| r.runtime.as_deref() == Some(&runtime.name))
            .map(|r| r.handler_seconds)
            .sum::<f64>()
            + shadowed;
        for (shadow, cpu) in shadows
            .iter_mut()
            .filter(|(s, _)| s.runtime == runtime.name && waited > 0.0)
        {
            *cpu = Some(used * shadow.shadow_seconds / waited);
        }
        for row in rows
            .iter_mut()
            .filter(|r| r.runtime.as_deref() == Some(&runtime.name))
        {
            row.cpu = Some(if waited > 0.0 {
                used * row.handler_seconds / waited
            } else {
                0.0
            });
        }
    }
    rows.sort_by(|a, b| {
        let key = |r: &Row| (r.cpu.unwrap_or(-1.0), r.seconds);
        key(b)
            .partial_cmp(&key(a))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out = String::new();
    let since = match before {
        Some(_) => format!("the last {period:.1}s"),
        None if period < 10.0 => format!("{period:.1}s since the server started"),
        None => format!("{period:.0}s since the server started"),
    };
    let _ = writeln!(out, "unsworn top: {since}\n");
    let width = rows
        .iter()
        .take(limit)
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(5)
        .max(5);
    let _ = writeln!(
        out,
        "{:<width$}  {:>8}  {:>8}  {:>8}  {:>10}  {:>8}  {:>6}  {:>6}  RUNTIME",
        "ROUTE", "REQ/S", "P50", "P99", "IN HANDLER", "CPU/REQ", "CPU", "ERRORS"
    );
    if rows.is_empty() {
        let _ = writeln!(out, "(no requests)");
    }
    for row in rows.iter().take(limit) {
        let p =
            |share| percentile(&row.buckets, &now.bucket_bounds_ms, share).map_or("-".into(), ms);
        let in_handler = match &row.runtime {
            Some(_) if row.seconds > 0.0 => {
                format!(
                    "{:.0}%",
                    100.0 * (row.handler_seconds / row.seconds).min(1.0)
                )
            }
            _ => "-".into(),
        };
        let (per_request, cores) = match row.cpu {
            Some(cpu) => (
                ms(cpu * 1000.0 / row.count as f64),
                format!("{:.2}", cpu / period),
            ),
            None => ("-".into(), "-".into()),
        };
        let errors = match row.errors {
            0 => "-".into(),
            n => format!("{:.1}%", 100.0 * n as f64 / row.count as f64),
        };
        let _ = writeln!(
            out,
            "{:<width$}  {:>8.1}  {:>8}  {:>8}  {:>10}  {:>8}  {:>6}  {:>6}  {}",
            row.name,
            row.count as f64 / period,
            p(0.5),
            p(0.99),
            in_handler,
            per_request,
            cores,
            errors,
            row.runtime.as_deref().unwrap_or("-"),
        );
    }
    if rows.len() > limit {
        let _ = writeln!(
            out,
            "... and {} more routes (-n to show them)",
            rows.len() - limit
        );
    }

    // Each shadow next to the handler it's compared with.
    for (shadow, cpu) in &shadows {
        let name = format!("{} {}", shadow.method, shadow.route);
        let _ = writeln!(
            out,
            "\nSHADOW of {name}: {} ({})",
            shadow.handler, shadow.runtime
        );
        let n = shadow.compared.max(1) as f64;
        let _ = writeln!(
            out,
            "  {} compared, {:.1}% the same{}",
            shadow.compared,
            100.0 * shadow.same as f64 / n,
            match shadow.failed {
                0 => String::new(),
                failed => format!(", {failed} the shadow didn't answer"),
            }
        );
        if shadow.compared > 0 {
            let _ = writeln!(
                out,
                "  time in handler  {:>8}  shadow {:>8}",
                ms(1000.0 * shadow.primary_seconds / n),
                ms(1000.0 * shadow.shadow_seconds / n)
            );
        }
        let own = rows
            .iter()
            .find(|r| r.name == name)
            .and_then(|r| Some(r.cpu? * 1000.0 / r.count as f64));
        if let (Some(own), Some(cpu)) = (own, cpu)
            && shadow.compared > 0
        {
            let _ = writeln!(
                out,
                "  CPU a request    {:>8}  shadow {:>8}",
                ms(own),
                ms(cpu * 1000.0 / n)
            );
        }
        if shadow.same < shadow.compared
            && let Some(what) = &shadow.last_difference
        {
            let _ = writeln!(out, "  last difference: {what}");
        }
    }

    let _ = writeln!(
        out,
        "\n{:<12}  {:>7}  {:>6}  {:>9}",
        "", "WORKERS", "CPU", "MEMORY"
    );
    for runtime in &now.runtimes {
        let used = cpu_used(&runtime.name, runtime.cpu_seconds);
        let _ = writeln!(
            out,
            "{:<12}  {:>7}  {:>6.2}  {:>9}",
            runtime.name,
            runtime.workers,
            used / period,
            memory(runtime.memory_kb)
        );
    }
    let own = now.unsworn.cpu_seconds - before.map_or(0.0, |b| b.unsworn.cpu_seconds);
    let _ = writeln!(
        out,
        "{:<12}  {:>7}  {:>6.2}  {:>9}",
        "unsworn",
        "",
        own.max(0.0) / period,
        memory(now.unsworn.memory_kb)
    );
    out
}

/// `GET path` from a server on this machine or its network, over plain
/// HTTP, which is what the admin listener speaks.
fn get(addr: &str, path: &str) -> Result<String, String> {
    let fail = |e: std::io::Error| format!("can't reach Unsworn at {addr}: {e}");
    let mut stream = TcpStream::connect(addr).map_err(fail)?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(fail)?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nhost: {addr}\r\nconnection: close\r\n\r\n"
    )
    .map_err(fail)?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(fail)?;
    let text = String::from_utf8_lossy(&response);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("{addr} didn't answer with HTTP"))?;
    let status = head.split(' ').nth(1).unwrap_or_default();
    if status != "200" {
        return Err(format!(
            "{addr}{path} answered {status}\n  → `unsworn top` reads the admin listener ([server].admin_listen), or a server run with `unsworn dev`"
        ));
    }
    // The server sends a length, but a proxy in between may not.
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        let mut out = String::new();
        let mut rest = body;
        while let Some((size, after)) = rest.split_once("\r\n") {
            let size = usize::from_str_radix(size.trim(), 16).unwrap_or(0);
            if size == 0 || after.len() < size {
                break;
            }
            out.push_str(&after[..size]);
            rest = after[size..].trim_start_matches("\r\n");
        }
        return Ok(out);
    }
    Ok(body.to_string())
}

/// Where to ask: the admin listener if the config has one, else where
/// `unsworn dev` listens.
fn address(root: &Path, config: &str) -> Result<String, String> {
    let host = FsHost {
        root: root.to_path_buf(),
    };
    let project = unsworn_core::compile_symbolic(&host, config).project;
    let server = project.as_ref().map(|p| &p.server);
    let addr = match server {
        Some(s) if s.admin_listen.is_some() => s.admin_listen.clone().unwrap_or_default(),
        Some(s) if s.listen_set => s.listen.clone(),
        _ => unsworn_core::config::DEFAULT_DEV_LISTEN.to_string(),
    };
    // A server listening on every address is reached on this one.
    Ok(addr
        .replace("0.0.0.0:", "127.0.0.1:")
        .replace("[::]:", "[::1]:"))
}

pub struct Options {
    pub addr: Option<String>,
    pub once: bool,
    pub json: bool,
    pub interval: f64,
    pub limit: usize,
}

pub fn run(root: &Path, config: &str, options: Options) -> Result<(), String> {
    let addr = match options.addr {
        Some(addr) => addr,
        None => address(root, config)?,
    };
    let fetch = || -> Result<(String, Snapshot), String> {
        let text = get(&addr, "/_unsworn/stats")?;
        let snapshot = serde_json::from_str(&text)
            .map_err(|e| format!("{addr} answered something that isn't stats: {e}"))?;
        Ok((text, snapshot))
    };
    let (text, mut last) = fetch()?;
    if options.json {
        println!("{text}");
        return Ok(());
    }
    if options.once {
        print!("{}", render(None, &last, options.limit));
        return Ok(());
    }
    use std::io::IsTerminal;
    let terminal = std::io::stdout().is_terminal();
    let mut shown = render(None, &last, options.limit);
    loop {
        if terminal {
            // Home, clear, then the table.
            print!("\x1b[H\x1b[2J");
        }
        println!("{shown}");
        let _ = std::io::stdout().flush();
        std::thread::sleep(Duration::from_secs_f64(options.interval.max(0.2)));
        let (_, now) = fetch()?;
        shown = render(Some(&last), &now, options.limit);
        last = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::support::{BUCKETS, bucket_bound_ms};

    fn bounds() -> Vec<f64> {
        (0..BUCKETS).map(bucket_bound_ms).collect()
    }

    /// A route whose `count` requests each took about `each_ms`.
    fn route(
        method: &str,
        path: &str,
        runtime: Option<&str>,
        count: u64,
        each_ms: f64,
        in_handler: f64,
    ) -> Route {
        let bounds = bounds();
        let mut buckets = vec![0; BUCKETS];
        let i = bounds.iter().position(|b| *b >= each_ms).unwrap();
        buckets[i] = count;
        let seconds = count as f64 * each_ms / 1000.0;
        Route {
            method: method.into(),
            route: path.into(),
            runtime: runtime.map(String::from),
            count,
            errors: 0,
            seconds,
            handler_seconds: seconds * in_handler,
            buckets,
        }
    }

    fn snapshot(uptime: f64, routes: Vec<Route>, py_cpu: f64, bun_cpu: f64) -> Snapshot {
        Snapshot {
            uptime_seconds: uptime,
            unsworn: Usage {
                cpu_seconds: uptime * 0.1,
                memory_kb: 28 * 1024,
            },
            runtimes: vec![
                Runtime {
                    name: "bun".into(),
                    workers: 1,
                    cpu_seconds: bun_cpu,
                    memory_kb: 120 * 1024,
                },
                Runtime {
                    name: "py".into(),
                    workers: 2,
                    cpu_seconds: py_cpu,
                    memory_kb: 90 * 1024,
                },
            ],
            routes,
            shadows: Vec::new(),
            bucket_bounds_ms: bounds(),
        }
    }

    #[test]
    fn a_shadow_is_shown_beside_the_handler_it_copies() {
        // /search in Python, with a Bun shadow that's five times quicker
        // and wrong for 2 requests in 100.
        let mut now = snapshot(
            10.0,
            vec![route("GET", "/search", Some("py"), 100, 20.0, 1.0)],
            2.0,
            0.3,
        );
        now.shadows.push(Shadow {
            method: "GET".into(),
            route: "/search".into(),
            handler: "src/search.ts#search".into(),
            runtime: "bun".into(),
            compared: 100,
            same: 98,
            failed: 1,
            primary_seconds: 2.0,
            shadow_seconds: 0.4,
            last_difference: Some(
                "$.items[0].score is 0.5, and 0.25 in the shadow's answer".into(),
            ),
        });
        let out = render(None, &now, 25);
        let at = out
            .find("SHADOW of GET /search: src/search.ts#search (bun)")
            .unwrap_or_else(|| panic!("{out}"));
        let section: Vec<&str> = out[at..].lines().take(5).collect();
        assert_eq!(
            section[1], "  100 compared, 98.0% the same, 1 the shadow didn't answer",
            "{out}"
        );
        assert_eq!(
            section[2].split_whitespace().collect::<Vec<_>>(),
            ["time", "in", "handler", "20ms", "shadow", "4.00ms"],
            "{out}"
        );
        // 2 CPU seconds in Python for 100 requests, 0.3 in Bun.
        assert_eq!(
            section[3].split_whitespace().collect::<Vec<_>>(),
            ["CPU", "a", "request", "20ms", "shadow", "3.00ms"],
            "{out}"
        );
        assert_eq!(
            section[4],
            "  last difference: $.items[0].score is 0.5, and 0.25 in the shadow's answer",
            "{out}"
        );
    }

    #[test]
    fn a_runtimes_cpu_is_shared_out_by_handler_time() {
        // Over 10 seconds: a Python route that takes 20 ms a request, a
        // quick one in the same runtime, a Bun route and a static one.
        let now = snapshot(
            10.0,
            vec![
                route("GET", "/search", Some("py"), 500, 20.0, 0.95),
                route("GET", "/ping", Some("py"), 500, 1.0, 0.5),
                route("GET", "/users/{id}", Some("bun"), 2000, 0.4, 0.5),
                route("GET", "/health", None, 100, 0.02, 0.0),
            ],
            9.0,
            1.0,
        );
        let out = render(None, &now, 25);
        let line = |name: &str| {
            out.lines()
                .find(|l| l.starts_with(name))
                .unwrap_or_else(|| panic!("no {name} in\n{out}"))
                .split_whitespace()
                .map(String::from)
                .collect::<Vec<_>>()
        };
        // /search has 9.5 of the 9.75 handler seconds in `py`, so about
        // 8.77 of its 9 CPU seconds: 17.5 ms a request, 0.88 cores.
        let search = line("GET /search");
        assert_eq!(search[2], "50.0", "{out}");
        assert_eq!(&search[5..], ["95%", "18ms", "0.88", "-", "py"], "{out}");
        // A percentile is the middle of the bucket it falls in, which is
        // within a fifth of the real figure.
        for shown in [&search[3], &search[4]] {
            let value: f64 = shown.trim_end_matches("ms").parse().unwrap();
            assert!((value / 20.0 - 1.0).abs() < 0.2, "{shown}");
        }
        let ping = line("GET /ping");
        assert_eq!(&ping[5..], ["50%", "0.46ms", "0.02", "-", "py"], "{out}");
        let users = line("GET /users/{id}");
        assert_eq!(users[2], "200.0", "{out}");
        assert_eq!(&users[5..], ["50%", "0.50ms", "0.10", "-", "bun"], "{out}");
        // A route with no handler has no handler columns.
        let health = line("GET /health");
        assert_eq!(
            &health[2..],
            ["10.0", "<0.05ms", "<0.05ms", "-", "-", "-", "-", "-"],
            "{out}"
        );
        // Busiest handler first.
        let order: Vec<usize> = ["GET /search", "GET /users", "GET /ping", "GET /health"]
            .iter()
            .map(|n| out.find(n).unwrap())
            .collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "{out}");
        assert_eq!(line("py "), ["py", "2", "0.90", "90", "MB"], "{out}");
        let last: Vec<&str> = out.lines().last().unwrap().split_whitespace().collect();
        assert_eq!(last, ["unsworn", "0.10", "28", "MB"], "{out}");
    }

    #[test]
    fn two_snapshots_show_what_happened_between_them() {
        let before = snapshot(
            10.0,
            vec![route("GET", "/a", Some("py"), 1000, 2.0, 0.5)],
            4.0,
            0.0,
        );
        let mut later = route("GET", "/a", Some("py"), 1100, 2.0, 0.5);
        later.errors = 10;
        let now = snapshot(
            12.0,
            vec![later, route("POST", "/b", Some("py"), 0, 1.0, 0.5)],
            4.5,
            0.0,
        );
        let out = render(Some(&before), &now, 25);
        assert!(out.starts_with("unsworn top: the last 2.0s\n"), "{out}");
        // 100 requests in 2 seconds, 10 of them errors, 0.5 CPU seconds.
        let a: Vec<&str> = out
            .lines()
            .find(|l| l.starts_with("GET /a"))
            .unwrap()
            .split_whitespace()
            .collect();
        assert_eq!(a[2], "50.0", "{out}");
        assert_eq!(&a[5..], ["50%", "5.00ms", "0.25", "10.0%", "py"], "{out}");
        // A route nothing asked for in the period isn't listed.
        assert!(!out.contains("POST /b"), "{out}");
    }

    #[test]
    fn percentiles_come_from_the_buckets() {
        let bounds = bounds();
        let mut buckets = vec![0; BUCKETS];
        let fast = bounds.iter().position(|b| *b >= 1.0).unwrap();
        let slow = bounds.iter().position(|b| *b >= 100.0).unwrap();
        buckets[fast] = 98;
        buckets[slow] = 2;
        let p50 = percentile(&buckets, &bounds, 0.5).unwrap();
        let p99 = percentile(&buckets, &bounds, 0.99).unwrap();
        assert!((0.8..=1.2).contains(&p50), "{p50}");
        assert!((80.0..=120.0).contains(&p99), "{p99}");
        assert_eq!(percentile(&[0; 4], &bounds, 0.5), None);
    }

    #[test]
    fn long_tables_are_cut() {
        let routes = (0..30)
            .map(|i| route("GET", &format!("/r{i}"), None, 10, 1.0, 0.0))
            .collect();
        let out = render(None, &snapshot(5.0, routes, 0.0, 0.0), 5);
        assert!(out.contains("... and 25 more routes"), "{out}");
    }
}
