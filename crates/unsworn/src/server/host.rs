//! The binary's host: hyper and tokio around the core's request pipeline.
//!
//! `unsworn_core::pipeline::Router` makes every decision about a request.
//! This module gives it the I/O it asks for (worker pools, the HTTP client,
//! static files, the store, the clock) and adds what only a server has:
//! request ids, `/_unsworn/health`, metrics and the access log.

use std::net::SocketAddr;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::future::BoxFuture;
use futures_util::{FutureExt, StreamExt};
use http::{HeaderValue, Request, Response, StatusCode};
use http_body_util::{BodyExt, Limited, StreamBody};
use hyper::body::{Frame, Incoming};
use serde_json::json;
use tokio_stream::wrappers::ReceiverStream;
use unsworn_core::MaybeSend;
use unsworn_core::pipeline::{
    self as core, CallError, Deadline, FetchError, Host, Problem, ProxyRequest, Reply,
    StaticRequest,
};
use unsworn_core::policy::HandlerRef;

use super::support::{Body, BoxError, empty, full, json_response};
use super::{Generation, Shared, access, proxy, static_files};
use crate::store::{Store, now_ms};
use crate::workers::{self, CallBody, ReplyBody};

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

    let (mut resp, route_label) = if path == "/_unsworn/health" {
        // Anyone can ask here, so outside dev mode the answer is only
        // whether the instance is healthy. The admin listener says why.
        (health(&shared, &generation, shared.dev), path.to_string())
    } else {
        let host = ServerHost {
            shared: shared.clone(),
            generation: generation.clone(),
            store: shared.store.load_full(),
            request_id: request_id.clone(),
            tls,
            trusted_peer,
        };
        let req = req.map(|b| b.map_err(BoxError::from).boxed_unsync());
        let handled = generation.router.handle(&host, req, client_ip).await;
        (into_http(handled.response), handled.route)
    };
    resp.headers_mut().insert(&X_REQUEST_ID, request_id.clone());
    let status = resp.status().as_u16();
    let elapsed = start.elapsed();
    shared
        .metrics
        .record(method.as_str(), &route_label, status, elapsed);
    access::log(&access::Entry {
        method: method.as_str(),
        path,
        status,
        route: &route_label,
        ms: (elapsed.as_secs_f64() * 1_000_000.0).round() / 1000.0,
        ip: client_ip,
        id: request_id.to_str().unwrap_or_default(),
    });
    resp
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

    fn limit_body(&self, body: Body, max: u64) -> Body {
        Limited::new(body, max as usize).boxed_unsync()
    }

    async fn collect(&self, body: Body, limit: usize) -> Result<Bytes, Body> {
        collect_up_to(body, limit).await
    }

    async fn call(
        &self,
        handler: &HandlerRef,
        request: unsworn_proto::Request,
        body: core::Body<Body>,
        deadline: Deadline,
    ) -> Result<Reply<Body>, CallError> {
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
        let reply = pool
            .call(
                request,
                body,
                instant(deadline),
                self.request_id.to_str().unwrap_or_default().to_string(),
            )
            .await?;
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
        request: Request<Bytes>,
        timeout: Duration,
        limit: usize,
    ) -> Result<http::Response<Bytes>, FetchError> {
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
        tokio::time::timeout(timeout, call)
            .await
            .unwrap_or(Err(FetchError::Timeout))
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
