//! The binary's host: hyper and tokio around the core's request pipeline.
//!
//! `unsworn_core::pipeline::Router` makes every decision about a request.
//! This module gives it the I/O it asks for (worker pools, the HTTP client,
//! static files, the store, the clock) and adds what only a server has:
//! request ids, `/_unsworn/health`, metrics and the access log.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::{FutureExt, StreamExt};
use http::{HeaderValue, Method, Request, Response, StatusCode, Uri};
use http_body_util::{BodyExt, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use serde_json::json;
use tokio_stream::wrappers::ReceiverStream;
use unsworn_core::MaybeSend;
use unsworn_core::pipeline::shadow::{Answer, ShadowCall, compare};
use unsworn_core::pipeline::{
    self as core, CallError, Deadline, FetchError, Host, Problem, ProxyRequest, Reply,
    StaticRequest, Step,
};
use unsworn_core::policy::HandlerRef;
use unsworn_core::trace::{TRACEPARENT, TRACESTATE};

use super::support::{Body, BoxError, empty, full, json_response};
use super::{Generation, Shared, access, proxy, static_files};
use crate::store::{Store, now_ms};
use crate::telemetry::{
    self, Attr, Decision, Ended, Outcome, Parent, Recorder, SpanRecord, Started, Steps, Tracer,
};
use crate::workers::{self, CallBody, ReplyBody, Timed};

static X_REQUEST_ID: http::HeaderName = http::HeaderName::from_static("x-request-id");

/// The zero of [`Host::monotonic`].
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

fn instant(deadline: Deadline) -> Instant {
    *EPOCH + deadline.0
}

pub fn handle(
    shared: Arc<Shared>,
    req: Request<Incoming>,
    peer: SocketAddr,
    tls: bool,
) -> BoxFuture<'static, Response<Body>> {
    Box::pin(handle_inner(shared, req, peer, tls))
}

async fn handle_inner(
    shared: Arc<Shared>,
    req: Request<Incoming>,
    peer: SocketAddr,
    tls: bool,
) -> Response<Body> {
    let start = Instant::now();
    let generation = shared.generation.load_full();
    let method = req.method().clone();
    // For the access log. A clone shares the request's bytes.
    let uri = req.uri().clone();
    let path = uri.path();
    let request_id = req
        .headers()
        .get(&X_REQUEST_ID)
        .filter(|v| v.len() <= 128 && v.to_str().is_ok())
        .cloned()
        .unwrap_or_else(|| shared.next_request_id());
    let client_ip = generation.client_ip.resolve(peer.ip(), req.headers());
    let trusted_peer = generation.client_ip.trusts(peer.ip());

    let mut in_handlers = Duration::ZERO;
    let mut runtime = None;
    // Set when the request is sampled.
    let mut traced = None;
    let (mut resp, route_label) = if path == "/_unsworn/health" {
        // Anyone can ask here, so outside dev mode the answer is only
        // whether the instance is healthy. The admin listener says why.
        (health(&shared, &generation, shared.dev), path.to_string())
    } else if path == "/_unsworn/stats" && shared.dev {
        // What `unsworn top` reads. Outside dev mode it's on the admin
        // listener only.
        (stats(&shared).await, path.to_string())
    } else {
        let mut req = req;
        if let Some(tracer) = &generation.telemetry {
            let arrival = Arrival {
                client_ip,
                tls,
                trusted_peer,
                request_id: &request_id,
            };
            traced = Traced::begin(tracer, &mut req, start, arrival);
        }
        let host = ServerHost {
            shared: shared.clone(),
            generation: generation.clone(),
            store: shared.store.load_full(),
            request_id: request_id.clone(),
            tls,
            trusted_peer,
            in_handlers: AtomicU64::new(0),
            runtime: std::sync::OnceLock::new(),
            span: traced.as_ref().map(|t| Parent::of(&t.tracer, &t.span)),
            steps: traced.as_ref().map(|t| t.steps.clone()),
        };
        let req = req.map(|b| b.map_err(BoxError::from).boxed_unsync());
        let handled = generation.router.handle(&host, req, client_ip).await;
        in_handlers = Duration::from_nanos(host.in_handlers.load(Ordering::Relaxed));
        runtime = host.runtime.into_inner();
        (into_http(handled.response), handled.route)
    };
    resp.headers_mut().insert(&X_REQUEST_ID, request_id.clone());
    let status = resp.status().as_u16();
    let elapsed = start.elapsed();
    shared.metrics.record(
        method.as_str(),
        &route_label,
        status,
        elapsed,
        in_handlers,
        runtime.as_ref(),
    );
    let trace_id = traced.as_ref().map(|t| t.span.trace_id.hex());
    access::log(&access::Entry {
        method: method.as_str(),
        path,
        status,
        route: &route_label,
        ms: (elapsed.as_secs_f64() * 1_000_000.0).round() / 1000.0,
        ip: client_ip,
        id: request_id.to_str().unwrap_or_default(),
        trace_id: trace_id.as_deref(),
    });
    if let Some(mut traced) = traced {
        let outcome = Outcome {
            route: &route_label,
            status,
            runtime: runtime.as_deref(),
            in_handlers,
        };
        traced.record(elapsed, Some(outcome));
    }
    resp
}

/// What the server knows of a request before the pipeline takes it, which
/// its span says.
struct Arrival<'a> {
    client_ip: IpAddr,
    tls: bool,
    /// Its peer is one of `[server].trusted_proxies`.
    trusted_peer: bool,
    request_id: &'a HeaderValue,
}

/// A sampled request: what's noted before the pipeline takes it. Its span
/// is recorded once it's answered, or when it's dropped without an answer,
/// as when the client goes away, so the spans below it keep their parent.
struct Traced {
    tracer: Arc<Tracer>,
    span: Started,
    /// When the request's own timing started, and the wall clock's time
    /// then.
    start: Instant,
    started_at: SystemTime,
    method: Method,
    uri: Uri,
    version: http::Version,
    client_ip: IpAddr,
    tls: bool,
    request_id: HeaderValue,
    /// What the pipeline decides, which the host is told as it goes.
    steps: Arc<Steps>,
    recorded: bool,
}

impl Traced {
    /// Samples a request. A sampled one goes on with this span as its
    /// parent, so its handlers, upstream, functions and services nest under
    /// it. Any other keeps the headers it came with, since a span id that's
    /// never sent would leave their spans without a parent. The exception is
    /// a sampled flag from a peer that isn't trusted, which is cleared.
    fn begin(
        tracer: &Arc<Tracer>,
        req: &mut Request<Incoming>,
        start: Instant,
        arrival: Arrival<'_>,
    ) -> Option<Self> {
        let span = match tracer.start(req.headers(), arrival.trusted_peer) {
            Decision::Sampled(span) => span,
            Decision::NotSampled { cleared } => {
                if let Some(value) = cleared.and_then(|c| HeaderValue::try_from(c).ok()) {
                    req.headers_mut().insert(TRACEPARENT, value);
                }
                return None;
            }
        };
        let traceparent = HeaderValue::try_from(span.traceparent()).ok()?;
        req.headers_mut().insert(TRACEPARENT, traceparent);
        // A `tracestate` belongs to the `traceparent` it came with.
        if span.new_root {
            req.headers_mut().remove(TRACESTATE);
        }
        let now = SystemTime::now();
        Some(Traced {
            tracer: tracer.clone(),
            span,
            start,
            started_at: now.checked_sub(start.elapsed()).unwrap_or(now),
            method: req.method().clone(),
            uri: req.uri().clone(),
            version: req.version(),
            client_ip: arrival.client_ip,
            tls: arrival.tls,
            request_id: arrival.request_id.clone(),
            steps: Arc::default(),
            recorded: false,
        })
    }

    /// Records the span. `outcome` is `None` for a request that's given up
    /// on before it's answered.
    fn record(&mut self, elapsed: Duration, outcome: Option<Outcome<'_>>) {
        self.recorded = true;
        let ended = Ended {
            start_unix_nano: telemetry::unix_nano(self.started_at),
            elapsed,
            method: self.method.as_str(),
            path: self.uri.path(),
            tls: self.tls,
            client: self.client_ip,
            version: self.version,
            request_id: self.request_id.to_str().unwrap_or_default(),
            answer: outcome,
        };
        let span = SpanRecord::new(&self.span, ended).with_steps(&self.steps);
        self.tracer.record(span);
    }
}

impl Drop for Traced {
    fn drop(&mut self) {
        if !self.recorded {
            self.record(self.start.elapsed(), None);
        }
    }
}

fn into_body(body: core::Body<Body>) -> Body {
    match body {
        core::Body::Empty => empty(),
        core::Body::Bytes(b) => full(b),
        core::Body::Host(b) => b,
    }
}

fn into_http(resp: core::Response<Body>) -> Response<Body> {
    resp.map(into_body)
}

fn chunk_to_frame(item: Result<Bytes, String>) -> Result<Frame<Bytes>, BoxError> {
    match item {
        Ok(bytes) => Ok(Frame::data(bytes)),
        Err(e) => Err(e.into()),
    }
}

/// Reads a body whole if it's at most `limit` bytes. Otherwise gives back a
/// body that replays what was read, then the rest (or the read error).
async fn collect_up_to(mut body: Body, limit: usize) -> Result<Bytes, Body> {
    let mut chunks: Vec<Bytes> = Vec::new();
    let mut len = 0;
    let rest = loop {
        match body.frame().await {
            None => break None,
            Some(Ok(frame)) => {
                // Trailers aren't kept; responses that cache don't send them.
                let Ok(data) = frame.into_data() else {
                    continue;
                };
                len += data.len();
                chunks.push(data);
                if len > limit {
                    break Some(Ok(body));
                }
            }
            Some(Err(e)) => break Some(Err(e)),
        }
    };
    let Some(rest) = rest else {
        let mut out = bytes::BytesMut::with_capacity(len);
        for c in chunks {
            out.extend_from_slice(&c);
        }
        return Ok(out.freeze());
    };
    let read = futures_util::stream::iter(chunks.into_iter().map(|c| Ok(Frame::data(c))));
    let rest = match rest {
        Ok(body) => StreamExt::boxed(http_body_util::BodyStream::new(body)),
        Err(e) => StreamExt::boxed(futures_util::stream::once(async move { Err(e) })),
    };
    Err(StreamBody::new(read.chain(rest)).boxed_unsync())
}

/// The I/O for one request.
struct ServerHost {
    shared: Arc<Shared>,
    generation: Arc<Generation>,
    /// Loaded once per request, so a reload that swaps the store doesn't
    /// split a request across two.
    store: Arc<Store>,
    request_id: HeaderValue,
    tls: bool,
    /// The connection came from one of `[server].trusted_proxies`.
    trusted_peer: bool,
    /// How long this request has waited for handlers, in nanoseconds, and
    /// the runtime that ran the route's own. For `unsworn top`.
    in_handlers: AtomicU64,
    runtime: std::sync::OnceLock<Arc<str>>,
    /// The request's span, when it's sampled: what the server asks of
    /// something else for it is recorded below.
    span: Option<Parent>,
    /// Where what the pipeline decides goes, when the request is sampled.
    steps: Option<Arc<Steps>>,
}

impl Host for ServerHost {
    type RequestBody = Body;
    type ResponseBody = Body;
    type Store = Arc<Store>;

    fn store(&self) -> Arc<Store> {
        self.store.clone()
    }

    fn now_ms(&self) -> u64 {
        now_ms()
    }

    fn monotonic(&self) -> Duration {
        EPOCH.elapsed()
    }

    fn dev(&self) -> bool {
        self.shared.dev
    }

    async fn read_body(&self, mut body: Body, max: u64) -> Result<Bytes, Problem> {
        // A plain loop rather than `Limited`, whose boxed error trips the
        // `Send` check on this future.
        // The declared length, when there is one, sizes the buffer once
        // instead of growing (and copying) it as the body arrives. A body
        // that comes in one piece isn't copied at all.
        let declared = http_body::Body::size_hint(&body).lower().min(max) as usize;
        let mut first: Option<Bytes> = None;
        let mut out = bytes::BytesMut::new();
        while let Some(frame) = body.frame().await {
            let Ok(frame) = frame else {
                return Err(Problem::new(
                    StatusCode::BAD_REQUEST,
                    "couldn't read the request body",
                ));
            };
            let Ok(data) = frame.into_data() else {
                continue;
            };
            let have = first.as_ref().map_or(0, Bytes::len) + out.len();
            if (have + data.len()) as u64 > max {
                return Err(Problem::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("the body is larger than {max} bytes"),
                ));
            }
            match first.take() {
                None if out.is_empty() => first = Some(data),
                None => out.extend_from_slice(&data),
                Some(one) => {
                    out.reserve(declared.max(one.len() + data.len()));
                    out.extend_from_slice(&one);
                    out.extend_from_slice(&data);
                }
            }
        }
        Ok(first.unwrap_or_else(|| out.freeze()))
    }

    /// The pool sends such a body to the worker as raw bytes, or checks it
    /// first for a worker that needs it inside the message.
    fn takes_unread_json(&self) -> bool {
        true
    }

    fn limit_body(&self, body: Body, max: u64) -> Body {
        Limited::new(body, max as usize).boxed_unsync()
    }

    async fn collect(&self, body: Body, limit: usize) -> Result<Bytes, Body> {
        collect_up_to(body, limit).await
    }

    async fn call(
        &self,
        handler: &HandlerRef,
        mut request: unsworn_proto::Request,
        body: core::Body<Body>,
        deadline: Deadline,
    ) -> Result<Reply<Body>, CallError> {
        if handler.runtime == unsworn_core::dimedb::RUNTIME {
            let Some(backend) = &self.generation.dimedb else {
                return Err(CallError::Unavailable(
                    "dimedb backend is not configured".into(),
                ));
            };
            let op = handler.name.as_deref().unwrap_or("");
            let input: serde_json::Value = match (&request.json, body) {
                (Some(unsworn_proto::Json::Value(v)), _) => v.clone(),
                (Some(unsworn_proto::Json::Raw(raw)), _) => serde_json::from_slice(raw.text())
                    .map_err(|e| CallError::BadJson(format!("invalid JSON body: {e}")))?,
                (None, core::Body::Bytes(b)) if !b.is_empty() => serde_json::from_slice(&b)
                    .map_err(|e| CallError::BadJson(format!("invalid JSON body: {e}")))?,
                (None, _) => serde_json::Value::Object(serde_json::Map::new()),
            };
            let began = Instant::now();
            let (status, json) = backend
                .dispatch(op, &input)
                .await
                .map_err(CallError::Handler)?;
            self.in_handlers
                .fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
            if request.phase == unsworn_proto::Phase::Handle {
                let _ = self.runtime.set(Arc::from(unsworn_core::dimedb::RUNTIME));
            }
            return Ok(Reply::Response {
                response: unsworn_proto::Response {
                    id: request.id,
                    status,
                    headers: std::collections::BTreeMap::new(),
                    json: Some(unsworn_proto::Json::Value(json)),
                    text: None,
                    body_len: None,
                    body_json: false,
                    stream: false,
                    mcp_content: None,
                },
                body: core::Body::Empty,
            });
        }
        let Some(pool) = self.generation.pools.get(&handler.runtime).cloned() else {
            return Err(CallError::Unavailable(format!(
                "no runtime \"{}\"",
                handler.runtime
            )));
        };
        let body = match body {
            core::Body::Empty => CallBody::Empty,
            core::Body::Bytes(b) => CallBody::Bytes(b),
            core::Body::Host(b) => CallBody::Stream(b),
        };
        let handles = request.phase == unsworn_proto::Phase::Handle;
        let span = self.span.as_ref().map(|parent| {
            let what = telemetry::handler_span(&handler.raw, &handler.runtime, handles);
            let span = parent.child(what);
            // The handler, and what it asks for, go below this span.
            request
                .headers
                .insert(TRACEPARENT.to_string(), span.traceparent());
            span
        });
        let began = Instant::now();
        let Timed { reply, waited } = pool
            .call_timed(
                request,
                body,
                instant(deadline),
                self.request_id.to_str().unwrap_or_default().to_string(),
            )
            .await;
        // Up to the handler's answer. A streamed body is the handler's
        // too, but by then the response is on its way.
        self.in_handlers
            .fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
        if handles {
            let _ = self.runtime.set(pool.name());
        }
        if let Some(mut span) = span {
            span.set(
                "unsworn.wait_ms",
                Attr::Double(waited.as_secs_f64() * 1000.0),
            );
            match &reply {
                Ok(workers::Reply::Response { response, .. }) => span.answered(response.status),
                Ok(workers::Reply::Continue(_)) => span.finish(None),
                Err(e) => span.finish(Some(e.to_string())),
            }
        }
        let reply = reply?;
        Ok(match reply {
            workers::Reply::Continue(context) => Reply::Continue(context),
            workers::Reply::Response { response, body } => Reply::Response {
                response,
                body: match body {
                    ReplyBody::Empty => core::Body::Empty,
                    ReplyBody::Bytes(b) => core::Body::Bytes(b),
                    ReplyBody::Stream(rx) => core::Body::Host(
                        StreamBody::new(ReceiverStream::new(rx).map(chunk_to_frame)).boxed_unsync(),
                    ),
                },
            },
        })
    }

    fn step(&self, step: Step<'_>) {
        if let Some(steps) = &self.steps {
            steps.note(step, telemetry::unix_nano(SystemTime::now()));
        }
    }

    fn shadow(&self, call: ShadowCall) {
        let Some(pool) = self.generation.pools.get(&call.handler.runtime).cloned() else {
            return;
        };
        let shared = self.shared.clone();
        let request_id = self.request_id.to_str().unwrap_or_default().to_string();
        let deadline = instant(call.deadline);
        // The client already has its answer on the way: this runs beside it.
        tokio::spawn(async move {
            let began = Instant::now();
            let reply = pool
                .call(call.request, call.body.into(), deadline, request_id.clone())
                .await;
            let took = began.elapsed();
            let outcome = match reply {
                Ok(workers::Reply::Response { response, body }) => {
                    let answer = Answer {
                        status: response.status,
                        body: match body {
                            ReplyBody::Empty => Some(Bytes::new()),
                            ReplyBody::Bytes(bytes) => Some(bytes),
                            ReplyBody::Stream(_) => None,
                        },
                    };
                    Ok(compare(&call.primary, &answer))
                }
                Ok(workers::Reply::Continue(_)) => {
                    Err("it answered `continue`, which only before handlers may send".to_string())
                }
                Err(e) => Err(e.to_string()),
            };
            let (counted, what) = match &outcome {
                Ok(Ok(())) => (Some(Ok(())), None),
                Ok(Err(difference)) => (Some(Err(difference.as_str())), Some(difference.clone())),
                Err(failure) => (None, Some(format!("the shadow gave no answer: {failure}"))),
            };
            let wrong = shared.shadows.record(
                &call.method,
                &call.route,
                &call.handler.raw,
                &call.handler.runtime,
                counted,
                call.primary_time,
                took,
            );
            // The first few, then now and then: a shadow that's wrong for
            // every request shouldn't fill the log.
            if let (Some(n), Some(what)) = (wrong, what)
                && (n <= 10 || n.is_multiple_of(100))
            {
                tracing::warn!(
                    target: "shadow",
                    route = %call.route,
                    shadow = %call.handler.raw,
                    id = %request_id,
                    "{what}"
                );
            }
        });
    }

    async fn proxy(&self, request: ProxyRequest<Body>) -> core::Response<Body> {
        let resp = proxy::forward(
            self.shared.clone(),
            request.proxy,
            request.request.map(into_body),
            core::proxy::ForwardInfo {
                client_ip: request.client_ip,
                tls: self.tls,
                request_id: Some(self.request_id.clone()),
                trusted_peer: self.trusted_peer,
            },
            proxy::ForCalls {
                context: request.context,
                deadline: instant(request.deadline),
                span: self.span.clone(),
            },
        )
        .await;
        resp.map(core::Body::Host)
    }

    async fn serve_static(&self, request: StaticRequest) -> core::Response<Body> {
        let resp = static_files::serve(static_files::StaticRequest {
            base: self.shared.root.join(&request.dir),
            rest: request.rest,
            index: request.index,
            spa_fallback: request.spa_fallback,
            cache_control: request.cache_control,
            method: request.method,
            headers: request.headers,
        })
        .await;
        resp.map(core::Body::Host)
    }

    async fn fetch(
        &self,
        mut request: Request<Bytes>,
        timeout: Duration,
        limit: usize,
    ) -> Result<http::Response<Bytes>, FetchError> {
        let span = self.span.as_ref().map(|parent| {
            let what = telemetry::fetch_span(request.method().as_str(), request.uri());
            let span = parent.child(what);
            if let Ok(value) = HeaderValue::try_from(span.traceparent()) {
                request.headers_mut().insert(TRACEPARENT, value);
            }
            span
        });
        let client = self.shared.client.clone();
        let call = async move {
            let resp = client
                .request(request.map(full))
                .await
                .map_err(|e| FetchError::Connect(e.to_string()))?;
            let (parts, body) = resp.into_parts();
            let body = Limited::new(body, limit)
                .collect()
                .await
                .map_err(|e| FetchError::Read(e.to_string()))?
                .to_bytes();
            Ok(http::Response::from_parts(parts, body))
        };
        let answer = tokio::time::timeout(timeout, call)
            .await
            .unwrap_or(Err(FetchError::Timeout));
        if let Some(span) = span {
            match &answer {
                Ok(response) => span.answered(response.status().as_u16()),
                Err(e) => span.finish(Some(e.to_string())),
            }
        }
        answer
    }

    /// Not an `async fn`: one would hold `future` twice over, once as its
    /// argument and once inside the timeout it awaits, and `future` is most
    /// of what a request keeps in memory.
    fn timeout<F>(
        &self,
        timeout: Duration,
        future: F,
    ) -> impl Future<Output = Option<F::Output>> + MaybeSend
    where
        F: Future + MaybeSend,
        F::Output: MaybeSend,
    {
        tokio::time::timeout(timeout, future).map(Result::ok)
    }

    /// A write to the memory store is immediate, so the next request sees
    /// it. A remote one runs in the background, so a slow store never
    /// delays the response.
    async fn background(&self, future: impl Future<Output = ()> + MaybeSend + 'static) {
        if self.store.is_local() {
            future.await;
        } else {
            tokio::spawn(future);
        }
    }
}

/// What `unsworn top` shows, as JSON: every route's requests so far, and
/// what each runtime and Unsworn itself have used.
pub async fn stats(shared: &Arc<Shared>) -> Response<Body> {
    let pools = shared.generation.load().pools.clone();
    // Reading `/proc` is file I/O.
    let usage = tokio::task::spawn_blocking(move || (pools.usage(), crate::scale::own_usage()))
        .await
        .unwrap_or_default();
    let (runtimes, own) = usage;
    let body = json!({
        "version": unsworn_core::VERSION,
        "uptime_seconds": shared.started.elapsed().as_secs_f64(),
        "unsworn": { "cpu_seconds": own.cpu_seconds, "memory_kb": own.memory_kb },
        "runtimes": runtimes,
        "routes": shared.metrics.routes(),
        "shadows": shared.shadows.all(),
        "bucket_bounds_ms": (0..crate::server::support::BUCKETS)
            .map(crate::server::support::bucket_bound_ms)
            .collect::<Vec<_>>(),
    });
    json_response(StatusCode::OK, &body)
}

/// The health report: `200` when healthy, `503` when degraded. With
/// `details`, it also names the version, the store and each runtime, which
/// is more than a stranger should learn about a deployment.
pub fn health(shared: &Shared, generation: &Generation, details: bool) -> Response<Body> {
    let runtimes = generation.pools.health();
    let degraded = runtimes
        .iter()
        .any(|(_, started, alive, crashes)| (*started && *alive == 0) || *crashes >= 5);
    // A store outage doesn't make the instance unhealthy: every instance
    // shares the store, so they'd all be pulled at once.
    let status = if degraded {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::OK
    };
    if !details {
        let body = json!({ "status": if degraded { "degraded" } else { "ok" } });
        return json_response(status, &body);
    }
    let store = shared.store.load();
    let body = json!({
        "status": if degraded { "degraded" } else { "ok" },
        "version": unsworn_core::VERSION,
        "store": { "backend": store.backend_name(), "reachable": store.is_up() },
        "runtimes": runtimes.iter().map(|(name, started, alive, crashes)| json!({
            "name": name,
            "started": started,
            "workers": alive,
            "recent_crashes": crashes,
        })).collect::<Vec<_>>(),
    });
    json_response(status, &body)
}
