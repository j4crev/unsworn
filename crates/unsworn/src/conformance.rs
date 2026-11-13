//! `unsworn conformance`: checks a runtime's workers against the protocol.
//!
//! The runtime must implement the conformance handlers described in
//! `docs/conformance.md` in one file, passed with `--handlers`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;
use unsworn_proto::{Phase, Request};

use crate::load;
use crate::workers::{CallError, Pool, PoolOptions, Reply, ReplyBody, SocketDir};

const HANDLERS: &[&str] = &[
    "echo", "text", "binary", "status", "stream", "fail", "slow", "crash", "allow", "deny", "noisy",
];

struct Suite {
    pool: Arc<Pool>,
    file: String,
    passed: usize,
    failed: usize,
}

impl Suite {
    fn request(&self, name: &str) -> Request {
        Request {
            handler: format!("{}#{name}", self.file),
            route: "/conformance".into(),
            method: "POST".into(),
            path: "/conformance".into(),
            ..Default::default()
        }
    }

    async fn call(
        &self,
        req: Request,
        body: Option<Bytes>,
        timeout: Duration,
    ) -> Result<Reply, CallError> {
        self.pool.call(req, body, Instant::now() + timeout).await
    }

    fn report(&mut self, name: &str, result: Result<(), String>) {
        match result {
            Ok(()) => {
                self.passed += 1;
                println!("  ✓ {name}");
            }
            Err(e) => {
                self.failed += 1;
                println!("  ✗ {name}: {e}");
            }
        }
    }
}

fn expect_response(
    reply: Result<Reply, CallError>,
) -> Result<(unsworn_proto::Response, ReplyBody), String> {
    match reply {
        Ok(Reply::Response { response, body }) => Ok((response, body)),
        Ok(Reply::Continue(_)) => Err("got `continue`, expected a response".into()),
        Err(e) => Err(e.to_string()),
    }
}

fn body_bytes(body: ReplyBody) -> Bytes {
    match body {
        ReplyBody::Bytes(b) => b,
        _ => Bytes::new(),
    }
}

async fn drain(mut rx: mpsc::Receiver<Result<Bytes, String>>) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    while let Some(chunk) = rx.recv().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

pub async fn run(root: &Path, config: &str, runtime: &str, handlers: &str) -> Result<(), String> {
    let compiled = load::load(root, config);
    let errors = load::print_diagnostics(&compiled.diagnostics);
    let project = compiled
        .project
        .ok_or_else(|| format!("{errors} error(s) in the project"))?;
    let rt = project
        .runtimes
        .get(runtime)
        .cloned()
        .ok_or_else(|| format!("no runtime named \"{runtime}\""))?;
    if !root.join(handlers).exists() {
        return Err(format!("handlers file \"{handlers}\" not found"));
    }
    let mut refs: Vec<String> = HANDLERS.iter().map(|h| format!("{handlers}#{h}")).collect();
    refs.push(format!("{handlers}#does_not_exist"));

    // One worker keeps the crash and concurrency checks predictable.
    let mut single = (*rt).clone();
    single.workers = Some(1);
    let sockets = SocketDir::create().map_err(|e| e.to_string())?;
    let pool = Pool::standalone(
        Arc::new(single),
        refs,
        PoolOptions {
            root: root.to_path_buf(),
            dev: true,
            socket_dir: sockets.path().to_path_buf(),
        },
    );

    println!("conformance: [runtimes.{runtime}] ({:?})", rt.protocol);
    let mut s = Suite {
        pool: pool.clone(),
        file: handlers.to_string(),
        passed: 0,
        failed: 0,
    };

    let started = pool.ensure_started().await;
    s.report(
        "worker starts and sends `ready`",
        started.clone().map(|_| ()),
    );
    if started.is_err() {
        return Err("the worker didn't start".into());
    }

    // verify
    let verify = if pool.verify_supported() {
        let missing = pool.missing();
        let wanted = format!("{handlers}#does_not_exist");
        if !missing.contains_key(&wanted) {
            Err(format!("`verify` didn't report {wanted} as missing"))
        } else if missing.len() > 1 {
            Err(format!(
                "`verify` reported real handlers as missing: {missing:?}"
            ))
        } else {
            Ok(())
        }
    } else {
        println!("  - verify: not supported by this worker (optional)");
        Ok(())
    };
    s.report("verify reports missing handlers", verify);

    // echo
    let mut req = s.request("echo");
    req.params.insert("id".into(), json!(42));
    req.query.insert("tags".into(), json!(["a", "b"]));
    req.headers.insert("x-test".into(), "yes".into());
    let mut ctx = Map::new();
    ctx.insert("tenant".into(), json!("acme"));
    req.context = ctx;
    req.json = Some(json!({"name": "Ada", "n": 1.5}));
    let r = s.call(req, None, Duration::from_secs(5)).await;
    let res = expect_response(r).and_then(|(resp, body)| {
        if resp.status != 200 {
            return Err(format!("status {}", resp.status));
        }
        let v: Value = match resp.json {
            Some(j) => j,
            None => {
                serde_json::from_slice(&body_bytes(body)).map_err(|e| format!("not JSON: {e}"))?
            }
        };
        let checks = [
            (
                v["params"]["id"] == json!(42),
                "params.id should be the number 42",
            ),
            (
                v["query"]["tags"] == json!(["a", "b"]),
                "query.tags should be [\"a\",\"b\"]",
            ),
            (
                v["context"]["tenant"] == json!("acme"),
                "context.tenant should be \"acme\"",
            ),
            (
                v["json"]["name"] == json!("Ada"),
                "json.name should be \"Ada\"",
            ),
            (
                v["header"] == json!("yes"),
                "header should echo the x-test header",
            ),
            (v["method"] == json!("POST"), "method should be POST"),
        ];
        match checks.iter().find(|(ok, _)| !ok) {
            Some((_, msg)) => Err(format!("{msg}; got {v}")),
            None => Ok(()),
        }
    });
    s.report(
        "params, query, headers, context and JSON body arrive with their types",
        res,
    );

    // text + headers
    let r = s
        .call(s.request("text"), None, Duration::from_secs(5))
        .await;
    let res = expect_response(r).and_then(|(resp, body)| {
        let text = resp
            .text
            .clone()
            .unwrap_or_else(|| String::from_utf8_lossy(&body_bytes(body)).to_string());
        if resp.status != 201 {
            Err(format!("status {} (want 201)", resp.status))
        } else if text != "hello" {
            Err(format!("body {text:?} (want \"hello\")"))
        } else if !resp
            .headers
            .get("x-conformance")
            .is_some_and(|v| v.iter().any(|v| v == "yes"))
        {
            Err("missing header x-conformance: yes".into())
        } else {
            Ok(())
        }
    });
    s.report("text responses with status and headers", res);

    // binary round trip
    let payload = Bytes::from_static(b"\x00\x01binary\nbody\xff");
    let r = s
        .call(
            s.request("binary"),
            Some(payload.clone()),
            Duration::from_secs(5),
        )
        .await;
    let res = expect_response(r).and_then(|(resp, body)| {
        let got = body_bytes(body);
        if resp.status != 200 || got != payload {
            Err(format!(
                "status {}, body {got:?} (want the request bytes back)",
                resp.status
            ))
        } else {
            Ok(())
        }
    });
    s.report("raw binary bodies in both directions", res);

    // status from query
    let mut req = s.request("status");
    req.query.insert("code".into(), json!(418));
    let r = s.call(req, None, Duration::from_secs(5)).await;
    let res = expect_response(r).and_then(|(resp, _)| {
        if resp.status == 418 {
            Ok(())
        } else {
            Err(format!("status {} (want 418)", resp.status))
        }
    });
    s.report("status codes", res);

    // streaming
    let r = s
        .call(s.request("stream"), None, Duration::from_secs(5))
        .await;
    let res = match expect_response(r) {
        Ok((resp, ReplyBody::Stream(rx))) if resp.stream => match drain(rx).await {
            Ok(bytes) if bytes == b"abc" => Ok(()),
            Ok(bytes) => Err(format!(
                "streamed {:?} (want \"abc\")",
                String::from_utf8_lossy(&bytes)
            )),
            Err(e) => Err(e),
        },
        Ok(_) => Err("expected a streamed response (`stream: true`)".into()),
        Err(e) => Err(e),
    };
    s.report("streaming with `chunk` and `end`", res);

    // error
    let r = s
        .call(s.request("fail"), None, Duration::from_secs(5))
        .await;
    let res = match r {
        Err(CallError::Handler(m)) if m.contains("boom") => Ok(()),
        Err(e) => Err(format!(
            "got {e} (want an `error` message containing \"boom\")"
        )),
        Ok(_) => Err("expected an `error` message".into()),
    };
    s.report("`error` messages", res);

    // before: allow / deny
    let mut req = s.request("allow");
    req.phase = Phase::Before;
    let r = s.call(req, None, Duration::from_secs(5)).await;
    let res = match r {
        Ok(Reply::Continue(ctx)) if ctx.get("checked") == Some(&json!(true)) => Ok(()),
        Ok(Reply::Continue(ctx)) => Err(format!("continue context {ctx:?} (want checked: true)")),
        Ok(_) => Err("expected `continue`".into()),
        Err(e) => Err(e.to_string()),
    };
    s.report("before handlers: `continue` with context", res);
    let mut req = s.request("deny");
    req.phase = Phase::Before;
    let r = s.call(req, None, Duration::from_secs(5)).await;
    let res = expect_response(r).and_then(|(resp, _)| {
        if resp.status == 403 {
            Ok(())
        } else {
            Err(format!("status {} (want 403)", resp.status))
        }
    });
    s.report("before handlers: rejecting with a response", res);

    // stdout noise (Tier 1)
    let r = s
        .call(s.request("noisy"), None, Duration::from_secs(5))
        .await;
    let res = expect_response(r).and_then(|(resp, _)| {
        if resp.status == 200 {
            Ok(())
        } else {
            Err(format!("status {}", resp.status))
        }
    });
    s.report("stray output doesn't break the worker", res);

    // concurrency
    let max = pool.max_in_flight();
    if max > 1 {
        let n = max.min(4);
        let begin = Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..n {
            let req = s.request("slow");
            let p = pool.clone();
            tasks.push(tokio::spawn(async move {
                p.call(req, None, Instant::now() + Duration::from_secs(10))
                    .await
            }));
        }
        let mut ok = true;
        for t in tasks {
            ok &= matches!(t.await, Ok(Ok(Reply::Response { .. })));
        }
        let elapsed = begin.elapsed();
        let res = if !ok {
            Err("not every concurrent request succeeded".into())
        } else if elapsed > Duration::from_millis(500 * n as u64 - 100) {
            Err(format!(
                "{n} requests took {elapsed:?}; they don't seem to run concurrently"
            ))
        } else {
            Ok(())
        };
        s.report(
            &format!("{n} requests in flight at once (max_in_flight = {max})"),
            res,
        );
    } else {
        println!("  - concurrency: max_in_flight is 1 (Unsworn starts more workers instead)");
    }

    // timeout → cancel, and the worker keeps working
    let r = s
        .call(s.request("slow"), None, Duration::from_millis(100))
        .await;
    let res = match r {
        Err(CallError::Timeout) => Ok(()),
        Err(e) => Err(format!("got {e} (want a timeout)")),
        Ok(_) => Err("the slow handler answered before the deadline".into()),
    };
    s.report("timeouts send `cancel`", res);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let r = s
        .call(s.request("text"), None, Duration::from_secs(5))
        .await;
    s.report(
        "the worker still answers after a cancelled request",
        expect_response(r).map(|_| ()),
    );

    // crash → failure, then a restarted worker answers
    let r = s
        .call(s.request("crash"), None, Duration::from_secs(5))
        .await;
    let res = match r {
        Err(CallError::WorkerFailed(_)) => Ok(()),
        Err(e) => Err(format!("got {e} (want a worker failure)")),
        Ok(_) => Err("the crash handler answered".into()),
    };
    s.report("a crash mid-request fails only that request", res);
    let r = s
        .call(s.request("text"), None, Duration::from_secs(15))
        .await;
    s.report(
        "the worker is restarted after a crash",
        expect_response(r).map(|_| ()),
    );

    pool.shutdown(Duration::from_secs(1)).await;
    println!();
    println!("{} passed, {} failed", s.passed, s.failed);
    if s.failed > 0 {
        Err(String::new())
    } else {
        Ok(())
    }
}
