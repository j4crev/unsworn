//! The core's [`Host`] over a JavaScript [`Bridge`].

use std::future::Future;
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::{Either, select};
use http::{HeaderValue, StatusCode, header};
use js_sys::{JSON, Uint8Array};
use serde_json::{Map, Value};
use unsworn_core::MaybeSend;
use unsworn_core::pipeline::proxy::{ForwardInfo, response_headers, rewrite};
use unsworn_core::pipeline::{
    self as core, BodyError, CallError, Deadline, FetchError, Host, Problem, ProxyRequest, Reply,
    StaticRequest, problem_response,
};
use unsworn_core::policy::HandlerRef;
use unsworn_core::store::{CachedResponse, RateLimiter, ResponseCache, StoreError};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::future_to_promise;

use crate::bridge::{
    Bridge, JsBody, body_from_js, body_to_js, bytes_to_js, error_text, get, headers_from_js,
    headers_to_js, request_to_js, resolve, response_from_js,
};
use crate::statics::candidates;

/// How long a store operation may take before it counts as failed, as in
/// the binary's Redis store.
const STORE_TIMEOUT: Duration = Duration::from_secs(1);

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> f64 {
    js_sys::Date::now()
}

async fn sleep(bridge: &Bridge, d: Duration) {
    if resolve(bridge.sleep(d.as_secs_f64() * 1000.0))
        .await
        .is_err()
    {
        // A broken timer mustn't look like a timeout.
        futures_util::future::pending::<()>().await;
    }
}

async fn timed<F: Future>(bridge: &Bridge, d: Duration, future: F) -> Option<F::Output> {
    let future = std::pin::pin!(future);
    let timer = std::pin::pin!(sleep(bridge, d));
    match select(future, timer).await {
        Either::Left((v, _)) => Some(v),
        Either::Right(_) => None,
    }
}

/// Rate limits and cached responses, kept wherever the JS host keeps them.
#[derive(Clone)]
pub struct WasmStore {
    pub bridge: Bridge,
}

impl WasmStore {
    async fn op(
        &self,
        name: &str,
        promise: Result<js_sys::Promise, JsValue>,
    ) -> Result<JsValue, StoreError> {
        let result = match timed(&self.bridge, STORE_TIMEOUT, resolve(promise)).await {
            Some(Ok(v)) => return Ok(v),
            Some(Err(e)) => error_text(&e),
            None => format!("no answer within {}s", STORE_TIMEOUT.as_secs()),
        };
        tracing::warn!("store {name} failed: {result}");
        Err(StoreError(result))
    }
}

impl RateLimiter for WasmStore {
    async fn take(
        &self,
        key: &str,
        requests: u32,
        per: Duration,
    ) -> Result<Option<Duration>, StoreError> {
        let promise = self
            .bridge
            .rate_limit(key, requests, per.as_secs_f64() * 1000.0);
        let wait = self.op("rate limit", promise).await?;
        Ok(wait
            .as_f64()
            .map(|ms| Duration::from_secs_f64(ms.max(0.0) / 1000.0)))
    }
}

impl ResponseCache for WasmStore {
    async fn get(&self, key: &str) -> Result<Option<CachedResponse>, StoreError> {
        let value = self.op("cache read", self.bridge.cache_get(key)).await?;
        Ok(value
            .dyn_ref::<Uint8Array>()
            .and_then(|a| CachedResponse::decode(&a.to_vec())))
    }

    async fn put(&self, key: &str, value: CachedResponse) -> Result<(), StoreError> {
        let bytes = bytes_to_js(&value.encode());
        let ttl = value.ttl.as_secs_f64() * 1000.0;
        self.op("cache write", self.bridge.cache_put(key, &bytes, ttl))
            .await
            .map(|_| ())
    }
}

/// The I/O for one request.
pub struct WasmHost {
    pub bridge: Bridge,
    pub store: WasmStore,
    pub dev: bool,
    pub request_id: String,
    /// The client connected over HTTPS.
    pub tls: bool,
}

fn problem<B>(status: StatusCode, detail: impl Into<String>) -> core::Response<B> {
    problem_response(Problem::new(status, detail))
}

/// Parses a JSON value the bridge handed over, such as a handler's reply.
fn json_of(value: &JsValue) -> Option<Value> {
    let text = JSON::stringify(value).ok()?.as_string()?;
    serde_json::from_str(&text).ok()
}

impl Host for WasmHost {
    type RequestBody = JsBody;
    type ResponseBody = JsBody;
    type Store = WasmStore;

    fn store(&self) -> WasmStore {
        self.store.clone()
    }

    fn now_ms(&self) -> u64 {
        now_ms() as u64
    }

    fn monotonic(&self) -> Duration {
        // Isolates don't expose a finer monotonic clock than this.
        Duration::from_secs_f64(now_ms() / 1000.0)
    }

    fn dev(&self) -> bool {
        self.dev
    }

    async fn read_body(&self, body: JsBody, max: u64) -> Result<Bytes, Problem> {
        let too_large = || {
            Problem::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!("the body is larger than {max} bytes"),
            )
        };
        if body.is_none() {
            return Ok(Bytes::new());
        }
        if let Some(a) = body.as_bytes() {
            if u64::from(a.length()) > max {
                return Err(too_large());
            }
            return Ok(Bytes::from(a.to_vec()));
        }
        match resolve(self.bridge.read_body(&body.0, max as f64)).await {
            Ok(v) => match v.dyn_ref::<Uint8Array>() {
                Some(a) => Ok(Bytes::from(a.to_vec())),
                None => Err(too_large()),
            },
            Err(_) => Err(Problem::new(
                StatusCode::BAD_REQUEST,
                "couldn't read the request body",
            )),
        }
    }

    fn limit_body(&self, body: JsBody, max: u64) -> JsBody {
        if body.is_none() || body.as_bytes().is_some() {
            return body;
        }
        match self.bridge.limit_body(&body.0, max as f64) {
            Ok(limited) => JsBody(limited),
            Err(_) => body,
        }
    }

    async fn collect(&self, body: JsBody, limit: usize) -> Result<Bytes, JsBody> {
        if body.is_none() {
            return Ok(Bytes::new());
        }
        if let Some(a) = body.as_bytes() {
            return if a.length() as usize <= limit {
                Ok(Bytes::from(a.to_vec()))
            } else {
                Err(body)
            };
        }
        match resolve(self.bridge.collect(&body.0, limit as f64)).await {
            Ok(v) => match get(&v, "bytes").dyn_ref::<Uint8Array>() {
                Some(a) => Ok(Bytes::from(a.to_vec())),
                None => Err(JsBody(get(&v, "body"))),
            },
            // The bridge promises not to reject; if it does, the body's gone.
            Err(e) => {
                tracing::warn!("collecting a response body failed: {}", error_text(&e));
                Err(JsBody(JsValue::NULL))
            }
        }
    }

    async fn call(
        &self,
        _handler: &HandlerRef,
        mut request: unsworn_proto::Request,
        body: core::Body<JsBody>,
        deadline: Deadline,
    ) -> Result<Reply<JsBody>, CallError> {
        request.id = self.request_id.clone();
        let timeout = deadline.0.saturating_sub(self.monotonic());
        let message = serde_json::to_string(&request)
            .map_err(|e| CallError::WorkerFailed(format!("can't encode the request: {e}")))?;
        let promise = self
            .bridge
            .call(&message, &body_to_js(body), timeout.as_secs_f64() * 1000.0);
        let out = resolve(promise)
            .await
            .map_err(|e| CallError::WorkerFailed(error_text(&e)))?;
        let text = |key: &str| get(&out, key).as_string().unwrap_or_default();
        match get(&out, "type").as_string().as_deref() {
            Some("response") => {
                let mut value = json_of(&get(&out, "response")).unwrap_or(Value::Null);
                if let Value::Object(m) = &mut value {
                    m.entry("id")
                        .or_insert_with(|| Value::String(request.id.clone()));
                }
                let response: unsworn_proto::Response = serde_json::from_value(value)
                    .map_err(|e| CallError::WorkerFailed(format!("bad handler response: {e}")))?;
                Ok(Reply::Response {
                    response,
                    body: body_from_js(get(&out, "body")),
                })
            }
            Some("continue") => {
                let context = match json_of(&get(&out, "context")) {
                    Some(Value::Object(m)) => m,
                    _ => Map::new(),
                };
                Ok(Reply::Continue(context))
            }
            Some("error") => Err(CallError::Handler(text("message"))),
            Some("missing") => Err(CallError::MissingHandler(text("message"))),
            Some("timeout") => Err(CallError::Timeout),
            Some("body") => Err(CallError::Body(match text("error").as_str() {
                "too_large" => BodyError::TooLarge,
                "idle" => BodyError::Idle,
                _ => BodyError::Read,
            })),
            other => Err(CallError::WorkerFailed(format!(
                "unexpected call result {other:?}"
            ))),
        }
    }

    async fn proxy(&self, request: ProxyRequest<JsBody>) -> core::Response<JsBody> {
        let (mut parts, body) = request.request.into_parts();
        let info = ForwardInfo {
            client_ip: request.client_ip,
            tls: self.tls,
            request_id: http::HeaderValue::from_str(&self.request_id).ok(),
            // The platform gives the client's address, and the forwarding
            // headers the request arrived with may be the client's own, so
            // the upstream gets fresh ones.
            trusted_peer: false,
        };
        let upstream = match rewrite(&request.proxy, &mut parts, &info) {
            Ok(index) => request.proxy.upstreams[index].as_str(),
            Err(p) => return problem_response(p),
        };
        let js = request_to_js(&parts, body_to_js(body));
        match resolve(self.bridge.proxy(&js)).await {
            Ok(v) => {
                let mut resp = response_from_js(&v);
                let version = resp.version();
                response_headers(resp.headers_mut(), version);
                resp
            }
            Err(e) => {
                tracing::warn!(upstream = %upstream, "proxy error: {}", error_text(&e));
                problem(
                    StatusCode::BAD_GATEWAY,
                    format!("the upstream at {upstream} couldn't be reached"),
                )
            }
        }
    }

    async fn serve_static(&self, request: StaticRequest) -> core::Response<JsBody> {
        if request.method != "GET" && request.method != "HEAD" {
            return problem_response(
                Problem::new(
                    StatusCode::METHOD_NOT_ALLOWED,
                    "static files only accept GET and HEAD",
                )
                .header(header::ALLOW, "GET, HEAD"),
            );
        }
        let mut forwarded = http::HeaderMap::new();
        for name in [
            header::RANGE,
            header::IF_RANGE,
            header::IF_MODIFIED_SINCE,
            header::IF_NONE_MATCH,
            header::ACCEPT_ENCODING,
        ] {
            if let Some(v) = request.headers.get(&name) {
                forwarded.insert(name, v.clone());
            }
        }
        let headers = headers_to_js(&forwarded);
        for path in candidates(&request) {
            let found = match resolve(self.bridge.asset(&path, &request.method, &headers)).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("reading asset {path} failed: {}", error_text(&e));
                    return problem(StatusCode::INTERNAL_SERVER_ERROR, "couldn't read the file");
                }
            };
            if found.is_null() || found.is_undefined() {
                continue;
            }
            let mut resp = response_from_js(&found);
            if resp.status() == StatusCode::NOT_FOUND {
                continue;
            }
            if let Some(cc) = &request.cache_control
                && let Ok(v) = HeaderValue::from_str(cc)
            {
                resp.headers_mut().insert(header::CACHE_CONTROL, v);
            }
            return resp;
        }
        problem(
            StatusCode::NOT_FOUND,
            format!("/{} not found", request.rest),
        )
    }

    async fn fetch(
        &self,
        request: http::Request<Bytes>,
        timeout: Duration,
        limit: usize,
    ) -> Result<http::Response<Bytes>, FetchError> {
        let (parts, body) = request.into_parts();
        let body = if body.is_empty() {
            JsValue::NULL
        } else {
            bytes_to_js(&body).into()
        };
        let js = request_to_js(&parts, body);
        let promise = self
            .bridge
            .fetch(&js, timeout.as_secs_f64() * 1000.0, limit as f64);
        let v = resolve(promise)
            .await
            .map_err(|e| FetchError::Connect(error_text(&e)))?;
        if let Some(kind) = get(&v, "error").as_string() {
            let message = get(&v, "message").as_string().unwrap_or_default();
            return Err(match kind.as_str() {
                "timeout" => FetchError::Timeout,
                "read" => FetchError::Read(message),
                _ => FetchError::Connect(message),
            });
        }
        let status = get(&v, "status")
            .as_f64()
            .and_then(|s| StatusCode::from_u16(s as u16).ok())
            .ok_or_else(|| FetchError::Read("no status".into()))?;
        let body = get(&v, "body")
            .dyn_ref::<Uint8Array>()
            .map(|a| Bytes::from(a.to_vec()))
            .unwrap_or_default();
        let mut resp = http::Response::new(body);
        *resp.status_mut() = status;
        *resp.headers_mut() = headers_from_js(&get(&v, "headers"));
        Ok(resp)
    }

    async fn timeout<F>(&self, timeout: Duration, future: F) -> Option<F::Output>
    where
        F: Future + MaybeSend,
        F::Output: MaybeSend,
    {
        timed(&self.bridge, timeout, future).await
    }

    async fn background(&self, future: impl Future<Output = ()> + MaybeSend + 'static) {
        let promise = future_to_promise(async move {
            future.await;
            Ok(JsValue::UNDEFINED)
        });
        if self.bridge.wait_until(&promise).is_err() {
            tracing::warn!("the host couldn't keep a background task alive");
        }
    }
}
