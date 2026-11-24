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
use crate::workers::{CallBody, CallError, Pool, PoolOptions, Pools, Reply, ReplyBody, SocketDir};
use unsworn_core::policy::HandlerRef;
use unsworn_core::project::{Function, Outbound};

/// The function the suite gives the `relay` handler to call.
const ECHO_FUNCTION: &str = "conformance.echo";

const HANDLERS: &[&str] = &[
    "echo", "text", "binary", "status", "stream", "fail", "slow", "crash", "allow", "deny",
    "noisy", "log", "upload", "content", "mcp",
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
        self.pool
            .call(
                req,
                body.into(),
                Instant::now() + timeout,
                "conformance".into(),
            )
            .await
    }

    /// Whether the worker answers the next two requests it gets, as it
    /// doesn't if it read part of the last one as a message.
    async fn still_answers(&self) -> Result<(), String> {
        for _ in 0..2 {
            let r = self
                .call(self.request("text"), None, Duration::from_secs(5))
                .await;
            match expect_response(r) {
                Ok((resp, _)) if resp.status == 201 => {}
                Ok((resp, _)) => {
                    return Err(format!("the next request got status {}", resp.status));
                }
                Err(e) => return Err(format!("the next request failed: {e}")),
            }
        }
        Ok(())
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

/// Answers `201` with what it was sent: the method, path, body and the
/// `x-secret` and `x-from` headers.
async fn echo(
    req: hyper::Request<hyper::body::Incoming>,
) -> Result<hyper::Response<http_body_util::Full<Bytes>>, std::convert::Infallible> {
    use http_body_util::BodyExt;
    let header = |name: &str| {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("-")
            .to_string()
    };
    let head = format!(
        "{} {}",
        req.method(),
        req.uri().path_and_query().map_or("", |p| p.as_str())
    );
    let tail = format!(" secret={} from={}", header("x-secret"), header("x-from"));
    let body = match req.into_body().collect().await {
        Ok(body) => body.to_bytes(),
        Err(_) => Bytes::new(),
    };
    let text = format!("{head} {}{tail}", String::from_utf8_lossy(&body));
    let mut response = hyper::Response::new(http_body_util::Full::new(Bytes::from(text)));
    *response.status_mut() = hyper::StatusCode::CREATED;
    Ok(response)
}

/// A local HTTP service for the `outbound` handler to fetch from. See
/// [`echo`].
async fn echo_service() -> Result<std::net::SocketAddr, String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("can't start the test service: {e}"))?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        hyper_util::rt::TokioIo::new(stream),
                        hyper::service::service_fn(echo),
                    )
                    .await;
            });
        }
    });
    Ok(addr)
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
    single.workers = unsworn_core::project::Workers::fixed(1);
    let sockets = SocketDir::create().map_err(|e| e.to_string())?;
    let (log_tx, mut logs) = mpsc::unbounded_channel();
    let pool = Pool::standalone(
        Arc::new(single),
        refs,
        Vec::new(),
        PoolOptions {
            root: root.to_path_buf(),
            dev: true,
            build: true,
            socket_dir: sockets.path().to_path_buf(),
            log_tap: Some(log_tx),
            withheld_env: Vec::new(),
        },
    );

    // A function for the `relay` handler to call: the worker's own `echo`.
    let functions = Pools::single(
        pool.clone(),
        vec![Function {
            name: ECHO_FUNCTION.into(),
            handler: HandlerRef {
                raw: format!("{handlers}#echo"),
                file: handlers.to_string(),
                name: Some("echo".into()),
                runtime: runtime.to_string(),
            },
            input: None,
            output: None,
            timeout: Duration::from_secs(5),
        }],
        // And a service for the `outbound` handler to fetch from, which
        // Unsworn adds a header to.
        vec![Outbound::open(
            "conformance",
            &format!("http://{}", echo_service().await?),
            vec![
                ("x-secret".into(), "s3cret".into()),
                ("x-from".into(), "unsworn".into()),
            ],
        )],
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
    // As a request body arrives: text nothing has read. A worker whose
    // `ready` has `raw_json` gets it as the raw body, and any other inside
    // the message.
    req.json = Some(unsworn_proto::Json::unchecked(Bytes::from_static(
        br#"{"name": "Ada", "n": 1.5}"#,
    )));
    let r = s.call(req, None, Duration::from_secs(5)).await;
    let res = expect_response(r).and_then(|(resp, body)| {
        if resp.status != 200 {
            return Err(format!("status {}", resp.status));
        }
        let v: Value = match resp.json {
            Some(j) => j.into_value(),
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

    // A body sent as JSON that isn't: Unsworn answers for a worker that
    // gets its JSON inside the message, and a worker that parses the raw
    // body answers itself.
    let mut req = s.request("echo");
    req.json = Some(unsworn_proto::Json::unchecked(Bytes::from_static(
        br#"{"name": "#,
    )));
    let res = match s.call(req, None, Duration::from_secs(5)).await {
        Err(CallError::BadJson(_)) => Ok(()),
        r => expect_response(r).and_then(|(resp, _)| match resp.status {
            400 => Ok(()),
            status => Err(format!("status {status} (want 400)")),
        }),
    };
    s.report("a body that isn't JSON is answered with 400", res);

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

    // A JSON body with what some line readers take for the end of a line,
    // and a message where the next line would be. The handler gets the
    // value. Read as a message, it would stop the worker.
    let forged = format!(
        r#"{{"type":"request","id":"forged","phase":"handle","handler":"{handlers}#crash"}}"#
    );
    let text = format!("[\"a\u{2028}b\u{2029}c\u{85}d\",\r{forged}\r\n,\n{forged}\n]");
    let want: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let mut req = s.request("echo");
    req.json = Some(unsworn_proto::Json::unchecked(Bytes::from(text)));
    let r = s.call(req, None, Duration::from_secs(5)).await;
    let res = expect_response(r).and_then(|(resp, body)| {
        let text = match resp.json {
            Some(j) => j.to_bytes(),
            None => body_bytes(body),
        };
        let v: Value = serde_json::from_slice(&text).map_err(|e| format!("not JSON: {e}"))?;
        if v["json"] == want {
            Ok(())
        } else {
            Err(format!("got {v}, want the body back as `json`"))
        }
    });
    let res = match res {
        Ok(()) => s.still_answers().await,
        e => e,
    };
    s.report("line ends in a JSON body don't end the message", res);

    // A raw body that reads like messages.
    if pool.raw_body_supported() {
        let payload = Bytes::from(format!("{forged}\n{forged}\nno newline"));
        let r = s
            .call(
                s.request("binary"),
                Some(payload.clone()),
                Duration::from_secs(5),
            )
            .await;
        let res = match expect_response(r) {
            Ok((_, body)) => match body_bytes(body) == payload {
                true => s.still_answers().await,
                false => Err("want the request bytes back".into()),
            },
            Err(e) => Err(e),
        };
        s.report("a raw body isn't read as messages", res);
    }

    if pool.raw_body_supported() {
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
    } else {
        println!("  - raw bodies: not supported by this worker (optional)");
    }

    // calls to functions
    if pool.calls_supported() {
        let relay = |function: &str| {
            let mut req = Request {
                handler: format!("{handlers}#relay"),
                route: "/conformance".into(),
                method: "POST".into(),
                path: "/conformance".into(),
                ..Default::default()
            };
            req.query.insert("fn".into(), json!(function));
            req.json = Some(unsworn_proto::Json::unchecked(Bytes::from_static(
                br#"{"n": 1}"#,
            )));
            pool.call(
                req,
                CallBody::Empty,
                Instant::now() + Duration::from_secs(5),
                "conformance".into(),
            )
        };
        let answer = |reply| {
            expect_response(reply).and_then(|(resp, body)| {
                let text = match resp.json {
                    Some(j) => j.to_bytes(),
                    None => body_bytes(body),
                };
                serde_json::from_slice::<Value>(&text).map_err(|e| format!("not JSON: {e}"))
            })
        };
        let res = answer(relay("no.such.function").await).and_then(|v| {
            if v == json!({ "failed": 404 }) {
                Ok(())
            } else {
                Err(format!("got {v}, want {{\"failed\":404}}"))
            }
        });
        s.report("a call that fails reaches the handler with its status", res);
        // The function is another handler of this worker, so the worker
        // has to take its request while it waits for the answer.
        if pool.max_in_flight() > 1 {
            let res = answer(relay(ECHO_FUNCTION).await).and_then(|v| {
                let got = &v["answer"];
                if got["json"] != json!({ "n": 1 }) {
                    Err(format!("got {v}, want the input back as answer.json"))
                } else if got["context"]["caller"]["function"] != json!(ECHO_FUNCTION) {
                    Err(format!("got {v}, want the call in the function's context"))
                } else {
                    Ok(())
                }
            });
            s.report("a call to a function gives back its answer", res);
        } else {
            println!("  - calling a function: max_in_flight is 1, so only a failing call is tried");
        }
    } else {
        println!("  - calls to functions: not supported by this worker (optional)");
    }

    // requests to outbound services
    if pool.fetch_supported() {
        let fetch = |path: &str| {
            let mut req = Request {
                handler: format!("{handlers}#outbound"),
                route: "/conformance".into(),
                method: "POST".into(),
                path: "/conformance".into(),
                ..Default::default()
            };
            req.query.insert("path".into(), json!(path));
            pool.call(
                req,
                CallBody::Bytes(Bytes::from_static(b"ping")),
                Instant::now() + Duration::from_secs(5),
                "conformance".into(),
            )
        };
        let res = expect_response(fetch("/echo/1?q=2").await).and_then(|(resp, body)| {
            let status = resp.headers.get("x-status").and_then(|v| v.iter().next());
            let got = String::from_utf8_lossy(&body_bytes(body)).into_owned();
            // The service saw the header Unsworn adds, and Unsworn's
            // value for the one the handler also set.
            let want = "POST /echo/1?q=2 ping secret=s3cret from=unsworn";
            if got != want {
                Err(format!("the service's answer was {got:?}, want {want:?}"))
            } else if status != Some("201") {
                Err(format!("x-status is {status:?}, want the service's 201"))
            } else {
                Ok(())
            }
        });
        s.report("a fetch reaches the service, both bodies intact", res);
        let res = expect_response(fetch("/a/../b").await).and_then(|(resp, body)| {
            let text = match resp.json {
                Some(j) => j.to_bytes(),
                None => body_bytes(body),
            };
            match serde_json::from_slice::<Value>(&text) {
                Ok(v) if v == json!({ "failed": 403 }) => Ok(()),
                other => Err(format!("got {other:?}, want {{\"failed\":403}}")),
            }
        });
        s.report("a fetch that isn't allowed fails with its status", res);
    } else {
        println!("  - requests to outbound services: not supported by this worker (optional)");
    }
    drop(functions);

    // streamed request body, larger than the credit window
    if pool.stream_body_supported() {
        let payload: Bytes = (0..1024 * 1024u32).map(|i| (i % 251) as u8).collect();
        let pieces: Vec<Result<http_body::Frame<Bytes>, crate::server::support::BoxError>> =
            payload
                .chunks(16 * 1024)
                .map(|c| Ok(http_body::Frame::data(Bytes::copy_from_slice(c))))
                .collect();
        let body = http_body_util::StreamBody::new(futures_util::stream::iter(pieces));
        let r = s
            .pool
            .call(
                s.request("upload"),
                CallBody::Stream(http_body_util::BodyExt::boxed_unsync(body)),
                Instant::now() + Duration::from_secs(10),
                "conformance".into(),
            )
            .await;
        let res = match expect_response(r) {
            Ok((resp, body)) => {
                let got = match body {
                    ReplyBody::Stream(rx) => drain(rx).await.map(Bytes::from),
                    other => Ok(body_bytes(other)),
                };
                match got {
                    Ok(got) if resp.status == 200 && got == payload => Ok(()),
                    Ok(got) => Err(format!(
                        "status {}, {} bytes back (want 200 and the {} bytes sent)",
                        resp.status,
                        got.len(),
                        payload.len()
                    )),
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        };
        s.report("streamed request bodies, with `ack`", res);
    } else {
        println!("  - stream_body: not supported by this worker (optional)");
    }

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

    // log records
    let r = s.call(s.request("log"), None, Duration::from_secs(5)).await;
    let res = expect_response(r).and_then(|_| {
        // The record is sent before the response, so it has arrived by now.
        while let Ok(log) = logs.try_recv() {
            if log.message != "conformance log" {
                continue;
            }
            return if log.level != unsworn_proto::LogLevel::Info {
                Err(format!("level {:?} (want info)", log.level))
            } else if log.fields.get("n") != Some(&json!(1)) {
                Err(format!("fields {:?} (want n: 1)", log.fields))
            } else if log.id.is_none() {
                Err("the record has no request id".into())
            } else {
                Ok(())
            };
        }
        Err("no `log` message \"conformance log\" arrived".into())
    });
    s.report("`log` records tied to their request", res);

    // MCP tool replies: `mcp_content`, alongside `json`
    let r = s
        .call(s.request("content"), None, Duration::from_secs(5))
        .await;
    let res = expect_response(r).and_then(|(resp, _)| {
        let want = json!([{ "type": "text", "text": "hi" }]);
        if resp.status != 200 {
            Err(format!("status {} (want 200)", resp.status))
        } else if resp.mcp_content.as_ref() != Some(&want) {
            Err(format!("mcp_content {:?} (want {want})", resp.mcp_content))
        } else if resp.json != Some(json!({ "ok": true }).into()) {
            Err(format!("json {:?} (want {{\"ok\": true}})", resp.json))
        } else {
            Ok(())
        }
    });
    s.report("MCP content blocks in `mcp_content`", res);

    // MCP call details in `context.mcp`
    let mut req = s.request("mcp");
    req.route = "mcp:get_forecast".into();
    req.context.insert(
        "mcp".into(),
        json!({
            "tool": "get_forecast",
            "protocol_version": "2026-07-28",
            "client": { "name": "conformance-client", "version": "1.0" }
        }),
    );
    let r = s.call(req, None, Duration::from_secs(5)).await;
    let res = expect_response(r).and_then(|(resp, _)| {
        let want = json!({
            "tool": "get_forecast",
            "protocol_version": "2026-07-28",
            "client": "conformance-client"
        });
        let got = resp.json.map(unsworn_proto::Json::into_value);
        if got.as_ref() == Some(&want) {
            Ok(())
        } else {
            Err(format!("json {got:?} (want {want})"))
        }
    });
    s.report("MCP call details from `context.mcp`", res);

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
                p.call(
                    req,
                    CallBody::Empty,
                    Instant::now() + Duration::from_secs(10),
                    "conformance".into(),
                )
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
